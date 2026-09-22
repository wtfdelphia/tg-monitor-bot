//! sqlx、`tenant_tx`、Repo trait 与实现。
//!
//! 本 crate 目前只有骨架。待落地的内容与其定义处：
//!   - `tenant_tx` / `admin_tx` 两个入口，两个独立连接池（eng/00 §4.2）
//!   - Repo 方法的第一个参数必须是 `TenantId`（eng/00 §4.1）
//!   - SQL 里显式写 `WHERE tenant_id = $1`，不因为有 RLS 就省（eng/00 §四）
//!
//! 迁移文件在仓库根的 `migrations/`，不在本 crate 内 —— `tgm migrate`
//! 与 `sqlx migrate run --source migrations` 指向同一份。

/// RLS 静态审计。`tgm audit-rls` 的实现体，断言全文见 `audit/queries.sql`。
pub mod audit;
