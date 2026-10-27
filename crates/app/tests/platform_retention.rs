use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};

#[sqlx::test(migrations = "../../migrations-test")]
async fn batches_wait_overlap_cancel_and_resume(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!(
            "postgres://platform_cleanup:pcpw@127.0.0.1:55432/{database}"
        ))
        .await?;
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&pool)
        .await?;
    assert_eq!(role, "platform_cleanup");
    sqlx::query("INSERT INTO system_logs (level,module,event,created_at) SELECT 'INFO','test','old',now()-interval '91 days' FROM generate_series(1,5001)")
        .execute(&admin).await?;
    let started = Instant::now();
    let result = tgm_app::platform_retention::run(&pool, std::future::pending()).await?;
    assert_eq!(result.deleted, 5001);
    assert_eq!(result.batches, 2);
    assert!(!result.overlapped && !result.stopped);
    assert!(started.elapsed() >= Duration::from_millis(200));
    let rows: i64=sqlx::query_scalar("SELECT sum((context->>'rows')::bigint)::bigint FROM system_logs WHERE module='platform_retention'").fetch_one(&admin).await?;
    assert_eq!(rows, 5001);
    let result = tgm_app::platform_retention::run(&pool, std::future::pending()).await?;
    assert_eq!((result.deleted, result.batches), (0, 1));
    let result = tgm_app::platform_retention::run(&pool, async {}).await?;
    assert!(result.stopped && result.batches == 0);
    let mut held = admin.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(tgm_db::platform_retention::LOCK_KEY)
        .execute(&mut *held)
        .await?;
    let result = tgm_app::platform_retention::run(&pool, std::future::pending()).await?;
    assert!(result.overlapped && result.batches == 0);
    held.rollback().await?;
    sqlx::query("INSERT INTO system_logs (level,module,event,created_at) VALUES ('INFO','test','locked',now()-interval '91 days')").execute(&admin).await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE system_logs SET context='{}' WHERE event='locked'")
        .execute(&mut *held)
        .await?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move {
        tgm_app::platform_retention::run(&task_pool, async {
            let _ = stopped.await;
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid
            WHERE activity.datname=current_database() AND activity.usename='platform_cleanup' AND NOT locks.granted)")
            .fetch_one(&admin).await?;
        if waiting {
            break;
        }
        assert!(Instant::now() < deadline, "未等到真实行锁");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).map_err(|_| "清理提前退出")?;
    let result = tokio::time::timeout(Duration::from_secs(10), task).await???;
    assert!(result.stopped);
    tokio::time::timeout(Duration::from_secs(10), pool.close()).await?;
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE event='locked'")
            .fetch_one(&admin)
            .await?,
        1
    );
    let pool = PgPool::connect(&format!(
        "postgres://platform_cleanup:pcpw@127.0.0.1:55432/{database}"
    ))
    .await?;
    let result = tgm_app::platform_retention::run(&pool, std::future::pending()).await?;
    assert_eq!(result.deleted, 1);
    Ok(())
}
