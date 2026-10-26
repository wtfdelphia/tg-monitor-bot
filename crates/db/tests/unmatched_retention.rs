use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_core::ids::TenantId;
use tgm_db::repo;
use tgm_db::tenant_retention::{Category, cleanup_batch, lock_event_lifecycle_shared, try_lock};

mod support;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn role_pool(admin: &PgPool, role: &str, password: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(3)
        .connect_with(
            admin
                .connect_options()
                .as_ref()
                .clone()
                .username(role)
                .password(password),
        )
        .await
}

async fn setup(
    injected: PgPool,
) -> Result<
    (
        PgPool,
        PgPool,
        support::AdminOnlyPool,
        TenantId,
        TenantId,
        i64,
    ),
    Box<dyn std::error::Error>,
> {
    let (app, admin) = support::pools(injected).await;
    let cleanup = role_pool(admin.raw(), "tenant_cleanup", "tcpw").await?;
    support::assert_role(&app, "app_user").await;
    support::assert_role(&cleanup, "tenant_cleanup").await;
    support::assert_not_bypassrls(&cleanup).await;
    tgm_db::tenant_retention::check_connection(&cleanup).await?;
    sqlx::raw_sql(
        "INSERT INTO tenants (name,owner_user_id,plan,status) VALUES
        ('short-free',99001,'free','suspended'),('short-pro',99002,'pro','deleted');
        INSERT INTO identities (kind) VALUES ('mtproto');",
    )
    .execute(admin.raw())
    .await?;
    let tenants: Vec<TenantId> = sqlx::query_scalar("SELECT id FROM tenants ORDER BY id")
        .fetch_all(admin.raw())
        .await?;
    let identity: i64 = sqlx::query_scalar("SELECT id FROM identities")
        .fetch_one(admin.raw())
        .await?;
    sqlx::query(
        "INSERT INTO source_subscriptions (tenant_id,tg_chat_id,receiver_identity_id)
        SELECT id,-1001,$1 FROM tenants",
    )
    .bind(identity)
    .execute(admin.raw())
    .await?;
    Ok((app, cleanup, admin, tenants[0], tenants[1], identity))
}

async fn seed_event(
    admin: &PgPool,
    identity: i64,
    message: i64,
    age: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,created_at,text)
        VALUES ('mtproto',$1,-1001,$2,now(),now()-$3::interval,'private body') RETURNING id")
        .bind(identity).bind(message).bind(age).fetch_one(admin).await
}

async fn projection(
    app: &PgPool,
    tenant: TenantId,
    event: i64,
) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(repo::fanout_canonical_event(app, tenant, event)
        .await?
        .ok_or("未生成投影")?
        .id)
}

async fn process_unmatched(
    app: &PgPool,
    tenant: TenantId,
    event: i64,
) -> Result<(), repo::EnqueueError> {
    repo::enqueue_delivery_tasks(app, tenant, event, &[], &[]).await?;
    Ok(())
}

async fn context(connection: &mut sqlx::PgConnection, tenant: TenantId) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(connection)
        .await?;
    Ok(())
}

async fn wait_for_lock(admin: &PgPool, role: &str) -> TestResult {
    wait_for_locks(admin, role, 1).await
}

async fn wait_for_locks(admin: &PgPool, role: &str, expected: i64) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: i64 = sqlx::query_scalar("SELECT count(DISTINCT activity.pid) FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid
            WHERE activity.datname=current_database() AND activity.usename=$1 AND NOT locks.granted")
            .bind(role).fetch_one(admin).await?;
        if waiting >= expected {
            return Ok(());
        }
        assert!(Instant::now() < deadline, "未观察到真实锁等待：{role}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn processed_projection_expires_before_plan_without_harming_other_tenant(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, pro, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let first = projection(&app, free, event).await?;
    let second = projection(&app, pro, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    process_unmatched(&app, free, first).await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(1)
    );
    assert_eq!(
        cleanup_batch(&cleanup, pro, Category::UnmatchedEvents).await?,
        Some(0)
    );
    assert_eq!(
        tgm_db::tenant_retention::retained_unmatched(&cleanup, pro).await?,
        1
    );
    assert!(
        repo::fanout_canonical_event(&app, free, event)
            .await?
            .is_none()
    );
    assert_eq!(
        repo::fanout_canonical_event(&app, pro, event)
            .await?
            .ok_or("已有投影丢失")?
            .id,
        second
    );
    let shared = role_pool(admin.raw(), "shared_cleanup", "scpw").await?;
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(0)
    );
    process_unmatched(&app, pro, second).await?;
    assert_eq!(
        cleanup_batch(&cleanup, pro, Category::UnmatchedEvents).await?,
        Some(1)
    );
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(1)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM canonical_events")
            .fetch_one(admin.raw())
            .await?,
        0
    );
    assert!(matches!(
        repo::enqueue_delivery_tasks(&app, free, first, &[], &[]).await,
        Err(repo::EnqueueError::EventUnavailable)
    ));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn all_task_states_unprocessed_matched_and_unfanned_are_protected(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, pro, identity) = setup(injected).await?;
    let bot: i64 = sqlx::query_scalar(
        "INSERT INTO identities (tenant_id,kind) VALUES ($1,'bot') RETURNING id",
    )
    .bind(free)
    .fetch_one(admin.raw())
    .await?;
    for (index, state) in ["PENDING", "SENDING", "FAILED", "DONE", "DEAD", "CANCELLED"]
        .iter()
        .enumerate()
    {
        let event = seed_event(admin.raw(), identity, i64::try_from(index)? + 1, "2 days").await?;
        let tenant_event = projection(&app, free, event).await?;
        repo::mark_canonical_event_fanned_out(&app, event).await?;
        process_unmatched(&app, free, tenant_event).await?;
        sqlx::query("INSERT INTO delivery_tasks (tenant_id,tenant_event_id,sender_identity_id,target_tg_chat_id,action,status)
            VALUES ($1,$2,$3,-1002,'COPY',$4)").bind(free).bind(tenant_event).bind(bot).bind(state).execute(admin.raw()).await?;
    }
    for (message, age, processed, matched, fanned) in [
        (10, "2 days", false, false, true),
        (11, "2 days", true, true, true),
        (12, "2 days", true, false, false),
        (13, "23 hours", true, false, true),
    ] {
        let event = seed_event(admin.raw(), identity, message, age).await?;
        let tenant_event = projection(&app, free, event).await?;
        sqlx::query("UPDATE tenant_events SET processed_at=CASE WHEN $2 THEN now() END,matched=$3 WHERE id=$1")
            .bind(tenant_event).bind(processed).bind(matched).execute(admin.raw()).await?;
        sqlx::query("UPDATE canonical_events SET fanned_out=$2 WHERE id=$1")
            .bind(event)
            .bind(fanned)
            .execute(admin.raw())
            .await?;
    }
    let other = seed_event(admin.raw(), identity, 20, "2 days").await?;
    let other_projection = projection(&app, pro, other).await?;
    repo::mark_canonical_event_fanned_out(&app, other).await?;
    process_unmatched(&app, pro, other_projection).await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(0)
    );
    let mut transaction = cleanup.begin().await?;
    context(&mut transaction, free).await?;
    assert_eq!(
        sqlx::query("DELETE FROM tenant_events WHERE tenant_id=$1")
            .bind(pro)
            .execute(&mut *transaction)
            .await?
            .rows_affected(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenant_events")
            .fetch_one(&mut *transaction)
            .await?,
        10
    );
    transaction.rollback().await?;
    for query in [
        "SELECT text FROM canonical_events",
        "SELECT matched_keywords FROM tenant_events",
        "SELECT payload_snapshot FROM delivery_tasks",
        "SELECT secret_ciphertext FROM identity_secrets",
        "UPDATE canonical_events SET fanned_out=true",
        "DELETE FROM canonical_events",
    ] {
        assert!(
            sqlx::raw_sql(query).execute(&cleanup).await.is_err(),
            "意外允许 {query}"
        );
    }
    assert!(
        sqlx::query("DELETE FROM tenant_events")
            .execute(&cleanup)
            .await
            .is_err()
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn stale_processed_plan_cannot_enqueue_new_work(injected: PgPool) -> TestResult {
    let (app, cleanup, admin, free, _, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let tenant_event = projection(&app, free, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    process_unmatched(&app, free, tenant_event).await?;
    let task = repo::DeliveryTaskInput {
        rule_id: 9999,
        rule_version: 1,
        target_tg_chat_id: -1002,
        sender_identity_id: sqlx::query_scalar("SELECT id FROM identities WHERE id=$1")
            .bind(identity)
            .fetch_one(admin.raw())
            .await?,
        action: "COPY".to_owned(),
        payload_snapshot: serde_json::json!({}),
        media_relay: false,
        dead_error_code: None,
    };
    assert!(matches!(
        repo::enqueue_delivery_tasks(&app, free, tenant_event, &[], &[task]).await,
        Err(repo::EnqueueError::EventProcessed)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_tasks")
            .fetch_one(admin.raw())
            .await?,
        0
    );
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(1)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_first_blocks_late_fanout_and_enqueue_then_preserves_completion(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, _, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let tenant_event = projection(&app, free, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    process_unmatched(&app, free, tenant_event).await?;
    let mut held = cleanup.begin().await?;
    context(&mut held, free).await?;
    assert!(try_lock(&mut held, free, Category::UnmatchedEvents).await?);
    let pool = app.clone();
    let fanout =
        tokio::spawn(async move { repo::fanout_canonical_event(&pool, free, event).await });
    wait_for_lock(admin.raw(), "app_user").await?;
    let pool = role_pool(admin.raw(), "app_user", "apw").await?;
    let enqueue = tokio::spawn(async move {
        repo::enqueue_delivery_tasks(&pool, free, tenant_event, &[], &[]).await
    });
    wait_for_locks(admin.raw(), "app_user", 2).await?;
    sqlx::query("DELETE FROM tenant_events WHERE tenant_id=$1 AND id=$2")
        .bind(free)
        .bind(tenant_event)
        .execute(&mut *held)
        .await?;
    held.commit().await?;
    assert!(fanout.await??.is_none());
    assert!(matches!(
        enqueue.await?,
        Err(repo::EnqueueError::EventUnavailable)
    ));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn writers_first_skip_cleanup_and_partial_fanout_can_resume(injected: PgPool) -> TestResult {
    let (app, cleanup, admin, free, pro, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let tenant_event = projection(&app, free, event).await?;
    process_unmatched(&app, free, tenant_event).await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(0)
    );
    assert_eq!(projection(&app, free, event).await?, tenant_event);
    projection(&app, pro, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    let mut held = app.begin().await?;
    lock_event_lifecycle_shared(&mut held, free).await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        None
    );
    held.commit().await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(1)
    );
    assert!(
        repo::fanout_canonical_event(&app, free, event)
            .await?
            .is_none()
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn failed_log_and_unsupported_isolation_never_delete_projection(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, _, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let tenant_event = projection(&app, free, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    process_unmatched(&app, free, tenant_event).await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_short_log CHECK(module <> 'tenant_retention') NOT VALID; RESET ROLE;")
        .execute(admin.raw()).await?;
    assert!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents)
            .await
            .is_err()
    );
    assert!(
        repo::load_tenant_event(&app, free, tenant_event)
            .await?
            .is_some()
    );
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_short_log; RESET ROLE;",
    )
    .execute(admin.raw())
    .await?;
    let isolated = PgPoolOptions::new()
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_isolation='repeatable read'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(cleanup.connect_options().as_ref().clone())
        .await?;
    assert!(
        cleanup_batch(&isolated, free, Category::UnmatchedEvents)
            .await
            .is_err()
    );
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(1)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn group_member_and_downloading_references_outlive_projection_cleanup(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, pro, identity) = setup(injected).await?;
    let group = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let member = seed_event(admin.raw(), identity, 2, "2 days").await?;
    sqlx::query("UPDATE canonical_events SET event_type='MEDIA_GROUP_COMPLETED',media_group_id='short-album' WHERE id=$1")
        .bind(group).execute(admin.raw()).await?;
    sqlx::query(
        "UPDATE canonical_events SET fanned_out=true,media_group_id='short-album' WHERE id=$1",
    )
    .bind(member)
    .execute(admin.raw())
    .await?;
    sqlx::query("INSERT INTO media_group_members (group_event_id,member_event_id,position) VALUES ($1,$2,0)")
        .bind(group).bind(member).execute(&app).await?;
    let first = projection(&app, free, group).await?;
    let second = projection(&app, pro, group).await?;
    process_unmatched(&app, free, first).await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(0)
    );
    repo::mark_canonical_event_fanned_out(&app, group).await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(1)
    );
    let shared = role_pool(admin.raw(), "shared_cleanup", "scpw").await?;
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(0)
    );
    process_unmatched(&app, pro, second).await?;
    sqlx::query("INSERT INTO media_relay_jobs (canonical_event_id,receiver_identity_id,status,next_retry_at)
        VALUES ($1,$2,'DOWNLOADING',now()+interval '1 hour')").bind(member).bind(identity).execute(&app).await?;
    assert_eq!(
        cleanup_batch(&cleanup, pro, Category::UnmatchedEvents).await?,
        Some(1)
    );
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(1)
    );
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(0)
    );
    sqlx::query("UPDATE media_relay_jobs SET status='PENDING',next_retry_at=NULL WHERE canonical_event_id=$1")
        .bind(member).execute(&app).await?;
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(1)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_group_members")
            .fetch_one(&app)
            .await?,
        0
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn real_enqueue_and_fanout_hold_lifecycle_until_row_wait_finishes(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, _, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let tenant_event = projection(&app, free, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("SELECT id FROM tenant_events WHERE id=$1 FOR UPDATE")
        .bind(tenant_event)
        .execute(&mut *held)
        .await?;
    let pool = app.clone();
    let enqueue = tokio::spawn(async move {
        repo::enqueue_delivery_tasks(&pool, free, tenant_event, &[], &[]).await
    });
    wait_for_lock(admin.raw(), "app_user").await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        None
    );
    held.commit().await?;
    enqueue.await??;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(1)
    );
    let next = seed_event(admin.raw(), identity, 2, "2 days").await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("SELECT id FROM canonical_events WHERE id=$1 FOR UPDATE")
        .bind(next)
        .execute(&mut *held)
        .await?;
    let pool = app.clone();
    let fanout = tokio::spawn(async move { repo::fanout_canonical_event(&pool, free, next).await });
    wait_for_lock(admin.raw(), "app_user").await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        None
    );
    held.commit().await?;
    assert!(fanout.await??.is_some());
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn matched_other_tenant_keeps_global_match_and_plan_retention(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, pro, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let first = projection(&app, free, event).await?;
    let second = projection(&app, pro, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    process_unmatched(&app, free, first).await?;
    sqlx::query("UPDATE tenant_events SET processed_at=now(),matched=true WHERE id=$1")
        .bind(second)
        .execute(admin.raw())
        .await?;
    sqlx::query("UPDATE canonical_events SET matched_any=true WHERE id=$1")
        .bind(event)
        .execute(admin.raw())
        .await?;
    assert_eq!(
        cleanup_batch(&cleanup, free, Category::UnmatchedEvents).await?,
        Some(1)
    );
    assert_eq!(
        cleanup_batch(&cleanup, pro, Category::UnmatchedEvents).await?,
        Some(0)
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT matched_any FROM canonical_events WHERE id=$1")
            .bind(event)
            .fetch_one(admin.raw())
            .await?
    );
    assert!(repo::load_tenant_event(&app, pro, second).await?.is_some());
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn broad_delete_policy_is_detected_by_protected_pending_projection(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, _, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let tenant_event = projection(&app, free, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    let mut transaction = cleanup.begin().await?;
    context(&mut transaction, free).await?;
    assert_eq!(
        sqlx::query("DELETE FROM tenant_events WHERE tenant_id=$1 AND id=$2")
            .bind(free)
            .bind(tenant_event)
            .execute(&mut *transaction)
            .await?
            .rows_affected(),
        0
    );
    transaction.rollback().await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER POLICY tenant_cleanup_unmatched ON tenant_events USING (tenant_id=current_setting('app.tenant_id')::bigint); RESET ROLE;")
        .execute(admin.raw()).await?;
    let mut transaction = cleanup.begin().await?;
    context(&mut transaction, free).await?;
    assert_eq!(
        sqlx::query("DELETE FROM tenant_events WHERE tenant_id=$1 AND id=$2")
            .bind(free)
            .bind(tenant_event)
            .execute(&mut *transaction)
            .await?
            .rows_affected(),
        1
    );
    transaction.rollback().await?;
    assert!(
        repo::load_tenant_event(&app, free, tenant_event)
            .await?
            .is_some()
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn simultaneous_projection_and_shared_cleanup_preserve_reference_then_finish(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, free, _, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let tenant_event = projection(&app, free, event).await?;
    repo::mark_canonical_event_fanned_out(&app, event).await?;
    process_unmatched(&app, free, tenant_event).await?;
    let shared = role_pool(admin.raw(), "shared_cleanup", "scpw").await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("SELECT id FROM tenant_events WHERE id=$1 FOR UPDATE")
        .bind(tenant_event)
        .execute(&mut *held)
        .await?;
    let pool = cleanup.clone();
    let cleaning =
        tokio::spawn(async move { cleanup_batch(&pool, free, Category::UnmatchedEvents).await });
    wait_for_lock(admin.raw(), "tenant_cleanup").await?;
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(0)
    );
    assert!(
        repo::load_tenant_event(&app, free, tenant_event)
            .await?
            .is_some()
    );
    held.commit().await?;
    assert_eq!(cleaning.await??, Some(1));
    assert_eq!(
        tgm_db::shared_retention::cleanup_batch(&shared).await?,
        Some(1)
    );
    Ok(())
}
