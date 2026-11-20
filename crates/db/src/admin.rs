//! 平台级事务入口。定义处是
//! ../../../docs/design/eng/00-工程约定.md §4.2，
//! 硬要求在 ../../../docs/design/plan/00-里程碑与工期.md §4.3 第 3 条。

use sqlx::{PgConnection, PgPool};

use crate::tenant::BoxFut;

/// 平台级连接池（`platform_ops` 角色）。
///
/// **刻意不实现 `Deref`**（plan/00 §4.3 第 3 条）：它与 `tenant_tx` 用的那个池
/// 类型本来完全一样，一旦能自动解引用成 `&PgPool`，它就会在业务代码里被当普通池用
/// —— 而那意味着某段业务逻辑绕过了租户谓词。包起来之后这种误用不可编译。
///
/// 两者是**两个池而不是同一个池换角色**：换角色要 `SET ROLE`，
/// 那是会话级动作，与连接复用相冲（eng/00 §4.2）。
pub struct AdminPool(PgPool);

impl AdminPool {
    /// 唯一的构造入口。放在这里而不是 `From<PgPool>`，是为了让
    /// 「谁把一个池当成了管理池」在 grep 里只有一处结果。
    pub fn new(pool: PgPool) -> Self {
        Self(pool)
    }
}

/// 平台级入口。不设 `app.tenant_id`，只查 `tenant_id IS NULL` 的行。
///
/// 使用面必须小且可枚举：平台日志查询（`system_logs` / `audit_logs` 的
/// `tenant_id IS NULL` 行）、管理端点。**业务代码里出现 `admin_tx` 就是评审阻塞项**
/// （eng/00 §4.2）。
pub async fn admin_tx<F, T>(pool: &AdminPool, f: F) -> Result<T, sqlx::Error>
where
    F: for<'a> FnOnce(&'a mut PgConnection) -> BoxFut<'a, T>,
{
    let mut tx = pool.0.begin().await?;
    let out = f(&mut tx).await?;
    tx.commit().await?;
    Ok(out)
}
