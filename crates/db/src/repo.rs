//! Repo 层。调用点形态的定义处是 ../../../docs/design/eng/00-工程约定.md §四。
//!
//! 本模块目前只有 `count_keywords` 一个方法 —— 它是 eng/00 §四 那个示例的实体，
//! 存在的理由是 R5（eng/03 §三）需要一个真实的被测对象：
//! R5 断言「关掉 RLS 后应用层仍不泄漏」，而「应用层」指的就是下面那句
//! 显式的 `WHERE tenant_id = $1`。没有它，R5 无从谈起。
//!
//! 其余 Repo 方法随各自的工作包进来。

use tgm_core::ids::TenantId;

use crate::tenant::tenant_tx;
use sqlx::PgPool;

/// 数本租户的关键词条数。
///
/// `Box::pin(async move { ... })` 是这个签名的必需样板，**每个查询都要写这一层**
/// （eng/00 §四）。SQL 里仍然显式写 `WHERE tenant_id = $1`，不因为有 RLS 就省
/// —— RLS 是第二道防线，不是唯一防线，而 R5 测的正是这一句在不在。
///
/// 用 `query_scalar!` 宏而不是运行时的 `query_as`，理由不在这一个查询本身：
/// `cargo sqlx prepare --check` 是 eng/04 §四 的一条卡口，而**全仓库零个宏调用时
/// 它返回 0**（`.sqlx/` 连目录都不生成）—— 一条什么都没管的卡口和一条通过的
/// 卡口同形。这里是第一个宏调用点，`.sqlx/` 从此有内容，那条卡口开始携带信息。
/// 审计模块是有意的例外（`audit/mod.rs` 开头写了为什么它必须用运行时 SQL）。
///
/// `count(*) as "n!"` 里的 `!` 不是装饰：PG 的聚合列在类型上可空，
/// 不加它宏推出来的是 `Option<i64>`，而 `count(*)` 不返回 NULL。
pub async fn count_keywords(pool: &PgPool, t: TenantId) -> Result<i64, sqlx::Error> {
    tenant_tx(pool, t, |conn| {
        Box::pin(async move {
            sqlx::query_scalar!(
                r#"SELECT count(*) as "n!" FROM keywords WHERE tenant_id = $1"#,
                t.get()
            )
            .fetch_one(conn)
            .await
        })
    })
    .await
}
