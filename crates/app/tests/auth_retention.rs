use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_db::auth_retention::Category;

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_runs_multiple_batches_waits_and_stops_without_leaking_transaction(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!(
            "postgres://auth_cleanup:cpw@127.0.0.1:55432/{database}"
        ))
        .await?;
    sqlx::query("INSERT INTO console_login_challenges (nonce, confirm_code, origin_ip, expires_at) SELECT int8send(number), 'ABCD', '127.0.0.1', now() - interval '1 hour' FROM generate_series(1, 5001) number")
        .execute(&admin).await?;
    let started = Instant::now();
    let result =
        tgm_app::auth_retention::run(&pool, Category::Challenges, std::future::pending()).await?;
    assert_eq!(result.deleted, 5001);
    assert_eq!(result.batches, 2);
    assert!(!result.overlapped && !result.stopped);
    assert!(started.elapsed() >= Duration::from_millis(200));
    let result = tgm_app::auth_retention::run(&pool, Category::Challenges, async {}).await?;
    assert!(result.stopped);
    assert_eq!(result.batches, 0);
    let mut lock = admin.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(Category::Challenges.lock_key())
        .execute(&mut *lock)
        .await?;
    let result =
        tgm_app::auth_retention::run(&pool, Category::Challenges, std::future::pending()).await?;
    assert!(result.overlapped);
    assert_eq!(result.batches, 0);
    lock.rollback().await?;
    let result =
        tgm_app::auth_retention::run(&pool, Category::Challenges, std::future::pending()).await?;
    assert_eq!(result.batches, 1);
    assert_eq!(result.deleted, 0);
    sqlx::query("INSERT INTO console_login_challenges (nonce, confirm_code, origin_ip, expires_at) VALUES (decode('01','hex'), 'ABCD', '127.0.0.1', now() - interval '1 hour')")
        .execute(&admin).await?;
    let mut held = admin.begin().await?;
    sqlx::query("UPDATE console_login_challenges SET confirm_code = 'EFGH' WHERE nonce = decode('01','hex')")
        .execute(&mut *held).await?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move {
        tgm_app::auth_retention::run(&task_pool, Category::Challenges, async {
            let _ = stopped.await;
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid = locks.pid WHERE activity.datname = current_database() AND activity.usename = 'auth_cleanup' AND NOT locks.granted)")
            .fetch_one(&admin).await?;
        if waiting {
            break;
        }
        assert!(Instant::now() < deadline, "清理没有等到行锁");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).map_err(|_| "清理提前退出")?;
    let result = tokio::time::timeout(Duration::from_secs(5), task).await???;
    assert!(result.stopped);
    tokio::time::timeout(Duration::from_secs(5), pool.close()).await?;
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM console_login_challenges")
            .fetch_one(&admin)
            .await?,
        1
    );
    Ok(())
}
