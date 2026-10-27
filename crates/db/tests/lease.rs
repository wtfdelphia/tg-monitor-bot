//! 租约 repo 的 L2 测试。行为定义见 docs/design/spec/08-账号接入.md §4。

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use std::time::Duration;
use tgm_core::ids::IdentityId;
use tgm_db::repo::{acquire_lease, release_lease, renew_lease};

async fn seed_identity(admin: &support::AdminOnlyPool) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, status) VALUES ('mtproto', 'offline') RETURNING id",
    )
    .fetch_one(admin.raw())
    .await
    .expect("建租约测试身份失败")
}

async fn lease_times(pool: &PgPool, identity_id: IdentityId) -> (f64, f64) {
    sqlx::query_as::<_, (f64, f64)>(
        "SELECT extract(epoch FROM heartbeat_at)::double precision,
                extract(epoch FROM expires_at)::double precision
           FROM account_leases
          WHERE identity_id = $1",
    )
    .bind(identity_id.get())
    .fetch_one(pool)
    .await
    .expect("读取租约时间失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn lease_acquire_takeover_and_stale_worker_are_guarded(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity_id = seed_identity(&admin).await;

    let first = acquire_lease(&app, None, identity_id, "worker-a")
        .await
        .expect("首次获取租约失败");
    assert_eq!(first, Some(1));

    let same_worker = acquire_lease(&app, None, identity_id, "worker-a")
        .await
        .expect("同 worker 重复获取租约失败");
    assert_eq!(same_worker, Some(1), "同 worker 获取不应递增 token");

    let other = acquire_lease(&app, None, identity_id, "worker-b")
        .await
        .expect("他人获取租约查询失败");
    assert_eq!(other, None, "有效租约不应被其他 worker 抢走");

    sqlx::query("UPDATE account_leases SET expires_at = now() - interval '1 second'")
        .execute(admin.raw())
        .await
        .expect("使租约过期失败");

    let takeover = acquire_lease(&app, None, identity_id, "worker-b")
        .await
        .expect("过期租约抢占失败");
    assert_eq!(takeover, Some(2));

    let stale_renew = renew_lease(&app, None, identity_id, "worker-a", 1)
        .await
        .expect("旧 worker 续租查询失败");
    assert!(!stale_renew, "旧 worker 不得续租已被抢占的身份");

    let current_renew = renew_lease(&app, None, identity_id, "worker-b", 2)
        .await
        .expect("当前 worker 续租失败");
    assert!(current_renew);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn lease_renew_keeps_token_and_moves_deadlines(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity_id = seed_identity(&admin).await;

    let token = acquire_lease(&app, None, identity_id, "worker-a")
        .await
        .expect("获取租约查询失败")
        .expect("首次获取租约没有 token");
    let before = lease_times(&app, identity_id).await;

    sqlx::query("SELECT pg_sleep(0.02)")
        .execute(admin.raw())
        .await
        .expect("等待数据库时钟推进失败");

    assert!(
        renew_lease(&app, None, identity_id, "worker-a", token)
            .await
            .expect("续租查询失败")
    );
    let after = lease_times(&app, identity_id).await;

    assert_eq!(
        acquire_lease(&app, None, identity_id, "worker-a")
            .await
            .expect("续租后读取 token 失败"),
        Some(token),
        "正常续租不得改变 fencing token"
    );
    assert!(after.0 > before.0, "heartbeat_at 没有前进");
    assert!(after.1 > before.1, "expires_at 没有前进");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn expired_lease_race_has_exactly_one_winner_and_release_preserves_monotonicity(
    admin: PgPool,
) {
    let (app_a, admin) = support::pools(admin).await;
    support::assert_role(&app_a, "app_user").await;
    let app_b = {
        let database = sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(admin.raw())
            .await
            .expect("读取测试库名失败");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!(
                "postgres://app_user:apw@127.0.0.1:55432/{database}"
            ))
            .await
            .expect("建立第二个受限池失败")
    };
    support::assert_role(&app_b, "app_user").await;

    let identity_id = seed_identity(&admin).await;
    assert_eq!(
        acquire_lease(&app_a, None, identity_id, "seed-worker")
            .await
            .expect("建立初始租约失败"),
        Some(1)
    );
    sqlx::query("UPDATE account_leases SET expires_at = now() - interval '1 second'")
        .execute(admin.raw())
        .await
        .expect("使并发测试租约过期失败");

    let (left, right) = tokio::join!(
        acquire_lease(&app_a, None, identity_id, "worker-a"),
        acquire_lease(&app_b, None, identity_id, "worker-b")
    );
    let results = [
        left.expect("worker-a 抢占查询失败"),
        right.expect("worker-b 抢占查询失败"),
    ];
    assert_eq!(
        results.iter().filter(|token| token.is_some()).count(),
        1,
        "过期租约并发抢占必须恰好成功一次：{results:?}"
    );
    assert!(results.contains(&Some(2)));

    let winner = if results[0] == Some(2) {
        ("worker-a", &app_a)
    } else {
        ("worker-b", &app_b)
    };
    assert!(
        release_lease(winner.1, identity_id, winner.0, 2)
            .await
            .expect("释放租约查询失败")
    );
    assert_eq!(
        acquire_lease(&app_a, None, identity_id, "worker-c")
            .await
            .expect("释放后重新获取租约失败"),
        Some(3),
        "释放后 token 仍必须单调递增"
    );

    app_b.close().await;
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn lease_expiry_is_rechecked_after_row_lock_wait(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity_id = seed_identity(&admin).await;

    assert_eq!(
        acquire_lease(&app, None, identity_id, "worker-a")
            .await
            .expect("建立续租测试租约失败"),
        Some(1)
    );

    sqlx::query(
        "UPDATE account_leases SET expires_at = clock_timestamp() + interval '50 milliseconds'",
    )
    .execute(admin.raw())
    .await
    .expect("设置短 TTL 失败");

    let mut blocker = admin.raw().begin().await.expect("开启行锁事务失败");
    sqlx::query("SELECT identity_id FROM account_leases WHERE identity_id = $1 FOR UPDATE")
        .bind(identity_id.get())
        .execute(&mut *blocker)
        .await
        .expect("持有租约行锁失败");

    let renew_pool = app.clone();
    let renew_task =
        tokio::spawn(
            async move { renew_lease(&renew_pool, None, identity_id, "worker-a", 1).await },
        );
    tokio::time::sleep(Duration::from_millis(150)).await;
    blocker.commit().await.expect("释放租约行锁失败");

    assert!(
        !renew_task
            .await
            .expect("等待续租任务失败")
            .expect("续租查询失败"),
        "实际过期后的租约不得因等待行锁而复活"
    );

    assert_eq!(
        acquire_lease(&app, None, identity_id, "worker-b")
            .await
            .expect("过期租约接管失败"),
        Some(2)
    );

    sqlx::query(
        "UPDATE account_leases SET expires_at = clock_timestamp() + interval '50 milliseconds'",
    )
    .execute(admin.raw())
    .await
    .expect("设置第二次短 TTL 失败");

    let mut blocker = admin.raw().begin().await.expect("开启第二次行锁事务失败");
    sqlx::query("SELECT identity_id FROM account_leases WHERE identity_id = $1 FOR UPDATE")
        .bind(identity_id.get())
        .execute(&mut *blocker)
        .await
        .expect("持有第二次租约行锁失败");

    let acquire_pool = app.clone();
    let acquire_task =
        tokio::spawn(
            async move { acquire_lease(&acquire_pool, None, identity_id, "worker-c").await },
        );
    tokio::time::sleep(Duration::from_millis(150)).await;
    blocker.commit().await.expect("释放第二次租约行锁失败");

    assert_eq!(
        acquire_task
            .await
            .expect("等待接管任务失败")
            .expect("接管查询失败"),
        Some(3),
        "等待行锁期间过期的租约应在解锁后立即允许接管"
    );
}
