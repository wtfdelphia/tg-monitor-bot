//! mtproto-worker 运行链（spec/08 §1.3、§1.4、§四、§五）。
//!
//! 一个身份一个任务：领租约 → 解密恢复 session → 建连 → 收消息落 `canonical_events`。
//! 运行中每 10s 续租，每 60s 落一次 session 快照并校验一次授权，退出前再落一次。
//!
//! 三条不得放宽的纪律：
//!
//! - 续租失败是「立刻自杀」不是重试（spec/08 §4.3）：断开、退出该身份，不重连。
//! - 事件与 session 落库都在事务里校验租约，僵尸 worker 写不进去。
//! - 采集身份只读：这里只调 `next_incoming`、快照与 `getMe` 授权校验，不调任何发送方法（spec/08 §1.4）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::types::chrono::{TimeZone, Utc};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::Kek;
use tgm_db::credentials::{CredentialError, store_identity_secret_leased};
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, LeaseGuard, acquire_lease,
    insert_canonical_event_leased, list_all_active_tenants, list_mtproto_worker_candidates,
    mark_leased_identity_unauthorized, release_lease, renew_lease, set_leased_identity_status,
};
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{IncomingMessage, TelegramClient, TelegramClientError};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::media_relay::{MediaStore, RelayOutcome, run_relay_once};
use crate::mtproto_session::{SessionStoreError, load_session};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MtprotoWorkerConfig {
    /// 续租周期。租约 TTL 是 30s（`acquire_lease`），三个周期的容错。
    pub renew_interval: Duration,
    /// session 定时落库周期（spec/08 §1.3）。
    pub save_interval: Duration,
    /// 扫描可接管身份的周期。
    pub discover_interval: Duration,
    /// 授权校验周期。会话被撤销时更新流只是静默，要靠主动 `getMe` 发现（spec/08 §五）。
    pub auth_check_interval: Duration,
    /// 单个身份异常退出后，下次再尝试接管前的等待。
    pub restart_delay: Duration,
}

impl Default for MtprotoWorkerConfig {
    fn default() -> Self {
        Self {
            renew_interval: Duration::from_secs(10),
            save_interval: Duration::from_secs(60),
            discover_interval: Duration::from_secs(15),
            auth_check_interval: Duration::from_secs(60),
            restart_delay: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MtprotoWorkerError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    #[error(transparent)]
    Client(#[from] TelegramClientError),
    #[error("消息时间戳无效: {0}")]
    InvalidDate(i64),
}

/// 一个身份的消费循环为什么结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityExit {
    /// 收到停止信号，已落 session 并主动释放租约。
    Shutdown,
    /// 租约被接管或过期。不重试、不重连。
    LeaseLost,
    /// 别的 worker 正持有租约。
    LeaseBusy,
    /// 库里没有 session：需要先走 QR 登录。
    NoSession,
    /// Telegram 判定授权失效，身份已转 `unauthorized`。
    Unauthorized,
}

/// 把一条采集消息转成第一层事件。与 Bot 路径同一张表、同一组唯一键（spec/03 §5.2）。
///
/// 编辑事件用 Telegram 的 `edit_date`（Unix 秒）当 `edit_version`：单调但不连续。
/// 不照 Bot 路径取 `MAX + 1`，因为 `catch_up` 补齐会重投同一次编辑，`MAX + 1` 会把它
/// 写成新版本；`edit_date` 让重投撞 `uq_canonical` 去重，不同次编辑各落一行。
pub fn canonical_event(
    receiver: IdentityId,
    incoming: &IncomingMessage,
    edit_version: i32,
) -> Result<CanonicalEventInput, MtprotoWorkerError> {
    let message_date = Utc
        .timestamp_opt(incoming.date, 0)
        .single()
        .ok_or(MtprotoWorkerError::InvalidDate(incoming.date))?;
    let (text, caption) = if incoming.media_type.is_some() {
        (None, incoming.text.clone())
    } else {
        (incoming.text.clone(), None)
    };
    Ok(CanonicalEventInput {
        adapter: CanonicalAdapter::MtProto,
        receiver_identity_id: receiver,
        tg_chat_id: incoming.tg_chat_id,
        tg_message_id: incoming.tg_message_id,
        event_type: if incoming.edited {
            CanonicalEventType::MessageEdited
        } else {
            CanonicalEventType::MessageCreated
        },
        edit_version,
        update_id: None,
        media_group_id: incoming.grouped_id.map(|id| id.to_string()),
        sender_id: incoming.sender_id,
        message_date,
        text,
        caption,
        media_type: incoming.media_type.clone(),
        content_hash: None,
        file_unique_id: None,
        file_name: incoming.file_name.clone(),
        file_size: incoming.file_size,
        mime_type: incoming.mime_type.clone(),
        raw_file_id: None,
    })
}

/// 一个待接管的身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MtprotoTarget {
    pub tenant: TenantId,
    pub identity: IdentityId,
    pub api_id: i32,
}

/// 跑一个身份直到停止信号、租约丢失或授权失效。
pub async fn run_identity(
    pool: &PgPool,
    kek: &dyn Kek,
    target: MtprotoTarget,
    worker_id: &str,
    config: MtprotoWorkerConfig,
    media_store: Option<&MediaStore>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<IdentityExit, MtprotoWorkerError> {
    let MtprotoTarget {
        tenant,
        identity,
        api_id,
    } = target;
    let Some(fencing_token) = acquire_lease(pool, identity, worker_id).await? else {
        return Ok(IdentityExit::LeaseBusy);
    };
    let lease = LeaseGuard {
        identity_id: identity,
        worker_id: worker_id.to_owned(),
        fencing_token,
    };
    let result = run_leased(
        pool,
        kek,
        tenant,
        &lease,
        api_id,
        config,
        media_store,
        &mut shutdown,
    )
    .await;
    // 失去租约时已不是持有者；授权失效已在标记事务里释放。其余出口主动释放。
    // 释放是优化不是要求（ops/00 §六），失败只记日志。
    if !matches!(
        result,
        Ok(IdentityExit::LeaseLost | IdentityExit::Unauthorized)
    ) {
        let _ = set_leased_identity_status(pool, tenant, &lease, "offline").await;
        if let Err(error) = release_lease(pool, identity, worker_id, fencing_token).await {
            tracing::warn!(error = %error, identity_id = identity.get(), "释放租约失败");
        }
    }
    result
}

async fn run_leased(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    lease: &LeaseGuard,
    api_id: i32,
    config: MtprotoWorkerConfig,
    media_store: Option<&MediaStore>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<IdentityExit, MtprotoWorkerError> {
    let Some(session) = load_session(pool, kek, tenant, lease.identity_id).await? else {
        return Ok(IdentityExit::NoSession);
    };
    let mut client =
        TelegramClient::connect_with_session(TelegramClientConfig::new(api_id)?, session).await?;
    // 上线前先确认授权：失效的 auth key 也能建连，不校验就会把它置成 online。
    match client.check_authorized().await {
        Ok(()) => {}
        Err(error) if error.is_unauthorized() => {
            let exit = mark_unauthorized(pool, tenant, lease).await;
            let _ = client.shutdown().await;
            return exit;
        }
        Err(error) => {
            let _ = client.shutdown().await;
            return Err(error.into());
        }
    }
    if !set_leased_identity_status(pool, tenant, lease, "online").await? {
        let _ = client.shutdown().await;
        return Ok(IdentityExit::LeaseLost);
    }
    tracing::info!(identity_id = lease.identity_id.get(), "mtproto 身份已上线");

    let mut renew = tokio::time::interval(config.renew_interval);
    let mut save = tokio::time::interval(config.save_interval);
    let mut auth_check = tokio::time::interval(config.auth_check_interval);
    renew.tick().await;
    save.tick().await;
    auth_check.tick().await;
    // 媒体中转下载在独立任务里跑：下载 50 MB 约 1 分钟，不能挡住收消息与续租。
    // 任务持同一租约；租约丢失时它的每次写入都会被拒，外层退出时一并 abort。
    let relay_task = media_store.map(|store| {
        let pool = pool.clone();
        let lease = lease.clone();
        let reader = client.media_reader();
        let store = store.clone();
        let interval = config.renew_interval;
        tokio::spawn(async move { relay_loop(&pool, &lease, &reader, &store, interval).await })
    });
    let exit = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break Ok(IdentityExit::Shutdown);
                }
            }
            _ = renew.tick() => {
                match renew_lease(pool, lease.identity_id, &lease.worker_id, lease.fencing_token).await {
                    Ok(true) => {}
                    // 续租失败不套重试：重试只会拉长双连接窗口（spec/08 §4.3）。
                    Ok(false) => break Ok(IdentityExit::LeaseLost),
                    Err(error) => {
                        tracing::warn!(error = %error, identity_id = lease.identity_id.get(), "续租失败，按失去租约处理");
                        break Ok(IdentityExit::LeaseLost);
                    }
                }
            }
            _ = save.tick() => {
                match save_leased(pool, kek, tenant, lease, &client).await {
                    Ok(()) => {}
                    Err(SaveError::LeaseLost) => break Ok(IdentityExit::LeaseLost),
                    // 落库失败不丢事件（事件已逐条落库），下个周期再写。
                    Err(SaveError::Other(error)) => {
                        tracing::warn!(error = %error, identity_id = lease.identity_id.get(), "session 落库失败");
                    }
                }
            }
            _ = auth_check.tick() => {
                match client.check_authorized().await {
                    Ok(()) => {}
                    Err(error) if error.is_unauthorized() => {
                        break mark_unauthorized(pool, tenant, lease).await;
                    }
                    // 网络抖动不等于授权失效：只记日志，下个周期再查。
                    Err(error) => {
                        tracing::warn!(error = %error, identity_id = lease.identity_id.get(), "授权校验失败");
                    }
                }
            }
            incoming = client.next_incoming() => {
                match incoming {
                    Ok(incoming) => {
                        let edit_version = if incoming.edited {
                            i32::try_from(incoming.edit_date.unwrap_or(incoming.date)).unwrap_or(i32::MAX)
                        } else {
                            0
                        };
                        let event = canonical_event(lease.identity_id, &incoming, edit_version)?;
                        if insert_canonical_event_leased(pool, lease, &event).await?.is_none() {
                            break Ok(IdentityExit::LeaseLost);
                        }
                    }
                    Err(error) if error.is_unauthorized() => {
                        break mark_unauthorized(pool, tenant, lease).await;
                    }
                    Err(error) => break Err(error.into()),
                }
            }
        }
    };

    if let Some(task) = relay_task {
        task.abort();
        let _ = task.await;
    }
    match exit {
        Ok(IdentityExit::Shutdown) => {
            // 退出前落最后一次，下次启动从这里续上更新状态。失败只记日志，不影响退出。
            if let Err(error) = save_leased(pool, kek, tenant, lease, &client).await {
                tracing::warn!(error = ?error, identity_id = lease.identity_id.get(), "退出前 session 落库失败");
            }
        }
        Ok(IdentityExit::LeaseLost) => {
            tracing::warn!(
                identity_id = lease.identity_id.get(),
                "租约已丢失，断开连接"
            );
        }
        _ => {}
    }
    let _ = client.shutdown().await;
    exit
}

/// Telegram 判定授权失效：转 `unauthorized`、写审计、释放租约，在一个持租约事务里完成。
async fn mark_unauthorized(
    pool: &PgPool,
    tenant: TenantId,
    lease: &LeaseGuard,
) -> Result<IdentityExit, MtprotoWorkerError> {
    tracing::warn!(
        identity_id = lease.identity_id.get(),
        "Telegram 判定授权失效"
    );
    if mark_leased_identity_unauthorized(pool, tenant, lease).await? {
        Ok(IdentityExit::Unauthorized)
    } else {
        Ok(IdentityExit::LeaseLost)
    }
}

/// 本身份的中转下载循环：有到期 job 就连续处理，空闲时按续租周期再扫。
async fn relay_loop(
    pool: &PgPool,
    lease: &LeaseGuard,
    reader: &tgm_tg::MediaReader,
    store: &MediaStore,
    interval: Duration,
) {
    loop {
        match run_relay_once(pool, lease, reader, store).await {
            Ok(RelayOutcome::Ready { .. } | RelayOutcome::Failed { .. }) => continue,
            Ok(RelayOutcome::LeaseLost) => return,
            Ok(RelayOutcome::Idle) => {}
            Err(error) => {
                tracing::warn!(error = %error, identity_id = lease.identity_id.get(), "媒体中转扫描失败，下个周期重试");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

#[derive(Debug)]
enum SaveError {
    LeaseLost,
    Other(MtprotoWorkerError),
}

async fn save_leased(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    lease: &LeaseGuard,
    client: &TelegramClient,
) -> Result<(), SaveError> {
    let snapshot = client
        .snapshot()
        .await
        .map_err(|error| SaveError::Other(error.into()))?;
    match store_identity_secret_leased(pool, kek, tenant, lease, &snapshot).await {
        Ok(()) => Ok(()),
        Err(CredentialError::LeaseLost) => Err(SaveError::LeaseLost),
        Err(error) => Err(SaveError::Other(SessionStoreError::from(error).into())),
    }
}

/// 进程级循环：定期扫描可接管的身份，每个身份一个任务。
pub struct MtprotoWorker {
    pool: PgPool,
    kek: Arc<dyn Kek>,
    worker_id: String,
    config: MtprotoWorkerConfig,
    media_store: Option<MediaStore>,
}

impl MtprotoWorker {
    pub fn new(
        pool: PgPool,
        kek: Arc<dyn Kek>,
        worker_id: String,
        config: MtprotoWorkerConfig,
        media_store: Option<MediaStore>,
    ) -> Self {
        Self {
            pool,
            kek,
            worker_id,
            config,
            media_store,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), MtprotoWorkerError> {
        // 值是任务句柄与最早可重启时间：异常退出的身份等 restart_delay 再试，不打满 Telegram。
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
                    if let Err(error) = self.spawn_candidates(&mut running, &mut cooldown, &shutdown).await {
                        tracing::warn!(error = %error, "扫描 mtproto 身份失败，下个周期重试");
                    }
                }
            }
        }
        for (_, handle) in running {
            let _ = handle.await;
        }
        Ok(())
    }

    async fn spawn_candidates(
        &self,
        running: &mut HashMap<IdentityId, JoinHandle<()>>,
        cooldown: &mut HashMap<IdentityId, tokio::time::Instant>,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(), sqlx::Error> {
        for tenant in list_all_active_tenants(&self.pool).await? {
            for (identity, api_id) in list_mtproto_worker_candidates(&self.pool, tenant).await? {
                if running.contains_key(&identity) || cooldown.contains_key(&identity) {
                    continue;
                }
                cooldown.insert(
                    identity,
                    tokio::time::Instant::now() + self.config.restart_delay,
                );
                let pool = self.pool.clone();
                let kek = Arc::clone(&self.kek);
                let worker_id = self.worker_id.clone();
                let config = self.config;
                let media_store = self.media_store.clone();
                let shutdown = shutdown.clone();
                running.insert(
                    identity,
                    tokio::spawn(async move {
                        let target = MtprotoTarget { tenant, identity, api_id };
                        let result = run_identity(
                            &pool,
                            kek.as_ref(),
                            target,
                            &worker_id,
                            config,
                            media_store.as_ref(),
                            shutdown,
                        )
                        .await;
                        match result {
                            Ok(exit) => tracing::info!(identity_id = identity.get(), exit = ?exit, "mtproto 身份循环结束"),
                            Err(error) => tracing::warn!(identity_id = identity.get(), error = %error, "mtproto 身份循环异常退出"),
                        }
                    }),
                );
            }
        }
        Ok(())
    }
}
