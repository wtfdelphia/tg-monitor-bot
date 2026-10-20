use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tgm_core::error::AppError;
use tgm_core::ids::IdentityId;
use tgm_db::repo::{
    DeliveryTaskFailureInput, DeliveryTaskFailureStatus, DeliveryTaskPayloadRecord,
    MediaRelayStateRecord, claim_due_delivery_task, complete_delivery_task, defer_delivery_task,
    load_delivery_task_payload, load_media_group_relay_for_task, load_media_relay_for_task,
    record_delivery_failure, recover_stale_delivery_tasks,
};
use tgm_tg::{TelegramClient, TelegramDeliveryAction, UploadMedia, telegram_text_len};

use crate::media_relay::MediaStore;
use tokio::sync::watch;

const MIN_WORKER_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_WORKER_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Telegram 媒体说明上限（片 1 实测 `MEDIA_CAPTION_TOO_LONG`）。
pub const CAPTION_MAX_LEN: usize = 1024;
/// 文本消息上限（eng/02 `message_length_max`）。
pub const MESSAGE_MAX_LEN: usize = 4096;
/// 媒体还没下载好时推后多久再看。
const MEDIA_WAIT_SECONDS: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryParseMode {
    #[serde(rename = "HTML")]
    Html,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedDeliveryMessage {
    pub text: String,
    pub parse_mode: DeliveryParseMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryAction {
    Copy,
    Forward,
    Resend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPayload {
    pub task_id: i64,
    pub telegram_random_id: i64,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: DeliveryAction,
    pub source: DeliverySourceMessage,
    pub message: RenderedDeliveryMessage,
    /// free 档媒体消息：只发文字（正文已在快照里替换成 caption 或占位说明）。
    pub text_only: bool,
    /// pro 档跨身份中转：已下载到对象存储的媒体。`None` = 不走中转。
    pub media: Option<DeliveryMedia>,
    pub media_group: Vec<DeliveryMedia>,
}

/// 中转 job 就绪后 delivery 要上传的对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryMedia {
    pub object_key: String,
    pub media_type: String,
    pub mime_type: Option<String>,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverySourceMessage {
    pub tg_chat_id: i64,
    pub tg_message_id: i64,
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_message_ids: Vec<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliverySnapshot {
    source: DeliverySourceMessage,
    message: RenderedDeliveryMessage,
    /// 片 2 新增。缺省为 `false`，片 2 之前冻结的快照照常可读；`false` 时不序列化，
    /// 文字与 pro 档快照的形状与之前一致。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    text_only: bool,
}

/// 媒体 + 说明怎么发（片 2 批准记录：超过 1024 拆成「媒体 + 一条文字」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionPlan {
    /// 说明跟媒体一条发完。
    Single,
    /// 先发不带说明的媒体，再发一条全文。
    Split,
    /// 全文也超过文本上限：判死，不截断原文（eng/02）。
    TooLong,
}

/// `len` 是 [`telegram_text_len`] 的结果（HTML 解析后的 UTF-16 码元数）。
pub fn caption_plan(len: usize) -> CaptionPlan {
    if len <= CAPTION_MAX_LEN {
        CaptionPlan::Single
    } else if len <= MESSAGE_MAX_LEN {
        CaptionPlan::Split
    } else {
        CaptionPlan::TooLong
    }
}

/// 拆两条时第二条文字的 `random_id`，由第一条确定性派生：重试时两条用同一对值整组重发，
/// 服务器按 `random_id` 去重，不用记录「第一条已成功」。恒为正，且与原值不同。
// ponytail: 派生值与同一 (sender, target) 下其他任务的 random_id 碰撞概率约 2^-63，
// 出现「编辑第二条文字」的需求时改为 delivery_tasks 独立列
pub fn split_text_random_id(random_id: i64) -> i64 {
    let derived = (random_id ^ 0x5a5a_5a5a_5a5a_5a5a) & i64::MAX;
    if derived == 0 { 1 } else { derived }
}

pub fn media_group_random_id(random_id: i64, position: usize) -> i64 {
    let offset = position as u64;
    (((random_id as u64 - 1 + offset) % i64::MAX as u64) + 1) as i64
}

fn send_error(code: &str, message: impl Into<String>) -> DeliverySendError {
    DeliverySendError {
        code: code.to_owned(),
        message: Some(message.into()),
        flood_wait_seconds: None,
    }
}

fn tg_send_error(error: tgm_tg::TelegramClientError) -> DeliverySendError {
    let error = error.delivery_error();
    DeliverySendError {
        code: error.code,
        message: error.message,
        flood_wait_seconds: error.flood_wait_seconds,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverySendError {
    pub code: String,
    pub message: Option<String>,
    pub flood_wait_seconds: Option<i64>,
}

#[async_trait::async_trait]
pub trait DeliveryTransport: Send + Sync {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError>;
}

pub struct TelegramDeliveryTransport {
    client: Arc<TelegramClient>,
    store: MediaStore,
}

impl TelegramDeliveryTransport {
    pub fn new(client: Arc<TelegramClient>, store: MediaStore) -> Self {
        Self { client, store }
    }

    async fn deliver_media_group(
        &self,
        payload: &DeliveryPayload,
    ) -> Result<i64, DeliverySendError> {
        if let [media] = payload.media_group.as_slice() {
            return self.deliver_media(payload, media).await;
        }
        let plan = caption_plan(telegram_text_len(&payload.message.text));
        if plan == CaptionPlan::TooLong {
            return Err(send_error(
                "MESSAGE_TOO_LONG",
                "说明超过文本消息上限，不截断",
            ));
        }
        let random_ids: Vec<_> = (0..payload.media_group.len())
            .map(|position| media_group_random_id(payload.telegram_random_id, position))
            .collect();
        let mut uploaded = Vec::with_capacity(payload.media_group.len());
        for media in &payload.media_group {
            let (mut reader, size) = self
                .store
                .get_stream(&media.object_key)
                .await
                .map_err(|error| send_error("MEDIA_STORE_FAILED", error.to_string()))?;
            uploaded.push(
                self.client
                    .prepare_album_media(
                        payload.target_tg_chat_id,
                        &mut reader,
                        size,
                        &UploadMedia {
                            media_type: media.media_type.clone(),
                            mime_type: media.mime_type.clone(),
                            file_name: media.file_name.clone(),
                        },
                    )
                    .await
                    .map_err(tg_send_error)?,
            );
        }
        let caption = if plan == CaptionPlan::Single {
            payload.message.text.as_str()
        } else {
            ""
        };
        let message_ids = self
            .client
            .send_media_group(payload.target_tg_chat_id, &random_ids, uploaded, caption)
            .await
            .map_err(tg_send_error)?;
        if plan == CaptionPlan::Split {
            self.client
                .copy_text(
                    payload.target_tg_chat_id,
                    split_text_random_id(payload.telegram_random_id),
                    &payload.message.text,
                )
                .await
                .map_err(tg_send_error)?;
        }
        message_ids
            .first()
            .copied()
            .ok_or_else(|| send_error("MESSAGE_NOT_RETURNED", "相册没有返回消息"))
    }

    async fn deliver_media(
        &self,
        payload: &DeliveryPayload,
        media: &DeliveryMedia,
    ) -> Result<i64, DeliverySendError> {
        let plan = caption_plan(telegram_text_len(&payload.message.text));
        if plan == CaptionPlan::TooLong {
            return Err(send_error(
                "MESSAGE_TOO_LONG",
                "说明超过文本消息上限，不截断",
            ));
        }
        let (mut reader, size) = self
            .store
            .get_stream(&media.object_key)
            .await
            .map_err(|error| send_error("MEDIA_STORE_FAILED", error.to_string()))?;
        let upload = UploadMedia {
            media_type: media.media_type.clone(),
            mime_type: media.mime_type.clone(),
            file_name: media.file_name.clone(),
        };
        let caption = match plan {
            CaptionPlan::Single => payload.message.text.as_str(),
            CaptionPlan::Split | CaptionPlan::TooLong => "",
        };
        let message_id = self
            .client
            .send_uploaded_media(
                payload.target_tg_chat_id,
                payload.telegram_random_id,
                &mut reader,
                size,
                &upload,
                caption,
            )
            .await
            .map_err(tg_send_error)?;
        if plan == CaptionPlan::Split {
            self.client
                .copy_text(
                    payload.target_tg_chat_id,
                    split_text_random_id(payload.telegram_random_id),
                    &payload.message.text,
                )
                .await
                .map_err(tg_send_error)?;
        }
        Ok(message_id)
    }
}

#[async_trait::async_trait]
impl DeliveryTransport for TelegramDeliveryTransport {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
        if !payload.media_group.is_empty() {
            return self.deliver_media_group(&payload).await;
        }
        if payload.action == DeliveryAction::Forward && !payload.source.group_message_ids.is_empty()
        {
            let random_ids: Vec<_> = (0..payload.source.group_message_ids.len())
                .map(|position| media_group_random_id(payload.telegram_random_id, position))
                .collect();
            return self
                .client
                .forward_media_group(
                    payload.source.tg_chat_id,
                    &payload.source.group_message_ids,
                    payload.target_tg_chat_id,
                    &random_ids,
                )
                .await
                .map_err(tg_send_error)?
                .first()
                .copied()
                .ok_or_else(|| send_error("MESSAGE_NOT_RETURNED", "转发相册没有返回消息"));
        }
        if let Some(media) = &payload.media {
            return self.deliver_media(&payload, media).await;
        }
        if payload.text_only && payload.action != DeliveryAction::Forward {
            return self
                .client
                .copy_text(
                    payload.target_tg_chat_id,
                    payload.telegram_random_id,
                    &payload.message.text,
                )
                .await
                .map_err(tg_send_error);
        }
        let action = match payload.action {
            DeliveryAction::Copy => TelegramDeliveryAction::Copy,
            DeliveryAction::Forward => TelegramDeliveryAction::Forward,
            DeliveryAction::Resend => TelegramDeliveryAction::Resend,
        };
        self.client
            .deliver_text_or_forward(
                action,
                payload.source.tg_chat_id,
                payload.source.tg_message_id,
                payload.target_tg_chat_id,
                payload.telegram_random_id,
                &payload.message.text,
                payload.source.media_type.as_deref(),
            )
            .await
            .map_err(tg_send_error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryRunOutcome {
    Idle,
    Done {
        task_id: i64,
        telegram_message_id: i64,
    },
    Failed {
        task_id: i64,
        status: DeliveryDecisionStatus,
        attempt_count: i32,
    },
    /// 等跨身份中转下载，任务已推后、未计尝试。
    Deferred {
        task_id: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryWorkerConfig {
    pub poll_interval: Duration,
}

impl Default for DeliveryWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryWorkerExit {
    Shutdown,
}

pub struct DeliveryWorker<T: DeliveryTransport + ?Sized> {
    pool: PgPool,
    tenant: tgm_core::ids::TenantId,
    sender_identity_id: IdentityId,
    transport: Arc<T>,
    config: DeliveryWorkerConfig,
}

impl<T: DeliveryTransport + ?Sized> DeliveryWorker<T> {
    pub fn new(
        pool: PgPool,
        tenant: tgm_core::ids::TenantId,
        sender_identity_id: IdentityId,
        transport: Arc<T>,
        config: DeliveryWorkerConfig,
    ) -> Self {
        Self {
            pool,
            tenant,
            sender_identity_id,
            transport,
            config,
        }
    }

    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<DeliveryWorkerExit, AppError> {
        let mut retry_delay = self.config.poll_interval;
        loop {
            if *shutdown.borrow() {
                return Ok(DeliveryWorkerExit::Shutdown);
            }

            let outcome = match run_delivery_once(
                &self.pool,
                self.tenant,
                self.sender_identity_id,
                self.transport.as_ref(),
            )
            .await
            {
                Ok(outcome) => {
                    retry_delay = self.config.poll_interval;
                    outcome
                }
                Err(error) if matches!(error, AppError::Db(_)) => {
                    tracing::warn!(
                        error = %error,
                        retry_after_ms = retry_delay.as_millis(),
                        tenant_id = self.tenant.get(),
                        sender_identity_id = self.sender_identity_id.get(),
                        "delivery worker 数据库操作失败，将退避重试"
                    );
                    if !wait_for_delivery_retry(&mut shutdown, retry_delay).await {
                        return Ok(DeliveryWorkerExit::Shutdown);
                    }
                    retry_delay = next_delivery_retry_delay(retry_delay, self.config.poll_interval);
                    continue;
                }
                Err(error) => return Err(error),
            };

            match outcome {
                DeliveryRunOutcome::Idle => {
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return Ok(DeliveryWorkerExit::Shutdown);
                            }
                        }
                        _ = tokio::time::sleep(self.config.poll_interval) => {}
                    }
                }
                DeliveryRunOutcome::Done { .. }
                | DeliveryRunOutcome::Failed { .. }
                | DeliveryRunOutcome::Deferred { .. } => {}
            }
        }
    }
}

fn next_delivery_retry_delay(current: Duration, poll_interval: Duration) -> Duration {
    let base = poll_interval.max(MIN_WORKER_RETRY_DELAY);
    let current = current.max(base);
    match current.checked_mul(2) {
        Some(next) => next.min(MAX_WORKER_RETRY_DELAY),
        None => MAX_WORKER_RETRY_DELAY,
    }
}

async fn wait_for_delivery_retry(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        changed = shutdown.changed() => changed.is_ok() && !*shutdown.borrow(),
        _ = tokio::time::sleep(delay) => true,
    }
}

#[derive(Debug, Clone)]
pub struct DeliveryReplacement {
    pub seq: i16,
    pub pattern: String,
    pub replacement: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct DeliveryRenderInput<'a> {
    pub template: Option<&'a str>,
    pub text: Option<&'a str>,
    pub caption: Option<&'a str>,
    pub chat_title: Option<&'a str>,
    pub username: Option<&'a str>,
    pub sender_id: Option<i64>,
    pub matched_keywords: &'a [String],
    pub tg_chat_id: i64,
    pub tg_message_id: i64,
    pub replacements: &'a [DeliveryReplacement],
}

pub fn build_delivery_payload(
    record: &DeliveryTaskPayloadRecord,
) -> Result<DeliveryPayload, AppError> {
    let snapshot: DeliverySnapshot = serde_json::from_value(
        record
            .payload_snapshot
            .clone()
            .ok_or_else(|| invalid_payload("缺少投递快照"))?,
    )
    .map_err(|_| invalid_payload("投递快照格式无效"))?;
    let random_id = record
        .telegram_random_id
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_payload("缺少有效 Telegram random_id"))?;

    Ok(DeliveryPayload {
        task_id: record.id,
        telegram_random_id: random_id,
        target_tg_chat_id: record.target_tg_chat_id,
        sender_identity_id: record.sender_identity_id,
        action: parse_delivery_action(&record.action)?,
        source: snapshot.source,
        message: snapshot.message,
        text_only: snapshot.text_only,
        media: None,
        media_group: Vec::new(),
    })
}

/// 这条任务要不要等跨身份中转：COPY / RESEND 的非文字媒体，且不是 free 档的只发文字。
pub fn needs_media_relay(payload: &DeliveryPayload) -> bool {
    payload.action != DeliveryAction::Forward
        && !payload.text_only
        && payload
            .source
            .media_type
            .as_deref()
            .is_some_and(|media_type| media_type != "TEXT")
}

/// `text_only`：free 档的媒体消息只发文字。正文用 caption，没有 caption 时用占位说明
/// 「[媒体：<media_type>] <来源链接>」，照常走模板渲染（片 2 提案）。
pub(crate) fn freeze_delivery_snapshot(
    event: &tgm_db::repo::TenantEventRecord,
    rule_snapshot: &Value,
    matched_keywords: &[String],
    text_only: bool,
    members: &[tgm_db::repo::MediaGroupMemberRecord],
) -> Result<Value, AppError> {
    let replacements = parse_replacements(Some(rule_snapshot))?;
    let placeholder = (text_only
        && event.text.as_deref().unwrap_or_default().is_empty()
        && event.caption.as_deref().unwrap_or_default().is_empty())
    .then(|| {
        if members.is_empty() {
            media_placeholder(event)
        } else {
            format!(
                "[媒体组：{} 项] {}",
                members.len(),
                message_link(
                    event.username.as_deref(),
                    event.tg_chat_id,
                    event.tg_message_id
                )
            )
        }
    });
    let message = render_delivery_message(DeliveryRenderInput {
        template: rule_snapshot
            .get("rule")
            .and_then(|rule| rule.get("template"))
            .and_then(Value::as_str),
        text: event.text.as_deref(),
        caption: placeholder.as_deref().or(event.caption.as_deref()),
        chat_title: event.chat_title.as_deref(),
        username: event.username.as_deref(),
        sender_id: event.sender_id,
        matched_keywords,
        tg_chat_id: event.tg_chat_id,
        tg_message_id: event.tg_message_id,
        replacements: &replacements,
    })?;
    serde_json::to_value(DeliverySnapshot {
        source: DeliverySourceMessage {
            tg_chat_id: event.tg_chat_id,
            tg_message_id: event.tg_message_id,
            media_type: event.media_type.clone(),
            group_message_ids: members.iter().map(|member| member.tg_message_id).collect(),
        },
        message,
        text_only,
    })
    .map_err(|_| invalid_payload("序列化投递快照失败"))
}

fn media_placeholder(event: &tgm_db::repo::TenantEventRecord) -> String {
    let username = event.username.as_deref().filter(|value| !value.is_empty());
    format!(
        "[媒体：{}] {}",
        event.media_type.as_deref().unwrap_or("DOCUMENT"),
        message_link(username, event.tg_chat_id, event.tg_message_id)
    )
}

/// 中转状态对这条任务的裁决。
enum RelayDecision {
    Send(DeliveryMedia),
    Wait,
    Dead(String),
}

fn relay_state_decision(state: MediaRelayStateRecord) -> RelayDecision {
    match state.job_status.as_deref() {
        None => RelayDecision::Dead("MEDIA_RELAY_UNAVAILABLE".to_owned()),
        Some("DEAD") => RelayDecision::Dead(
            state
                .job_error_code
                .unwrap_or_else(|| "MEDIA_RELAY_FAILED".to_owned()),
        ),
        Some("READY") => match state.object_key {
            Some(object_key) => RelayDecision::Send(DeliveryMedia {
                object_key,
                media_type: state.media_type.unwrap_or_else(|| "DOCUMENT".to_owned()),
                mime_type: state.mime_type,
                file_name: state.file_name,
            }),
            None => RelayDecision::Dead("MEDIA_RELAY_UNAVAILABLE".to_owned()),
        },
        Some(_) if state.timed_out => RelayDecision::Dead("MEDIA_RELAY_TIMEOUT".to_owned()),
        Some(_) => RelayDecision::Wait,
    }
}

async fn decide_media_relay(
    pool: &PgPool,
    tenant: tgm_core::ids::TenantId,
    task_id: i64,
) -> Result<RelayDecision, AppError> {
    let Some(state) = load_media_relay_for_task(pool, tenant, task_id).await? else {
        return Ok(RelayDecision::Dead("MEDIA_RELAY_UNAVAILABLE".to_owned()));
    };
    Ok(relay_state_decision(state))
}

async fn record_dead(
    pool: &PgPool,
    tenant: tgm_core::ids::TenantId,
    task: &tgm_db::repo::DeliveryTaskRecord,
    error_code: String,
    error_message: Option<String>,
) -> Result<DeliveryRunOutcome, AppError> {
    let attempt_count = task.attempt_count.saturating_add(1);
    record_delivery_failure(
        pool,
        tenant,
        task.id,
        DeliveryTaskFailureInput {
            status: DeliveryTaskFailureStatus::Dead,
            attempt_count,
            next_retry_at: None,
            error_code,
            error_message,
            latency_ms: None,
        },
    )
    .await?;
    Ok(DeliveryRunOutcome::Failed {
        task_id: task.id,
        status: DeliveryDecisionStatus::Dead,
        attempt_count,
    })
}

pub async fn run_delivery_once<T: DeliveryTransport + ?Sized>(
    pool: &PgPool,
    tenant: tgm_core::ids::TenantId,
    sender_identity_id: IdentityId,
    transport: &T,
) -> Result<DeliveryRunOutcome, AppError> {
    recover_stale_delivery_tasks(pool, tenant, sender_identity_id, 60).await?;
    let Some(task) = claim_due_delivery_task(pool, tenant, sender_identity_id).await? else {
        return Ok(DeliveryRunOutcome::Idle);
    };

    let payload = match load_delivery_task_payload(pool, tenant, task.id).await? {
        Some(record) => build_delivery_payload(&record),
        None => Err(invalid_payload("投递任务载荷不存在")),
    };
    let mut payload = match payload {
        Ok(payload) => payload,
        Err(error) => {
            return record_dead(
                pool,
                tenant,
                &task,
                "PAYLOAD_INVALID".to_owned(),
                Some(error.to_string()),
            )
            .await;
        }
    };
    if needs_media_relay(&payload) && !payload.source.group_message_ids.is_empty() {
        let states = load_media_group_relay_for_task(pool, tenant, task.id).await?;
        if states.len() != payload.source.group_message_ids.len() {
            return record_dead(
                pool,
                tenant,
                &task,
                "MEDIA_RELAY_UNAVAILABLE".to_owned(),
                None,
            )
            .await;
        }
        let mut waiting = false;
        for state in states {
            match relay_state_decision(state) {
                RelayDecision::Send(media) => payload.media_group.push(media),
                RelayDecision::Wait => waiting = true,
                RelayDecision::Dead(code) => {
                    return record_dead(pool, tenant, &task, code, None).await;
                }
            }
        }
        if waiting {
            defer_delivery_task(pool, tenant, task.id, MEDIA_WAIT_SECONDS).await?;
            return Ok(DeliveryRunOutcome::Deferred { task_id: task.id });
        }
    } else if needs_media_relay(&payload) {
        match decide_media_relay(pool, tenant, task.id).await? {
            RelayDecision::Send(media) => payload.media = Some(media),
            // 媒体还没下好：放回队列，不计尝试。
            RelayDecision::Wait => {
                defer_delivery_task(pool, tenant, task.id, MEDIA_WAIT_SECONDS).await?;
                return Ok(DeliveryRunOutcome::Deferred { task_id: task.id });
            }
            RelayDecision::Dead(code) => {
                return record_dead(pool, tenant, &task, code, None).await;
            }
        }
    }

    let started = std::time::Instant::now();
    match transport.deliver(payload).await {
        Ok(telegram_message_id) => {
            let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
            if !complete_delivery_task(pool, tenant, task.id, telegram_message_id, Some(latency_ms))
                .await?
            {
                return Err(internal_delivery_error("投递任务完成状态丢失"));
            }
            Ok(DeliveryRunOutcome::Done {
                task_id: task.id,
                telegram_message_id,
            })
        }
        Err(error) => {
            let decision = decide_failure(
                task.attempt_count,
                DeliveryErrorKind::from_code(&error.code, error.flood_wait_seconds),
                0,
            );
            let next_retry_at = decision
                .next_retry_after_seconds
                .map(|seconds| Utc::now() + std::time::Duration::from_secs(seconds as u64));
            let status = match decision.status {
                DeliveryDecisionStatus::Pending => DeliveryTaskFailureStatus::Pending,
                DeliveryDecisionStatus::Dead => DeliveryTaskFailureStatus::Dead,
            };
            record_delivery_failure(
                pool,
                tenant,
                task.id,
                DeliveryTaskFailureInput {
                    status,
                    attempt_count: decision.attempt_count,
                    next_retry_at,
                    error_code: error.code,
                    error_message: error.message,
                    latency_ms: Some(
                        i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX),
                    ),
                },
            )
            .await?;
            Ok(DeliveryRunOutcome::Failed {
                task_id: task.id,
                status: decision.status,
                attempt_count: decision.attempt_count,
            })
        }
    }
}

fn parse_delivery_action(value: &str) -> Result<DeliveryAction, AppError> {
    match value {
        "COPY" => Ok(DeliveryAction::Copy),
        "FORWARD" => Ok(DeliveryAction::Forward),
        "RESEND" => Ok(DeliveryAction::Resend),
        other => Err(invalid_payload(&format!("未知投递动作: {other}"))),
    }
}

fn parse_replacements(snapshot: Option<&Value>) -> Result<Vec<DeliveryReplacement>, AppError> {
    let Some(replacements) = snapshot
        .and_then(|value| value.get("replacements"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };

    replacements
        .iter()
        .map(|replacement| {
            let seq = replacement
                .get("seq")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid_payload("规则快照缺少替换 seq"))?;
            let seq =
                i16::try_from(seq).map_err(|_| invalid_payload("规则快照替换 seq 超出范围"))?;
            let pattern = replacement
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_payload("规则快照缺少替换 pattern"))?;
            let replacement_text = replacement
                .get("replacement")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_payload("规则快照缺少 replacement"))?;
            let enabled = replacement
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid_payload("规则快照缺少替换 enabled"))?;
            Ok(DeliveryReplacement {
                seq,
                pattern: pattern.to_owned(),
                replacement: replacement_text.to_owned(),
                enabled,
            })
        })
        .collect()
}

fn invalid_payload(message: &str) -> AppError {
    AppError::internal(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.to_owned(),
    ))
}

fn internal_delivery_error(message: &str) -> AppError {
    AppError::internal(std::io::Error::other(message.to_owned()))
}

pub fn render_delivery_message(
    input: DeliveryRenderInput<'_>,
) -> Result<RenderedDeliveryMessage, AppError> {
    let mut body = input.text.or(input.caption).unwrap_or_default().to_owned();
    let mut replacements: Vec<&DeliveryReplacement> = input
        .replacements
        .iter()
        .filter(|replacement| replacement.enabled)
        .collect();
    replacements.sort_by_key(|replacement| replacement.seq);
    for replacement in replacements {
        let regex = Regex::new(&replacement.pattern)
            .map_err(|error| AppError::RegexInvalid(error.to_string()))?;
        body = regex
            .replace_all(&body, replacement.replacement.as_str())
            .into_owned();
    }

    let escaped_body = escape_html(&body);
    let Some(template) = input.template else {
        return Ok(RenderedDeliveryMessage {
            text: escaped_body,
            parse_mode: DeliveryParseMode::Html,
        });
    };

    let username = input.username.filter(|value| !value.is_empty());
    let message_link = message_link(username, input.tg_chat_id, input.tg_message_id);
    let mut header = template.to_owned();
    if username.is_none() {
        header = header.replace("(@{username})", "");
    }
    header = header.replace(
        "{chat_title}",
        &escape_html(input.chat_title.unwrap_or_default()),
    );
    header = header.replace("{username}", &escape_html(username.unwrap_or_default()));
    header = header.replace(
        "{sender}",
        &escape_html(&input.sender_id.map(|id| id.to_string()).unwrap_or_default()),
    );
    header = header.replace(
        "{matched_keywords}",
        &escape_html(&input.matched_keywords.join(", ")),
    );
    header = header.replace("{message_link}", &message_link);

    let text = if escaped_body.is_empty() {
        header
    } else if header.is_empty() {
        escaped_body
    } else {
        format!("{header}\n{escaped_body}")
    };
    Ok(RenderedDeliveryMessage {
        text,
        parse_mode: DeliveryParseMode::Html,
    })
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn message_link(username: Option<&str>, tg_chat_id: i64, tg_message_id: i64) -> String {
    if let Some(username) = username {
        return format!("https://t.me/{username}/{tg_message_id}");
    }
    let chat_id = tg_chat_id.to_string();
    if let Some(private_id) = chat_id.strip_prefix("-100") {
        return format!("https://t.me/c/{private_id}/{tg_message_id}");
    }
    format!("chat_id={tg_chat_id} message_id={tg_message_id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryErrorKind {
    FloodWait { seconds: i64 },
    RandomIdUncertain,
    Retryable,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDecisionStatus {
    Pending,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryFailureDecision {
    pub status: DeliveryDecisionStatus,
    pub attempt_count: i32,
    pub next_retry_after_seconds: Option<i64>,
}

impl DeliveryErrorKind {
    pub fn from_code(code: &str, flood_wait_seconds: Option<i64>) -> Self {
        if code.eq_ignore_ascii_case("FLOOD_WAIT") {
            return Self::FloodWait {
                seconds: flood_wait_seconds.unwrap_or_default().max(0),
            };
        }
        if code.eq_ignore_ascii_case("RANDOM_ID_DUPLICATE")
            || code.eq_ignore_ascii_case("MESSAGE_NOT_RETURNED")
        {
            return Self::RandomIdUncertain;
        }
        if [
            "CONTENT_PROTECTED",
            "FORBIDDEN",
            "CHAT_WRITE_FORBIDDEN",
            "ChatWriteForbidden",
            "CHANNEL_NOT_FOUND",
            "CHANNEL_INVALID",
            "CHANNEL_PRIVATE",
            "PEER_ID_INVALID",
            "MEDIA_COPY_UNAVAILABLE",
            "MEDIA_RESEND_UNAVAILABLE",
            "MEDIA_GROUP_INVALID",
            // 长度超限重试不会变短（片 1 实测 caption 报 MEDIA_CAPTION_TOO_LONG）
            "MEDIA_CAPTION_TOO_LONG",
            "MESSAGE_TOO_LONG",
        ]
        .iter()
        .any(|permanent| code.eq_ignore_ascii_case(permanent))
        {
            return Self::Permanent;
        }
        Self::Retryable
    }
}

pub fn decide_failure(
    previous_attempt_count: i32,
    error: DeliveryErrorKind,
    jitter_seconds: i64,
) -> DeliveryFailureDecision {
    let current_attempt = previous_attempt_count.saturating_add(1);
    if matches!(error, DeliveryErrorKind::Permanent) {
        return DeliveryFailureDecision {
            status: DeliveryDecisionStatus::Dead,
            attempt_count: current_attempt,
            next_retry_after_seconds: None,
        };
    }

    if matches!(error, DeliveryErrorKind::RandomIdUncertain) {
        let retry_after = match current_attempt {
            1 => 30,
            2 => 5 * 60,
            _ => 30 * 60,
        };
        return DeliveryFailureDecision {
            status: DeliveryDecisionStatus::Pending,
            attempt_count: current_attempt,
            next_retry_after_seconds: Some(retry_after + jitter_seconds.max(0)),
        };
    }

    let retry_after = match current_attempt {
        1 => 30,
        2 => 5 * 60,
        3 => 30 * 60,
        _ => {
            return DeliveryFailureDecision {
                status: DeliveryDecisionStatus::Dead,
                attempt_count: current_attempt,
                next_retry_after_seconds: None,
            };
        }
    };

    let flood_wait = match error {
        DeliveryErrorKind::FloodWait { seconds } => seconds,
        DeliveryErrorKind::RandomIdUncertain
        | DeliveryErrorKind::Retryable
        | DeliveryErrorKind::Permanent => 0,
    };

    DeliveryFailureDecision {
        status: DeliveryDecisionStatus::Pending,
        attempt_count: current_attempt,
        next_retry_after_seconds: Some(retry_after.max(flood_wait) + jitter_seconds.max(0)),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        DeliveryDecisionStatus, DeliveryErrorKind, DeliveryParseMode, DeliveryRenderInput,
        DeliveryReplacement, MAX_WORKER_RETRY_DELAY, MIN_WORKER_RETRY_DELAY, decide_failure,
        next_delivery_retry_delay, render_delivery_message,
    };
    use std::time::Duration;

    #[test]
    fn retry_ladder_is_30_seconds_five_minutes_then_thirty_minutes() {
        let first = decide_failure(0, DeliveryErrorKind::Retryable, 7);
        let second = decide_failure(1, DeliveryErrorKind::Retryable, 7);
        let third = decide_failure(2, DeliveryErrorKind::Retryable, 7);

        assert_eq!(first.status, DeliveryDecisionStatus::Pending);
        assert_eq!(first.attempt_count, 1);
        assert_eq!(first.next_retry_after_seconds, Some(37));
        assert_eq!(second.next_retry_after_seconds, Some(307));
        assert_eq!(third.next_retry_after_seconds, Some(1807));
    }

    #[test]
    fn flood_wait_never_retries_before_server_delay() {
        let decision = decide_failure(0, DeliveryErrorKind::FloodWait { seconds: 90 }, 3);
        assert_eq!(decision.status, DeliveryDecisionStatus::Pending);
        assert_eq!(decision.next_retry_after_seconds, Some(93));
    }

    #[test]
    fn permanent_errors_go_directly_to_dead_without_retry_time() {
        let decision = decide_failure(0, DeliveryErrorKind::Permanent, 9);
        assert_eq!(decision.status, DeliveryDecisionStatus::Dead);
        assert_eq!(decision.attempt_count, 1);
        assert_eq!(decision.next_retry_after_seconds, None);
    }

    #[test]
    fn fourth_retry_enters_dead_letter_state() {
        let decision = decide_failure(3, DeliveryErrorKind::Retryable, 0);
        assert_eq!(decision.status, DeliveryDecisionStatus::Dead);
        assert_eq!(decision.attempt_count, 4);
        assert_eq!(decision.next_retry_after_seconds, None);
    }

    #[test]
    fn error_codes_classify_permanent_and_flood_wait_failures() {
        assert!(matches!(
            DeliveryErrorKind::from_code("ChatWriteForbidden", None),
            DeliveryErrorKind::Permanent
        ));
        for removed_from_channel in ["CHANNEL_INVALID", "CHANNEL_PRIVATE"] {
            assert_eq!(
                DeliveryErrorKind::from_code(removed_from_channel, None),
                DeliveryErrorKind::Permanent
            );
        }
        assert_eq!(
            DeliveryErrorKind::from_code("TELEGRAM_CLIENT_ERROR", None),
            DeliveryErrorKind::Retryable
        );
        assert!(matches!(
            DeliveryErrorKind::from_code("FLOOD_WAIT", Some(12)),
            DeliveryErrorKind::FloodWait { seconds: 12 }
        ));
    }

    #[test]
    fn random_id_uncertainty_never_enters_dead_letter_after_retry_limit() {
        assert_eq!(
            DeliveryErrorKind::from_code("RANDOM_ID_DUPLICATE", None),
            DeliveryErrorKind::RandomIdUncertain
        );
        assert_eq!(
            DeliveryErrorKind::from_code("MESSAGE_NOT_RETURNED", None),
            DeliveryErrorKind::RandomIdUncertain
        );

        for previous_attempt_count in 0..10 {
            let decision = decide_failure(
                previous_attempt_count,
                DeliveryErrorKind::RandomIdUncertain,
                0,
            );
            assert_eq!(decision.status, DeliveryDecisionStatus::Pending);
            assert!(decision.next_retry_after_seconds.is_some());
        }
    }

    #[test]
    fn render_applies_replacements_in_sequence_and_escapes_html() {
        let keywords = vec!["urgent".to_owned()];
        let replacements = vec![
            DeliveryReplacement {
                seq: 2,
                pattern: "URGENT".to_owned(),
                replacement: "done".to_owned(),
                enabled: true,
            },
            DeliveryReplacement {
                seq: 1,
                pattern: "urgent".to_owned(),
                replacement: "URGENT".to_owned(),
                enabled: true,
            },
        ];
        let rendered = render_delivery_message(DeliveryRenderInput {
            template: Some("📢 来源: {chat_title} (@{username})\n🔑 命中: {matched_keywords}"),
            text: Some("urgent <payload> & raw"),
            caption: None,
            chat_title: Some("A < B"),
            username: Some("source_channel"),
            sender_id: Some(7),
            matched_keywords: &keywords,
            tg_chat_id: -1009876543210,
            tg_message_id: 42,
            replacements: &replacements,
        })
        .expect("正文渲染失败");

        assert_eq!(rendered.parse_mode, DeliveryParseMode::Html);
        assert_eq!(
            rendered.text,
            "📢 来源: A &lt; B (@source_channel)\n🔑 命中: urgent\ndone &lt;payload&gt; &amp; raw"
        );
    }

    #[test]
    fn render_omits_empty_username_parentheses_and_builds_private_link() {
        let rendered = render_delivery_message(DeliveryRenderInput {
            template: Some("📢 来源: {chat_title} (@{username})\n🔗 {message_link}"),
            text: Some("body"),
            caption: None,
            chat_title: Some("Private & Group"),
            username: None,
            sender_id: None,
            matched_keywords: &[],
            tg_chat_id: -1001234567890,
            tg_message_id: 8,
            replacements: &[],
        })
        .expect("私有频道模板渲染失败");

        assert_eq!(
            rendered.text,
            "📢 来源: Private &amp; Group \n🔗 https://t.me/c/1234567890/8\nbody"
        );
    }

    #[test]
    fn render_falls_back_to_plain_body_without_template() {
        let rendered = render_delivery_message(DeliveryRenderInput {
            template: None,
            text: None,
            caption: Some("<caption>"),
            chat_title: None,
            username: None,
            sender_id: None,
            matched_keywords: &[],
            tg_chat_id: -42,
            tg_message_id: 9,
            replacements: &[],
        })
        .expect("无模板正文渲染失败");

        assert_eq!(rendered.text, "&lt;caption&gt;");
    }

    #[test]
    fn invalid_replacement_pattern_is_reported() {
        let error = render_delivery_message(DeliveryRenderInput {
            template: None,
            text: Some("body"),
            caption: None,
            chat_title: None,
            username: None,
            sender_id: None,
            matched_keywords: &[],
            tg_chat_id: -42,
            tg_message_id: 9,
            replacements: &[DeliveryReplacement {
                seq: 1,
                pattern: "(".to_owned(),
                replacement: "".to_owned(),
                enabled: true,
            }],
        })
        .expect_err("非法替换正则未报错");
        assert!(matches!(error, tgm_core::error::AppError::RegexInvalid(_)));
    }

    #[test]
    fn caption_plan_splits_above_1024_and_dies_above_4096() {
        use super::{CaptionPlan, caption_plan};
        assert_eq!(caption_plan(0), CaptionPlan::Single);
        assert_eq!(caption_plan(1024), CaptionPlan::Single);
        assert_eq!(caption_plan(1025), CaptionPlan::Split);
        assert_eq!(caption_plan(4096), CaptionPlan::Split);
        assert_eq!(caption_plan(4097), CaptionPlan::TooLong);
    }

    /// 长度按 HTML 解析后的 UTF-16 码元计：实体标签不计，转义按一个字符，
    /// 非 BMP 的 emoji 计 2。1024 个 😀 是 2048 码元，按 code point 计会误判成一条发完。
    #[test]
    fn caption_length_counts_utf16_after_html_parsing() {
        use super::{CaptionPlan, caption_plan};
        use tgm_tg::telegram_text_len;
        assert_eq!(telegram_text_len("<b>ab</b> &amp;"), 4);
        assert_eq!(telegram_text_len("😀"), 2);
        assert_eq!(
            caption_plan(telegram_text_len(&"😀".repeat(512))),
            CaptionPlan::Single
        );
        assert_eq!(
            caption_plan(telegram_text_len(&"😀".repeat(513))),
            CaptionPlan::Split
        );
        assert_eq!(
            caption_plan(telegram_text_len(&"<b>a</b>".repeat(1024))),
            CaptionPlan::Single
        );
    }

    #[test]
    fn split_text_random_id_is_positive_and_differs() {
        use super::split_text_random_id;
        for random_id in [1, 2, 0x5a5a_5a5a_5a5a_5a5a, i64::MAX, 0x2a5a_5a5a_5a5a_5a5a] {
            let derived = split_text_random_id(random_id);
            assert!(derived > 0, "{random_id} 派生出 {derived}");
            assert_ne!(derived, random_id);
            assert_eq!(derived, split_text_random_id(random_id), "派生必须确定");
        }
        assert_eq!(
            split_text_random_id(0x5a5a_5a5a_5a5a_5a5a),
            1,
            "异或为 0 时取 1"
        );
    }

    #[test]
    fn album_random_ids_are_positive_distinct_and_stable_across_wrap() {
        for base in [1, 2, i64::MAX - 4, i64::MAX, 0x5a5a_5a5a_5a5a_5a5a] {
            let ids: Vec<_> = (0..10)
                .map(|position| super::media_group_random_id(base, position))
                .collect();
            assert_eq!(ids[0], base);
            for (position, id) in ids.iter().enumerate() {
                assert!(*id > 0);
                assert!(!ids[..position].contains(id));
                assert_ne!(*id, super::split_text_random_id(base));
                assert_eq!(*id, super::media_group_random_id(base, position));
            }
        }
        assert_eq!(super::media_group_random_id(i64::MAX, 1), 1);
    }

    #[test]
    fn length_errors_are_permanent() {
        for code in ["MEDIA_CAPTION_TOO_LONG", "MESSAGE_TOO_LONG"] {
            assert_eq!(
                DeliveryErrorKind::from_code(code, None),
                DeliveryErrorKind::Permanent,
                "{code}"
            );
        }
    }

    #[test]
    fn delivery_retry_delay_starts_at_poll_interval_and_caps() {
        assert_eq!(
            next_delivery_retry_delay(Duration::from_millis(1), Duration::from_secs(2)),
            Duration::from_secs(4)
        );
        assert_eq!(
            next_delivery_retry_delay(Duration::from_millis(1), Duration::ZERO),
            MIN_WORKER_RETRY_DELAY * 2
        );
        assert_eq!(
            next_delivery_retry_delay(MAX_WORKER_RETRY_DELAY, Duration::from_secs(1)),
            MAX_WORKER_RETRY_DELAY
        );
    }
}
