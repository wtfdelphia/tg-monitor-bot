//! 领域类型：newtype、错误、配置、AuthContext。
//!
//! 本 crate 目前只有骨架。待落地的内容与其定义处：
//!   - `error.rs`     `AppError` 与 11 个错误码的映射（eng/00 §三）
//!   - `secret.rs`    `Secret<T>` 及手工实现的 `Debug`（eng/00 §五）
//!   - `telemetry.rs` registry + layer 形态的 subscriber（eng/00 §5.1）
//!   - `TenantId`     只有两条构造路径，不提供 `From<i64>`（eng/00 §4.1）
