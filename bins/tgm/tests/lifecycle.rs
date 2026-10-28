use anyhow::Result;
use sqlx::PgPool;
use std::process::Command;

fn command(url: &str, action: &str, tenant: i64) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tgm"));
    command
        .env_clear()
        .env("TGM_PROFILE", "development")
        .env("TGM_DB_URL_TENANT_LIFECYCLE", url)
        .args(["tenant-lifecycle", action, "--tenant-id"])
        .arg(tenant.to_string());
    command
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn logical_delete_cli_enforces_state_credentials_role_and_redaction(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let url = format!("postgres://tenant_lifecycle:tlpw@127.0.0.1:55432/{database}");
    let tenant: i64 = sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id) VALUES('cli-delete',97501) RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    assert!(!command(&url, "delete", tenant).output()?.status.success());
    for name in [
        "DATABASE_URL",
        "TGM_DB_URL_OWNER",
        "TG_API_HASH",
        "TGM_LOCAL_KEK",
    ] {
        let output = command(&url, "delete", tenant)
            .env(name, "private-credential")
            .output()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-credential"));
    }
    for (role, password) in [("app_user", "apw"), ("auth_lookup", "lpw")] {
        let output = command(
            &format!("postgres://{role}:{password}@127.0.0.1:55432/{database}"),
            "delete",
            tenant,
        )
        .output()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("postgres://"));
    }
    let suspended = command(&url, "suspend", tenant).output()?;
    assert!(
        suspended.status.success(),
        "{}",
        String::from_utf8_lossy(&suspended.stderr)
    );
    for _attempt in 0..2 {
        let deleted = command(&url, "delete", tenant).output()?;
        assert!(
            deleted.status.success(),
            "{}",
            String::from_utf8_lossy(&deleted.stderr)
        );
        assert!(String::from_utf8_lossy(&deleted.stdout).contains("逻辑注销完成"));
    }
    assert!(!command(&url, "resume", tenant).output()?.status.success());
    assert!(!command(&url, "delete", i64::MAX).output()?.status.success());
    let state: (String, bool) =
        sqlx::query_as("SELECT status,deleted_at IS NOT NULL FROM tenants WHERE id=$1")
            .bind(tenant)
            .fetch_one(&admin)
            .await?;
    assert_eq!(state, ("deleted".to_owned(), true));
    Ok(())
}
