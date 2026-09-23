//! sqlx、`tenant_tx`、Repo trait 与实现。
//!
//! eng/00 §四 的三条形态约定都已落地并各有实体：`tenant_tx` / `admin_tx`
//! 在 `tenant.rs` / `admin.rs`（§4.2），Repo 方法第一参数是 `TenantId`、
//! SQL 里显式写 `WHERE tenant_id = $1` 在 `repo::count_keywords`（§4.1、§四）。
//!
//! 待落地的是 Repo 的其余方法，随各自的工作包进来 —— 形态照着
//! `count_keywords` 抄，那个方法存在的首要理由就是当样板（见该函数注释）。
//!
//! 两个独立连接池目前还是同一个 `PgPool` 传进两个入口：`AdminPool` 的
//! newtype 已经在了，但建池的地方还没有（`tgm serve` 是空壳）。
//! 「两个池」这件事要到 serve 落地才真正成立。
//!
//! 迁移文件在仓库根的 `migrations/`，不在本 crate 内 —— `tgm migrate`
//! 与 `sqlx migrate run --source migrations` 指向同一份。

/// 平台级入口。`AdminPool` 刻意不实现 `Deref`（plan/00 §4.3 第 3 条）。
pub mod admin;
/// RLS 静态审计。`tgm audit-rls` 的实现体，断言全文见 `audit/queries.sql`。
pub mod audit;
/// 启动期校验里要连库的两条（eng/01 §4.2 版本下限、§4.3 角色自检）。
pub mod preflight;
/// Repo 层。方法第一个参数必须是 `TenantId`（eng/00 §4.1）。
pub mod repo;
/// 租户事务入口。业务唯一入口，`set_config` 的作用域即事务边界。
pub mod tenant;
