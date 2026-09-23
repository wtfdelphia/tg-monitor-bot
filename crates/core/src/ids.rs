//! ID newtype。定义处是 ../../docs/design/spec/05-安全与租户隔离.md §二，
//! 构造路径的约束在 ../../docs/design/eng/00-工程约定.md §4.1。
//!
//! 本文件目前只落 `TenantId` —— 其余几个（`TenantEventId` / `TgChatId` /
//! `SourceRef`）随用到它们的工作包进来，现在建了也没有调用点。

/// 租户标识。
///
/// **刻意不提供 `From<i64>`、也不提供 `new(i64)`**（eng/00 §4.1）：
/// 构造只有两条路径 —— 从 `AuthContext` 取，或 sqlx 从库里解码
/// （`#[sqlx(transparent)]` 提供）。于是「凭一个来路不明的整数造出租户身份」
/// 在编译期就不可表达，而不是靠评审时留意。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct TenantId(i64);

impl TenantId {
    /// 取出内部值，用于绑参。
    pub fn get(self) -> i64 {
        self.0
    }
}
