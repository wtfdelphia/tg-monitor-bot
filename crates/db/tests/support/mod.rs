//! 隔离测试的连接脚手架。定义处是 docs/design/eng/03-测试策略.md §二。
//!
//! 本模块存在的唯一理由是 §零 发现 1：`#[sqlx::test]` 注入的 `PgPool` 走
//! `DATABASE_URL`，而它要 `CREATE DATABASE`，所以那必然是超级用户 ——
//! 超级用户即使在 FORCE 的表上、且完全不设 `app.tenant_id`，也能看到全部行。
//! 用它跑隔离测试会「全绿但什么都没测到」。

// Rust 的每个 integration test 文件是独立 crate，本模块会被各自编译一遍，
// 每个 target 只用得到其中一部分 —— 所以 dead_code 在这里不代表「没人用」。
// 代价：一个真的没人用的 helper 也不会再报警，比如某条断言里本该调用它却漏了。
// 这一层由「每条测试先断言 current_user」的纪律兜（eng/03 §2.1），不靠编译器。
#![allow(dead_code)]
// 本模块的 assert_* 是断言 helper，同 isolation.rs 的理由。
#![allow(clippy::expect_used, clippy::panic)]

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

/// `sqlx::test` 注入的超级用户池。
///
/// 包成 newtype 且**不实现 `Deref`**（eng/03 §2.1）：两个池的类型本来一样，
/// 误用靠评审拦不住，包起来能让它不可编译。取用要显式写 `.raw()`，
/// 那个调用点就是评审要看的地方。
pub struct AdminOnlyPool(PgPool);

impl AdminOnlyPool {
    /// 只允许两种用途：跑 DDL 前置、以及断言的**事前/事后**取数。
    /// 任何验证隔离的查询都不得用它 —— 用 [`app_pool`] 换到受限角色。
    pub fn raw(&self) -> &PgPool {
        &self.0
    }
}

/// 把 `sqlx::test` 的池收进 newtype，同时开出一个受限角色池。
///
/// 返回顺序刻意是 (受限, 管理)：测试里先拿到的那个是该用的那个。
pub async fn pools(injected: PgPool) -> (PgPool, AdminOnlyPool) {
    let app = app_pool(&injected).await;
    (app, AdminOnlyPool(injected))
}

/// 以 `app_user` 连到 `sqlx::test` 刚建出来的那个库。
///
/// 口令是 eng/02 §二 的本地开发值，与 scripts/init/01-roles.sql 一致。
/// 端口 55432 是直连 PG；经 PgBouncer 的 56432 由 [`app_pool_via_bouncer`] 用。
async fn app_pool(admin: &PgPool) -> PgPool {
    let db = current_database(admin).await;
    connect_as("app_user", "apw", 55432, &db).await
}

/// 经 PgBouncer 的受限角色池。R3/R4 必须走这条（eng/03 §四）。
///
/// PgBouncer 的 `DB_NAME` 固定指向 `tgm`，连不到 `sqlx::test` 建的临时库 ——
/// 所以走这条的测试用的是 compose 里那个常驻库，不是每测试一库。
pub async fn app_pool_via_bouncer() -> PgPool {
    connect_as("app_user", "apw", 56432, "tgm").await
}

async fn current_database(pool: &PgPool) -> String {
    sqlx::query("SELECT current_database()")
        .fetch_one(pool)
        .await
        .expect("取当前库名失败")
        .get(0)
}

async fn connect_as(role: &str, pw: &str, port: u16, db: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!("postgres://{role}:{pw}@127.0.0.1:{port}/{db}"))
        .await
        .unwrap_or_else(|e| panic!("以 {role} 连 {db}:{port} 失败：{e}"))
}

/// eng/03 §2.3：每个隔离测试开头都要断言自己的角色。
///
/// 这一句防的是 §2.1 那个错误静默发生 —— 两个池类型一样，
/// 而 newtype 只挡得住直接误用，挡不住有人自己另开一个池。
pub async fn assert_role(pool: &PgPool, expect: &str) {
    let actual: String = sqlx::query("SELECT current_user")
        .fetch_one(pool)
        .await
        .expect("查 current_user 失败")
        .get(0);
    assert_eq!(actual, expect, "连接角色不对，这个测试测不到隔离");
}

/// 顺带断言这个角色确实不是 BYPASSRLS —— 角色对了但带 bypassrls 一样是假绿。
pub async fn assert_not_bypassrls(pool: &PgPool) {
    let bypass: bool =
        sqlx::query("SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(pool)
            .await
            .expect("查 rolbypassrls 失败")
            .get(0);
    assert!(!bypass, "当前角色带 BYPASSRLS，RLS 被整个旁路了");
}
