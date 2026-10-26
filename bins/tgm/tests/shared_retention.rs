use anyhow::Result;
use sqlx::PgPool;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct CleanupProcess(Child);

impl Drop for CleanupProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tgm"));
    command
        .env_clear()
        .env("TGM_PROFILE", "development")
        .env("TGM_DB_URL_SHARED_CLEANUP", url)
        .arg("cleanup-shared");
    command
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cli_uses_only_its_own_credentials_and_redacts_database_failures(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let url = format!("postgres://shared_cleanup:scpw@127.0.0.1:55432/{database}");
    let output = command(&url).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("batches=1"));
    for (role, password) in [
        ("app_user", "apw"),
        ("auth_lookup", "lpw"),
        ("tenant_cleanup", "tcpw"),
        ("auth_cleanup", "cpw"),
        ("postgres", "pw"),
    ] {
        let output = command(&format!(
            "postgres://{role}:{password}@127.0.0.1:55432/{database}"
        ))
        .output()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("postgres://"));
    }
    for name in [
        "TGM_DB_URL_OWNER",
        "TGM_DB_URL_APP",
        "TGM_DB_URL_AUTH_CLEANUP",
        "TGM_DB_URL_TENANT_CLEANUP",
        "DATABASE_URL",
        "TGM_LOCAL_KEK",
    ] {
        let output = command(&url).env(name, "private-value").output()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-value"));
    }
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_shared CHECK (module <> 'shared_retention') NOT VALID; RESET ROLE;")
        .execute(&admin).await?;
    let output = command(&url).output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("回滚"));
    assert!(!stderr.contains("reject_shared") && !stderr.contains("postgres://"));
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_shared; RESET ROLE;",
    )
    .execute(&admin)
    .await?;
    let identity: i64 =
        sqlx::query_scalar("INSERT INTO identities (kind) VALUES ('mtproto') RETURNING id")
            .fetch_one(&admin)
            .await?;
    sqlx::query("INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,
        message_date,created_at,fanned_out) VALUES ('mtproto',$1,-1001,1,now(),now()-interval '2 days',true)")
        .bind(identity).execute(&admin).await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE canonical_events SET text='private'")
        .execute(&mut *held)
        .await?;
    let mut process = CleanupProcess(
        command(&url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid
            WHERE activity.datname=current_database() AND activity.usename='shared_cleanup' AND NOT locks.granted)")
            .fetch_one(&admin).await?;
        if waiting {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "清理进程未等待行锁");
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
        anyhow::ensure!(Instant::now() < deadline, "SIGTERM 后清理进程未退出");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM canonical_events")
            .fetch_one(&admin)
            .await?,
        1
    );
    assert!(command(&url).output()?.status.success());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM canonical_events")
            .fetch_one(&admin)
            .await?,
        0
    );
    Ok(())
}

#[test]
fn independent_hourly_timer_and_credential_file_are_consistent() {
    let service = include_str!("../../../deploy/systemd/tgm-shared-cleanup.service");
    assert!(service.contains("Requires=tgm-tenant-cleanup@unmatched-events.service\n"));
    assert!(service.contains("After=tgm-tenant-cleanup@unmatched-events.service\n"));
    let timer = include_str!("../../../deploy/systemd/tgm-shared-cleanup.timer");
    let environment = include_str!("../../../deploy/systemd/shared-cleanup.env.example");
    assert!(service.contains("User=tgm-shared-cleanup"));
    assert!(service.contains("ExecStart=/usr/local/bin/tgm cleanup-shared"));
    assert!(service.contains("KillMode=control-group"));
    assert!(timer.contains("OnCalendar=hourly"));
    assert!(environment.contains("TGM_DB_URL_SHARED_CLEANUP="));
    assert!(!environment.contains("TGM_DB_URL_OWNER="));
}
