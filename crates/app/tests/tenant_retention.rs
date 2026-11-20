use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_core::ids::TenantId;
use tgm_db::tenant_retention::Category;

#[sqlx::test(migrations = "../../migrations-test")]
async fn notification_batches_preserve_pending_and_resume(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::raw_sql("INSERT INTO tenants (name,owner_user_id,status) VALUES ('notification-batch',99351,'suspended');
        INSERT INTO identities (tenant_id,kind) SELECT id,'bot' FROM tenants;
        INSERT INTO tenant_notifications (tenant_id,identity_id,kind,status,terminal_at)
          SELECT identities.tenant_id,identities.id,'identity.banned','DEAD',now()-interval '100 days' FROM identities CROSS JOIN generate_series(1,5001);
        INSERT INTO tenant_notifications (tenant_id,identity_id,kind,created_at)
          SELECT identities.tenant_id,identities.id,'identity.banned',now()-interval '400 days' FROM identities;")
        .execute(&admin).await?;
    let cleanup = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            admin
                .connect_options()
                .as_ref()
                .clone()
                .username("tenant_cleanup")
                .password("tcpw"),
        )
        .await?;
    let started = Instant::now();
    let result = tgm_app::tenant_retention::run(
        &cleanup,
        Category::TenantNotifications,
        std::future::pending(),
    )
    .await?;
    assert_eq!(
        (result.deleted, result.batches, result.tenants),
        (5001, 2, 1)
    );
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM tenant_notifications WHERE status='PENDING'"
        )
        .fetch_one(&admin)
        .await?,
        1
    );
    let counts:Vec<i64>=sqlx::query_scalar("SELECT (context->>'rows')::bigint FROM system_logs WHERE context->>'table'='tenant_notifications' ORDER BY id").fetch_all(&admin).await?;
    assert_eq!(counts, vec![5000, 1]);
    let rerun = tgm_app::tenant_retention::run(
        &cleanup,
        Category::TenantNotifications,
        std::future::pending(),
    )
    .await?;
    assert_eq!(rerun.deleted, 0);
    cleanup.close().await;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn paginated_all_status_cleanup_waits_skips_overlap_and_cancels_locked_query(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::raw_sql("INSERT INTO tenants (name,owner_user_id,status) SELECT 'tenant-'||number,93000+number,CASE number%3 WHEN 0 THEN 'active' WHEN 1 THEN 'suspended' ELSE 'deleted' END FROM generate_series(1,102) number;
        INSERT INTO login_sessions (tenant_id,kind,state,expires_at) SELECT id,'qr','expired',now()-interval '1 day' FROM tenants;")
        .execute(&admin).await?;
    let first: TenantId = sqlx::query_scalar("SELECT id FROM tenants ORDER BY id LIMIT 1")
        .fetch_one(&admin)
        .await?;
    sqlx::query("INSERT INTO login_sessions (tenant_id,kind,state,expires_at) SELECT $1,'qr','expired',now()-interval '1 day' FROM generate_series(1,5000)").bind(first.get()).execute(&admin).await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            admin
                .connect_options()
                .as_ref()
                .clone()
                .username("tenant_cleanup")
                .password("tcpw"),
        )
        .await?;
    let mut held = pool.begin().await?;
    assert!(tgm_db::tenant_retention::try_lock(&mut held, first, Category::LoginSessions).await?);
    let result =
        tgm_app::tenant_retention::run(&pool, Category::LoginSessions, std::future::pending())
            .await?;
    assert_eq!(
        (result.tenants, result.deleted, result.overlaps),
        (102, 101, 1)
    );
    held.rollback().await?;
    let started = Instant::now();
    let result =
        tgm_app::tenant_retention::run(&pool, Category::LoginSessions, std::future::pending())
            .await?;
    assert_eq!(
        (result.tenants, result.deleted, result.batches),
        (102, 5001, 103)
    );
    assert!(started.elapsed() >= Duration::from_millis(200));
    let result = tgm_app::tenant_retention::run(&pool, Category::LoginSessions, async {}).await?;
    assert!(result.stopped);
    assert_eq!(result.tenants, 0);
    sqlx::query("INSERT INTO login_sessions (tenant_id,kind,state,expires_at) VALUES ($1,'qr','expired',now()-interval '1 day')").bind(first.get()).execute(&admin).await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE login_sessions SET state='failed' WHERE tenant_id=$1")
        .bind(first.get())
        .execute(&mut *held)
        .await?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move {
        tgm_app::tenant_retention::run(&task_pool, Category::LoginSessions, async {
            let _ = stopped.await;
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.usename='tenant_cleanup' AND NOT locks.granted)").fetch_one(&admin).await?;
        if waiting {
            break;
        }
        assert!(Instant::now() < deadline, "清理未等待行锁");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).map_err(|_| "清理提前结束")?;
    let result = tokio::time::timeout(Duration::from_secs(5), task).await???;
    assert!(result.stopped);
    tokio::time::timeout(Duration::from_secs(5), pool.close()).await?;
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM login_sessions")
            .fetch_one(&admin)
            .await?,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn unmatched_batches_report_protected_backlog_and_cancel_uncommitted_rows(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::raw_sql("INSERT INTO tenants (name,owner_user_id) VALUES ('short-batch',99201);
        INSERT INTO identities (kind) VALUES ('mtproto');
        INSERT INTO source_subscriptions (tenant_id,tg_chat_id,receiver_identity_id)
            SELECT tenants.id,-1001,identities.id FROM tenants CROSS JOIN identities;
        INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,created_at,fanned_out)
            SELECT 'mtproto',id,-1001,number,now(),now()-interval '2 days',true FROM identities CROSS JOIN generate_series(1,5001) number;
        INSERT INTO tenant_events (tenant_id,canonical_event_id,source_ref,processed_at)
            SELECT sources.tenant_id,events.id,sources.id,now() FROM source_subscriptions sources CROSS JOIN canonical_events events;")
        .execute(&admin).await?;
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants")
        .fetch_one(&admin)
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            admin
                .connect_options()
                .as_ref()
                .clone()
                .username("tenant_cleanup")
                .password("tcpw"),
        )
        .await?;
    let mut held = admin.begin().await?;
    sqlx::query("SELECT id FROM tenant_events ORDER BY id LIMIT 1 FOR UPDATE")
        .execute(&mut *held)
        .await?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move {
        tgm_app::tenant_retention::run(&task_pool, Category::UnmatchedEvents, async {
            let _ = stopped.await;
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.usename='tenant_cleanup' AND NOT locks.granted)").fetch_one(&admin).await?;
        if waiting {
            break;
        }
        assert!(Instant::now() < deadline, "清理未等待行锁");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).map_err(|_| "清理提前结束")?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await???
            .stopped
    );
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenant_events")
            .fetch_one(&admin)
            .await?,
        5001
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM system_logs WHERE module='tenant_retention'"
        )
        .fetch_one(&admin)
        .await?,
        0
    );
    sqlx::query(
        "UPDATE tenant_events SET processed_at=NULL WHERE id=(SELECT max(id) FROM tenant_events)",
    )
    .execute(&admin)
    .await?;
    let started = Instant::now();
    let result =
        tgm_app::tenant_retention::run(&pool, Category::UnmatchedEvents, std::future::pending())
            .await?;
    assert_eq!(
        (result.deleted, result.retained, result.batches),
        (5000, 1, 2)
    );
    assert!(started.elapsed() >= Duration::from_millis(200));
    sqlx::query("UPDATE tenant_events SET processed_at=now() WHERE tenant_id=$1")
        .bind(tenant)
        .execute(&admin)
        .await?;
    let result =
        tgm_app::tenant_retention::run(&pool, Category::UnmatchedEvents, std::future::pending())
            .await?;
    assert_eq!((result.deleted, result.retained), (1, 0));
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT sum((context->>'rows')::bigint)::bigint FROM system_logs WHERE module='tenant_retention'").fetch_one(&admin).await?,5001);
    Ok(())
}
