//! delivery 运行链（ADR-0033）：按发送身份各起一个 [`DeliveryWorker`]。
//!
//! 每个发送身份：解密 Bot Token 与落库的 session → 有 session 先复用（ADR-0034），
//! 没有或已失效时用平台 `api_id` 经 MTProto `bot_sign_in` 登录并写回 session →
//! 跑既有的 outbox 消费循环。复用 session 不调 `auth.importBotAuthorization`，
//! 重启不再消耗登录配额（限流实测见 `docs/plan/evidence/WP-5-delivery常驻长跑-2026-10-19.md`）。
//!
//! 两条纪律：
//!
//! - 只接 `kind = 'bot'` 的发送身份（spec/08 §1.4）。候选查询已过滤，这里登录前再核一次
//!   Token 里的 Bot id 与登录返回的 id 一致，防止存错的 Token 以别的 Bot 发消息。
//! - Telegram 拒绝 Token 时身份转 `unauthorized` 并写审计，不再重试登录；
//!   其余登录失败按 `restart_delay` 冷却后重试，`FLOOD_WAIT` 时等满服务器给的秒数再重试。
//!
//! 退出（ops/00 §六）：登录与限流等待都响应退出信号；收到信号后最多等
//! [`SHUTDOWN_GRACE`] 让在途投递收尾，超时直接退出 —— 正确性靠 `random_id` 与
//! 卡住 `SENDING` 的回收，不靠优雅退出。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::Kek;
use tgm_core::secret::Secret;
use tgm_db::credentials::{
    BotCredentials, CredentialError, clear_bot_session, load_bot_credentials, store_bot_session,
};
use tgm_db::repo::{
    list_all_active_tenants, list_delivery_sender_candidates, mark_bot_sender_unauthorized,
};
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{StoredSession, TelegramClient, TelegramClientError};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::delivery::{DeliveryWorker, DeliveryWorkerConfig, TelegramDeliveryTransport};
use crate::media_relay::MediaStore;

/// 收到退出信号后等在途发送身份收尾的上限（ops/00 §六）。
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// 平台自己的 MTProto 应用凭据（ADR-0033），只用于驱动租户的 Bot。
#[derive(Clone)]
pub struct PlatformApp {
    pub api_id: i32,
    pub api_hash: Arc<Secret<String>>,
    /// 跨身份媒体中转的对象存储（片 2）。
    pub media_store: MediaStore,
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
    Ok(load_bot_credentials(pool, kek, tenant, identity)
        .await?
        .map(|credentials| parse_bot_token(&credentials.token)))
}

fn parse_bot_token(plain: &Secret<Vec<u8>>) -> Option<Secret<String>> {
    std::str::from_utf8(plain.expose())
        .ok()
        .filter(|token| bot_id_from_token(token).is_some())
        .map(|token| Secret::new(token.to_owned()))
}

/// 落库 session 复用的结局。
enum Resume {
    Connected(TelegramClient),
    /// 没有 session，或 session 已失效（已清掉），改用 Token 登录。
    SignIn,
}

/// 用落库的 session 建连。失效（auth key 被注销、快照坏、登的不是这个 Bot）时清掉
/// session 退回 Token 登录；网络等其他错误原样返回，交给外层冷却重试。
async fn resume_from_stored(
    pool: &PgPool,
    tenant: TenantId,
    identity: IdentityId,
    client_config: TelegramClientConfig,
    session: Option<&Secret<Vec<u8>>>,
    expected_bot_id: i64,
) -> Result<Resume, DeliverySenderError> {
    let Some(session) = session else {
        return Ok(Resume::SignIn);
    };
    let Ok(stored) = StoredSession::restore(session.expose()) else {
        clear_bot_session(pool, tenant, identity).await?;
        return Ok(Resume::SignIn);
    };
    match TelegramClient::resume_bot_sender(client_config, stored).await {
        Ok((client, bot_id)) if bot_id == expected_bot_id => Ok(Resume::Connected(client)),
        Ok((client, _)) => {
            let _ = client.shutdown().await;
            clear_bot_session(pool, tenant, identity).await?;
            Ok(Resume::SignIn)
        }
        Err(error)
            if error.is_unauthorized()
                || error.is_banned()
                || matches!(error, TelegramClientError::NotBot) =>
        {
            tracing::info!(
                sender_identity_id = identity.get(),
                "落库的 Bot session 已失效，改用 Token 登录"
            );
            clear_bot_session(pool, tenant, identity).await?;
            Ok(Resume::SignIn)
        }
        Err(error) => Err(error.into()),
    }
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
    let Some(credentials) = load_bot_credentials(pool, kek, tenant, identity).await? else {
        return Ok(SenderExit::NoToken);
    };
    let Some(token) = parse_bot_token(&credentials.token) else {
        mark_bot_sender_unauthorized(pool, tenant, identity).await?;
        return Ok(SenderExit::TokenInvalid);
    };
    let expected_bot_id = bot_id_from_token(token.expose()).unwrap_or_default();
    let client_config = TelegramClientConfig::new(app.api_id)?;
    let mut login_shutdown = shutdown.clone();
    let resumed = tokio::select! {
        resumed = resume_from_stored(
            pool,
            tenant,
            identity,
            client_config.clone(),
            credentials.session.as_ref(),
            expected_bot_id,
        ) => resumed?,
        _ = wait_for_shutdown(&mut login_shutdown) => return Ok(SenderExit::Shutdown),
    };
    let (client, resumed_session) = match resumed {
        Resume::Connected(client) => (client, true),
        Resume::SignIn => {
            let connected = tokio::select! {
                connected = TelegramClient::connect_bot_sender(client_config, &app.api_hash, &token) => connected,
                _ = wait_for_shutdown(&mut login_shutdown) => return Ok(SenderExit::Shutdown),
            };
            let (client, bot_id) = match connected {
                Ok(connected) => connected,
                Err(error)
                    if error.is_unauthorized()
                        || error.is_banned()
                        || error.is_bot_token_rejected() =>
                {
                    mark_bot_sender_unauthorized(pool, tenant, identity).await?;
                    return Ok(SenderExit::Unauthorized);
                }
                Err(error) => {
                    if let Some(wait) = error.flood_wait() {
                        tracing::warn!(
                            sender_identity_id = identity.get(),
                            flood_wait_seconds = wait.as_secs(),
                            "bot_sign_in 被限流，等满服务器给的时间再重试"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = wait_for_shutdown(&mut login_shutdown) => return Ok(SenderExit::Shutdown),
                        }
                    }
                    return Err(error.into());
                }
            };
            if bot_id != expected_bot_id {
                let _ = client.shutdown().await;
                mark_bot_sender_unauthorized(pool, tenant, identity).await?;
                return Ok(SenderExit::TokenInvalid);
            }
            save_session(pool, kek, tenant, identity, &credentials, &client).await;
            (client, false)
        }
    };
    drop(token);
    drop(credentials);
    // 两种结局用不同消息而非字段区分：pretty 格式把字段打在下一行，长跑按行计数。
    if resumed_session {
        tracing::info!(
            tenant_id = tenant.get(),
            sender_identity_id = identity.get(),
            "delivery 发送身份已登录（复用 session）"
        );
    } else {
        tracing::info!(
            tenant_id = tenant.get(),
            sender_identity_id = identity.get(),
            "delivery 发送身份已登录（Token）"
        );
    }
    let client = Arc::new(client);
    let worker = DeliveryWorker::new(
        pool.clone(),
        tenant,
        identity,
        Arc::new(TelegramDeliveryTransport::new(
            Arc::clone(&client),
            app.media_store.clone(),
        )),
        config.worker,
    );
    let result = worker.run(shutdown).await;
    if let Ok(client) = Arc::try_unwrap(client) {
        let _ = client.shutdown().await;
    }
    result?;
    Ok(SenderExit::Shutdown)
}

/// 登录成功后写回 session。失败只告警：投递照常，下次重启退回 Token 登录。
async fn save_session(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
    credentials: &BotCredentials,
    client: &TelegramClient,
) {
    let result = match client.bot_session_snapshot() {
        Ok(session) => store_bot_session(pool, kek, tenant, identity, credentials, &session)
            .await
            .map_err(DeliverySenderError::from),
        Err(error) => Err(error.into()),
    };
    match result {
        Ok(true) => {}
        Ok(false) => tracing::info!(
            sender_identity_id = identity.get(),
            "Token 在登录期间被替换，丢弃这份 session"
        ),
        Err(error) => tracing::warn!(
            sender_identity_id = identity.get(),
            error = %error,
            "Bot session 写回失败，下次重启改用 Token 登录"
        ),
    }
}

/// 信号已发出或发送端已关闭时返回。
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// 等全部发送身份收尾，最多 `grace`。返回超时后仍未结束、被放弃的数量。
pub async fn join_within(handles: Vec<JoinHandle<()>>, grace: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + grace;
    let mut abandoned = 0;
    for mut handle in handles {
        if tokio::time::timeout_at(deadline, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            abandoned += 1;
        }
    }
    abandoned
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
        let abandoned = join_within(running.into_values().collect(), SHUTDOWN_GRACE).await;
        if abandoned > 0 {
            tracing::warn!(abandoned, "退出等待超过上限，放弃仍在途的发送身份");
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
#[allow(clippy::expect_used)]
mod tests {
    use super::{bot_id_from_token, join_within};
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_join_gives_up_after_grace() {
        let stuck = tokio::spawn(std::future::pending::<()>());
        let quick = tokio::spawn(async {});
        let started = tokio::time::Instant::now();
        let abandoned = join_within(vec![quick, stuck], Duration::from_millis(200)).await;
        assert_eq!(abandoned, 1);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

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
