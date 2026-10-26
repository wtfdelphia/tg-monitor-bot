use anyhow::{Context, Result};
use sqlx::PgPool;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn command(url: &str, category: &str) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_tgm"));
    process
        .env_clear()
        .env("TGM_PROFILE", "development")
        .env("TGM_DB_URL_TENANT_CLEANUP", url)
        .args(["cleanup-tenant", "--category", category]);
    process
}

struct CleanupProcess(Child);

impl Drop for CleanupProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cli_uses_only_cleanup_identity_redacts_failures_and_stops_on_sigterm(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let cleanup_url = format!(
        "postgres://tenant_cleanup:tcpw@127.0.0.1:55432/{database}?application_name=tgm-tenant-retention-cli"
    );
    sqlx::query("INSERT INTO tenants (name,owner_user_id,status) VALUES ('cli',94001,'suspended')")
        .execute(&admin)
        .await?;
    for category in [
        "unmatched-events",
        "tenant-events",
        "delivery-tasks",
        "login-sessions",
        "notify-dedup",
        "bot-console-updates",
        "delivery-logs",
        "audit-logs",
        "system-logs",
    ] {
        let output = command(&cleanup_url, category).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("tenants=1"));
    }
    for (role, password) in [
        ("app_user", "apw"),
        ("auth_lookup", "lpw"),
        ("auth_cleanup", "cpw"),
        ("postgres", "pw"),
    ] {
        let output = command(
            &format!("postgres://{role}:{password}@127.0.0.1:55432/{database}"),
            "login-sessions",
        )
        .output()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("postgres://"));
    }
    let output = command(&cleanup_url, "login-sessions")
        .env("TGM_DB_URL_AUTH_CLEANUP", "sensitive-other-url")
        .output()?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-other-url"));
    assert!(!command(&cleanup_url, "unknown").output()?.status.success());
    sqlx::query("INSERT INTO login_sessions (tenant_id,kind,state,expires_at) SELECT id,'qr','expired',now()-interval '1 day' FROM tenants").execute(&admin).await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_tenant_cleanup CHECK (module <> 'tenant_retention') NOT VALID; RESET ROLE;").execute(&admin).await?;
    let output = command(&cleanup_url, "login-sessions").output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("回滚"));
    for sensitive in [
        "reject_tenant_cleanup",
        "postgres://",
        "INSERT INTO",
        "tcpw",
    ] {
        assert!(!stderr.contains(sensitive));
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM login_sessions")
            .fetch_one(&admin)
            .await?,
        1
    );
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_tenant_cleanup; REVOKE DELETE ON login_sessions FROM tenant_cleanup; RESET ROLE;").execute(&admin).await?;
    let output = command(&cleanup_url, "login-sessions").output()?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("permission denied"));
    sqlx::raw_sql(
        "SET ROLE tgm_owner; GRANT DELETE ON login_sessions TO tenant_cleanup; RESET ROLE;",
    )
    .execute(&admin)
    .await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE login_sessions SET state='failed'")
        .execute(&mut *held)
        .await?;
    let mut process = CleanupProcess(
        command(&cleanup_url, "login-sessions")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.application_name='tgm-tenant-retention-cli' AND NOT locks.granted)").fetch_one(&admin).await?;
        if waiting {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "清理子进程未等到行锁");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &process.0.id().to_string()])
            .status()?
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = process.0.try_wait()? {
            assert!(status.success());
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "清理进程未及时退出");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM login_sessions")
            .fetch_one(&admin)
            .await?,
        1
    );
    let output = command(&cleanup_url, "login-sessions")
        .output()
        .context("退出后重跑")?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("deleted=1"));
    Ok(())
}

#[test]
fn scheduling_units_keep_tenant_cleanup_credentials_separate() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd");
    let service = std::fs::read_to_string(root.join("tgm-tenant-cleanup@.service"))?;
    for required in [
        "User=tgm-tenant-cleanup",
        "EnvironmentFile=/etc/tgm/tenant-cleanup.env",
        "ExecStart=/usr/local/bin/tgm cleanup-tenant --category %i",
        "KillMode=control-group",
    ] {
        assert!(service.contains(required));
    }
    for (category, calendar) in [
        ("tenant-events", "daily"),
        ("delivery-tasks", "daily"),
        ("login-sessions", "*-*-* *:00/10:00"),
        ("notify-dedup", "hourly"),
        ("bot-console-updates", "daily"),
        ("delivery-logs", "daily"),
        ("audit-logs", "daily"),
        ("system-logs", "daily"),
    ] {
        let timer =
            std::fs::read_to_string(root.join(format!("tgm-tenant-cleanup-{category}.timer")))?;
        assert!(timer.contains(&format!("OnCalendar={calendar}\n")));
        assert!(timer.contains(&format!("Unit=tgm-tenant-cleanup@{category}.service\n")));
        assert!(timer.contains("Persistent=true"));
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn unmatched_cli_reports_protected_rows_and_refuses_incomplete_cycle(
    admin: PgPool,
) -> Result<()> {
    sqlx::raw_sql("INSERT INTO tenants (name,owner_user_id) VALUES ('short-cli',99202);
        INSERT INTO identities (kind) VALUES ('mtproto');
        INSERT INTO source_subscriptions (tenant_id,tg_chat_id,receiver_identity_id)
            SELECT tenants.id,-1001,identities.id FROM tenants CROSS JOIN identities;
        INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,created_at,fanned_out)
            SELECT 'mtproto',id,-1001,1,now(),now()-interval '2 days',true FROM identities;
        INSERT INTO tenant_events (tenant_id,canonical_event_id,source_ref)
            SELECT sources.tenant_id,events.id,sources.id FROM source_subscriptions sources CROSS JOIN canonical_events events;")
        .execute(&admin).await?;
    let tenant: tgm_core::ids::TenantId = sqlx::query_scalar("SELECT id FROM tenants")
        .fetch_one(&admin)
        .await?;
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let url = format!("postgres://tenant_cleanup:tcpw@127.0.0.1:55432/{database}");
    let output = command(&url, "unmatched-events").output()?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("retained=1"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("受保护"));
    let cleanup = sqlx::postgres::PgPoolOptions::new().connect(&url).await?;
    let mut held = cleanup.begin().await?;
    assert!(
        tgm_db::tenant_retention::try_lock(
            &mut held,
            tenant,
            tgm_db::tenant_retention::Category::UnmatchedEvents
        )
        .await?
    );
    let output = command(&url, "unmatched-events").output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("overlaps=1"));
    held.rollback().await?;
    sqlx::query("UPDATE tenant_events SET processed_at=now()")
        .execute(&admin)
        .await?;
    let output = command(&url, "unmatched-events").output()?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("deleted=1 retained=0"));
    Ok(())
}
