mod support;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn purge_pool(admin: &support::AdminOnlyPool) -> Result<PgPool, sqlx::Error> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin.raw())
        .await?;
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!(
            "postgres://tenant_purge:pupw@127.0.0.1:55432/{database}"
        ))
        .await
}

async fn deleted_tenant(admin: &PgPool, age: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id,status,deleted_at)
         VALUES('purge',99001,'deleted',statement_timestamp()-$1::interval)
         RETURNING id",
    )
    .bind(age)
    .fetch_one(admin)
    .await
}

async fn finalize(pool: &PgPool, tenant: i64) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT public.tenant_purge_finalize($1)")
        .bind(tenant)
        .fetch_one(pool)
        .await
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn finalizer_deletes_due_empty_tenant_and_writes_platform_log(
    injected: PgPool,
) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    support::assert_role(&purge, "tenant_purge").await;
    support::assert_not_bypassrls(&purge).await;
    let tenant = deleted_tenant(admin.raw(), "31 days").await?;

    assert_eq!(finalize(&purge, tenant).await?, "deleted");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants WHERE id=$1")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await?,
        0
    );
    let context: serde_json::Value = sqlx::query_scalar(
        "SELECT context FROM system_logs
          WHERE tenant_id IS NULL AND module='tenant_purge' AND event='cleanup.done'
          ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(admin.raw())
    .await?;
    assert_eq!(context["tenant_id"], tenant);
    assert_eq!(context["category"], "tenant_root");
    assert_eq!(context["result"], "deleted");
    assert_eq!(context["rows"], 1);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn finalizer_rechecks_state_time_and_residual_members(injected: PgPool) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    let recent = deleted_tenant(admin.raw(), "29 days").await?;
    assert_eq!(finalize(&purge, recent).await?, "not_due");

    let active: i64 = sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id,status) VALUES('active',99002,'active') RETURNING id",
    )
    .fetch_one(admin.raw())
    .await?;
    assert_eq!(finalize(&purge, active).await?, "not_deleted");

    let blocked = deleted_tenant(admin.raw(), "31 days").await?;
    sqlx::query("INSERT INTO tenant_members(tenant_id,user_id) VALUES($1,99003)")
        .bind(blocked)
        .execute(admin.raw())
        .await?;
    assert_eq!(finalize(&purge, blocked).await?, "blocked");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants WHERE id=$1")
            .bind(blocked)
            .fetch_one(admin.raw())
            .await?,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn finalizer_returns_locked_and_logs_failure_rolls_back(injected: PgPool) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    let locked = deleted_tenant(admin.raw(), "31 days").await?;
    let mut lock_tx = admin.raw().begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('tenant_lifecycle:' || $1::text, 419871))",
    )
    .bind(locked)
    .execute(&mut *lock_tx)
    .await?;
    assert_eq!(finalize(&purge, locked).await?, "locked");
    lock_tx.rollback().await?;

    let failed = deleted_tenant(admin.raw(), "31 days").await?;
    sqlx::raw_sql(
        "SET ROLE tgm_owner;
         ALTER TABLE system_logs ADD CONSTRAINT purge_log_failure CHECK (module <> 'tenant_purge');
         RESET ROLE;",
    )
    .execute(admin.raw())
    .await?;
    assert!(finalize(&purge, failed).await.is_err());
    sqlx::raw_sql(
        "SET ROLE tgm_owner;
         ALTER TABLE system_logs DROP CONSTRAINT purge_log_failure;
         RESET ROLE;",
    )
    .execute(admin.raw())
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants WHERE id=$1")
            .bind(failed)
            .fetch_one(admin.raw())
            .await?,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn purge_login_cannot_directly_delete_or_create_objects(injected: PgPool) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    let tenant = deleted_tenant(admin.raw(), "31 days").await?;
    assert!(
        sqlx::query("DELETE FROM tenants WHERE id=$1")
            .bind(tenant)
            .execute(&purge)
            .await
            .is_err()
    );
    for query in [
        "SELECT secret_ciphertext FROM identity_secrets LIMIT 1",
        "SELECT id FROM web_sessions LIMIT 1",
        "SELECT login_token FROM login_sessions LIMIT 1",
        "SELECT text FROM canonical_events LIMIT 1",
        "SELECT object_key FROM media_archive LIMIT 1",
    ] {
        assert!(
            sqlx::query(query).fetch_optional(&purge).await.is_err(),
            "tenant_purge unexpectedly accessed restricted column: {query}"
        );
    }
    assert!(
        sqlx::query("CREATE TABLE purge_probe(id bigint)")
            .execute(&purge)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(1)).await;
    Ok(())
}
