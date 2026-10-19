//! delivery 运行链（ADR-0033）：按发送身份各起一个 [`DeliveryWorker`]。
//!
//! 每个发送身份：解密 Bot Token → 用平台 `api_id` 经 MTProto `bot_sign_in` →
//! 跑既有的 outbox 消费循环。session 只在内存里，进程重启即重新登录，不落库。
//!
//! 两条纪律：
//!
//! - 只接 `kind = 'bot'` 的发送身份（spec/08 §1.4）。候选查询已过滤，这里登录前再核一次
//!   Token 里的 Bot id 与登录返回的 id 一致，防止存错的 Token 以别的 Bot 发消息。
//! - Telegram 拒绝 Token 时身份转 `unauthorized` 并写审计，不再重试登录；
//!   其余登录失败按 `restart_delay` 冷却后重试。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::Kek;
use tgm_core::secret::Secret;
use tgm_db::credentials::{CredentialError, load_identity_secret};
use tgm_db::repo::{
    list_all_active_tenants, list_delivery_sender_candidates, mark_bot_sender_unauthorized,
};
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{TelegramClient, TelegramClientError};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::delivery::{DeliveryWorker, DeliveryWorkerConfig, TelegramDeliveryTransport};

/// 平台自己的 MTProto 应用凭据（ADR-0033），只用于驱动租户的 Bot。
#[derive(Clone)]
pub struct PlatformApp {
    pub api_id: i32,
    pub api_hash: Arc<Secret<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryRuntimeConfig {
    pub discover_interval: Duration,
    pub restart_delay: Duration,
    pub worker: DeliveryWorkerConfig,
}

impl Default for DeliveryRuntimeConfig {
    fn default() -> Self {
        Self {
            discover_interval: Duration::from_secs(15),
            restart_delay: Duration::from_secs(30),
            worker: DeliveryWorkerConfig::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeliverySenderError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Client(#[from] TelegramClientError),
    #[error(transparent)]
    Worker(#[from] tgm_core::error::AppError),
}

/// 一个发送身份的循环为什么结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderExit {
    Shutdown,
    /// 库里没有 Token。
    NoToken,
    /// Token 不是合法 Bot Token，或与登录得到的 Bot 不一致。身份已转 `unauthorized`。
    TokenInvalid,
    /// Telegram 拒绝了 Token。身份已转 `unauthorized`。
    Unauthorized,
}

/// 从 Bot Token 取 Bot id（`<id>:<secret>` 的前半）。格式不对返回 `None`。
pub fn bot_id_from_token(token: &str) -> Option<i64> {
    let (id, secret) = token.split_once(':')?;
    if secret.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    id.parse::<i64>().ok().filter(|id| *id > 0)
}

/// 解密 Token 并转成字符串。不是 UTF-8 或格式不对时返回 `Ok(None)`。
pub async fn load_bot_token(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
) -> Result<Option<Option<Secret<String>>>, CredentialError> {
    let Some(plain) = load_identity_secret(pool, kek, tenant, identity).await? else {
        return Ok(None);
    };
    Ok(Some(
        std::str::from_utf8(plain.expose())
            .ok()
            .filter(|token| bot_id_from_token(token).is_some())
            .map(|token| Secret::new(token.to_owned())),
    ))
}

pub async fn run_sender(
    pool: &PgPool,
    kek: &dyn Kek,
    app: &PlatformApp,
    tenant: TenantId,
    identity: IdentityId,
    config: DeliveryRuntimeConfig,
    shutdown: watch::Receiver<bool>,
) -> Result<SenderExit, DeliverySenderError> {
    let token = match load_bot_token(pool, kek, tenant, identity).await? {
        None => return Ok(SenderExit::NoToken),
        Some(None) => {
            mark_bot_sender_unauthorized(pool, tenant, identity).await?;
            return Ok(SenderExit::TokenInvalid);
        }
        Some(Some(token)) => token,
    };
    let expected_bot_id = bot_id_from_token(token.expose()).unwrap_or_default();
    let client_config = TelegramClientConfig::new(app.api_id)?;
    let (client, bot_id) =
        match TelegramClient::connect_bot_sender(client_config, &app.api_hash, &token).await {
            Ok(connected) => connected,
            Err(error) if error.is_unauthorized() || error.is_bot_token_rejected() => {
                mark_bot_sender_unauthorized(pool, tenant, identity).await?;
                return Ok(SenderExit::Unauthorized);
            }
            Err(error) => return Err(error.into()),
        };
    drop(token);
    if bot_id != expected_bot_id {
        let _ = client.shutdown().await;
        mark_bot_sender_unauthorized(pool, tenant, identity).await?;
        return Ok(SenderExit::TokenInvalid);
    }
    tracing::info!(
        tenant_id = tenant.get(),
        sender_identity_id = identity.get(),
        "delivery 发送身份已登录"
    );
    let client = Arc::new(client);
    let worker = DeliveryWorker::new(
        pool.clone(),
        tenant,
        identity,
        Arc::new(TelegramDeliveryTransport::new(Arc::clone(&client))),
        config.worker,
    );
    let result = worker.run(shutdown).await;
    if let Ok(client) = Arc::try_unwrap(client) {
        let _ = client.shutdown().await;
    }
    result?;
    Ok(SenderExit::Shutdown)
}

pub struct DeliveryRuntime {
    pool: PgPool,
    kek: Arc<dyn Kek>,
    app: PlatformApp,
    config: DeliveryRuntimeConfig,
}

impl DeliveryRuntime {
    pub fn new(
        pool: PgPool,
        kek: Arc<dyn Kek>,
        app: PlatformApp,
        config: DeliveryRuntimeConfig,
    ) -> Self {
        Self {
            pool,
            kek,
            app,
            config,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), sqlx::Error> {
        let mut running: HashMap<IdentityId, JoinHandle<()>> = HashMap::new();
        let mut cooldown: HashMap<IdentityId, tokio::time::Instant> = HashMap::new();
        let mut discover = tokio::time::interval(self.config.discover_interval);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = discover.tick() => {
                    running.retain(|_, handle| !handle.is_finished());
                    let now = tokio::time::Instant::now();
                    cooldown.retain(|_, until| *until > now);
                    if let Err(error) = self.spawn_senders(&mut running, &mut cooldown, &shutdown).await {
                        tracing::warn!(error = %error, "扫描发送身份失败，下个周期重试");
                    }
                }
            }
        }
        for (_, handle) in running {
            let _ = handle.await;
        }
        Ok(())
    }

    async fn spawn_senders(
        &self,
        running: &mut HashMap<IdentityId, JoinHandle<()>>,
        cooldown: &mut HashMap<IdentityId, tokio::time::Instant>,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(), sqlx::Error> {
        for tenant in list_all_active_tenants(&self.pool).await? {
            for identity in list_delivery_sender_candidates(&self.pool, tenant).await? {
                if running.contains_key(&identity) || cooldown.contains_key(&identity) {
                    continue;
                }
                cooldown.insert(
                    identity,
                    tokio::time::Instant::now() + self.config.restart_delay,
                );
                let pool = self.pool.clone();
                let kek = Arc::clone(&self.kek);
                let app = self.app.clone();
                let config = self.config;
                let shutdown = shutdown.clone();
                running.insert(
                    identity,
                    tokio::spawn(async move {
                        let result = run_sender(
                            &pool,
                            kek.as_ref(),
                            &app,
                            tenant,
                            identity,
                            config,
                            shutdown,
                        )
                        .await;
                        match result {
                            Ok(exit) => tracing::info!(sender_identity_id = identity.get(), exit = ?exit, "发送身份循环结束"),
                            Err(error) => tracing::warn!(sender_identity_id = identity.get(), error = %error, "发送身份循环异常退出"),
                        }
                    }),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::bot_id_from_token;

    #[test]
    fn bot_id_comes_from_token_prefix() {
        assert_eq!(
            bot_id_from_token("123456:AAbbCCddEEffGGhhIIjjKK"),
            Some(123_456)
        );
        assert_eq!(bot_id_from_token("0:AAbb"), None);
        assert_eq!(bot_id_from_token("12a:AAbb"), None);
        assert_eq!(bot_id_from_token("123456:"), None);
        assert_eq!(bot_id_from_token("123456"), None);
    }
}
