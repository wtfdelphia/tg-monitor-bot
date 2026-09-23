//! 租户事务入口。签名与安全不变量的定义处是
//! ../../../docs/design/spec/05-安全与租户隔离.md §3.2，
//! 调用点形态在 ../../../docs/design/eng/00-工程约定.md §四。

use sqlx::{PgConnection, PgPool};
use std::future::Future;
use std::pin::Pin;
use tgm_core::ids::TenantId;

/// 闭包返回的 future。`spec/05` §3.2 写的是 `BoxFuture`，
/// 这里展开成等价的裸类型 —— 只为不引 futures 这个依赖。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T, sqlx::Error>> + Send + 'a>>;

/// 唯一的租户事务入口。业务代码拿不到裸 `Transaction`。
///
/// **`pool.begin()` 必须在 `set_config` 之前，这个顺序是安全要求不是风格**
/// （spec/05 §3.2）：`set_config(..., true)` 在事务外调用会静默失效且无 WARNING，
/// 而 `SET LOCAL` 至少会告警 —— 即更安全的写法恰好是失效时更安静的写法。
/// 所以也**不要**把 `set_config` 提取成能在事务外被调用的独立函数。
///
/// 用 `set_config` 而非拼 `SET LOCAL app.tenant_id = '...'`：`SET` 是 utility
/// 命令，不接受绑定参数，只能拼字符串 —— 那等于在多租户边界上开一个注入面。
pub async fn tenant_tx<F, T>(pool: &PgPool, tenant: TenantId, f: F) -> Result<T, sqlx::Error>
where
    F: for<'a> FnOnce(&'a mut PgConnection) -> BoxFut<'a, T>,
{
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.get().to_string())
        .execute(&mut *tx)
        .await?;
    let out = f(&mut tx).await?;
    tx.commit().await?;
    Ok(out)
}
