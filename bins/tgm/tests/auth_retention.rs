use anyhow::{Context, Result};
use sqlx::PgPool;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn command(url: &str, category: &str) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_tgm"));
    process
        .env_clear()
        .env("TGM_PROFILE", "development")
        .env("TGM_DB_URL_AUTH_CLEANUP", url)
        .args(["cleanup-auth", "--category", category]);
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
async fn cli_cleans_fixed_categories_rejects_wrong_roles_and_stops_on_sigterm(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let cleanup_url = format!(
        "postgres://auth_cleanup:cpw@127.0.0.1:55432/{database}?application_name=tgm-auth-retention-cli"
    );
    for category in ["challenges", "web-sessions", "bot-sessions"] {
        let output = command(&cleanup_url, category).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("batches=1"));
    }
    let mut lock = admin.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(tgm_db::auth_retention::Category::Challenges.lock_key())
        .execute(&mut *lock)
        .await?;
    let output = command(&cleanup_url, "challenges").output()?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("overlapped=true"));
    lock.rollback().await?;
    for (role, password) in [
        ("app_user", "apw"),
        ("auth_lookup", "lpw"),
        ("postgres", "pw"),
    ] {
        let output = command(
            &format!("postgres://{role}:{password}@127.0.0.1:55432/{database}"),
            "challenges",
        )
        .output()?;
        assert!(!output.status.success(), "错误角色必须拒绝");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("postgres://") && !stderr.contains(password));
    }
    let output = command(&cleanup_url, "challenges")
        .env("TGM_DB_URL_OWNER", "sensitive-owner-url")
        .output()?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-owner-url"));
    let output = command(&cleanup_url, "unknown").output()?;
    assert!(!output.status.success());
    sqlx::query("INSERT INTO console_login_challenges (nonce, confirm_code, origin_ip, expires_at) VALUES (decode('01','hex'), 'ABCD', '127.0.0.1', now() - interval '1 hour')").execute(&admin).await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_cleanup CHECK (event <> 'cleanup.done') NOT VALID; RESET ROLE;")
        .execute(&admin).await?;
    let output = command(&cleanup_url, "challenges").output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("回滚"));
    for sensitive in ["reject_cleanup", "postgres://", "INSERT INTO", "ABCD"] {
        assert!(!stderr.contains(sensitive));
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM console_login_challenges")
            .fetch_one(&admin)
            .await?,
        1
    );
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_cleanup; REVOKE DELETE ON console_login_challenges FROM auth_cleanup; RESET ROLE;")
        .execute(&admin).await?;
    let output = command(&cleanup_url, "challenges").output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("回滚"));
    assert!(!stderr.contains("permission denied") && !stderr.contains("postgres://"));
    sqlx::raw_sql(
        "SET ROLE tgm_owner; GRANT DELETE ON console_login_challenges TO auth_cleanup; RESET ROLE;",
    )
    .execute(&admin)
    .await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE console_login_challenges SET confirm_code = 'EFGH' WHERE nonce = decode('01','hex')").execute(&mut *held).await?;
    let mut process = CleanupProcess(
        command(&cleanup_url, "challenges")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let child = &mut process.0;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid = locks.pid WHERE activity.datname = current_database() AND activity.application_name = 'tgm-auth-retention-cli' AND NOT locks.granted)").fetch_one(&admin).await?;
        if waiting {
            break;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("清理子进程没有等待行锁");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let sent = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()?;
    assert!(sent.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait()? {
            assert!(status.success());
            break;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("清理子进程未及时退出");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    held.rollback().await?;
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM console_login_challenges")
        .fetch_one(&admin)
        .await?;
    assert_eq!(remaining, 1);
    let output = command(&cleanup_url, "challenges")
        .output()
        .context("退出后重试")?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("deleted=1"));
    Ok(())
}

#[test]
fn scheduling_units_preserve_credential_separation_and_fixed_frequencies() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/systemd");
    let service = std::fs::read_to_string(root.join("tgm-auth-cleanup@.service"))?;
    assert!(service.contains("User=tgm-auth-cleanup"));
    assert!(service.contains("EnvironmentFile=/etc/tgm/auth-cleanup.env"));
    assert!(service.contains("ExecStart=/usr/local/bin/tgm cleanup-auth --category %i"));
    assert!(service.contains("KillMode=control-group"));
    for (category, calendar) in [
        ("challenges", "*-*-* *:00/10:00"),
        ("web-sessions", "hourly"),
        ("bot-sessions", "daily"),
    ] {
        let timer =
            std::fs::read_to_string(root.join(format!("tgm-auth-cleanup-{category}.timer")))?;
        assert!(timer.contains(&format!("OnCalendar={calendar}\n")));
        assert!(timer.contains(&format!("Unit=tgm-auth-cleanup@{category}.service\n")));
        assert!(timer.contains("Persistent=true"));
    }
    Ok(())
}
