mod support;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn purge_pool(admin: &support::AdminOnlyPool) -> Result<PgPool, sqlx::Error> {
    let options = admin
        .raw()
        .connect_options()
        .as_ref()
        .clone()
        .username("tenant_purge")
        .password("pupw");
    PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options)
        .await
}

async fn batch(
    pool: &PgPool,
    tenant: i64,
    category: &str,
) -> Result<(String, i64, i64), sqlx::Error> {
    sqlx::query_as("SELECT result, rows_deleted, blocked FROM public.tenant_purge_batch($1, $2)")
        .bind(tenant)
        .bind(category)
        .fetch_one(pool)
        .await
}

async fn due_tenant(admin: &PgPool, status: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id,status,deleted_at)
         VALUES('batch-purge',99010,$1,CASE WHEN $1='deleted' THEN statement_timestamp()-interval '31 days' END)
         RETURNING id",
    )
    .bind(status)
    .fetch_one(admin)
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

#[sqlx::test(migrations = "../../migrations-test")]
async fn batch_rechecks_state_and_writes_fixed_result_log(injected: PgPool) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    support::assert_role(&purge, "tenant_purge").await;
    support::assert_not_bypassrls(&purge).await;

    let recent = deleted_tenant(admin.raw(), "29 days").await?;
    assert_eq!(
        batch(&purge, recent, "tenant_audit").await?,
        ("not_due".into(), 0, 0)
    );
    let active = due_tenant(admin.raw(), "active").await?;
    assert_eq!(
        batch(&purge, active, "tenant_audit").await?,
        ("not_deleted".into(), 0, 0)
    );
    assert_eq!(
        batch(&purge, 9_999_999, "tenant_audit").await?,
        ("not_deleted".into(), 0, 0)
    );

    let tenant = deleted_tenant(admin.raw(), "31 days").await?;
    assert_eq!(
        batch(&purge, tenant, "tenant_audit").await?,
        ("empty".into(), 0, 0)
    );
    let context: serde_json::Value = sqlx::query_scalar(
        "SELECT context FROM system_logs
          WHERE tenant_id IS NULL AND module='tenant_purge' AND event='cleanup.done'
          ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(admin.raw())
    .await?;
    assert_eq!(context.as_object().map(|object| object.len()), Some(5));
    assert_eq!(context["tenant_id"], tenant);
    assert_eq!(context["category"], "tenant_audit");
    assert_eq!(context["result"], "empty");
    assert_eq!(context["rows"], 0);
    assert_eq!(context["blocked"], 0);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batch_deletes_private_rows_in_order_and_is_repeatable(injected: PgPool) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    let tenant = deleted_tenant(admin.raw(), "31 days").await?;
    let identity: i64 =
        sqlx::query_scalar("INSERT INTO identities(tenant_id,kind) VALUES($1,'bot') RETURNING id")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await?;
    sqlx::query(
        "INSERT INTO identity_secrets(identity_id,secret_ciphertext,nonce,dek_ciphertext,kek_id)
         VALUES($1,'\\x00','\\x00','\\x00','test')",
    )
    .bind(identity)
    .execute(admin.raw())
    .await?;
    sqlx::query(
        "INSERT INTO credential_keys(tenant_id,dek_ciphertext,kek_id) VALUES($1,'\\x00','test')",
    )
    .bind(tenant)
    .execute(admin.raw())
    .await?;

    assert_eq!(
        batch(&purge, tenant, "identity_dependents").await?,
        ("deleted".into(), 1, 0)
    );
    assert_eq!(
        batch(&purge, tenant, "identity_dependents").await?,
        ("deleted".into(), 1, 0)
    );
    assert_eq!(
        batch(&purge, tenant, "identity_dependents").await?,
        ("empty".into(), 0, 0)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM identity_secrets WHERE identity_id=$1")
            .bind(identity)
            .fetch_one(admin.raw())
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM credential_keys WHERE tenant_id=$1")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await?,
        0
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batch_blocks_inflight_delivery_and_retry_continues_after_resolution(
    injected: PgPool,
) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    let tenant = deleted_tenant(admin.raw(), "31 days").await?;
    let identity: i64 =
        sqlx::query_scalar("INSERT INTO identities(tenant_id,kind) VALUES($1,'bot') RETURNING id")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await?;
    let task: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks(tenant_id,sender_identity_id,target_tg_chat_id,action,status)
         VALUES($1,$2,-1001,'COPY','PENDING') RETURNING id",
    )
    .bind(tenant)
    .bind(identity)
    .fetch_one(admin.raw())
    .await?;
    assert_eq!(
        batch(&purge, tenant, "terminal_delivery").await?,
        ("blocked_inflight".into(), 0, 1)
    );
    sqlx::query("UPDATE delivery_tasks SET status='DONE' WHERE id=$1")
        .bind(task)
        .execute(admin.raw())
        .await?;
    assert_eq!(
        batch(&purge, tenant, "terminal_delivery").await?,
        ("deleted".into(), 1, 0)
    );
    assert_eq!(
        batch(&purge, tenant, "terminal_delivery").await?,
        ("empty".into(), 0, 0)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batch_event_shared_reference_and_media_blockers_are_repeatable(
    injected: PgPool,
) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    let tenant = deleted_tenant(admin.raw(), "31 days").await?;
    let other = deleted_tenant(admin.raw(), "31 days").await?;
    let identity: i64 =
        sqlx::query_scalar("INSERT INTO identities(tenant_id,kind) VALUES($1,'bot') RETURNING id")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await?;
    let other_identity: i64 =
        sqlx::query_scalar("INSERT INTO identities(tenant_id,kind) VALUES($1,'bot') RETURNING id")
            .bind(other)
            .fetch_one(admin.raw())
            .await?;
    let source: i64 = sqlx::query_scalar("INSERT INTO source_subscriptions(tenant_id,tg_chat_id,receiver_identity_id) VALUES($1,-1001,$2) RETURNING id")
        .bind(tenant).bind(identity).fetch_one(admin.raw()).await?;
    let other_source: i64 = sqlx::query_scalar("INSERT INTO source_subscriptions(tenant_id,tg_chat_id,receiver_identity_id) VALUES($1,-1002,$2) RETURNING id")
        .bind(other).bind(other_identity).fetch_one(admin.raw()).await?;
    let event: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events(adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date)
         VALUES('bot',$1,-1001,1,now()) RETURNING id",
    ).bind(identity).fetch_one(admin.raw()).await?;
    sqlx::query("INSERT INTO tenant_events(tenant_id,canonical_event_id,source_ref) VALUES($1,$2,$3),($4,$2,$5)")
        .bind(tenant).bind(event).bind(source).bind(other).bind(other_source).execute(admin.raw()).await?;
    assert_eq!(
        batch(&purge, tenant, "tenant_events").await?,
        ("blocked_shared_reference".into(), 0, 1)
    );
    sqlx::query("DELETE FROM tenant_events WHERE tenant_id=$1")
        .bind(other)
        .execute(admin.raw())
        .await?;
    assert_eq!(
        batch(&purge, tenant, "tenant_events").await?,
        ("deleted".into(), 1, 0)
    );

    let event_two: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events(adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date)
         VALUES('bot',$1,-1001,2,now()) RETURNING id",
    ).bind(identity).fetch_one(admin.raw()).await?;
    sqlx::query(
        "INSERT INTO tenant_events(tenant_id,canonical_event_id,source_ref) VALUES($1,$2,$3)",
    )
    .bind(tenant)
    .bind(event_two)
    .bind(source)
    .execute(admin.raw())
    .await?;
    let mut seed_tx = admin.raw().begin().await?;
    sqlx::raw_sql("SET LOCAL ROLE tgm_owner")
        .execute(&mut *seed_tx)
        .await?;
    sqlx::query(
        "INSERT INTO media_relay_jobs(canonical_event_id,receiver_identity_id,status)
         VALUES ($1,$2,'PENDING')",
    )
    .bind(event_two)
    .bind(identity)
    .execute(&mut *seed_tx)
    .await?;
    seed_tx.commit().await?;
    assert_eq!(
        batch(&purge, tenant, "tenant_events").await?,
        ("blocked_shared_reference".into(), 0, 1)
    );
    let mut update_tx = admin.raw().begin().await?;
    sqlx::raw_sql("SET LOCAL ROLE tgm_owner")
        .execute(&mut *update_tx)
        .await?;
    sqlx::query("UPDATE media_relay_jobs SET status='DEAD' WHERE canonical_event_id=$1")
        .bind(event_two)
        .execute(&mut *update_tx)
        .await?;
    update_tx.commit().await?;
    assert_eq!(
        batch(&purge, tenant, "tenant_events").await?,
        ("deleted".into(), 1, 0)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batch_lock_and_log_failure_roll_back(injected: PgPool) -> TestResult {
    let (_app, admin) = support::pools(injected).await;
    let purge = purge_pool(&admin).await?;
    let tenant = deleted_tenant(admin.raw(), "31 days").await?;
    let mut lock_tx = admin.raw().begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('tenant_lifecycle:' || $1::text, 419871))",
    )
    .bind(tenant)
    .execute(&mut *lock_tx)
    .await?;
    assert_eq!(
        batch(&purge, tenant, "tenant_audit").await?,
        ("locked".into(), 0, 0)
    );
    lock_tx.rollback().await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT batch_log_failure CHECK (module <> 'tenant_purge') NOT VALID; RESET ROLE;")
        .execute(admin.raw()).await?;
    assert!(batch(&purge, tenant, "tenant_audit").await.is_err());
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT batch_log_failure; RESET ROLE;")
        .execute(admin.raw()).await?;
    Ok(())
}
