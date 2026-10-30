use anyhow::{Context, Result};
use sqlx::PgPool;
use std::process::{Command, Output};

fn direct_port() -> u16 {
    std::env::var("TGM_TEST_PG_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(55432)
}

fn command(url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tgm"));
    command
        .env_clear()
        .env("TGM_PROFILE", "development")
        .env("TGM_DB_URL_TENANT_PURGE", url)
        .arg("purge-deleted-tenants");
    command
}

fn assert_no_url(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("postgres://"),
        "stderr 泄漏了连接串：{stderr}"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cli_empty_scan_and_explicit_tenant_recheck(admin: PgPool) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let url = format!(
        "postgres://tenant_purge:pupw@127.0.0.1:{}/{database}?application_name=ignored",
        direct_port()
    );
    let output = command(&url).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("candidates=0"));

    let recent: i64 = sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id,status,deleted_at)
         VALUES ('recent-purge',99001,'deleted',now()-interval '29 days') RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    let output = command(&url)
        .args(["--tenant-id", &recent.to_string()])
        .output()?;
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("candidates=1"),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("not_due"),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(stderr.contains("需重跑"), "stdout={stdout} stderr={stderr}");
    assert_no_url(&output);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cli_purges_empty_due_tenant_and_is_idempotent(admin: PgPool) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let url = format!(
        "postgres://tenant_purge:pupw@127.0.0.1:{}/{database}",
        direct_port()
    );
    let tenant: i64 = sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id,status,deleted_at)
         VALUES ('empty-purge',99002,'deleted',now()-interval '31 days') RETURNING id",
    )
    .fetch_one(&admin)
    .await?;

    let output = command(&url).args(["--max-tenants", "1"]).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("candidates=1") && stdout.contains("completed=1"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants WHERE id=$1")
            .bind(tenant)
            .fetch_one(&admin)
            .await?,
        0
    );

    let output = command(&url)
        .args(["--tenant-id", &tenant.to_string()])
        .output()?;
    assert!(!output.status.success());
    let output_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_text.contains("not_deleted"));
    assert_no_url(&output);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cli_stops_after_inflight_block_and_does_not_run_later_categories(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let url = format!(
        "postgres://tenant_purge:pupw@127.0.0.1:{}/{database}",
        direct_port()
    );
    let tenant: i64 = sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id,status,deleted_at)
         VALUES ('blocked-purge',99003,'deleted',now()-interval '31 days') RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    let identity: i64 = sqlx::query_scalar(
        "INSERT INTO identities(tenant_id,kind,status) VALUES($1,'bot','offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await?;
    sqlx::query(
        "INSERT INTO delivery_tasks
            (tenant_id,sender_identity_id,target_tg_chat_id,action,status)
         VALUES($1,$2,-1001,'COPY','PENDING')",
    )
    .bind(tenant)
    .bind(identity)
    .execute(&admin)
    .await?;

    let output = command(&url)
        .args(["--tenant-id", &tenant.to_string()])
        .output()
        .context("执行 tenant purge CLI")?;
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let output_text = format!("{}{}", stdout, String::from_utf8_lossy(&output.stderr));
    assert!(
        output_text.contains("failed=1") && output_text.contains("blocked_inflight"),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("tenant_finalize"),
        "终结入口不应在 terminal_delivery 阻塞时运行：{stdout}"
    );
    assert_no_url(&output);
    Ok(())
}

#[test]
fn systemd_units_keep_purge_identity_and_daily_schedule() {
    let service = include_str!("../../../deploy/systemd/tgm-tenant-purge.service");
    assert!(service.contains("User=tgm-tenant-purge"));
    assert!(service.contains("EnvironmentFile=/etc/tgm/tenant-purge.env"));
    assert!(service.contains("purge-deleted-tenants --max-tenants 100"));
    assert!(service.contains("ProtectSystem=strict"));
    assert!(service.contains("CapabilityBoundingSet="));

    let timer = include_str!("../../../deploy/systemd/tgm-tenant-purge.timer");
    assert!(timer.contains("OnCalendar=daily"));
    assert!(timer.contains("Persistent=true"));
    assert!(timer.contains("Unit=tgm-tenant-purge.service"));
}
