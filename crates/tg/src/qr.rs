//! QR 登录状态机。状态形状来自 docs/design/spec/08-账号接入.md §3.4。

use thiserror::Error;

/// 不含 token 明文的 QR 登录 token 句柄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QrToken {
    generation: u64,
    expires_at_unix: i64,
}

impl QrToken {
    /// 创建一个 token 句柄。generation 从 1 开始，过期时间使用 Unix 秒。
    pub const fn new(generation: u64, expires_at_unix: i64) -> Result<Self, QrLoginError> {
        if generation == 0 || expires_at_unix <= 0 {
            return Err(QrLoginError::InvalidToken);
        }
        Ok(Self {
            generation,
            expires_at_unix,
        })
    }

    /// 取 token 代次，用于拒绝旧二维码的响应。
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// 取 token 的 Unix 过期时间。
    pub const fn expires_at_unix(self) -> i64 {
        self.expires_at_unix
    }
}

/// QR 登录当前状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrLoginState {
    /// 尚未拿到 Telegram 返回的二维码 token。
    AwaitingExport,
    /// 已拿到 token，等待用户扫码或轮询结果。
    AwaitingScan { token: QrToken },
    /// Telegram 要求切换 DC 后再导入 token。
    Migrating { token: QrToken, dc_id: i32 },
    /// 登录成功但还需要输入 2FA 密码。
    AwaitingPassword { generation: u64 },
    /// 已完成授权。
    Authorized,
    /// 当前 token 已过期，可以用新代次重新导出。
    Expired { token: QrToken },
    /// 用户或上层流程主动取消登录。
    Cancelled,
}

/// Telegram 结果和上层动作归一后的状态事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrLoginEvent {
    /// 首次导出，或用新代次刷新二维码。
    TokenExported { token: QrToken },
    /// 轮询仍返回当前 token，表示用户尚未确认。
    TokenStillPending { generation: u64, now_unix: i64 },
    /// Telegram 要求在指定 DC 导入 token。
    TokenMigrateTo {
        generation: u64,
        dc_id: i32,
        now_unix: i64,
    },
    /// 当前 DC 直接返回登录结果。
    TokenSuccess {
        generation: u64,
        password_required: bool,
        now_unix: i64,
    },
    /// DC 迁移后的导入返回登录结果。
    ImportSucceeded {
        generation: u64,
        password_required: bool,
        now_unix: i64,
    },
    /// 轮询或后台时钟确认 token 已过期。
    TokenExpired { now_unix: i64 },
    /// 2FA 校验成功。
    PasswordAccepted,
    /// 2FA 校验失败，仍停留在输入密码状态。
    PasswordRejected,
    /// 主动停止当前登录流程。
    Cancelled,
}

/// 状态迁移失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum QrLoginError {
    #[error("QR 登录事件不适用于当前状态")]
    InvalidTransition,
    #[error("QR 登录事件引用了旧 token")]
    StaleToken,
    #[error("QR token 无效")]
    InvalidToken,
    #[error("DC id 无效")]
    InvalidDc,
    #[error("QR token 尚未过期")]
    TokenNotExpired,
    #[error("QR token 已过期")]
    TokenExpired,
}

impl QrLoginState {
    /// 返回状态机初始状态。
    pub const fn initial() -> Self {
        Self::AwaitingExport
    }

    /// 应用一个事件并返回迁移后的状态。
    pub fn transition(self, event: QrLoginEvent) -> Result<Self, QrLoginError> {
        match (self, event) {
            (Self::AwaitingExport, QrLoginEvent::TokenExported { token }) => {
                Ok(Self::AwaitingScan { token })
            }
            (Self::AwaitingScan { token: current }, QrLoginEvent::TokenExported { token })
            | (Self::Expired { token: current }, QrLoginEvent::TokenExported { token }) => {
                if token.generation <= current.generation {
                    return Err(QrLoginError::StaleToken);
                }
                Ok(Self::AwaitingScan { token })
            }
            (
                Self::AwaitingScan { token },
                QrLoginEvent::TokenStillPending {
                    generation,
                    now_unix,
                },
            ) => {
                ensure_generation(token, generation)?;
                ensure_not_expired(token, now_unix)?;
                Ok(Self::AwaitingScan { token })
            }
            (
                Self::AwaitingScan { token },
                QrLoginEvent::TokenMigrateTo {
                    generation,
                    dc_id,
                    now_unix,
                },
            ) => {
                ensure_generation(token, generation)?;
                ensure_not_expired(token, now_unix)?;
                if dc_id <= 0 {
                    return Err(QrLoginError::InvalidDc);
                }
                Ok(Self::Migrating { token, dc_id })
            }
            (
                Self::AwaitingScan { token },
                QrLoginEvent::TokenSuccess {
                    generation,
                    password_required,
                    now_unix,
                },
            ) => {
                ensure_generation(token, generation)?;
                ensure_not_expired(token, now_unix)?;
                Ok(success_state(token.generation, password_required))
            }
            (
                Self::Migrating { token, dc_id: _ },
                QrLoginEvent::ImportSucceeded {
                    generation,
                    password_required,
                    now_unix,
                },
            ) => {
                ensure_generation(token, generation)?;
                ensure_not_expired(token, now_unix)?;
                Ok(success_state(token.generation, password_required))
            }
            (Self::AwaitingScan { token }, QrLoginEvent::TokenExpired { now_unix })
            | (Self::Migrating { token, dc_id: _ }, QrLoginEvent::TokenExpired { now_unix }) => {
                if now_unix < token.expires_at_unix {
                    return Err(QrLoginError::TokenNotExpired);
                }
                Ok(Self::Expired { token })
            }
            (Self::AwaitingPassword { generation }, QrLoginEvent::PasswordAccepted) => {
                let _ = generation;
                Ok(Self::Authorized)
            }
            (Self::AwaitingPassword { generation }, QrLoginEvent::PasswordRejected) => {
                Ok(Self::AwaitingPassword { generation })
            }
            (
                Self::AwaitingExport
                | Self::AwaitingScan { .. }
                | Self::Migrating { .. }
                | Self::Expired { .. }
                | Self::AwaitingPassword { .. },
                QrLoginEvent::Cancelled,
            ) => Ok(Self::Cancelled),
            _ => Err(QrLoginError::InvalidTransition),
        }
    }
}

fn ensure_generation(token: QrToken, generation: u64) -> Result<(), QrLoginError> {
    if token.generation == generation {
        Ok(())
    } else {
        Err(QrLoginError::StaleToken)
    }
}

fn ensure_not_expired(token: QrToken, now_unix: i64) -> Result<(), QrLoginError> {
    if now_unix >= token.expires_at_unix {
        Err(QrLoginError::TokenExpired)
    } else {
        Ok(())
    }
}

fn success_state(generation: u64, password_required: bool) -> QrLoginState {
    if password_required {
        QrLoginState::AwaitingPassword { generation }
    } else {
        QrLoginState::Authorized
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{QrLoginError, QrLoginEvent, QrLoginState, QrToken};
    use rstest::{fixture, rstest};

    #[fixture]
    fn token_one() -> QrToken {
        QrToken::new(1, 30).expect("token 1 应有效")
    }

    fn transition(state: QrLoginState, event: QrLoginEvent) -> QrLoginState {
        state
            .transition(event)
            .unwrap_or_else(|error| panic!("状态迁移失败：{error}"))
    }

    #[rstest]
    #[case(QrLoginEvent::TokenSuccess { generation: 1, password_required: false, now_unix: 1 }, QrLoginState::Authorized)]
    #[case(QrLoginEvent::TokenSuccess { generation: 1, password_required: true, now_unix: 1 }, QrLoginState::AwaitingPassword { generation: 1 })]
    fn token_success_selects_the_authentication_path(
        token_one: QrToken,
        #[case] event: QrLoginEvent,
        #[case] expected: QrLoginState,
    ) {
        let state = transition(
            QrLoginState::initial(),
            QrLoginEvent::TokenExported { token: token_one },
        );
        assert_eq!(transition(state, event), expected);
    }

    #[rstest]
    #[case(QrLoginEvent::TokenStillPending { generation: 1, now_unix: 1 })]
    #[case(QrLoginEvent::TokenMigrateTo { generation: 1, dc_id: 2, now_unix: 1 })]
    #[case(QrLoginEvent::TokenSuccess { generation: 1, password_required: false, now_unix: 1 })]
    fn current_token_events_are_accepted(token_one: QrToken, #[case] event: QrLoginEvent) {
        let state = transition(
            QrLoginState::initial(),
            QrLoginEvent::TokenExported { token: token_one },
        );
        assert!(!matches!(
            state.transition(event),
            Err(QrLoginError::StaleToken)
        ));
    }

    #[test]
    fn migrate_then_import_requires_the_same_token_generation() {
        let token = QrToken::new(1, 30).expect("token 应有效");
        let state = transition(
            QrLoginState::initial(),
            QrLoginEvent::TokenExported { token },
        );
        let state = transition(
            state,
            QrLoginEvent::TokenMigrateTo {
                generation: 1,
                dc_id: 2,
                now_unix: 1,
            },
        );
        assert_eq!(
            state.transition(QrLoginEvent::ImportSucceeded {
                generation: 9,
                password_required: false,
                now_unix: 1,
            }),
            Err(QrLoginError::StaleToken)
        );
        assert_eq!(
            transition(
                state,
                QrLoginEvent::ImportSucceeded {
                    generation: 1,
                    password_required: false,
                    now_unix: 1,
                },
            ),
            QrLoginState::Authorized
        );
    }

    #[test]
    fn expired_token_requires_a_newer_export() {
        let token_one = QrToken::new(1, 30).expect("token 1 应有效");
        let token_two = QrToken::new(2, 60).expect("token 2 应有效");
        let state = transition(
            QrLoginState::initial(),
            QrLoginEvent::TokenExported { token: token_one },
        );
        let expired = transition(state, QrLoginEvent::TokenExpired { now_unix: 30 });
        assert_eq!(
            expired.transition(QrLoginEvent::TokenExported { token: token_one }),
            Err(QrLoginError::StaleToken)
        );
        assert_eq!(
            transition(expired, QrLoginEvent::TokenExported { token: token_two },),
            QrLoginState::AwaitingScan { token: token_two }
        );
    }

    #[test]
    fn expired_token_events_cannot_authorize_without_an_explicit_expiry_event() {
        let token = QrToken::new(1, 30).expect("token 应有效");
        let state = transition(
            QrLoginState::initial(),
            QrLoginEvent::TokenExported { token },
        );

        assert_eq!(
            state.transition(QrLoginEvent::TokenSuccess {
                generation: 1,
                password_required: false,
                now_unix: 30,
            }),
            Err(QrLoginError::TokenExpired)
        );
        assert_eq!(
            state.transition(QrLoginEvent::TokenMigrateTo {
                generation: 1,
                dc_id: 2,
                now_unix: 31,
            }),
            Err(QrLoginError::TokenExpired)
        );
    }

    #[test]
    fn stale_token_events_are_rejected_after_refresh() {
        let token_one = QrToken::new(1, 30).expect("token 1 应有效");
        let token_two = QrToken::new(2, 60).expect("token 2 应有效");
        let state = transition(
            transition(
                QrLoginState::initial(),
                QrLoginEvent::TokenExported { token: token_one },
            ),
            QrLoginEvent::TokenExported { token: token_two },
        );
        assert_eq!(
            state.transition(QrLoginEvent::TokenSuccess {
                generation: 1,
                password_required: false,
                now_unix: 31,
            }),
            Err(QrLoginError::StaleToken)
        );
    }

    #[test]
    fn password_rejection_keeps_the_2fa_state() {
        let token = QrToken::new(1, 30).expect("token 应有效");
        let state = transition(
            transition(
                QrLoginState::initial(),
                QrLoginEvent::TokenExported { token },
            ),
            QrLoginEvent::TokenSuccess {
                generation: 1,
                password_required: true,
                now_unix: 1,
            },
        );
        assert_eq!(
            transition(state, QrLoginEvent::PasswordRejected),
            QrLoginState::AwaitingPassword { generation: 1 }
        );
        assert_eq!(
            transition(
                QrLoginState::AwaitingPassword { generation: 1 },
                QrLoginEvent::PasswordAccepted,
            ),
            QrLoginState::Authorized
        );
    }

    #[rstest]
    #[case(QrLoginState::Authorized, QrLoginEvent::PasswordAccepted)]
    #[case(QrLoginState::Cancelled, QrLoginEvent::TokenExported { token: QrToken { generation: 1, expires_at_unix: 30 } })]
    #[case(QrLoginState::AwaitingScan { token: QrToken { generation: 1, expires_at_unix: 30 } }, QrLoginEvent::TokenMigrateTo { generation: 1, dc_id: 0, now_unix: 1 })]
    fn invalid_events_are_rejected(#[case] state: QrLoginState, #[case] event: QrLoginEvent) {
        assert!(matches!(
            state.transition(event),
            Err(QrLoginError::InvalidTransition) | Err(QrLoginError::InvalidDc)
        ));
    }

    #[test]
    fn cancellation_is_only_available_before_terminal_states() {
        assert_eq!(
            transition(QrLoginState::AwaitingExport, QrLoginEvent::Cancelled),
            QrLoginState::Cancelled
        );
        assert_eq!(
            QrLoginState::Authorized.transition(QrLoginEvent::Cancelled),
            Err(QrLoginError::InvalidTransition)
        );
    }

    #[test]
    fn token_constructor_rejects_invalid_values() {
        assert_eq!(QrToken::new(0, 30), Err(QrLoginError::InvalidToken));
        assert_eq!(QrToken::new(1, 0), Err(QrLoginError::InvalidToken));
    }
}
