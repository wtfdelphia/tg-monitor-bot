//! `AdminPool` 与 `AdminOnlyPool` 的编译期约束自检。
//! 判据出处：plan/00-里程碑与工期.md §4.3 第 3 条、eng/03-测试策略.md §2.1。
//!
//! 这两个 newtype 的全部价值在于「误用不可编译」，而运行时测试证明不了
//! 「不可编译」—— 一个忘了「不实现 Deref」的版本会让所有普通测试照样全绿。
//!
//! 所以这里不试图在 Rust 里表达这个命题（试过两次都失败，记录在文件末尾），
//! 改成让 `scripts/check-newtype.sh` 这条 grep 级检查来管，而本文件只保留它的
//! 正向对偶：`AdminPool` 必须能用（`admin_tx` 这条唯一入口是通的）。

// 同 isolation.rs：测试里的 expect 就是断言。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use tgm_db::admin::{AdminPool, admin_tx};

/// `AdminPool` 的取用路径只有 `admin_tx`。
///
/// 这条测试的作用是反向的：如果有人为了方便给 `AdminPool` 加了 `Deref` 或
/// 一个 `pub fn pool(&self)`，本测试**不会**变红 —— 那由 `scripts/check-newtype.sh`
/// 管（它对 `AdminPool` 不放行任何取池方法名，`raw` 也不放行 ——
/// 那个白名单只对本文件用的测试侧 `AdminOnlyPool` 生效）。
/// 这里只保证 `admin_tx` 这条正路是通的，
/// 否则「唯一入口」可能是一个谁都用不了的入口。
#[sqlx::test(migrations = "../../migrations")]
async fn admin_tx_is_the_usable_path(admin: sqlx::PgPool) {
    let (_app, injected) = support::pools(admin).await;

    // 平台池在测试里用 platform_ops 连（eng/00 §4.2：两个池不是同一个池换角色）。
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(injected.raw())
        .await
        .expect("取库名失败");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("postgres://platform_ops:ppw@127.0.0.1:55432/{db}"))
        .await
        .expect("以 platform_ops 连库失败");
    let admin_pool = AdminPool::new(pool);

    // admin_tx 不设 app.tenant_id，只查 tenant_id IS NULL 的行。
    let n = admin_tx(&admin_pool, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE tenant_id IS NULL")
                .fetch_one(conn)
                .await
        })
    })
    .await
    .expect("admin_tx 查平台日志失败 —— 唯一入口不通");
    assert_eq!(n, 0, "干净的测试库里平台日志应为空");
}

// ── 为什么「不实现 Deref」这条不在本文件里测 ─────────────────────────────
//
// 试过两条路，都失败。记在这里是因为第二条的失败方式值得记：
//
// 1 autoref 特化探针，第一版。`Specific` 挂在 `&Probe<T>` 上、`Fallback` 挂在
//   `Probe<T>` 上，靠方法解析优先选取值接收者、bound 不满足时再取引用来区分
//   「T 实现了 Deref」与「没实现」。
//   编译过了，两条断言也绿了。反证时给 `AdminPool` 加上 `impl Deref` ——
//   **测试照样全绿**。原因是方法解析第一轮就命中了挂在 `Probe<T>` 上的
//   `Fallback`，`Specific` 从头到尾没被考虑过，探针恒返回同一个值。
//   这是「验证手段坏掉时表现为通过」的一例，只有反证能发现。
//
// 2 把两侧调过来（`Specific` 挂 `Probe<T>`、`Fallback` 挂 `&Probe<T>`）。
//   正向与反证都报 E0599：`the method is_pgpool exists for struct Probe<T>,
//   but its trait bounds were not satisfied`。
//   结论：autoref 回退在泛型 `T` 上不成立 —— bound 不满足时 rustc 直接报错，
//   不会继续尝试 `&Probe<T>`。这个技巧只在具体类型上有效。
//   用 UFCS 写成 `Specific::is_pgpool(&&p)` 则变成 E0277，强制要求 `T: Deref`。
//
// 同一条路第二次失败后换了方向：「某类型不实现某 trait」在稳定 Rust 里
// 写不成运行时断言，把它交给源码层面的检查，并给那条检查配上自检。
