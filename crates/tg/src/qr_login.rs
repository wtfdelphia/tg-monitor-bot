//! MTProto 用户账号 QR 登录驱动（spec/08 §3.3–§3.4）。
//!
//! 一个 [`QrLoginDriver`] 持有一条独立的 grammers 连接，从空 session 开始：
//! `exportLoginToken` → 等扫码（[`QrLoginDriver::poll`] 重复调用）→ `MigrateTo` 时切 home DC
//! 再 `importLoginToken` → 可选 2FA（[`QrLoginDriver::submit_password`]）→ 已授权后取 session 快照。
//! 每一步同时喂给 [`crate::qr::QrLoginState`]，真机序列与纯状态机共用一套迁移规则。
//!
//! 二维码 token 是 bearer 凭据：本模块只把它编码成 `tg://login?token=…` 交给调用方，
//! 不打印、不落盘；`Debug` 不输出它。采集身份只读：这里不调任何发送方法（spec/08 §1.4）。

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use grammers_client::client::PasswordToken;
use grammers_client::{Client, InvocationError, SenderPool, SignInError, tl};
use grammers_session::Session;
use grammers_session::types::{PeerAuth, PeerInfo};
use tgm_core::secret::Secret;
use thiserror::Error;

use crate::qr::{QrLoginError, QrLoginEvent, QrLoginState, QrToken};
use crate::session::{StoredSession, StoredSessionError};

#[derive(Debug, Error)]
pub enum QrLoginDriverError {
    #[error("Telegram api_id 必须为正整数")]
    InvalidApiId,
    #[error("Telegram 调用失败: {0}")]
    Invocation(#[source] InvocationError),
    #[error("QR 登录状态不允许该操作: {0}")]
    State(#[from] QrLoginError),
    #[error("QR 登录状态不允许该操作")]
    WrongStep,
    #[error("登录到的是 Bot，不是用户账号")]
    NotUser,
    #[error("session 读写失败: {0}")]
    Session(#[from] StoredSessionError),
}

impl QrLoginDriverError {
    /// 租户提交的 `api_id` / `api_hash` 被 Telegram 拒绝。
    pub fn is_api_credential_rejected(&self) -> bool {
        matches!(
            self,
            Self::Invocation(InvocationError::Rpc(error))
                if error.is("API_ID_INVALID") || error.is("API_ID_PUBLISHED_FLOOD")
        )
    }
}

/// [`QrLoginDriver::poll`] 的结果。
pub enum QrPoll {
    /// 等待扫码。`link` 是当前代的 `tg://login?token=…`，过期后自动换新代。
    Pending {
        link: Secret<String>,
        generation: u64,
        expires_at_unix: i64,
    },
    /// 已扫码，需要 2FA 密码。
    PasswordNeeded,
    /// 已授权。
    Authorized,
}

impl std::fmt::Debug for QrPoll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending {
                generation,
                expires_at_unix,
                ..
            } => f
                .debug_struct("Pending")
                .field("generation", generation)
                .field("expires_at_unix", expires_at_unix)
                .finish_non_exhaustive(),
            Self::PasswordNeeded => f.write_str("PasswordNeeded"),
            Self::Authorized => f.write_str("Authorized"),
        }
    }
}

/// [`QrLoginDriver::submit_password`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrPassword {
    Accepted,
    /// 密码错误，状态停在等密码，可重试（spec/08 §3.4）。
    Rejected,
}

/// 已授权账号的公开信息与 session 快照。快照含 auth key，只能交给信封加密落库。
pub struct QrAuthorized {
    pub tg_user_id: i64,
    pub display_name: Option<String>,
    pub session: Secret<Vec<u8>>,
}

impl std::fmt::Debug for QrAuthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrAuthorized")
            .field("tg_user_id", &self.tg_user_id)
            .finish_non_exhaustive()
    }
}

pub struct QrLoginDriver {
    client: Client,
    session: Arc<StoredSession>,
    handle: grammers_client::sender::SenderPoolHandle,
    runner: tokio::task::JoinHandle<()>,
    export: tl::functions::auth::ExportLoginToken,
    state: QrLoginState,
    generation: u64,
    current: Option<Vec<u8>>,
    migrations: Vec<(i32, i32)>,
}

impl std::fmt::Debug for QrLoginDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrLoginDriver")
            .field("state", &self.state)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

enum Step {
    Pending { token: Vec<u8>, expires: i64 },
    Migrate { dc_id: i32, token: Vec<u8> },
    Success,
    PasswordNeeded,
}

fn classify(
    result: Result<tl::enums::auth::LoginToken, InvocationError>,
) -> Result<Step, QrLoginDriverError> {
    match result {
        Ok(tl::enums::auth::LoginToken::Token(token)) => Ok(Step::Pending {
            token: token.token,
            expires: i64::from(token.expires),
        }),
        Ok(tl::enums::auth::LoginToken::MigrateTo(migrate)) => Ok(Step::Migrate {
            dc_id: migrate.dc_id,
            token: migrate.token,
        }),
        Ok(tl::enums::auth::LoginToken::Success(_)) => Ok(Step::Success),
        Err(InvocationError::Rpc(error)) if error.is("SESSION_PASSWORD_NEEDED") => {
            Ok(Step::PasswordNeeded)
        }
        Err(error) => Err(QrLoginDriverError::Invocation(error)),
    }
}

/// `tg://login?token=<base64url 无 padding>`（spec/08 §3.3）。
pub fn login_link(token: &[u8]) -> Secret<String> {
    Secret::new(format!(
        "tg://login?token={}",
        URL_SAFE_NO_PAD.encode(token)
    ))
}

impl QrLoginDriver {
    /// 建一条新连接（空 session）。不发起任何请求，第一次 [`Self::poll`] 才导出 token。
    pub fn start(api_id: i32, api_hash: &Secret<String>) -> Result<Self, QrLoginDriverError> {
        if api_id <= 0 {
            return Err(QrLoginDriverError::InvalidApiId);
        }
        let session = Arc::new(StoredSession::default());
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(Arc::clone(&session), api_id);
        drop(updates);
        let client = Client::new(handle.clone());
        let runner = tokio::spawn(async move { runner.run().await });
        Ok(Self {
            client,
            session,
            handle: handle.thin,
            runner,
            export: tl::functions::auth::ExportLoginToken {
                api_id,
                api_hash: api_hash.expose().clone(),
                except_ids: Vec::new(),
            },
            state: QrLoginState::initial(),
            generation: 0,
            current: None,
            migrations: Vec::new(),
        })
    }

    pub fn state(&self) -> QrLoginState {
        self.state
    }

    /// 本次登录经历的 DC 迁移（起点 DC, 目标 DC）。
    pub fn migrations(&self) -> &[(i32, i32)] {
        &self.migrations
    }

    pub fn home_dc_id(&self) -> Result<i32, QrLoginDriverError> {
        Ok(self.session.home_dc_id()?)
    }

    fn apply(&mut self, event: QrLoginEvent) -> Result<(), QrLoginDriverError> {
        self.state = self.state.transition(event)?;
        Ok(())
    }

    /// 推进一步。等扫码时重复调用：token 未变返回同一代，过期后导出新一代。
    /// 已在等密码或已授权时不再访问网络，直接返回当前结论。
    pub async fn poll(&mut self, now_unix: i64) -> Result<QrPoll, QrLoginDriverError> {
        match self.state {
            QrLoginState::AwaitingPassword { .. } => return Ok(QrPoll::PasswordNeeded),
            QrLoginState::Authorized => return Ok(QrPoll::Authorized),
            QrLoginState::Cancelled | QrLoginState::Migrating { .. } => {
                return Err(QrLoginDriverError::WrongStep);
            }
            QrLoginState::AwaitingExport
            | QrLoginState::AwaitingScan { .. }
            | QrLoginState::Expired { .. } => {}
        }
        match classify(self.client.invoke(&self.export).await)? {
            Step::Pending { token, expires } => {
                if self.current.as_ref() != Some(&token) {
                    if let QrLoginState::AwaitingScan { token: old } = self.state
                        && now_unix >= old.expires_at_unix()
                    {
                        self.apply(QrLoginEvent::TokenExpired { now_unix })?;
                    }
                    self.generation += 1;
                    let handle = QrToken::new(self.generation, expires)?;
                    self.apply(QrLoginEvent::TokenExported { token: handle })?;
                    self.current = Some(token);
                } else if now_unix < expires {
                    self.apply(QrLoginEvent::TokenStillPending {
                        generation: self.generation,
                        now_unix,
                    })?;
                }
                let link = login_link(self.current.as_deref().unwrap_or_default());
                Ok(QrPoll::Pending {
                    link,
                    generation: self.generation,
                    expires_at_unix: expires,
                })
            }
            Step::Migrate { dc_id, token } => {
                let old_dc = self.session.home_dc_id()?;
                self.apply(QrLoginEvent::TokenMigrateTo {
                    generation: self.generation,
                    dc_id,
                    now_unix,
                })?;
                self.migrations.push((old_dc, dc_id));
                self.handle.disconnect_from_dc(old_dc);
                self.session.set_home_dc_id(dc_id).await?;
                let password_required = match classify(
                    self.client
                        .invoke(&tl::functions::auth::ImportLoginToken { token })
                        .await,
                )? {
                    Step::Success => false,
                    Step::PasswordNeeded => true,
                    Step::Pending { .. } | Step::Migrate { .. } => {
                        return Err(QrLoginDriverError::WrongStep);
                    }
                };
                self.apply(QrLoginEvent::ImportSucceeded {
                    generation: self.generation,
                    password_required,
                    now_unix,
                })?;
                self.current = None;
                Ok(self.settled(password_required))
            }
            step @ (Step::Success | Step::PasswordNeeded) => {
                let password_required = matches!(step, Step::PasswordNeeded);
                self.apply(QrLoginEvent::TokenSuccess {
                    generation: self.generation,
                    password_required,
                    now_unix,
                })?;
                self.current = None;
                Ok(self.settled(password_required))
            }
        }
    }

    fn settled(&self, password_required: bool) -> QrPoll {
        if password_required {
            QrPoll::PasswordNeeded
        } else {
            QrPoll::Authorized
        }
    }

    /// 提交 2FA 密码。每次尝试都重新取密码参数（`srp_id` 一次性）。
    pub async fn submit_password(
        &mut self,
        password: &Secret<String>,
    ) -> Result<QrPassword, QrLoginDriverError> {
        if !matches!(self.state, QrLoginState::AwaitingPassword { .. }) {
            return Err(QrLoginDriverError::WrongStep);
        }
        let tl::enums::account::Password::Password(params) = self
            .client
            .invoke(&tl::functions::account::GetPassword {})
            .await
            .map_err(QrLoginDriverError::Invocation)?;
        match self
            .client
            .check_password(PasswordToken::new(params), password.expose().as_str())
            .await
        {
            Ok(_) => {
                self.apply(QrLoginEvent::PasswordAccepted)?;
                Ok(QrPassword::Accepted)
            }
            Err(SignInError::InvalidPassword(_)) => {
                self.apply(QrLoginEvent::PasswordRejected)?;
                Ok(QrPassword::Rejected)
            }
            Err(SignInError::Other(error)) => Err(QrLoginDriverError::Invocation(error)),
            Err(_) => Err(QrLoginDriverError::WrongStep),
        }
    }

    /// 已授权后取账号信息与 session 快照。连接保持打开：落库失败时调用方还要 [`Self::log_out`]，
    /// 成功后再 [`Self::close`]。
    ///
    /// grammers 以缓存里 `is_self` 的 User 判断「已登录」；`importLoginToken` 路径不像 `sign_in`
    /// 那样替我们写入，这里补上，worker 恢复 session 时才走已登录分支。
    pub async fn finish(&self) -> Result<QrAuthorized, QrLoginDriverError> {
        if self.state != QrLoginState::Authorized {
            return Err(QrLoginDriverError::WrongStep);
        }
        let me = self
            .client
            .get_me()
            .await
            .map_err(QrLoginDriverError::Invocation)?;
        if me.is_bot() {
            return Err(QrLoginDriverError::NotUser);
        }
        let tl::enums::User::User(raw) = &me.raw else {
            return Err(QrLoginDriverError::NotUser);
        };
        self.session
            .cache_peer(&PeerInfo::User {
                id: raw.id,
                auth: raw.access_hash.map(PeerAuth::from_hash),
                bot: Some(false),
                is_self: Some(true),
            })
            .await?;
        let display_name = [raw.first_name.as_deref(), raw.last_name.as_deref()]
            .into_iter()
            .flatten()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let authorized = QrAuthorized {
            tg_user_id: raw.id,
            display_name: Some(display_name).filter(|name| !name.is_empty()),
            session: self.session.snapshot()?,
        };
        Ok(authorized)
    }

    /// 已授权但不落库（账号重复、账号不符、写库失败）时注销本次登录，
    /// 免得 Telegram 侧留下一个没人持有的已授权设备。注销失败只能交给用户在「设备」里手动移除。
    pub async fn log_out(self) -> Result<(), QrLoginDriverError> {
        let result = self
            .client
            .invoke(&tl::functions::auth::LogOut {})
            .await
            .map(|_| ())
            .map_err(QrLoginDriverError::Invocation);
        self.close().await;
        result
    }

    /// 放弃本次登录并关闭连接。
    pub async fn close(self) {
        self.handle.quit();
        let _ = self.runner.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_link_is_base64url_without_padding() {
        let link = login_link(&[0xfb, 0xff, 0x01]);
        assert_eq!(link.expose(), "tg://login?token=-_8B");
    }

    #[test]
    fn pending_debug_hides_link() {
        let poll = QrPoll::Pending {
            link: login_link(b"secret-token"),
            generation: 3,
            expires_at_unix: 10,
        };
        let text = format!("{poll:?}");
        assert!(!text.contains("tg://"), "{text}");
        assert!(text.contains("generation: 3"), "{text}");
    }
}
