//! Repo 层。调用点形态的定义处是 ../../../docs/design/eng/00-工程约定.md §四。
//!
//! 本模块目前只有 `count_keywords` 一个方法 —— 它是 eng/00 §四 那个示例的实体，
//! 存在的理由是 R5（eng/03 §三）需要一个真实的被测对象：
//! R5 断言「关掉 RLS 后应用层仍不泄漏」，而「应用层」指的就是下面那句
//! 显式的 `WHERE tenant_id = $1`。没有它，R5 无从谈起。
//!
//! 租约方法在本文件中直接使用 PostgreSQL 的原子写法，避免把抢占判断拆成
//! “先查再写”的竞态窗口。

use tgm_core::ids::{IdentityId, TenantId};

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

/// 获取身份租约。
///
/// 首次获取从 token 1 开始；有效租约由同一 worker 再次获取时只刷新时间，
/// 不改变 token；过期租约由任意 worker 接管时递增 token。有效租约由其他
/// worker 持有时返回 `None`，不等待也不报错。
pub async fn acquire_lease(
    pool: &PgPool,
    identity_id: IdentityId,
    worker_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        INSERT INTO account_leases
            (identity_id, worker_id, fencing_token, heartbeat_at, expires_at)
        VALUES ($1, $2, 1, clock_timestamp(), clock_timestamp() + interval '30 seconds')
        ON CONFLICT (identity_id) DO UPDATE SET
            worker_id = CASE
                WHEN account_leases.expires_at < clock_timestamp() THEN EXCLUDED.worker_id
                ELSE account_leases.worker_id
            END,
            fencing_token = CASE
                WHEN account_leases.expires_at < clock_timestamp()
                    THEN account_leases.fencing_token + 1
                ELSE account_leases.fencing_token
            END,
            heartbeat_at = clock_timestamp(),
            expires_at = clock_timestamp() + interval '30 seconds'
        WHERE account_leases.expires_at < clock_timestamp()
           OR account_leases.worker_id = EXCLUDED.worker_id
        RETURNING fencing_token
        "#,
        identity_id.get(),
        worker_id
    )
    .fetch_optional(pool)
    .await
}

/// 续租身份。
///
/// 续租必须同时匹配身份、worker 和 fencing token，且旧租约已经过期时不允许
/// 复活。返回 `false` 表示当前 worker 已失去租约，调用方不得重试续租。
pub async fn renew_lease(
    pool: &PgPool,
    identity_id: IdentityId,
    worker_id: &str,
    fencing_token: i64,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query!(
        r#"
        UPDATE account_leases
           SET heartbeat_at = clock_timestamp(),
               expires_at = clock_timestamp() + interval '30 seconds'
         WHERE identity_id = $1
           AND worker_id = $2
           AND fencing_token = $3
           AND expires_at > clock_timestamp()
        RETURNING fencing_token
        "#,
        identity_id.get(),
        worker_id,
        fencing_token
    )
    .fetch_optional(pool)
    .await?;

    Ok(updated.is_some())
}

/// 主动释放身份租约。
///
/// 不删除行，而是立即过期并保留 fencing token。这样下一次接管仍会递增 token，
/// 不会因为删除后重新插入而让旧 worker 的 token 再次有效。
pub async fn release_lease(
    pool: &PgPool,
    identity_id: IdentityId,
    worker_id: &str,
    fencing_token: i64,
) -> Result<bool, sqlx::Error> {
    let released = sqlx::query!(
        r#"
        UPDATE account_leases
           SET expires_at = clock_timestamp()
         WHERE identity_id = $1
           AND worker_id = $2
           AND fencing_token = $3
        RETURNING fencing_token
        "#,
        identity_id.get(),
        worker_id,
        fencing_token
    )
    .fetch_optional(pool)
    .await?;

    Ok(released.is_some())
}
