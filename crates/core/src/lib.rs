//! 领域类型：newtype、错误、配置、AuthContext。
//!
//! `error.rs` 已落地，HTTP 层的 `IntoResponse` 实现在 `tgm-app::control`。
//! 映射本身仍收在 `error.rs` 里，避免各 handler 各自决定状态码。

/// REST 与 Bot 入口共享的已验证认证上下文。
pub mod auth;
/// 启动期配置校验的不连库部分（eng/01 §4.1 与 §4.5）。
pub mod config;
/// 凭据密文的 AES-256-GCM 加解密，AAD 绑定身份（spec/05 §七）。
pub mod credential;
/// `AppError` 与 spec/07 §2.1 错误码的映射（eng/00 §三）。
pub mod error;
/// ID newtype。`TenantId` 与 `IdentityId` 已落地，其余几个随用到它们的工作包进来。
pub mod ids;
/// KEK 的 trait 与 `EnvKek`（eng/01 §三，ADR-0011/0012）。`KmsKek` 不在此 crate。
pub mod kek;
/// OIDC 登录协议材料、算法白名单与 nonce 一次性消费。
pub mod oidc;
/// `TGM_PROFILE` 的取值（eng/01 §二）。两处安全检查以它为条件。
pub mod profile;
/// 敏感值包装，`Debug` 恒为 `[REDACTED]`（eng/00 §五）。
pub mod secret;
/// subscriber 初始化，registry + layer 形态（eng/00 §5.1）。
pub mod telemetry;
