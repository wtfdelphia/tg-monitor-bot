//! 反向测试：经 PgBouncer 的那两条。清单与编号在 docs/design/eng/03-测试策略.md §三，
//! 实测形态在 §四。
//!
//! 与 isolation.rs 分文件而不是并进去，是两条硬约束逼出来的：
//!
//!   1. **不能用 `#[sqlx::test]`。** 那个宏每条测试建一个临时库，而 PgBouncer 的
//!      `DB_NAME` 固定指向 `tgm`（compose.yaml），连不到临时库。所以这里跑在
//!      compose 起的那个常驻库上 —— 共享库，所以要灌数据的 R4 自己建自己删，
//!      名字带本测试专用前缀，见 `seed_two_tenants`。
//!   2. **测试必须自己确认命中了复用的 server 连接。** 拿不到复用时，B 看到的
//!      是全新连接的形状（NULL，那是 R1 管的事），R3 什么都没验到却会绿。

// 同 isolation.rs：反向测试里的 expect 与 panic 就是断言本身。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tgm_core::ids::TenantId;
use tgm_db::tenant::tenant_tx;

/// 客户端 A：用 **session 级** `set_config` 设上租户，事务结束后把连接交还给
/// PgBouncer。返回它落在哪条 server 连接上（后端 pid）。
///
/// 第三个参数是 `false`，这正是 R3 的成因 —— 业务代码里必须是 `true`
/// （isolation.rs 每条测试都写 `true`，tenant_tx 也是）。
async fn leak_attempt(pool: &PgPool) -> i32 {
    let mut tx = pool.begin().await.expect("A 开事务失败");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .expect("取 A 的后端 pid 失败");

    sqlx::query("SELECT set_config('app.tenant_id', '2', false)")
        .execute(&mut *tx)
        .await
        .expect("A 设租户上下文失败");

    // 自检，不是样板：要在 A 自己的连接上确认它真的设上了。
    // 漏掉这一句的话，下面「B 看不到 2」有可能只是因为 A 根本没设成功 ——
    // 那种情况下整条测试是空的。
    let seen: String = sqlx::query_scalar("SELECT current_setting('app.tenant_id')")
        .fetch_one(&mut *tx)
        .await
        .expect("A 读回自己设的值失败");
    assert_eq!(seen, "2", "A 没能设上 session 级变量 —— 本测试测不到泄漏");

    tx.commit().await.expect("A 提交失败");
    pid
}

/// R3 会话变量跨客户端泄漏 → 必须 fail-loud，且报错文本与 R1 不同。
///
/// 承重的配置是 compose.yaml 里 pgbouncer 的 `SERVER_RESET_QUERY_ALWAYS: 1`
/// （§四 第 1 条：那是安全配置，不是性能调优）。缺了它，下面 B 读到的是 `"2"`，
/// 即真实的跨租户泄漏，方向取决于连接复用顺序。
#[tokio::test]
async fn r3_session_var_does_not_leak_across_clients() {
    let a = support::app_pool_via_bouncer().await;
    support::assert_role(&a, "app_user").await;
    support::assert_not_bypassrls(&a).await;

    let b = support::app_pool_via_bouncer().await;

    // 循环到 B 真的落在 A 用过的那条 server 连接上。transaction 模式下
    // 客户端连接与 server 连接是解耦的，命中不是必然的 —— 而没命中的那一轮
    // 看起来和通过没有区别，所以「命中」本身要断言（见文件头第 2 条）。
    const ROUNDS: usize = 20;
    let mut hit = None;
    for _ in 0..ROUNDS {
        let a_pid = leak_attempt(&a).await;

        let mut tx = b.begin().await.expect("B 开事务失败");
        let b_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .expect("取 B 的后端 pid 失败");
        // 用 `current_setting(..., true)` 的 missing_ok 形态取原始值：
        // 这一句不能报错，否则下面分不清「拿到什么」与「为什么失败」。
        let raw: Option<String> =
            sqlx::query_scalar("SELECT current_setting('app.tenant_id', true)")
                .fetch_one(&mut *tx)
                .await
                .expect("B 读 app.tenant_id 失败");

        if b_pid != a_pid {
            continue; // 落在别的 server 连接上，这一轮不算数
        }

        // 同一个事务里接着跑业务查询：换事务的话就不保证还在这条连接上了。
        // 这一句会失败并让事务 abort，所以顺序不能和上面那句对调。
        let err = sqlx::query("SELECT count(*) FROM rules")
            .fetch_one(&mut *tx)
            .await
            .expect_err("B 在复用的连接上查 rules 竟然成功了 —— 会话变量泄漏了");
        hit = Some((raw, err.to_string()));
        break;
    }

    let (raw, msg) = hit.unwrap_or_else(|| {
        panic!("{ROUNDS} 轮都没命中 A 用过的那条 server 连接 —— 本测试没验到任何东西")
    });

    assert_ne!(
        raw.as_deref(),
        Some("2"),
        "B 在复用的连接上读到了 A 设的租户 —— 真实的跨租户泄漏，\
         检查 compose.yaml 的 SERVER_RESET_QUERY_ALWAYS"
    );
    // §四 第 2 条：`DISCARD ALL` 把参数重置为**空串，不是 NULL**。
    // 全新连接（R1）那边是 NULL，所以这一条的期望值与 R1 不同。
    assert_eq!(
        raw.as_deref(),
        Some(""),
        "复用连接上读到的既不是泄漏值也不是空串 —— DISCARD ALL 的行为变了，\
         §四 第 2 条那条实测结论以及 R1/R3 两套断言都要重新核对"
    );
    // 承接上一条：报错文本必须是空串转 bigint 那条，不是 R1 的
    // `unrecognized configuration parameter`。两条都 fail-loud 但不能共用断言。
    assert!(
        msg.contains("invalid input syntax for type bigint"),
        "fail-loud 了，但文本不是预期的那条（与 R1 混了？）：{msg}"
    );
}

/// R4 用的种子数据。常驻库是共享的，所以名字带 `r4-` 前缀且用完即删。
///
/// 用超级用户直连 55432 灌：这一步是前置条件不是被测对象，而 `app_user`
/// 在 FORCE 的表上灌两个租户的数据要来回切 `app.tenant_id`，
/// 那会把前置条件本身变成一个要验的东西。
///
/// 返回 `TenantId` 而不是 `i64`：它没有 `new` 也没有 `From<i64>`（eng/00 §4.1），
/// 只能从库里解码 —— `RETURNING id` 就是那条路径。
async fn seed_two_tenants(admin: &PgPool) -> (TenantId, TenantId) {
    let mut ids = Vec::new();
    for (name, owner) in [("r4-a", 4001_i64), ("r4-b", 4002)] {
        let id: TenantId = sqlx::query_scalar(
            "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(name)
        .bind(owner)
        .fetch_one(admin)
        .await
        .expect("灌租户失败");
        sqlx::query("INSERT INTO keywords (tenant_id, word) VALUES ($1, $2)")
            .bind(id.get())
            .bind(format!("kw-{name}"))
            .execute(admin)
            .await
            .expect("灌关键词失败");
        ids.push(id);
    }
    (ids[0], ids[1])
}

/// 一轮取样：该租户可见的关键词、落在哪条 server 连接上、以及那条连接上
/// 此刻存在的协议级预编译语句条数。
///
/// 三样一起取而且都在**同一个** `tenant_tx` 里：换事务就不保证还在同一条
/// server 连接上，那时读到的预编译语句表是别人的。
async fn sample(pool: &PgPool, tenant: TenantId) -> (Vec<String>, i32, i64) {
    tenant_tx(pool, tenant, |tx| {
        Box::pin(async move {
            let words: Vec<String> = sqlx::query_scalar("SELECT word FROM keywords ORDER BY word")
                .fetch_all(&mut *tx)
                .await?;
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await?;
            let cached: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_prepared_statements")
                .fetch_one(&mut *tx)
                .await?;
            Ok((words, pid, cached))
        })
    })
    .await
    .expect("tenant_tx 取样失败")
}

/// R4 预编译语句跨租户复用 → 第二次必须返回**第二个**租户的数据。
///
/// §四 记了这条的测法为什么要改：SQL 层 `PREPARE` 在 transaction 模式下
/// 根本测不到（同一客户端的下一条 `EXECUTE` 就 `does not exist`，语句停在了
/// 另一条 server 连接上）。所以必须走 sqlx 的协议级预编译语句 + 语句缓存，
/// 配 PgBouncer 的 `MAX_PREPARED_STATEMENTS: 200`（compose.yaml）。
///
/// 测的是「缓存下来的执行计划会不会把第一个租户的 `app.tenant_id` 一起记住」。
/// 不会 —— RLS 策略里的 `current_setting` 在执行期求值，不是计划期。但那句话是
/// 推理，本测试是证据；它同时是一条回归网：若日后有人把租户条件搬进计划期
/// 能固化的位置，这里会红。
///
/// **三条自检缺一不可**，因为这条测试的失败方式主要是「什么都没测到」：
/// 单连接池（否则第二次可能走一条没有缓存的新连接）、两轮落在同一 server 连接、
/// 以及那条连接上确实已经有缓存的预编译语句。
#[tokio::test]
async fn r4_prepared_statement_reuse_returns_the_second_tenant() {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect("postgres://postgres:pw@127.0.0.1:55432/tgm")
        .await
        .expect("以 postgres 连 tgm:55432 失败");
    // 上一轮若中途 panic 会留下残留，先清。
    sqlx::query("DELETE FROM tenants WHERE name LIKE 'r4-%'")
        .execute(&admin)
        .await
        .expect("清理上一轮残留失败");
    let (a, b) = seed_two_tenants(&admin).await;

    // **一条连接**的池。`max_connections(1)` 是承重的：两条连接的话第二次查询
    // 可能走一条全新的、没有语句缓存的连接，那就什么都没测到。
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect("postgres://app_user:apw@127.0.0.1:56432/tgm")
        .await
        .expect("以 app_user 经 PgBouncer 连库失败");
    support::assert_role(&pool, "app_user").await;
    support::assert_not_bypassrls(&pool).await;

    let (first, pid1, cached1) = sample(&pool, a).await;
    let (second, pid2, cached2) = sample(&pool, b).await;
    // 第三次回到 A：证明这不是「只有第一次对」，也不是某种一次性的切换。
    let (third, _, _) = sample(&pool, a).await;

    pool.close().await;
    sqlx::query("DELETE FROM tenants WHERE name LIKE 'r4-%'")
        .execute(&admin)
        .await
        .expect("清理种子数据失败");
    admin.close().await;

    // 自检先行。两个租户各自只看得见自己那一条 —— 这一句若是 0 行，
    // 下面的 assert_ne 会因为「两边都是空」而无意义地绿。
    assert_eq!(first, vec!["kw-r4-a".to_string()], "A 看到的不对");
    assert_eq!(second, vec!["kw-r4-b".to_string()], "B 看到的不对");
    // 自检：两轮真的落在同一条 server 连接上。不同连接的话「语句缓存被跨租户
    // 复用」这个前提根本不成立，测试测的是别的东西。
    assert_eq!(
        pid1, pid2,
        "两轮落在了不同的 server 连接上（{pid1} vs {pid2}）—— 本测试没验到语句复用"
    );
    // 自检：那条连接上确实有缓存下来的预编译语句。实测第二轮起 PgBouncer
    // 会把 `PGBOUNCER_*` 那些留在 server 连接上，所以这个数不会是 0；
    // 若哪天它变成 0（比如 MAX_PREPARED_STATEMENTS 被改掉），这条测试
    // 就退化成一条普通的 RLS 测试，那时要在这里看见。
    assert!(
        cached2 > 0,
        "第二轮那条 server 连接上没有任何预编译语句（第一轮 {cached1}）—— \
         检查 compose.yaml 的 MAX_PREPARED_STATEMENTS"
    );

    assert_eq!(
        third, first,
        "第三次回到 A 却看到了别的 —— 租户上下文粘住了"
    );
    assert_ne!(
        first, second,
        "同一个池上连续两个租户看到了同一份数据 —— 预编译语句把第一个租户的上下文带过去了"
    );
}
