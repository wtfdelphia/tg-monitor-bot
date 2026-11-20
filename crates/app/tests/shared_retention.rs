use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};

#[sqlx::test(migrations = "../../migrations-test")]
async fn batches_group_progress_overlap_and_cancellation(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            admin
                .connect_options()
                .as_ref()
                .clone()
                .username("shared_cleanup")
                .password("scpw"),
        )
        .await?;
    tgm_db::shared_retention::check_connection(&pool).await?;
    let identity: i64 =
        sqlx::query_scalar("INSERT INTO identities (kind) VALUES ('mtproto') RETURNING id")
            .fetch_one(&admin)
            .await?;
    sqlx::query("INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,created_at,fanned_out)
        SELECT 'mtproto',$1,-1001,number,now(),now()-interval '2 days',true FROM generate_series(1,5001) number")
        .bind(identity).execute(&admin).await?;
    let started = Instant::now();
    let result = tgm_app::shared_retention::run(&pool, std::future::pending()).await?;
    assert_eq!(result.deleted, 5001);
    assert_eq!(result.batches, 3);
    assert!(started.elapsed() >= Duration::from_millis(400));
    let result = tgm_app::shared_retention::run(&pool, async {}).await?;
    assert!(result.stopped);
    assert_eq!(result.batches, 0);
    let mut held = admin.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('shared_retention:canonical_events',1952935218))")
        .execute(&mut *held).await?;
    let result = tgm_app::shared_retention::run(&pool, std::future::pending()).await?;
    assert!(result.overlapped);
    held.rollback().await?;
    let app = PgPoolOptions::new()
        .connect_with(
            admin
                .connect_options()
                .as_ref()
                .clone()
                .username("app_user")
                .password("apw"),
        )
        .await?;
    let group: i64 = sqlx::query_scalar("INSERT INTO canonical_events
        (adapter,receiver_identity_id,tg_chat_id,tg_message_id,event_type,message_date,created_at,fanned_out)
        VALUES ('mtproto',$1,-1001,8001,'MEDIA_GROUP_COMPLETED',now(),now()-interval '2 days',true) RETURNING id")
        .bind(identity).fetch_one(&admin).await?;
    let member: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events
        (adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,created_at,fanned_out)
        VALUES ('mtproto',$1,-1001,8002,now(),now()-interval '2 days',true) RETURNING id",
    )
    .bind(identity)
    .fetch_one(&admin)
    .await?;
    sqlx::query("INSERT INTO media_group_members (group_event_id,member_event_id,position) VALUES ($1,$2,0)")
        .bind(group).bind(member).execute(&app).await?;
    let result = tgm_app::shared_retention::run(&pool, std::future::pending()).await?;
    assert_eq!(result.deleted, 2);
    assert_eq!(result.batches, 3);
    sqlx::query("INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,created_at,fanned_out)
        VALUES ('mtproto',$1,-1001,9999,now(),now()-interval '2 days',true)")
        .bind(identity).execute(&admin).await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE canonical_events SET text='private'")
        .execute(&mut *held)
        .await?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move {
        tgm_app::shared_retention::run(&task_pool, async {
            let _ = stopped.await;
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid
            WHERE activity.datname=current_database() AND activity.usename='shared_cleanup' AND NOT locks.granted)")
            .fetch_one(&admin).await?;
        if waiting {
            break;
        }
        assert!(Instant::now() < deadline, "清理未等待行锁");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).map_err(|_| "清理已提前退出")?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await???
            .stopped
    );
    tokio::time::timeout(Duration::from_secs(5), pool.close()).await?;
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM canonical_events")
            .fetch_one(&admin)
            .await?,
        1
    );
    Ok(())
}
