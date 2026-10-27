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
        .env("TGM_DB_URL_PLATFORM_CLEANUP", url)
        .arg("cleanup-platform");
    command
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cli_credentials_redaction_sigterm_and_resume(admin: PgPool) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let url = format!("postgres://platform_cleanup:pcpw@127.0.0.1:55432/{database}");
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
        ("auth_cleanup", "cpw"),
        ("tenant_cleanup", "tcpw"),
        ("shared_cleanup", "scpw"),
        ("media_cleanup", "mcpw"),
        ("postgres", "pw"),
    ] {
        let output = command(&format!(
            "postgres://{role}:{password}@127.0.0.1:55432/{database}"
        ))
        .output()?;
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("postgres://") && !stderr.contains("permission denied"));
    }
    for name in [
        "TGM_DB_URL_APP",
        "TGM_DB_URL_OWNER",
        "TGM_DB_URL_AUTH_CLEANUP",
        "TGM_DB_URL_TENANT_CLEANUP",
        "TGM_DB_URL_SHARED_CLEANUP",
        "TGM_DB_URL_MEDIA_CLEANUP",
        "DATABASE_URL",
        "AWS_ACCESS_KEY_ID",
        "TGM_S3_SECRET_ACCESS_KEY",
    ] {
        let output = command(&url).env(name, "private-value").output()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-value"));
    }
    sqlx::query("INSERT INTO system_logs (level,module,event,created_at) VALUES ('INFO','test','old',now()-interval '91 days')").execute(&admin).await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_platform CHECK (module<>'platform_retention') NOT VALID; RESET ROLE").execute(&admin).await?;
    let output = command(&url).output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("回滚"));
    assert!(!stderr.contains("reject_platform") && !stderr.contains("postgres://"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE event='old'")
            .fetch_one(&admin)
            .await?,
        1
    );
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_platform; RESET ROLE",
    )
    .execute(&admin)
    .await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE system_logs SET context='{}' WHERE event='old'")
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
        let waiting: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid
            WHERE activity.datname=current_database() AND activity.usename='platform_cleanup' AND NOT locks.granted)").fetch_one(&admin).await?;
        if waiting {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "CLI 未等待真实行锁");
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
            assert!(!status.success(), "取消不能报告成功");
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "SIGTERM 后未退出");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE event='old'")
            .fetch_one(&admin)
            .await?,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM system_logs WHERE module='platform_retention'"
        )
        .fetch_one(&admin)
        .await?,
        1
    );
    assert!(command(&url).output()?.status.success());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE event='old'")
            .fetch_one(&admin)
            .await?,
        0
    );
    let mut held = admin.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(tgm_db::platform_retention::LOCK_KEY)
        .execute(&mut *held)
        .await?;
    let output = command(&url).output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("overlapped=true"));
    held.rollback().await?;
    Ok(())
}

#[test]
fn daily_units_keep_credentials_separate() {
    let service = include_str!("../../../deploy/systemd/tgm-platform-cleanup.service");
    let timer = include_str!("../../../deploy/systemd/tgm-platform-cleanup.timer");
    let environment = include_str!("../../../deploy/systemd/platform-cleanup.env.example");
    assert!(service.contains("User=tgm-platform-cleanup"));
    assert!(service.contains("EnvironmentFile=/etc/tgm/platform-cleanup.env"));
    assert!(service.contains("ExecStart=/usr/local/bin/tgm cleanup-platform"));
    assert!(service.contains("KillMode=control-group") && service.contains("NoNewPrivileges=true"));
    assert!(timer.contains("OnCalendar=daily") && timer.contains("Persistent=true"));
    assert!(timer.contains("Unit=tgm-platform-cleanup.service"));
    assert!(environment.contains("TGM_DB_URL_PLATFORM_CLEANUP=postgres://platform_cleanup:"));
    assert!(!environment.contains("TGM_DB_URL_APP=") && !environment.contains("TGM_DB_URL_OWNER="));
}
