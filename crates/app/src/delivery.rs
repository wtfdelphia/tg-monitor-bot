use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tgm_core::error::AppError;
use tgm_core::ids::IdentityId;
use tgm_db::repo::{
    DeliveryTaskFailureInput, DeliveryTaskFailureStatus, DeliveryTaskPayloadRecord,
    claim_due_delivery_task, complete_delivery_task, load_delivery_task_payload,
    record_delivery_failure, recover_stale_delivery_tasks,
};
use tgm_tg::{TelegramClient, TelegramDeliveryAction};
use tokio::sync::watch;

const MIN_WORKER_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_WORKER_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryParseMode {
    Html,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: DeliveryAction,
    pub source: DeliverySourceMessage,
    pub message: RenderedDeliveryMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverySourceMessage {
    pub receiver_identity_id: IdentityId,
    pub tg_chat_id: i64,
    pub tg_message_id: i64,
    pub sender_id: Option<i64>,
    pub media_group_id: Option<String>,
    pub media_type: Option<String>,
    pub file_unique_id: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
    pub mime_type: Option<String>,
    pub raw_file_id: Option<String>,
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
}

impl TelegramDeliveryTransport {
    pub fn new(client: Arc<TelegramClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl DeliveryTransport for TelegramDeliveryTransport {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
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
                &payload.message.text,
                payload.source.media_type.as_deref(),
            )
            .await
            .map_err(|error| {
                let error = error.delivery_error();
                DeliverySendError {
                    code: error.code,
                    message: error.message,
                    flood_wait_seconds: error.flood_wait_seconds,
                }
            })
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
                DeliveryRunOutcome::Done { .. } | DeliveryRunOutcome::Failed { .. } => {}
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
    let snapshot = record.rule_snapshot.as_ref();
    if record.rule_id.is_some() && record.rule_version.is_some() && snapshot.is_none() {
        return Err(invalid_payload("规则快照不存在"));
    }
    let template = snapshot
        .and_then(|value| value.get("rule"))
        .and_then(|rule| rule.get("template"))
        .and_then(Value::as_str);
    let replacements = parse_replacements(snapshot)?;
    let matched_keywords = record.matched_keywords.clone().unwrap_or_default();
    let source = DeliverySourceMessage {
        receiver_identity_id: record
            .receiver_identity_id
            .ok_or_else(|| invalid_payload("缺少来源接收身份"))?,
        tg_chat_id: record
            .tg_chat_id
            .ok_or_else(|| invalid_payload("缺少来源 chat_id"))?,
        tg_message_id: record
            .tg_message_id
            .ok_or_else(|| invalid_payload("缺少来源 message_id"))?,
        sender_id: record.sender_id,
        media_group_id: record.media_group_id.clone(),
        media_type: record.media_type.clone(),
        file_unique_id: record.file_unique_id.clone(),
        file_name: record.file_name.clone(),
        file_size: record.file_size,
        mime_type: record.mime_type.clone(),
        raw_file_id: record.raw_file_id.clone(),
    };
    let message = render_delivery_message(DeliveryRenderInput {
        template,
        text: record.text.as_deref(),
        caption: record.caption.as_deref(),
        chat_title: record.chat_title.as_deref(),
        username: record.username.as_deref(),
        sender_id: record.sender_id,
        matched_keywords: &matched_keywords,
        tg_chat_id: record
            .tg_chat_id
            .ok_or_else(|| invalid_payload("缺少来源 chat_id"))?,
        tg_message_id: record
            .tg_message_id
            .ok_or_else(|| invalid_payload("缺少来源 message_id"))?,
        replacements: &replacements,
    })?;

    Ok(DeliveryPayload {
        task_id: record.id,
        target_tg_chat_id: record.target_tg_chat_id,
        sender_identity_id: record.sender_identity_id,
        action: parse_delivery_action(&record.action)?,
        source,
        message,
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
    let payload = match payload {
        Ok(payload) => payload,
        Err(error) => {
            let attempt_count = task.attempt_count.saturating_add(1);
            record_delivery_failure(
                pool,
                tenant,
                task.id,
                DeliveryTaskFailureInput {
                    status: DeliveryTaskFailureStatus::Dead,
                    attempt_count,
                    next_retry_at: None,
                    error_code: "PAYLOAD_INVALID".to_owned(),
                    error_message: Some(error.to_string()),
                    latency_ms: None,
                },
            )
            .await?;
            return Ok(DeliveryRunOutcome::Failed {
                task_id: task.id,
                status: DeliveryDecisionStatus::Dead,
                attempt_count,
            });
        }
    };

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
        if [
            "CONTENT_PROTECTED",
            "FORBIDDEN",
            "CHAT_WRITE_FORBIDDEN",
            "ChatWriteForbidden",
            "CHANNEL_NOT_FOUND",
            "PEER_ID_INVALID",
            "MEDIA_COPY_UNAVAILABLE",
            "MEDIA_RESEND_UNAVAILABLE",
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
        DeliveryErrorKind::Retryable | DeliveryErrorKind::Permanent => 0,
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
        assert!(matches!(
            DeliveryErrorKind::from_code("FLOOD_WAIT", Some(12)),
            DeliveryErrorKind::FloodWait { seconds: 12 }
        ));
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
