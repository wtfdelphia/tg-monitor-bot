//! 领域类型：newtype、错误、配置、AuthContext。
//!
//! `error.rs` 已落地，但 HTTP 那一半没有：`IntoResponse` 的 impl
//! 随 WP-5 的 axum 进来。映射本身已经收在 `error.rs` 里了（见该文件抬头）。

/// 启动期配置校验的不连库部分（eng/01 §4.1 与 §4.5）。
pub mod config;
/// `AppError` 与 spec/07 §2.1 那 11 个错误码的映射（eng/00 §三）。
pub mod error;
/// ID newtype。`TenantId` 已落地，其余几个随用到它们的工作包进来。
pub mod ids;
/// KEK 的 trait 与 `EnvKek`（eng/01 §三，ADR-0011/0012）。`KmsKek` 不在此 crate。
pub mod kek;
/// `TGM_PROFILE` 的取值（eng/01 §二）。两处安全检查以它为条件。
pub mod profile;
/// 敏感值包装，`Debug` 恒为 `[REDACTED]`（eng/00 §五）。
pub mod secret;
/// subscriber 初始化，registry + layer 形态（eng/00 §5.1）。
pub mod telemetry;
