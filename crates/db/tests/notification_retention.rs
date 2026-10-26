use sqlx::{
    PgPool,
    postgres::PgPoolOptions,
    types::chrono::{DateTime, Utc},
};
use std::time::{Duration, Instant};
use tgm_core::ids::TenantId;
use tgm_db::{
    repo::{TenantNotificationOutcome, record_tenant_notification},
    tenant_retention::{Category, cleanup_batch},
};

mod support;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type NotificationRow = (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

async fn setup(
    injected: PgPool,
) -> Result<(PgPool, PgPool, support::AdminOnlyPool, Vec<TenantId>), Box<dyn std::error::Error>> {
    let (app, admin) = support::pools(injected).await;
    let cleanup = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(
            admin
                .raw()
                .connect_options()
                .as_ref()
                .clone()
                .username("tenant_cleanup")
                .password("tcpw"),
        )
        .await?;
    support::assert_role(&cleanup, "tenant_cleanup").await;
    support::assert_not_bypassrls(&cleanup).await;
    sqlx::raw_sql("INSERT INTO tenants (name,owner_user_id,plan,status) VALUES ('notify-active',95001,'free','active'),('notify-suspended',95002,'pro','suspended'),('notify-deleted',95003,'free','deleted'); INSERT INTO identities (tenant_id,kind) SELECT id,'bot' FROM tenants;")
        .execute(admin.raw()).await?;
    let tenants = sqlx::query_scalar("SELECT id FROM tenants ORDER BY id")
        .fetch_all(admin.raw())
        .await?;
    Ok((app, cleanup, admin, tenants))
}

async fn seed(
    admin: &PgPool,
    tenant: TenantId,
    status: &str,
    age: i32,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO tenant_notifications (tenant_id,identity_id,kind,status,created_at,sent_at,terminal_at)
        SELECT tenant_id,id,'identity.unauthorized',$2,now()-interval '400 days',
          CASE WHEN $2='SENT' THEN now()-make_interval(days=>$3) END,
          CASE WHEN $2<>'PENDING' THEN now()-make_interval(days=>$3) END
        FROM identities WHERE tenant_id=$1 RETURNING id")
        .bind(tenant.get()).bind(status).bind(age).fetch_one(admin).await
}

async fn context(connection: &mut sqlx::PgConnection, tenant: TenantId) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(connection)
        .await?;
    Ok(())
}

async fn wait_for_lock(admin: &PgPool, role: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.usename=$1 AND NOT locks.granted)")
            .bind(role).fetch_one(admin).await?;
        if waiting {
            return Ok(());
        }
        assert!(Instant::now() < deadline, "数据库连接未进入锁等待：{role}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn terminal_age_not_enqueue_age_controls_all_tenants(injected: PgPool) -> TestResult {
    let (_app, cleanup, admin, tenants) = setup(injected).await?;
    for tenant in &tenants {
        for status in ["SENT", "DEAD"] {
            seed(admin.raw(), *tenant, status, 100).await?;
            seed(admin.raw(), *tenant, status, 1).await?;
        }
        seed(admin.raw(), *tenant, "PENDING", 400).await?;
    }
    for tenant in &tenants {
        assert_eq!(
            cleanup_batch(&cleanup, *tenant, Category::TenantNotifications).await?,
            Some(2)
        );
        assert_eq!(
            cleanup_batch(&cleanup, *tenant, Category::TenantNotifications).await?,
            Some(0)
        );
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM tenant_notifications WHERE tenant_id=$1")
                .bind(tenant.get())
                .fetch_one(admin.raw())
                .await?;
        assert_eq!(left, 3);
    }
    let rows: i64=sqlx::query_scalar("SELECT sum((context->>'rows')::bigint)::bigint FROM system_logs WHERE context->>'table'='tenant_notifications'").fetch_one(admin.raw()).await?;
    assert_eq!(rows, 6);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn outcomes_preserve_terminal_time_and_never_recreate_cleaned_rows(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, tenants) = setup(injected).await?;
    let tenant = tenants[0];
    for outcome in [
        TenantNotificationOutcome::Sent,
        TenantNotificationOutcome::Dead {
            error_code: "FORBIDDEN".to_owned(),
        },
    ] {
        let id = seed(admin.raw(), tenant, "PENDING", 400).await?;
        assert!(
            record_tenant_notification(
                &app,
                tenant,
                id,
                TenantNotificationOutcome::Retry {
                    error_code: "RATE_LIMITED".to_owned(),
                    retry_after_seconds: 1
                }
            )
            .await?
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT terminal_at IS NULL FROM tenant_notifications WHERE id=$1"
            )
            .bind(id)
            .fetch_one(admin.raw())
            .await?
        );
        assert!(record_tenant_notification(&app, tenant, id, outcome.clone()).await?);
        let terminal: DateTime<Utc> =
            sqlx::query_scalar("SELECT terminal_at FROM tenant_notifications WHERE id=$1")
                .bind(id)
                .fetch_one(admin.raw())
                .await?;
        assert!(!record_tenant_notification(&app, tenant, id, outcome.clone()).await?);
        assert_eq!(
            sqlx::query_scalar::<_, DateTime<Utc>>(
                "SELECT terminal_at FROM tenant_notifications WHERE id=$1"
            )
            .bind(id)
            .fetch_one(admin.raw())
            .await?,
            terminal
        );
        assert_eq!(
            cleanup_batch(&cleanup, tenant, Category::TenantNotifications).await?,
            Some(0)
        );
        sqlx::query("UPDATE tenant_notifications SET terminal_at=now()-interval '100 days',sent_at=CASE WHEN status='SENT' THEN now()-interval '100 days' END WHERE id=$1")
            .bind(id).execute(admin.raw()).await?;
        assert_eq!(
            cleanup_batch(&cleanup, tenant, Category::TenantNotifications).await?,
            Some(1)
        );
        assert!(!record_tenant_notification(&app, tenant, id, outcome).await?);
    }
    for query in [
        "UPDATE tenant_notifications SET terminal_at=now() WHERE status='PENDING'",
        "UPDATE tenant_notifications SET status='DEAD' WHERE status='PENDING'",
        "UPDATE tenant_notifications SET status='SENT',sent_at=now(),terminal_at=now()-interval '1 second' WHERE status='PENDING'",
    ] {
        seed(admin.raw(), tenant, "PENDING", 400).await?;
        let error = sqlx::query(sqlx::AssertSqlSafe(query))
            .execute(admin.raw())
            .await
            .err()
            .ok_or("非法终态形状未被拒绝")?;
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref(),
            Some("23514")
        );
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn legacy_dead_backfill_is_conservative_and_repeatable(injected: PgPool) -> TestResult {
    let (_app, _cleanup, admin, tenants) = setup(injected).await?;
    let mut transaction = admin.raw().begin().await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE tenant_notifications DROP COLUMN terminal_at CASCADE; RESET ROLE;")
        .execute(&mut *transaction).await?;
    sqlx::query("INSERT INTO tenant_notifications (tenant_id,identity_id,kind,status,created_at,sent_at) SELECT tenant_id,id,'identity.banned',statuses.status,now()-interval '400 days',CASE WHEN statuses.status='SENT' THEN now()-interval '100 days' END FROM identities CROSS JOIN (VALUES ('PENDING'),('SENT'),('DEAD')) statuses(status) WHERE tenant_id=$1")
        .bind(tenants[0].get()).execute(&mut *transaction).await?;
    let before: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    sqlx::raw_sql(include_str!(
        "../../../migrations/0035_notification_retention.sql"
    ))
    .execute(&mut *transaction)
    .await?;
    let after: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    let rows: Vec<NotificationRow> =
        sqlx::query_as("SELECT status,sent_at,terminal_at FROM tenant_notifications ORDER BY id")
            .fetch_all(&mut *transaction)
            .await?;
    for (status, sent, terminal) in &rows {
        match status.as_str() {
            "SENT" => assert_eq!(sent, terminal),
            "DEAD" => {
                assert!(terminal.is_some_and(|instant| instant >= before && instant <= after))
            }
            _ => assert!(terminal.is_none()),
        }
    }
    sqlx::raw_sql(include_str!(
        "../../../migrations/0035_notification_retention.sql"
    ))
    .execute(&mut *transaction)
    .await?;
    let repeated: Vec<NotificationRow> =
        sqlx::query_as("SELECT status,sent_at,terminal_at FROM tenant_notifications ORDER BY id")
            .fetch_all(&mut *transaction)
            .await?;
    assert_eq!(rows, repeated);
    transaction.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn restricted_role_policy_and_c9_reject_permission_changes(injected: PgPool) -> TestResult {
    let (app, cleanup, admin, tenants) = setup(injected).await?;
    let expired = seed(admin.raw(), tenants[0], "DEAD", 100).await?;
    seed(admin.raw(), tenants[0], "DEAD", 1).await?;
    seed(admin.raw(), tenants[0], "PENDING", 400).await?;
    seed(admin.raw(), tenants[1], "SENT", 100).await?;
    assert!(
        sqlx::query("SELECT id FROM tenant_notifications")
            .fetch_all(&cleanup)
            .await
            .is_err()
    );
    let mut transaction = cleanup.begin().await?;
    context(&mut transaction, tenants[0]).await?;
    let visible: Vec<i64> = sqlx::query_scalar("SELECT id FROM tenant_notifications")
        .fetch_all(&mut *transaction)
        .await?;
    assert_eq!(visible, vec![expired]);
    assert_eq!(
        sqlx::query("DELETE FROM tenant_notifications WHERE tenant_id=$1")
            .bind(tenants[1].get())
            .execute(&mut *transaction)
            .await?
            .rows_affected(),
        0
    );
    transaction.rollback().await?;
    for query in [
        "SELECT kind FROM tenant_notifications",
        "SELECT identity_id FROM tenant_notifications",
        "SELECT last_error_code FROM tenant_notifications",
        "UPDATE tenant_notifications SET status='DEAD'",
        "TRUNCATE tenant_notifications",
        "SELECT secret_ciphertext FROM identity_secrets",
    ] {
        assert!(
            sqlx::query(sqlx::AssertSqlSafe(query))
                .execute(&cleanup)
                .await
                .is_err(),
            "意外授权：{query}"
        );
    }
    assert!(
        sqlx::query("DELETE FROM tenant_notifications")
            .execute(&app)
            .await
            .is_err()
    );
    let assertion = tgm_db::audit::parse(include_str!("../src/audit/queries.sql"))
        .into_iter()
        .find(|assertion| assertion.id == "C9")
        .ok_or("缺少 C9")?;
    let mut transaction = admin.raw().begin().await?;
    sqlx::raw_sql("SET LOCAL ROLE tgm_owner")
        .execute(&mut *transaction)
        .await?;
    for (broken, restored) in [
        (
            "GRANT SELECT (kind) ON tenant_notifications TO tenant_cleanup",
            "REVOKE SELECT (kind) ON tenant_notifications FROM tenant_cleanup",
        ),
        (
            "REVOKE SELECT (terminal_at) ON tenant_notifications FROM tenant_cleanup",
            "GRANT SELECT (terminal_at) ON tenant_notifications TO tenant_cleanup",
        ),
        (
            "GRANT SELECT (id) ON tenant_notifications TO tenant_cleanup WITH GRANT OPTION",
            "REVOKE GRANT OPTION FOR SELECT (id) ON tenant_notifications FROM tenant_cleanup",
        ),
    ] {
        sqlx::raw_sql(broken).execute(&mut *transaction).await?;
        assert!(
            !sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(assertion.sql.as_str()))
                .fetch_all(&mut *transaction)
                .await?
                .is_empty()
        );
        sqlx::raw_sql(restored).execute(&mut *transaction).await?;
        assert!(
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(assertion.sql.as_str()))
                .fetch_all(&mut *transaction)
                .await?
                .is_empty()
        );
    }
    transaction.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn refresh_and_pending_transition_survive_cleanup_and_overlap(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, tenants) = setup(injected).await?;
    let tenant = tenants[0];
    let id = seed(admin.raw(), tenant, "DEAD", 100).await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("UPDATE tenant_notifications SET terminal_at=now() WHERE id=$1")
        .bind(id)
        .execute(&mut *held)
        .await?;
    let pool = cleanup.clone();
    let running =
        tokio::spawn(
            async move { cleanup_batch(&pool, tenant, Category::TenantNotifications).await },
        );
    wait_for_lock(admin.raw(), "tenant_cleanup").await?;
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::TenantNotifications).await?,
        None
    );
    held.commit().await?;
    assert_eq!(running.await??, Some(0));
    let pending = seed(admin.raw(), tenant, "PENDING", 400).await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("SELECT id FROM tenant_notifications WHERE id=$1 FOR UPDATE")
        .bind(pending)
        .execute(&mut *held)
        .await?;
    let pool = app.clone();
    let recording = tokio::spawn(async move {
        record_tenant_notification(&pool, tenant, pending, TenantNotificationOutcome::Sent).await
    });
    wait_for_lock(admin.raw(), "app_user").await?;
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::TenantNotifications).await?,
        Some(0)
    );
    let released: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *held)
        .await?;
    held.commit().await?;
    assert!(recording.await??);
    let terminal: DateTime<Utc> =
        sqlx::query_scalar("SELECT terminal_at FROM tenant_notifications WHERE id=$1")
            .bind(pending)
            .fetch_one(admin.raw())
            .await?;
    assert!(terminal >= released);
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::TenantNotifications).await?,
        Some(0)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_first_blocks_duplicate_confirmation_and_log_failure_rolls_back(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin, tenants) = setup(injected).await?;
    let tenant = tenants[0];
    let id = seed(admin.raw(), tenant, "DEAD", 100).await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_notification_cleanup CHECK (context->>'table'<>'tenant_notifications') NOT VALID; RESET ROLE;").execute(admin.raw()).await?;
    assert!(
        cleanup_batch(&cleanup, tenant, Category::TenantNotifications)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenant_notifications WHERE id=$1")
            .bind(id)
            .fetch_one(admin.raw())
            .await?,
        1
    );
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_notification_cleanup; RESET ROLE;").execute(admin.raw()).await?;
    let mut held = cleanup.begin().await?;
    context(&mut held, tenant).await?;
    assert_eq!(
        sqlx::query("DELETE FROM tenant_notifications WHERE id=$1")
            .bind(id)
            .execute(&mut *held)
            .await?
            .rows_affected(),
        1
    );
    let pool = app.clone();
    let recording = tokio::spawn(async move {
        record_tenant_notification(&pool, tenant, id, TenantNotificationOutcome::Sent).await
    });
    held.commit().await?;
    assert!(!recording.await??);
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::TenantNotifications).await?,
        Some(0)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn broad_policy_is_caught_by_long_pending_sentinel(injected: PgPool) -> TestResult {
    let (_app, cleanup, admin, tenants) = setup(injected).await?;
    let tenant = tenants[0];
    let id = seed(admin.raw(), tenant, "PENDING", 400).await?;
    let mut transaction = cleanup.begin().await?;
    context(&mut transaction, tenant).await?;
    assert_eq!(
        sqlx::query("DELETE FROM tenant_notifications WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?
            .rows_affected(),
        0
    );
    transaction.rollback().await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER POLICY tenant_cleanup_expired ON tenant_notifications USING (tenant_id=current_setting('app.tenant_id')::bigint); RESET ROLE;").execute(admin.raw()).await?;
    let mut transaction = cleanup.begin().await?;
    context(&mut transaction, tenant).await?;
    assert_eq!(
        sqlx::query("DELETE FROM tenant_notifications WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?
            .rows_affected(),
        1
    );
    transaction.rollback().await?;
    Ok(())
}
