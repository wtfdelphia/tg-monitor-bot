//! Repo 层。调用点形态的定义处是 ../../../docs/design/eng/00-工程约定.md §四。
//!
//! `count_keywords` 是 eng/00 §四 那个示例的实体，存在的理由是 R5
//!（eng/03 §三）需要一个真实的被测对象：
//! R5 断言「关掉 RLS 后应用层仍不泄漏」，而「应用层」指的就是下面那句
//! 显式的 `WHERE tenant_id = $1`。没有它，R5 无从谈起。
//!
//! 事件写入方法按 `canonical_events` 的唯一键完成幂等去重；租约方法直接使用
//! PostgreSQL 的原子写法，避免把抢占判断拆成“先查再写”的竞态窗口。

use tgm_core::ids::{IdentityId, TenantId};

use crate::tenant::tenant_tx;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};

const CONSOLE_LOGIN_NONCE_LEN: usize = 32;
const WEB_SESSION_TOKEN_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum ConsoleLoginRepoError {
    #[error("控制台登录 nonce 必须是 32 字节")]
    InvalidNonce,
    #[error("控制台登录确认码必须是 4 位 ASCII 字母或数字")]
    InvalidConfirmCode,
    #[error("Telegram 用户 id 必须是正整数")]
    InvalidTelegramUserId,
    #[error("生成 web session 随机材料失败")]
    Random(#[from] getrandom::Error),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalAdapter {
    Bot,
    MtProto,
}

impl CanonicalAdapter {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::MtProto => "mtproto",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalEventType {
    MessageCreated,
    MessageEdited,
    MessageDeleted,
    MediaGroupCompleted,
    SourcePermissionChanged,
}

impl CanonicalEventType {
    fn as_str(self) -> &'static str {
        match self {
            Self::MessageCreated => "MESSAGE_CREATED",
            Self::MessageEdited => "MESSAGE_EDITED",
            Self::MessageDeleted => "MESSAGE_DELETED",
            Self::MediaGroupCompleted => "MEDIA_GROUP_COMPLETED",
            Self::SourcePermissionChanged => "SOURCE_PERMISSION_CHANGED",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalEventInput {
    pub adapter: CanonicalAdapter,
    pub receiver_identity_id: IdentityId,
    pub tg_chat_id: i64,
    pub tg_message_id: i64,
    pub event_type: CanonicalEventType,
    pub edit_version: i32,
    pub update_id: Option<i64>,
    pub media_group_id: Option<String>,
    pub sender_id: Option<i64>,
    pub message_date: DateTime<Utc>,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub media_type: Option<String>,
    pub content_hash: Option<String>,
    pub file_unique_id: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
    pub mime_type: Option<String>,
    pub raw_file_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalEventWrite {
    pub id: i64,
    pub inserted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantEventWrite {
    pub id: i64,
    pub inserted: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TenantEventRecord {
    pub id: i64,
    pub canonical_event_id: i64,
    pub source_ref: i64,
    pub receiver_identity_id: IdentityId,
    pub tg_chat_id: i64,
    pub tg_message_id: i64,
    pub event_type: String,
    pub sender_id: Option<i64>,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub chat_title: Option<String>,
    pub username: Option<String>,
    pub media_type: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
    pub processed: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleConditionRecord {
    pub rule_id: i64,
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleReplacementRecord {
    pub rule_id: i64,
    pub seq: i16,
    pub pattern: String,
    pub replacement: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleVersionRecord {
    pub rule_id: i64,
    pub version: i32,
    pub snapshot: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RuleRecord {
    pub id: i64,
    pub tenant_id: i64,
    pub keyword_id: i64,
    pub keyword: String,
    pub keyword_match_type: String,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub version: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct RuleConditionInput {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct RuleReplacementInput {
    pub seq: i16,
    pub pattern: String,
    pub replacement: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct RuleCreateInput {
    pub keyword_id: i64,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub enabled: bool,
    pub conditions: Vec<RuleConditionInput>,
    pub replacements: Vec<RuleReplacementInput>,
}

#[derive(Debug, Clone)]
pub struct RuleUpdateInput {
    pub keyword_id: i64,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub enabled: bool,
    pub conditions: Vec<RuleConditionInput>,
    pub replacements: Vec<RuleReplacementInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleWrite {
    pub id: i64,
    pub version: i32,
    pub semantic_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleReplacementCopyWrite {
    pub rule_id: i64,
    pub version: i32,
    pub semantic_changed: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ExistingRule {
    pub keyword_id: i64,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: String,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub version: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ReplacementCopyTarget {
    pub id: i64,
    pub keyword_id: i64,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub version: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct CopyRuleSource {
    pub keyword_id: i64,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ExistingCondition {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ExistingReplacement {
    pub seq: i16,
    pub pattern: String,
    pub replacement: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct DeliveryTaskInput {
    pub rule_id: i64,
    pub rule_version: i32,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: String,
    pub payload_snapshot: Value,
    /// 跨身份媒体中转：同一事务里为该事件建 `media_relay_jobs`（片 2）。
    pub media_relay: bool,
    /// 入队即判死的错误码（如 `MEDIA_TOO_LARGE`）。`None` = 正常 PENDING。
    pub dead_error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryTaskWrite {
    pub id: i64,
    pub inserted: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryTaskRecord {
    pub id: i64,
    pub tenant_event_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub rule_version: Option<i32>,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: String,
    pub status: String,
    pub attempt_count: i32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub idem_key: Option<String>,
    pub telegram_random_id: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryTaskListRecord {
    pub id: i64,
    pub tenant_event_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub rule_version: Option<i32>,
    pub target_ref: Option<i64>,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: String,
    pub status: String,
    pub attempt_count: i32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub last_error_code: Option<String>,
    pub telegram_message_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryTaskDetailRecord {
    pub id: i64,
    pub tenant_event_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub rule_version: Option<i32>,
    pub target_ref: Option<i64>,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: String,
    pub status: String,
    pub attempt_count: i32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub last_error_code: Option<String>,
    pub telegram_message_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub rule_snapshot: Option<Value>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryLogRecord {
    pub id: i64,
    pub task_id: i64,
    pub attempt: i32,
    pub ok: bool,
    pub error_code: Option<String>,
    pub error_msg: Option<String>,
    pub latency_ms: Option<i32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuditLogRecord {
    pub id: i64,
    pub actor_user_id: Option<i64>,
    pub action: String,
    pub resource: String,
    pub resource_id: Option<i64>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub source: String,
    pub ip: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SystemLogRecord {
    pub id: i64,
    pub level: String,
    pub module: String,
    pub event: String,
    pub context: Option<Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryTaskPayloadRecord {
    pub id: i64,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub action: String,
    pub telegram_random_id: Option<i64>,
    pub payload_snapshot: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryTaskFailureStatus {
    Pending,
    Dead,
}

impl DeliveryTaskFailureStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Dead => "DEAD",
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryTaskFailureWrite {
    pub id: i64,
    pub status: String,
    pub attempt_count: i32,
    pub next_retry_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct DeliveryTaskFailureInput {
    pub status: DeliveryTaskFailureStatus,
    pub attempt_count: i32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub error_code: String,
    pub error_message: Option<String>,
    pub latency_ms: Option<i32>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SourceSubscriptionRecord {
    pub id: i64,
    pub tg_chat_id: i64,
    pub title: Option<String>,
    pub username: Option<String>,
    pub receiver_identity_id: i64,
    pub enabled: bool,
    pub discovered: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IdentityRecord {
    pub id: IdentityId,
    pub kind: String,
    pub tg_user_id: Option<i64>,
    pub display_name: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WebSessionRecord {
    pub tenant_id: TenantId,
    pub user_id: i64,
    pub csrf_token: Vec<u8>,
    pub role: String,
}

#[derive(Clone, sqlx::FromRow)]
pub struct ManagedWebSessionRecord {
    pub reference: Vec<u8>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub login_method: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub is_current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSessionLoginMethod {
    Oidc,
    BotQr,
}

impl WebSessionLoginMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::BotQr => "bot_qr",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebSessionIssue {
    pub session_id: Vec<u8>,
    pub csrf_token: Vec<u8>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConsoleLoginChallengeRecord {
    pub nonce: Vec<u8>,
    pub confirm_code: String,
    pub state: String,
    pub tg_user_id: Option<i64>,
    pub origin_ip: String,
    pub user_agent: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct IdentityUpdateInput {
    pub proxy_url: Option<Option<String>>,
    pub display_name: Option<Option<String>>,
    pub actor_user_id: i64,
    pub source: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct IdentityWriteRecord {
    id: IdentityId,
    kind: String,
    tg_user_id: Option<i64>,
    display_name: Option<String>,
    status: String,
    proxy_url: Option<String>,
}

impl IdentityWriteRecord {
    fn public_record(self) -> IdentityRecord {
        IdentityRecord {
            id: self.id,
            kind: self.kind,
            tg_user_id: self.tg_user_id,
            display_name: self.display_name,
            status: self.status,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DashboardSummaryRecord {
    pub identity_online: i64,
    pub identity_offline: i64,
    pub identity_flood_wait: i64,
    pub identity_banned: i64,
    pub identity_unauthorized: i64,
    pub queue_depth: i64,
    pub received_today: i64,
    pub matched_today: i64,
    pub delivered_today: i64,
    pub failed_today: i64,
    pub flood_wait_until: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceSubscriptionCreateInput {
    pub tg_chat_id: i64,
    pub receiver_identity_id: IdentityId,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SourceSubscriptionUpdateInput {
    pub enabled: Option<bool>,
    pub receiver_identity_id: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KeywordRecord {
    pub id: i64,
    pub word: String,
    pub match_type: String,
    pub category: Option<String>,
    pub priority: i32,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct KeywordCreateInput {
    pub word: String,
    pub match_type: String,
    pub category: Option<String>,
    pub priority: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct KeywordImportRuleInput {
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub enabled: bool,
    pub conditions: Vec<RuleConditionInput>,
    pub replacements: Vec<RuleReplacementInput>,
}

#[derive(Debug, Clone)]
pub struct KeywordImportWrite {
    pub keywords: Vec<KeywordRecord>,
    pub rule_ids: Vec<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct KeywordUpdateInput {
    pub word: Option<String>,
    pub match_type: Option<String>,
    pub category: Option<Option<String>>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TargetChannelRecord {
    pub id: i64,
    pub tg_chat_id: i64,
    pub title: Option<String>,
    pub sender_identity_id: IdentityId,
    pub can_send: Option<bool>,
    pub checked_at: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub probe_state: String,
    pub probe_generation: i64,
    pub probe_error_code: Option<String>,
    pub content_protected: Option<bool>,
    pub bot_role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TargetChannelCreateInput {
    pub tg_chat_id: i64,
    pub title: Option<String>,
    pub sender_identity_id: IdentityId,
}

#[derive(Debug, Clone, Default)]
pub struct TargetChannelUpdateInput {
    pub tg_chat_id: Option<i64>,
    pub title: Option<Option<String>>,
    pub sender_identity_id: Option<i64>,
    pub enabled: Option<bool>,
}

pub async fn list_tenant_identities(
    pool: &PgPool,
    tenant: TenantId,
    kind: Option<&str>,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<IdentityRecord>, sqlx::Error> {
    let kind = kind.map(str::to_owned);
    let status = status.map(str::to_owned);
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, IdentityRecord>(
                r#"
                SELECT id, kind, tg_user_id, display_name, status
                  FROM identities
                 WHERE (tenant_id = $1 OR tenant_id IS NULL)
                   AND ($2::text IS NULL OR kind = $2)
                   AND ($3::text IS NULL OR status = $3)
                 ORDER BY id
                 LIMIT $4 OFFSET $5
                "#,
            )
            .bind(tenant.get())
            .bind(kind.as_deref())
            .bind(status.as_deref())
            .bind(limit)
            .bind(offset)
            .fetch_all(conn)
            .await
        })
    })
    .await
}

pub async fn load_dashboard_summary(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<DashboardSummaryRecord, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, DashboardSummaryRecord>(
                r#"
                SELECT
                    (SELECT count(*)::bigint
                       FROM identities
                      WHERE (tenant_id = $1 OR tenant_id IS NULL)
                        AND status = 'online') AS identity_online,
                    (SELECT count(*)::bigint
                       FROM identities
                      WHERE (tenant_id = $1 OR tenant_id IS NULL)
                        AND status = 'offline') AS identity_offline,
                    (SELECT count(*)::bigint
                       FROM identities
                      WHERE (tenant_id = $1 OR tenant_id IS NULL)
                        AND status = 'flood_wait') AS identity_flood_wait,
                    (SELECT count(*)::bigint
                       FROM identities
                      WHERE (tenant_id = $1 OR tenant_id IS NULL)
                        AND status = 'banned') AS identity_banned,
                    (SELECT count(*)::bigint
                       FROM identities
                      WHERE (tenant_id = $1 OR tenant_id IS NULL)
                        AND status = 'unauthorized') AS identity_unauthorized,
                    (SELECT count(*)::bigint
                       FROM delivery_tasks
                      WHERE tenant_id = $1
                        AND status = 'PENDING') AS queue_depth,
                    (SELECT count(*)::bigint
                       FROM tenant_events
                      WHERE tenant_id = $1
                        AND created_at >= CURRENT_DATE) AS received_today,
                    (SELECT count(*)::bigint
                       FROM tenant_events
                      WHERE tenant_id = $1
                        AND created_at >= CURRENT_DATE
                        AND matched) AS matched_today,
                    (SELECT count(*)::bigint
                       FROM delivery_logs AS log
                       JOIN delivery_tasks AS task ON task.id = log.task_id
                      WHERE task.tenant_id = $1
                        AND log.created_at >= CURRENT_DATE
                        AND log.ok) AS delivered_today,
                    (SELECT count(*)::bigint
                       FROM delivery_logs AS log
                       JOIN delivery_tasks AS task ON task.id = log.task_id
                      WHERE task.tenant_id = $1
                        AND log.created_at >= CURRENT_DATE
                        AND NOT log.ok) AS failed_today,
                    (SELECT max(next_retry_at)::text
                       FROM delivery_tasks
                      WHERE tenant_id = $1
                        AND last_error_code = 'FLOOD_WAIT'
                        AND status IN ('PENDING', 'FAILED')
                        AND next_retry_at > clock_timestamp()) AS flood_wait_until
                "#,
            )
            .bind(tenant.get())
            .fetch_one(conn)
            .await
        })
    })
    .await
}

pub async fn list_source_subscriptions(
    pool: &PgPool,
    tenant: TenantId,
    status: Option<&str>,
    identity_id: Option<i64>,
    query: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<SourceSubscriptionRecord>, sqlx::Error> {
    let status = status.map(str::to_owned);
    let query = query.map(str::to_owned);
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, SourceSubscriptionRecord>(
                r#"
                SELECT id, tg_chat_id, title, username, receiver_identity_id, enabled, discovered
                  FROM source_subscriptions
                 WHERE tenant_id = $1
                   AND (
                       $2::text IS NULL
                       OR ($2 = 'enabled' AND enabled)
                       OR ($2 = 'disabled' AND NOT enabled)
                   )
                   AND ($3::bigint IS NULL OR receiver_identity_id = $3)
                   AND (
                       $4::text IS NULL
                       OR title ILIKE '%' || $4 || '%'
                       OR username ILIKE '%' || $4 || '%'
                   )
                 ORDER BY id
                 LIMIT $5 OFFSET $6
                "#,
            )
            .bind(tenant.get())
            .bind(status.as_deref())
            .bind(identity_id)
            .bind(query.as_deref())
            .bind(limit)
            .bind(offset)
            .fetch_all(conn)
            .await
        })
    })
    .await
}

pub async fn load_source_subscription(
    pool: &PgPool,
    tenant: TenantId,
    subscription_id: i64,
) -> Result<Option<SourceSubscriptionRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, SourceSubscriptionRecord>(
                r#"
                SELECT id, tg_chat_id, title, username, receiver_identity_id, enabled, discovered
                  FROM source_subscriptions
                 WHERE tenant_id = $1 AND id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(subscription_id)
            .fetch_optional(conn)
            .await
        })
    })
    .await
}

pub async fn load_tenant_identity(
    pool: &PgPool,
    tenant: TenantId,
    identity_id: i64,
) -> Result<Option<IdentityId>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, IdentityId>(
                "SELECT id FROM identities WHERE tenant_id = $1 AND id = $2",
            )
            .bind(tenant.get())
            .bind(identity_id)
            .fetch_optional(conn)
            .await
        })
    })
    .await
}

pub async fn load_web_session(
    pool: &PgPool,
    session_id: &[u8],
) -> Result<Option<WebSessionRecord>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.session_id', '\\x' || encode($1, 'hex'), true)")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;

    let session = sqlx::query_as::<_, (TenantId, i64)>(
        "SELECT tenant_id, user_id
           FROM web_sessions
          WHERE id = $1
            AND expires_at > now()",
    )
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((tenant_id, user_id)) = session else {
        tx.commit().await?;
        return Ok(None);
    };

    sqlx::query("SELECT set_config('app.auth_user_id', $1, true)")
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;

    let record = sqlx::query_as::<_, (Vec<u8>, String)>(
        "SELECT s.csrf_token, m.role
           FROM web_sessions s
           JOIN tenant_members m
             ON m.tenant_id = s.tenant_id
            AND m.user_id = s.user_id
          WHERE s.id = $1
            AND s.tenant_id = $2
            AND s.user_id = $3
            AND s.expires_at > now()",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(record.map(|(csrf_token, role)| WebSessionRecord {
        tenant_id,
        user_id,
        csrf_token,
        role,
    }))
}

pub async fn delete_web_session(pool: &PgPool, session_id: &[u8]) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.session_id', '\\x' || encode($1, 'hex'), true)")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM web_sessions WHERE id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

pub async fn list_web_sessions(
    pool: &PgPool,
    user_id: i64,
    current_session_id: &[u8; WEB_SESSION_TOKEN_LEN],
) -> Result<Vec<ManagedWebSessionRecord>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.auth_user_id', $1, true)")
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;
    let sessions = sqlx::query_as::<_, ManagedWebSessionRecord>(
        "SELECT sha256(id) AS reference, host(ip) AS ip, user_agent, login_method,
                created_at, expires_at, id = $2 AS is_current
           FROM web_sessions
          WHERE user_id = $1 AND expires_at > now()
          ORDER BY created_at DESC, id",
    )
    .bind(user_id)
    .bind(current_session_id.as_slice())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(sessions)
}

pub async fn revoke_web_session(
    pool: &PgPool,
    user_id: i64,
    reference: &[u8; WEB_SESSION_TOKEN_LEN],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.auth_user_id', $1, true)")
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM web_sessions WHERE user_id = $1 AND sha256(id) = $2")
        .bind(user_id)
        .bind(reference.as_slice())
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

pub fn web_session_reference(session_id: &[u8]) -> [u8; WEB_SESSION_TOKEN_LEN] {
    Sha256::digest(session_id).into()
}

/// 以已验证的 Telegram 用户身份签发一条 web session。
///
/// 该入口只允许鉴权域角色调用；租户不由调用方传入，而是从该用户唯一的成员关系
/// 推导。一个用户属于多个租户时返回 `None`，避免在登录阶段猜租户。
pub async fn issue_web_session_for_user(
    pool: &PgPool,
    user_id: i64,
    origin_ip: &str,
    user_agent: Option<&str>,
    login_method: WebSessionLoginMethod,
) -> Result<Option<WebSessionIssue>, ConsoleLoginRepoError> {
    validate_console_login_user_id(user_id)?;
    let mut tx = pool.begin().await?;
    let issue =
        issue_web_session_for_user_in_tx(&mut tx, user_id, origin_ip, user_agent, login_method)
            .await?;
    tx.commit().await?;
    Ok(issue)
}

async fn issue_web_session_for_user_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
    origin_ip: &str,
    user_agent: Option<&str>,
    login_method: WebSessionLoginMethod,
) -> Result<Option<WebSessionIssue>, ConsoleLoginRepoError> {
    sqlx::query("SELECT set_config('app.auth_user_id', $1, true)")
        .bind(user_id.to_string())
        .execute(&mut **tx)
        .await?;
    let memberships = sqlx::query_as::<_, (TenantId, String)>(
        "SELECT tenant_id, role
           FROM tenant_members
          WHERE user_id = $1
          ORDER BY tenant_id",
    )
    .bind(user_id)
    .fetch_all(&mut **tx)
    .await?;
    let Some((tenant_id, _role)) = (memberships.len() == 1)
        .then(|| memberships.into_iter().next())
        .flatten()
    else {
        return Ok(None);
    };

    let mut session_id = vec![0_u8; WEB_SESSION_TOKEN_LEN];
    let mut csrf_token = vec![0_u8; WEB_SESSION_TOKEN_LEN];
    getrandom::fill(&mut session_id)?;
    getrandom::fill(&mut csrf_token)?;
    sqlx::query(
        "INSERT INTO web_sessions
            (tenant_id, id, user_id, csrf_token, ip, user_agent, login_method, expires_at)
         VALUES ($1, $2, $3, $4, $5::inet, $6, $7, now() + interval '30 days')",
    )
    .bind(tenant_id)
    .bind(&session_id)
    .bind(user_id)
    .bind(&csrf_token)
    .bind(origin_ip)
    .bind(user_agent)
    .bind(login_method.as_str())
    .execute(&mut **tx)
    .await?;

    Ok(Some(WebSessionIssue {
        session_id,
        csrf_token,
    }))
}

/// 只消费已确认的控制台挑战，并在同一事务里写入 web_sessions。
///
/// 挑战行作为一次性消费标记：成功签发后立即删除，避免轮询重复创建会话。
/// 会话 id 与挑战 nonce 完全独立，不能从二维码材料推导出 bearer cookie。
pub async fn consume_confirmed_console_login_challenge(
    pool: &PgPool,
    nonce: &[u8],
) -> Result<Option<WebSessionIssue>, ConsoleLoginRepoError> {
    validate_console_login_nonce(nonce)?;
    let mut tx = pool.begin().await?;
    let expired = sqlx::query(
        "UPDATE console_login_challenges
            SET state = 'expired'
          WHERE nonce = $1
            AND state = 'confirmed'
            AND expires_at <= clock_timestamp()",
    )
    .bind(nonce)
    .execute(&mut *tx)
    .await?;
    if expired.rows_affected() == 1 {
        tx.commit().await?;
        return Ok(None);
    }
    let challenge = sqlx::query_as::<_, (Option<i64>, String, Option<String>)>(
        "SELECT tg_user_id, origin_ip::text, user_agent
           FROM console_login_challenges
          WHERE nonce = $1
            AND state = 'confirmed'
            AND expires_at > clock_timestamp()
          FOR UPDATE",
    )
    .bind(nonce)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((Some(user_id), origin_ip, user_agent)) = challenge else {
        tx.commit().await?;
        return Ok(None);
    };
    validate_console_login_user_id(user_id)?;

    let issue = issue_web_session_for_user_in_tx(
        &mut tx,
        user_id,
        &origin_ip,
        user_agent.as_deref(),
        WebSessionLoginMethod::BotQr,
    )
    .await?;
    if issue.is_none() {
        sqlx::query(
            "UPDATE console_login_challenges
                SET state = 'rejected'
              WHERE nonce = $1
                AND state = 'confirmed'",
        )
        .bind(nonce)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(None);
    }
    sqlx::query(
        "DELETE FROM console_login_challenges
          WHERE nonce = $1
            AND state = 'confirmed'",
    )
    .bind(nonce)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(issue)
}

pub async fn create_console_login_challenge(
    pool: &PgPool,
    nonce: &[u8],
    confirm_code: &str,
    origin_ip: &str,
    user_agent: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<(), ConsoleLoginRepoError> {
    validate_console_login_nonce(nonce)?;
    validate_console_login_confirm_code(confirm_code)?;
    sqlx::query(
        "INSERT INTO console_login_challenges
            (nonce, confirm_code, origin_ip, user_agent, expires_at)
         VALUES ($1, $2, $3::inet, $4, $5)",
    )
    .bind(nonce)
    .bind(confirm_code)
    .bind(origin_ip)
    .bind(user_agent)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn load_console_login_challenge(
    pool: &PgPool,
    nonce: &[u8],
) -> Result<Option<ConsoleLoginChallengeRecord>, ConsoleLoginRepoError> {
    validate_console_login_nonce(nonce)?;
    sqlx::query_as::<_, ConsoleLoginChallengeRecord>(
        "SELECT nonce,
                confirm_code::text AS confirm_code,
                state,
                tg_user_id,
                origin_ip::text AS origin_ip,
                user_agent,
                expires_at
           FROM console_login_challenges
          WHERE nonce = $1",
    )
    .bind(nonce)
    .fetch_optional(pool)
    .await
    .map_err(ConsoleLoginRepoError::Database)
}

pub async fn claim_console_login_challenge(
    pool: &PgPool,
    nonce: &[u8],
    tg_user_id: i64,
) -> Result<bool, ConsoleLoginRepoError> {
    validate_console_login_nonce(nonce)?;
    validate_console_login_user_id(tg_user_id)?;
    let result = sqlx::query(
        "UPDATE console_login_challenges
            SET state = 'awaiting_confirm', tg_user_id = $2
          WHERE nonce = $1
            AND state = 'pending'
            AND expires_at > clock_timestamp()",
    )
    .bind(nonce)
    .bind(tg_user_id)
    .execute(pool)
    .await;
    let result = match result {
        Ok(result) => result,
        Err(sqlx::Error::Database(error))
            if error.constraint() == Some("uq_console_login_pending") =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    Ok(result.rows_affected() == 1)
}

pub async fn confirm_console_login_challenge(
    pool: &PgPool,
    nonce: &[u8],
    tg_user_id: i64,
    confirm_code: &str,
) -> Result<bool, ConsoleLoginRepoError> {
    validate_console_login_nonce(nonce)?;
    validate_console_login_user_id(tg_user_id)?;
    validate_console_login_confirm_code(confirm_code)?;
    let result = sqlx::query(
        "UPDATE console_login_challenges
            SET state = 'confirmed'
          WHERE nonce = $1
            AND state = 'awaiting_confirm'
            AND tg_user_id = $2
            AND confirm_code = $3
            AND expires_at > clock_timestamp()",
    )
    .bind(nonce)
    .bind(tg_user_id)
    .bind(confirm_code)
    .execute(pool)
    .await?;
    if result.rows_affected() == 1 {
        return Ok(true);
    }

    sqlx::query(
        "UPDATE console_login_challenges
            SET state = 'rejected'
          WHERE nonce = $1
            AND state = 'awaiting_confirm'
            AND tg_user_id = $2
            AND confirm_code <> $3
            AND expires_at > clock_timestamp()",
    )
    .bind(nonce)
    .bind(tg_user_id)
    .bind(confirm_code)
    .execute(pool)
    .await?;
    Ok(false)
}

pub async fn reject_console_login_challenge(
    pool: &PgPool,
    nonce: &[u8],
    tg_user_id: i64,
) -> Result<bool, ConsoleLoginRepoError> {
    validate_console_login_nonce(nonce)?;
    validate_console_login_user_id(tg_user_id)?;
    let result = sqlx::query(
        "UPDATE console_login_challenges
            SET state = 'rejected'
          WHERE nonce = $1
            AND state = 'awaiting_confirm'
            AND tg_user_id = $2
            AND expires_at > clock_timestamp()",
    )
    .bind(nonce)
    .bind(tg_user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn expire_console_login_challenge(
    pool: &PgPool,
    nonce: &[u8],
) -> Result<bool, ConsoleLoginRepoError> {
    validate_console_login_nonce(nonce)?;
    let result = sqlx::query(
        "UPDATE console_login_challenges
            SET state = 'expired'
          WHERE nonce = $1
            AND state IN ('pending', 'awaiting_confirm')
            AND expires_at <= clock_timestamp()",
    )
    .bind(nonce)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

fn validate_console_login_nonce(nonce: &[u8]) -> Result<(), ConsoleLoginRepoError> {
    (nonce.len() == CONSOLE_LOGIN_NONCE_LEN)
        .then_some(())
        .ok_or(ConsoleLoginRepoError::InvalidNonce)
}

fn validate_console_login_confirm_code(confirm_code: &str) -> Result<(), ConsoleLoginRepoError> {
    (confirm_code.len() == 4
        && confirm_code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric()))
    .then_some(())
    .ok_or(ConsoleLoginRepoError::InvalidConfirmCode)
}

fn validate_console_login_user_id(tg_user_id: i64) -> Result<(), ConsoleLoginRepoError> {
    (tg_user_id > 0)
        .then_some(())
        .ok_or(ConsoleLoginRepoError::InvalidTelegramUserId)
}

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
struct SourceIdentityScopeRecord {
    id: IdentityId,
    shared: bool,
}

pub async fn load_source_identity(
    pool: &PgPool,
    tenant: TenantId,
    identity_id: i64,
) -> Result<IdentityId, tgm_core::error::AppError> {
    let scope = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, SourceIdentityScopeRecord>(
                r#"
                SELECT id, (tenant_id IS NULL) AS shared
                  FROM identities
                 WHERE (tenant_id = $1 OR tenant_id IS NULL)
                   AND id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(identity_id)
            .fetch_optional(conn)
            .await
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)?;
    match scope {
        Some(record) if !record.shared => Ok(record.id),
        Some(_) => Err(tgm_core::error::AppError::Forbidden),
        None => Err(tgm_core::error::AppError::NotFound),
    }
}

pub async fn update_identity(
    pool: &PgPool,
    tenant: TenantId,
    identity_id: i64,
    input: IdentityUpdateInput,
) -> Result<IdentityRecord, tgm_core::error::AppError> {
    if input.proxy_url.is_none() && input.display_name.is_none() {
        return Err(tgm_core::error::AppError::ConditionInvalid(
            "至少提供一个身份字段".to_owned(),
        ));
    }
    let proxy_set = input.proxy_url.is_some();
    let proxy_url = input.proxy_url.flatten();
    let display_name_set = input.display_name.is_some();
    let display_name = input.display_name.flatten();
    let actor_user_id = input.actor_user_id;
    let source = input.source;
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let before = sqlx::query_as::<_, IdentityWriteRecord>(
                r#"
                SELECT id, kind, tg_user_id, display_name, status, proxy_url
                  FROM identities
                 WHERE tenant_id = $1 AND id = $2
                 FOR UPDATE
                "#,
            )
            .bind(tenant.get())
            .bind(identity_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;

            let after = sqlx::query_as::<_, IdentityWriteRecord>(
                r#"
                UPDATE identities
                   SET proxy_url = CASE WHEN $3 THEN $4::text ELSE proxy_url END,
                       display_name = CASE WHEN $5 THEN $6::text ELSE display_name END,
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1 AND id = $2
                RETURNING id, kind, tg_user_id, display_name, status, proxy_url
                "#,
            )
            .bind(tenant.get())
            .bind(identity_id)
            .bind(proxy_set)
            .bind(proxy_url.as_deref())
            .bind(display_name_set)
            .bind(display_name.as_deref())
            .fetch_one(&mut *conn)
            .await?;

            if before.proxy_url != after.proxy_url || before.display_name != after.display_name {
                sqlx::query(
                    r#"
                    INSERT INTO audit_logs
                        (tenant_id, actor_user_id, action, resource, resource_id, before, after, source)
                    VALUES ($1, $2, 'identity.update', 'identity', $3, $4, $5, $6)
                    "#,
                )
                .bind(tenant.get())
                .bind(actor_user_id)
                .bind(identity_id)
                .bind(serde_json::json!({
                    "display_name": before.display_name,
                    "proxy_url_set": before.proxy_url.is_some(),
                }))
                .bind(serde_json::json!({
                    "display_name": after.display_name,
                    "proxy_url_set": after.proxy_url.is_some(),
                }))
                .bind(source)
                .execute(&mut *conn)
                .await?;
            }

            Ok(after.public_record())
        })
    })
    .await
    .map_err(|error| match error {
        sqlx::Error::RowNotFound => tgm_core::error::AppError::NotFound,
        error => tgm_core::error::AppError::Db(error),
    })
}

pub async fn reset_identity_status(
    pool: &PgPool,
    tenant: TenantId,
    identity_id: i64,
    actor_user_id: i64,
    source: &str,
) -> Result<IdentityRecord, tgm_core::error::AppError> {
    let source = source.to_owned();
    enum ResetResult {
        NotFound,
        Invalid(String),
        Updated(IdentityRecord),
    }
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let before = sqlx::query_as::<_, IdentityWriteRecord>(
                r#"
                SELECT id, kind, tg_user_id, display_name, status, proxy_url
                  FROM identities
                 WHERE tenant_id = $1 AND id = $2
                 FOR UPDATE
                "#,
            )
            .bind(tenant.get())
            .bind(identity_id)
            .fetch_optional(&mut *conn)
            .await?;
            let Some(before) = before else {
                return Ok(ResetResult::NotFound);
            };
            if before.status != "flood_wait" {
                return Ok(ResetResult::Invalid(
                    "只有 flood_wait 状态可以手动复位；失效或封禁身份必须重新授权".to_owned(),
                ));
            }
            let after = sqlx::query_as::<_, IdentityWriteRecord>(
                r#"
                UPDATE identities
                   SET status = 'offline', updated_at = clock_timestamp()
                 WHERE tenant_id = $1 AND id = $2 AND status = 'flood_wait'
                RETURNING id, kind, tg_user_id, display_name, status, proxy_url
                "#,
            )
            .bind(tenant.get())
            .bind(identity_id)
            .fetch_one(&mut *conn)
            .await?;
            sqlx::query(
                "UPDATE account_leases
                    SET expires_at = clock_timestamp()
                  WHERE identity_id = $1",
            )
            .bind(identity_id)
            .execute(&mut *conn)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO audit_logs
                    (tenant_id, actor_user_id, action, resource, resource_id, before, after, source)
                VALUES ($1, $2, 'identity.reset_status', 'identity', $3, $4, $5, $6)
                "#,
            )
            .bind(tenant.get())
            .bind(actor_user_id)
            .bind(identity_id)
            .bind(serde_json::json!({ "status": before.status }))
            .bind(serde_json::json!({ "status": after.status }))
            .bind(source)
            .execute(&mut *conn)
            .await?;
            Ok(ResetResult::Updated(after.public_record()))
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)?;
    match result {
        ResetResult::NotFound => Err(tgm_core::error::AppError::NotFound),
        ResetResult::Invalid(reason) => Err(tgm_core::error::AppError::ConditionInvalid(reason)),
        ResetResult::Updated(record) => Ok(record),
    }
}

pub async fn delete_identity(
    pool: &PgPool,
    tenant: TenantId,
    identity_id: i64,
    actor_user_id: i64,
    source: &str,
) -> Result<(), tgm_core::error::AppError> {
    let source = source.to_owned();
    enum DeleteResult {
        NotFound,
        Blocked(String),
        Deleted,
    }
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let before = sqlx::query_as::<_, IdentityWriteRecord>(
                r#"
                SELECT id, kind, tg_user_id, display_name, status, proxy_url
                  FROM identities
                 WHERE tenant_id = $1 AND id = $2
                 FOR UPDATE
                "#,
            )
            .bind(tenant.get())
            .bind(identity_id)
            .fetch_optional(&mut *conn)
            .await?;
            let Some(before) = before else {
                return Ok(DeleteResult::NotFound);
            };

            let dependencies = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
                r#"
                SELECT
                    (SELECT count(*) FROM source_subscriptions WHERE tenant_id = $1 AND receiver_identity_id = $2),
                    (SELECT count(*) FROM target_channels WHERE tenant_id = $1 AND sender_identity_id = $2),
                    (SELECT count(*) FROM delivery_tasks WHERE tenant_id = $1 AND sender_identity_id = $2),
                    (SELECT count(*) FROM canonical_events WHERE receiver_identity_id = $2),
                    (SELECT count(*) FROM bot_console_updates WHERE tenant_id = $1 AND bot_id = $2)
                "#,
            )
            .bind(tenant.get())
            .bind(identity_id)
            .fetch_one(&mut *conn)
            .await?;
            if dependencies.0 > 0
                || dependencies.1 > 0
                || dependencies.2 > 0
                || dependencies.3 > 0
                || dependencies.4 > 0
            {
                return Ok(DeleteResult::Blocked(format!(
                    "identity has dependent records: sources={}, targets={}, delivery_tasks={}, events={}, bot_updates={}",
                    dependencies.0, dependencies.1, dependencies.2, dependencies.3, dependencies.4
                )));
            }

            sqlx::query(
                r#"
                INSERT INTO audit_logs
                    (tenant_id, actor_user_id, action, resource, resource_id, before, source)
                VALUES ($1, $2, 'identity.delete', 'identity', $3, $4, $5)
                "#,
            )
            .bind(tenant.get())
            .bind(actor_user_id)
            .bind(identity_id)
            .bind(serde_json::json!({
                "kind": before.kind,
                "tg_user_id": before.tg_user_id,
                "display_name": before.display_name,
                "status": before.status,
                "proxy_url_set": before.proxy_url.is_some(),
            }))
            .bind(source)
            .execute(&mut *conn)
            .await?;
            sqlx::query("DELETE FROM identities WHERE tenant_id = $1 AND id = $2")
                .bind(tenant.get())
                .bind(identity_id)
                .execute(&mut *conn)
                .await?;
            let remaining = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM identities WHERE tenant_id = $1)",
            )
            .bind(tenant.get())
            .fetch_one(&mut *conn)
            .await?;
            if !remaining {
                sqlx::query("DELETE FROM credential_keys WHERE tenant_id = $1")
                    .bind(tenant.get())
                    .execute(&mut *conn)
                    .await?;
            }
            Ok(DeleteResult::Deleted)
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)?;
    match result {
        DeleteResult::NotFound => Err(tgm_core::error::AppError::NotFound),
        DeleteResult::Blocked(reason) => {
            Err(tgm_core::error::AppError::IdentityReferenced { reason })
        }
        DeleteResult::Deleted => Ok(()),
    }
}

pub async fn create_source_subscription(
    pool: &PgPool,
    tenant: TenantId,
    input: SourceSubscriptionCreateInput,
) -> Result<SourceSubscriptionRecord, tgm_core::error::AppError> {
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let identity_exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM identities WHERE id = $1 AND tenant_id = $2)",
            )
            .bind(input.receiver_identity_id.get())
            .bind(tenant.get())
            .fetch_one(&mut *conn)
            .await?;
            if !identity_exists {
                return Err(sqlx::Error::RowNotFound);
            }

            sqlx::query_as::<_, SourceSubscriptionRecord>(
                r#"
                INSERT INTO source_subscriptions
                    (tenant_id, tg_chat_id, receiver_identity_id, enabled, discovered, updated_at)
                VALUES ($1, $2, $3, FALSE, FALSE, clock_timestamp())
                ON CONFLICT (tenant_id, tg_chat_id, receiver_identity_id)
                DO UPDATE SET
                    updated_at = clock_timestamp()
                RETURNING id, tg_chat_id, title, username, receiver_identity_id, enabled, discovered
                "#,
            )
            .bind(tenant.get())
            .bind(input.tg_chat_id)
            .bind(input.receiver_identity_id.get())
            .fetch_one(&mut *conn)
            .await
        })
    })
    .await;

    match result {
        Ok(record) => Ok(record),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn update_source_subscription(
    pool: &PgPool,
    tenant: TenantId,
    subscription_id: i64,
    input: SourceSubscriptionUpdateInput,
) -> Result<SourceSubscriptionRecord, tgm_core::error::AppError> {
    if input.enabled.is_none() && input.receiver_identity_id.is_none() {
        return Err(tgm_core::error::AppError::ConditionInvalid(
            "至少提供 enabled 或 receiver_identity_id".to_owned(),
        ));
    }

    if input.enabled == Some(true) {
        let authorized = tenant_tx(pool, tenant, |conn| {
            Box::pin(async move {
                sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT authorized_by FROM source_subscriptions WHERE tenant_id = $1 AND id = $2",
                )
                .bind(tenant.get())
                .bind(subscription_id)
                .fetch_optional(conn)
                .await
            })
        })
        .await
        .map_err(tgm_core::error::AppError::Db)?;
        let Some(authorized_by) = authorized else {
            return Err(tgm_core::error::AppError::NotFound);
        };
        if authorized_by.is_none() {
            return Err(tgm_core::error::AppError::Forbidden);
        }
    }

    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            if let Some(identity_id) = input.receiver_identity_id {
                let identity_exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM identities WHERE id = $1 AND tenant_id = $2)",
                )
                .bind(identity_id)
                .bind(tenant.get())
                .fetch_one(&mut *conn)
                .await?;
                if !identity_exists {
                    return Err(sqlx::Error::RowNotFound);
                }
            }

            sqlx::query_as::<_, SourceSubscriptionRecord>(
                r#"
                UPDATE source_subscriptions
                   SET enabled = COALESCE($3, enabled),
                       receiver_identity_id = COALESCE($4, receiver_identity_id),
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1 AND id = $2
                RETURNING id, tg_chat_id, title, username, receiver_identity_id, enabled, discovered
                "#,
            )
            .bind(tenant.get())
            .bind(subscription_id)
            .bind(input.enabled)
            .bind(input.receiver_identity_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
        })
    })
    .await;

    match result {
        Ok(record) => Ok(record),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn list_target_channels(
    pool: &PgPool,
    tenant: TenantId,
    limit: i64,
    offset: i64,
) -> Result<Vec<TargetChannelRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, TargetChannelRecord>(
                r#"
                SELECT id, tg_chat_id, title, sender_identity_id, can_send, checked_at, enabled,
                       probe_state, probe_generation, probe_error_code, content_protected, bot_role
                  FROM target_channels
                 WHERE tenant_id = $1
                 ORDER BY id
                 LIMIT $2 OFFSET $3
                "#,
            )
            .bind(tenant.get())
            .bind(limit)
            .bind(offset)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn load_target_channel(
    pool: &PgPool,
    tenant: TenantId,
    target_id: i64,
) -> Result<Option<TargetChannelRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, TargetChannelRecord>(
                r#"
                SELECT id, tg_chat_id, title, sender_identity_id, can_send, checked_at, enabled,
                       probe_state, probe_generation, probe_error_code, content_protected, bot_role
                  FROM target_channels
                 WHERE tenant_id = $1 AND id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(target_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
}

/// 发送身份必须属于本租户且 `kind = 'bot'`（spec/08 §1.4：投递一律走 Bot 身份）。
/// 外层 `Err` 是数据库错误；内层 `Err` 是要返回给调用方的业务错误。
async fn check_sender_identity(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    identity_id: i64,
) -> Result<Result<(), tgm_core::error::AppError>, sqlx::Error> {
    let kind = sqlx::query_scalar::<_, String>(
        "SELECT kind FROM identities WHERE id = $1 AND tenant_id = $2",
    )
    .bind(identity_id)
    .bind(tenant.get())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match kind.as_deref() {
        None => Err(tgm_core::error::AppError::NotFound),
        Some("bot") => Ok(()),
        Some(_) => Err(tgm_core::error::AppError::ConditionInvalid(
            "发送身份必须是 Bot".to_owned(),
        )),
    })
}

pub async fn create_target_channel(
    pool: &PgPool,
    tenant: TenantId,
    input: TargetChannelCreateInput,
) -> Result<TargetChannelRecord, tgm_core::error::AppError> {
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            if let Err(error) =
                check_sender_identity(conn, tenant, input.sender_identity_id.get()).await?
            {
                return Ok(Err(error));
            }

            sqlx::query_as::<_, TargetChannelRecord>(
                r#"
                INSERT INTO target_channels
                    (tenant_id, tg_chat_id, title, sender_identity_id, can_send, checked_at, enabled, updated_at)
                VALUES ($1, $2, $3, $4, NULL, NULL, TRUE, clock_timestamp())
                RETURNING id, tg_chat_id, title, sender_identity_id, can_send, checked_at, enabled,
                          probe_state, probe_generation, probe_error_code, content_protected, bot_role
                "#,
            )
            .bind(tenant.get())
            .bind(input.tg_chat_id)
            .bind(&input.title)
            .bind(input.sender_identity_id.get())
            .fetch_one(&mut *conn)
            .await
            .map(Ok)
        })
    })
    .await;

    match result {
        Ok(Ok(record)) => Ok(record),
        Ok(Err(error)) => Err(error),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) if is_target_conflict(&error) => {
            match find_existing_target(pool, tenant, input.tg_chat_id, None).await? {
                Some(existing_id) => Err(tgm_core::error::AppError::TargetConflict { existing_id }),
                None => Err(tgm_core::error::AppError::Db(error)),
            }
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn update_target_channel(
    pool: &PgPool,
    tenant: TenantId,
    target_id: i64,
    input: TargetChannelUpdateInput,
) -> Result<TargetChannelRecord, tgm_core::error::AppError> {
    if input.tg_chat_id.is_none()
        && input.title.is_none()
        && input.sender_identity_id.is_none()
        && input.enabled.is_none()
    {
        return Err(tgm_core::error::AppError::ConditionInvalid(
            "至少提供一个目标频道字段".to_owned(),
        ));
    }
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            if let Some(sender_identity_id) = input.sender_identity_id
                && let Err(error) = check_sender_identity(conn, tenant, sender_identity_id).await?
            {
                return Ok(Err(error));
            }

            sqlx::query_as::<_, TargetChannelRecord>(
                r#"
                UPDATE target_channels
                   SET tg_chat_id = COALESCE($3, tg_chat_id),
                       title = CASE WHEN $7 THEN $4 ELSE title END,
                       sender_identity_id = COALESCE($5, sender_identity_id),
                       enabled = COALESCE($6, enabled),
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1 AND id = $2
                RETURNING id, tg_chat_id, title, sender_identity_id, can_send, checked_at, enabled,
                          probe_state, probe_generation, probe_error_code, content_protected, bot_role
                "#,
            )
            .bind(tenant.get())
            .bind(target_id)
            .bind(input.tg_chat_id)
            .bind(input.title.as_ref().and_then(|title| title.as_deref()))
            .bind(input.sender_identity_id)
            .bind(input.enabled)
            .bind(input.title.is_some())
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
            .map(Ok)
        })
    })
    .await;

    match result {
        Ok(Ok(record)) => Ok(record),
        Ok(Err(error)) => Err(error),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) if is_target_conflict(&error) => {
            match find_existing_target(
                pool,
                tenant,
                input.tg_chat_id.unwrap_or_default(),
                Some(target_id),
            )
            .await?
            {
                Some(existing_id) => Err(tgm_core::error::AppError::TargetConflict { existing_id }),
                None => Err(tgm_core::error::AppError::Db(error)),
            }
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn delete_target_channel(
    pool: &PgPool,
    tenant: TenantId,
    target_id: i64,
) -> Result<bool, tgm_core::error::AppError> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            Ok(sqlx::query_scalar::<_, i64>(
                "UPDATE target_channels SET enabled = FALSE, updated_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2 RETURNING id",
            )
            .bind(tenant.get())
            .bind(target_id)
            .fetch_optional(&mut *conn)
            .await?
            .is_some())
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)
}

fn is_target_conflict(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.constraint() == Some("uq_target"))
}

async fn find_existing_target(
    pool: &PgPool,
    tenant: TenantId,
    tg_chat_id: i64,
    exclude_target_id: Option<i64>,
) -> Result<Option<i64>, tgm_core::error::AppError> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT id
                  FROM target_channels
                 WHERE tenant_id = $1
                   AND tg_chat_id = $2
                   AND ($3::bigint IS NULL OR id <> $3)
                 LIMIT 1
                "#,
            )
            .bind(tenant.get())
            .bind(tg_chat_id)
            .bind(exclude_target_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)
}

/// 数本租户的关键词条数。
///
/// `Box::pin(async move { ... })` 是这个签名的必需样板，**每个查询都要写这一层**
/// （eng/00 §四）。SQL 里仍然显式写 `WHERE tenant_id = $1`，不因为有 RLS 就省
/// —— RLS 是第二道防线，不是唯一防线，而 R5 测的正是这一句在不在。
///
/// 用 `query_scalar!` 宏而不是运行时的 `query_as`，理由不在这一个查询本身：
/// `cargo sqlx prepare --check` 是 eng/04 §四 的一条卡口，而**全仓库零个宏调用时
/// 它返回 0**（`.sqlx/` 连目录都不生成）—— 一条什么都没管的卡口和一条通过的
/// 卡口同形。这里是第一个宏调用点，`.sqlx/` 从此有内容，那条卡口开始携带信息。
/// 审计模块是有意的例外（`audit/mod.rs` 开头写了为什么它必须用运行时 SQL）。
///
/// `count(*) as "n!"` 里的 `!` 不是装饰：PG 的聚合列在类型上可空，
/// 不加它宏推出来的是 `Option<i64>`，而 `count(*)` 不返回 NULL。
pub async fn count_keywords(pool: &PgPool, t: TenantId) -> Result<i64, sqlx::Error> {
    tenant_tx(pool, t, |conn| {
        Box::pin(async move {
            sqlx::query_scalar!(
                r#"SELECT count(*) as "n!" FROM keywords WHERE tenant_id = $1"#,
                t.get()
            )
            .fetch_one(conn)
            .await
        })
    })
    .await
}

pub async fn list_keywords(
    pool: &PgPool,
    tenant: TenantId,
    query: Option<&str>,
    match_type: Option<&str>,
    category: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<KeywordRecord>, sqlx::Error> {
    let query = query.map(str::to_owned);
    let match_type = match_type.map(str::to_owned);
    let category = category.map(str::to_owned);
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, KeywordRecord>(
                r#"
                SELECT id, word, match_type, category, priority, enabled, created_at, updated_at
                  FROM keywords
                 WHERE tenant_id = $1
                   AND ($2::text IS NULL OR word ILIKE '%' || $2 || '%')
                   AND ($3::text IS NULL OR match_type = $3)
                   AND ($4::text IS NULL OR category = $4)
                 ORDER BY priority DESC, id ASC
                 LIMIT $5 OFFSET $6
                "#,
            )
            .bind(tenant.get())
            .bind(query.as_deref())
            .bind(match_type.as_deref())
            .bind(category.as_deref())
            .bind(limit)
            .bind(offset)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn list_enabled_keywords(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<Vec<KeywordRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, KeywordRecord>(
                r#"
                SELECT id, word, match_type, category, priority, enabled, created_at, updated_at
                  FROM keywords
                 WHERE tenant_id = $1
                   AND enabled
                 ORDER BY priority DESC, id ASC
                "#,
            )
            .bind(tenant.get())
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn load_keyword(
    pool: &PgPool,
    tenant: TenantId,
    keyword_id: i64,
) -> Result<Option<KeywordRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, KeywordRecord>(
                r#"
                SELECT id, word, match_type, category, priority, enabled, created_at, updated_at
                  FROM keywords
                 WHERE tenant_id = $1 AND id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(keyword_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn create_keyword(
    pool: &PgPool,
    tenant: TenantId,
    input: KeywordCreateInput,
) -> Result<KeywordRecord, tgm_core::error::AppError> {
    let word = input.word.clone();
    let match_type = input.match_type.clone();
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, KeywordRecord>(
                r#"
                INSERT INTO keywords (tenant_id, word, match_type, category, priority, enabled, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp())
                RETURNING id, word, match_type, category, priority, enabled, created_at, updated_at
                "#,
            )
            .bind(tenant.get())
            .bind(&input.word)
            .bind(&input.match_type)
            .bind(&input.category)
            .bind(input.priority)
            .bind(input.enabled)
            .fetch_one(&mut *conn)
            .await
        })
    })
    .await;

    match result {
        Ok(record) => Ok(record),
        Err(error) if is_keyword_conflict(&error) => {
            match find_existing_keyword(pool, tenant, &word, &match_type, None).await? {
                Some(existing_id) => {
                    Err(tgm_core::error::AppError::KeywordConflict { existing_id })
                }
                None => Err(tgm_core::error::AppError::Db(error)),
            }
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn import_keywords(
    pool: &PgPool,
    tenant: TenantId,
    inputs: Vec<KeywordCreateInput>,
    rule_template: Option<KeywordImportRuleInput>,
) -> Result<KeywordImportWrite, tgm_core::error::AppError> {
    if inputs.is_empty() {
        return Err(tgm_core::error::AppError::ConditionInvalid(
            "keywords 不能为空".to_owned(),
        ));
    }
    if let Some(rule) = &rule_template {
        validate_rule_inputs(&rule.conditions, &rule.replacements)?;
    }

    let mut seen = std::collections::HashSet::with_capacity(inputs.len());
    for input in &inputs {
        if !seen.insert((input.word.clone(), input.match_type.clone())) {
            return Err(tgm_core::error::AppError::ConditionInvalid(format!(
                "导入数据含重复关键词: {}",
                input.word
            )));
        }
    }

    let keyword_keys: Vec<_> = inputs
        .iter()
        .map(|input| (input.word.clone(), input.match_type.clone()))
        .collect();
    let conflict_rule = rule_template.clone();
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let mut keywords = Vec::with_capacity(inputs.len());
            let mut rule_ids = Vec::with_capacity(if rule_template.is_some() {
                inputs.len()
            } else {
                0
            });
            for input in inputs {
                let keyword = sqlx::query_as::<_, KeywordRecord>(
                    r#"
                    INSERT INTO keywords (tenant_id, word, match_type, category, priority, enabled, updated_at)
                    VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp())
                    RETURNING id, word, match_type, category, priority, enabled, created_at, updated_at
                    "#,
                )
                .bind(tenant.get())
                .bind(&input.word)
                .bind(&input.match_type)
                .bind(&input.category)
                .bind(input.priority)
                .bind(input.enabled)
                .fetch_one(&mut *conn)
                .await?;

                if let Some(template) = &rule_template {
                    let rule_input = RuleCreateInput {
                        keyword_id: keyword.id,
                        source_ref: template.source_ref,
                        media_type: template.media_type.clone(),
                        target_ref: template.target_ref,
                        action: template.action.clone(),
                        priority: template.priority,
                        stop_processing: template.stop_processing,
                        template: template.template.clone(),
                        enabled: template.enabled,
                        conditions: template.conditions.clone(),
                        replacements: template.replacements.clone(),
                    };
                    rule_ids.push(insert_rule_tx(&mut *conn, tenant, &rule_input).await?.id);
                }
                keywords.push(keyword);
            }
            Ok(KeywordImportWrite { keywords, rule_ids })
        })
    })
    .await;

    match result {
        Ok(write) => Ok(write),
        Err(error) if is_keyword_conflict(&error) => {
            for (word, match_type) in &keyword_keys {
                if let Some(existing_id) =
                    find_existing_keyword(pool, tenant, word, match_type, None).await?
                {
                    return Err(tgm_core::error::AppError::KeywordConflict { existing_id });
                }
            }
            Err(tgm_core::error::AppError::Db(error))
        }
        Err(error) if is_rule_conflict(&error) => {
            if let Some(template) = conflict_rule {
                for (word, match_type) in &keyword_keys {
                    let Some(keyword_id) =
                        find_existing_keyword(pool, tenant, word, match_type, None).await?
                    else {
                        continue;
                    };
                    let key = ConflictKey {
                        keyword_id,
                        source_ref: template.source_ref,
                        media_type: template.media_type.clone(),
                        target_ref: template.target_ref,
                    };
                    if let Some(existing_id) =
                        find_conflicting_rule(pool, tenant, &key, None).await?
                    {
                        return Err(tgm_core::error::AppError::RuleConflict { existing_id });
                    }
                }
            }
            Err(tgm_core::error::AppError::Db(error))
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn update_keyword(
    pool: &PgPool,
    tenant: TenantId,
    keyword_id: i64,
    input: KeywordUpdateInput,
) -> Result<KeywordRecord, tgm_core::error::AppError> {
    let current = load_keyword(pool, tenant, keyword_id)
        .await
        .map_err(tgm_core::error::AppError::Db)?
        .ok_or(tgm_core::error::AppError::NotFound)?;
    let next_word = input.word.clone().unwrap_or(current.word);
    let next_match_type = input.match_type.clone().unwrap_or(current.match_type);
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, KeywordRecord>(
                r#"
                UPDATE keywords
                   SET word = COALESCE($3, word),
                       match_type = COALESCE($4, match_type),
                       category = CASE WHEN $8 THEN $5 ELSE category END,
                       priority = COALESCE($6, priority),
                       enabled = COALESCE($7, enabled),
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1 AND id = $2
                RETURNING id, word, match_type, category, priority, enabled, created_at, updated_at
                "#,
            )
            .bind(tenant.get())
            .bind(keyword_id)
            .bind(&input.word)
            .bind(&input.match_type)
            .bind(
                input
                    .category
                    .as_ref()
                    .and_then(|category| category.as_deref()),
            )
            .bind(input.priority)
            .bind(input.enabled)
            .bind(input.category.is_some())
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
        })
    })
    .await;

    match result {
        Ok(record) => Ok(record),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) if is_keyword_conflict(&error) => {
            match find_existing_keyword(
                pool,
                tenant,
                &next_word,
                &next_match_type,
                Some(keyword_id),
            )
            .await?
            {
                Some(existing_id) => {
                    Err(tgm_core::error::AppError::KeywordConflict { existing_id })
                }
                None => Err(tgm_core::error::AppError::Db(error)),
            }
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn delete_keyword(
    pool: &PgPool,
    tenant: TenantId,
    keyword_id: i64,
) -> Result<(bool, Vec<i64>), tgm_core::error::AppError> {
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let referenced_rule_ids = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM rules WHERE tenant_id = $1 AND keyword_id = $2 ORDER BY id",
            )
            .bind(tenant.get())
            .bind(keyword_id)
            .fetch_all(&mut *conn)
            .await?;
            if !referenced_rule_ids.is_empty() {
                return Ok((false, referenced_rule_ids));
            }

            let deleted = sqlx::query_scalar::<_, i64>(
                "DELETE FROM keywords WHERE tenant_id = $1 AND id = $2 RETURNING id",
            )
            .bind(tenant.get())
            .bind(keyword_id)
            .fetch_optional(&mut *conn)
            .await?
            .is_some();
            Ok((deleted, Vec::new()))
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)?;

    if !result.1.is_empty() {
        return Err(tgm_core::error::AppError::KeywordReferenced { rule_ids: result.1 });
    }
    Ok(result)
}

fn is_keyword_conflict(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.constraint() == Some("uq_keyword"))
}

async fn find_existing_keyword(
    pool: &PgPool,
    tenant: TenantId,
    word: &str,
    match_type: &str,
    exclude_keyword_id: Option<i64>,
) -> Result<Option<i64>, tgm_core::error::AppError> {
    let word = word.to_owned();
    let match_type = match_type.to_owned();
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT id
                  FROM keywords
                 WHERE tenant_id = $1
                   AND word = $2
                   AND match_type = $3
                   AND ($4::bigint IS NULL OR id <> $4)
                 LIMIT 1
                "#,
            )
            .bind(tenant.get())
            .bind(word)
            .bind(match_type)
            .bind(exclude_keyword_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)
}

pub async fn list_rules(
    pool: &PgPool,
    tenant: TenantId,
    enabled: Option<bool>,
    target_ref: Option<i64>,
    source_ref: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<Vec<RuleRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, RuleRecord>(
                r#"
                SELECT r.id,
                       r.tenant_id,
                       k.id AS keyword_id,
                       k.word AS keyword,
                       k.match_type AS keyword_match_type,
                       r.source_ref,
                       r.media_type,
                       r.target_ref,
                       tc.tg_chat_id AS target_tg_chat_id,
                       tc.sender_identity_id,
                       r.action,
                       r.priority,
                       r.stop_processing,
                       r.template,
                       r.version,
                       r.enabled
                  FROM rules AS r
                  JOIN keywords AS k
                    ON k.tenant_id = r.tenant_id
                   AND k.id = r.keyword_id
                  JOIN target_channels AS tc
                    ON tc.tenant_id = r.tenant_id
                   AND tc.id = r.target_ref
                 WHERE r.tenant_id = $1
                   AND ($2::boolean IS NULL OR r.enabled = $2)
                   AND ($3::bigint IS NULL OR r.target_ref = $3)
                   AND ($4::bigint IS NULL OR r.source_ref = $4)
                 ORDER BY r.priority DESC, r.id ASC
                 LIMIT $5 OFFSET $6
                "#,
            )
            .bind(tenant.get())
            .bind(enabled)
            .bind(target_ref)
            .bind(source_ref)
            .bind(limit)
            .bind(offset)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn load_rule_detail(
    pool: &PgPool,
    tenant: TenantId,
    rule_id: i64,
) -> Result<
    Option<(
        RuleRecord,
        Vec<RuleConditionRecord>,
        Vec<RuleReplacementRecord>,
    )>,
    sqlx::Error,
> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let Some(rule) = sqlx::query_as::<_, RuleRecord>(
                r#"
                SELECT r.id,
                       r.tenant_id,
                       k.id AS keyword_id,
                       k.word AS keyword,
                       k.match_type AS keyword_match_type,
                       r.source_ref,
                       r.media_type,
                       r.target_ref,
                       tc.tg_chat_id AS target_tg_chat_id,
                       tc.sender_identity_id,
                       r.action,
                       r.priority,
                       r.stop_processing,
                       r.template,
                       r.version,
                       r.enabled
                  FROM rules AS r
                  JOIN keywords AS k
                    ON k.tenant_id = r.tenant_id
                   AND k.id = r.keyword_id
                  JOIN target_channels AS tc
                    ON tc.tenant_id = r.tenant_id
                   AND tc.id = r.target_ref
                 WHERE r.tenant_id = $1
                   AND r.id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(rule_id)
            .fetch_optional(&mut *conn)
            .await?
            else {
                return Ok(None);
            };

            let conditions = sqlx::query_as::<_, RuleConditionRecord>(
                r#"
                SELECT rc.rule_id, rc.kind, rc.value
                  FROM rule_conditions AS rc
                  JOIN rules AS r ON r.id = rc.rule_id
                 WHERE r.tenant_id = $1
                   AND rc.rule_id = $2
                 ORDER BY rc.id ASC
                "#,
            )
            .bind(tenant.get())
            .bind(rule_id)
            .fetch_all(&mut *conn)
            .await?;

            let replacements = sqlx::query_as::<_, RuleReplacementRecord>(
                r#"
                SELECT rule_id, seq, pattern, replacement, enabled
                  FROM rule_replacements
                 WHERE tenant_id = $1
                   AND rule_id = $2
                 ORDER BY seq ASC
                "#,
            )
            .bind(tenant.get())
            .bind(rule_id)
            .fetch_all(&mut *conn)
            .await?;

            Ok(Some((rule, conditions, replacements)))
        })
    })
    .await
}

pub async fn list_rule_versions(
    pool: &PgPool,
    tenant: TenantId,
    rule_id: i64,
) -> Result<Vec<RuleVersionRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, RuleVersionRecord>(
                r#"
                SELECT rv.rule_id, rv.version, rv.snapshot, rv.created_at
                  FROM rule_versions AS rv
                  JOIN rules AS r
                    ON r.tenant_id = rv.tenant_id
                   AND r.id = rv.rule_id
                 WHERE rv.tenant_id = $1
                   AND rv.rule_id = $2
                 ORDER BY rv.version DESC
                "#,
            )
            .bind(tenant.get())
            .bind(rule_id)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn load_rule_version(
    pool: &PgPool,
    tenant: TenantId,
    rule_id: i64,
    version: i32,
) -> Result<Option<RuleVersionRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, RuleVersionRecord>(
                r#"
                SELECT rv.rule_id, rv.version, rv.snapshot, rv.created_at
                  FROM rule_versions AS rv
                  JOIN rules AS r
                    ON r.tenant_id = rv.tenant_id
                   AND r.id = rv.rule_id
                 WHERE rv.tenant_id = $1
                   AND rv.rule_id = $2
                   AND rv.version = $3
                "#,
            )
            .bind(tenant.get())
            .bind(rule_id)
            .bind(version)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn create_rule(
    pool: &PgPool,
    tenant: TenantId,
    input: RuleCreateInput,
) -> Result<RuleWrite, tgm_core::error::AppError> {
    validate_rule_inputs(&input.conditions, &input.replacements)?;
    let conflict_key = ConflictKey::from_create(&input);
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move { insert_rule_tx(&mut *conn, tenant, &input).await })
    })
    .await;

    match result {
        Ok(write) => Ok(write),
        Err(error) if is_rule_conflict(&error) => {
            match find_conflicting_rule(pool, tenant, &conflict_key, None).await? {
                Some(existing_id) => Err(tgm_core::error::AppError::RuleConflict { existing_id }),
                None => Err(tgm_core::error::AppError::Db(error)),
            }
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn copy_rule(
    pool: &PgPool,
    tenant: TenantId,
    source_rule_id: i64,
    target_ref: i64,
) -> Result<RuleWrite, tgm_core::error::AppError> {
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let Some(source) = sqlx::query_as::<_, CopyRuleSource>(
                r#"
                SELECT keyword_id,
                       source_ref,
                       media_type,
                       action,
                       priority,
                       stop_processing,
                       template,
                       enabled
                  FROM rules
                 WHERE tenant_id = $1 AND id = $2
                 FOR UPDATE
                "#,
            )
            .bind(tenant.get())
            .bind(source_rule_id)
            .fetch_optional(&mut *conn)
            .await?
            else {
                return Err(sqlx::Error::RowNotFound);
            };

            let target_exists = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM target_channels WHERE tenant_id = $1 AND id = $2",
            )
            .bind(tenant.get())
            .bind(target_ref)
            .fetch_optional(&mut *conn)
            .await?
            .is_some();
            if !target_exists {
                return Err(sqlx::Error::RowNotFound);
            }

            let conditions = sqlx::query_as::<_, ExistingCondition>(
                "SELECT kind, value FROM rule_conditions WHERE rule_id = $1 ORDER BY id ASC",
            )
            .bind(source_rule_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|condition| RuleConditionInput {
                kind: condition.kind,
                value: condition.value,
            })
            .collect();

            let replacements = sqlx::query_as::<_, ExistingReplacement>(
                "SELECT seq, pattern, replacement, enabled FROM rule_replacements WHERE tenant_id = $1 AND rule_id = $2 ORDER BY seq",
            )
            .bind(tenant.get())
            .bind(source_rule_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|replacement| RuleReplacementInput {
                seq: replacement.seq,
                pattern: replacement.pattern,
                replacement: replacement.replacement,
                enabled: replacement.enabled,
            })
            .collect();

            let input = RuleCreateInput {
                keyword_id: source.keyword_id,
                source_ref: source.source_ref,
                media_type: source.media_type,
                target_ref,
                action: source.action,
                priority: source.priority,
                stop_processing: source.stop_processing,
                template: source.template,
                enabled: source.enabled,
                conditions,
                replacements,
            };
            insert_rule_tx(&mut *conn, tenant, &input).await
        })
    })
    .await;

    match result {
        Ok(write) => Ok(write),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) if is_rule_conflict(&error) => {
            let source = load_rule_detail(pool, tenant, source_rule_id)
                .await?
                .ok_or(tgm_core::error::AppError::NotFound)?
                .0;
            let key = ConflictKey {
                keyword_id: source.keyword_id,
                source_ref: source.source_ref,
                media_type: source.media_type,
                target_ref,
            };
            match find_conflicting_rule(pool, tenant, &key, None).await? {
                Some(existing_id) => Err(tgm_core::error::AppError::RuleConflict { existing_id }),
                None => Err(tgm_core::error::AppError::Db(error)),
            }
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

async fn insert_rule_tx(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    input: &RuleCreateInput,
) -> Result<RuleWrite, sqlx::Error> {
    let rule_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO rules (
            tenant_id, keyword_id, source_ref, media_type, target_ref,
            action, priority, stop_processing, template, version, enabled
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 1, $10)
        RETURNING id
        "#,
    )
    .bind(tenant.get())
    .bind(input.keyword_id)
    .bind(input.source_ref)
    .bind(&input.media_type)
    .bind(input.target_ref)
    .bind(&input.action)
    .bind(input.priority)
    .bind(input.stop_processing)
    .bind(&input.template)
    .bind(input.enabled)
    .fetch_one(&mut *conn)
    .await?;

    insert_rule_children(
        conn,
        tenant,
        rule_id,
        &input.conditions,
        &input.replacements,
    )
    .await?;
    insert_rule_snapshot(conn, tenant, rule_id, 1, input).await?;

    Ok(RuleWrite {
        id: rule_id,
        version: 1,
        semantic_changed: true,
    })
}

pub async fn update_rule(
    pool: &PgPool,
    tenant: TenantId,
    rule_id: i64,
    input: RuleUpdateInput,
) -> Result<RuleWrite, tgm_core::error::AppError> {
    validate_rule_inputs(&input.conditions, &input.replacements)?;
    let conflict_key = ConflictKey::from_update(&input);
    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let Some(existing) = sqlx::query_as::<_, ExistingRule>(
                r#"
                SELECT keyword_id, source_ref, media_type, target_ref,
                       action, stop_processing, template, version
                  FROM rules
                 WHERE tenant_id = $1 AND id = $2
                 FOR UPDATE
                "#,
            )
            .bind(tenant.get())
            .bind(rule_id)
            .fetch_optional(&mut *conn)
            .await?
            else {
                return Err(sqlx::Error::RowNotFound);
            };

            let current_conditions = sqlx::query_as::<_, ExistingCondition>(
                "SELECT kind, value FROM rule_conditions WHERE rule_id = $1 ORDER BY kind, value",
            )
            .bind(rule_id)
            .fetch_all(&mut *conn)
            .await?;
            let current_replacements = sqlx::query_as::<_, ExistingReplacement>(
                "SELECT seq, pattern, replacement, enabled FROM rule_replacements WHERE tenant_id = $1 AND rule_id = $2 ORDER BY seq",
            )
            .bind(tenant.get())
            .bind(rule_id)
            .fetch_all(&mut *conn)
            .await?;

            let semantic_changed = existing.keyword_id != input.keyword_id
                || existing.source_ref != input.source_ref
                || existing.media_type != input.media_type
                || existing.target_ref != input.target_ref
                || existing.action != input.action
                || existing.stop_processing != input.stop_processing
                || existing.template != input.template
                || !conditions_equal(&current_conditions, &input.conditions)
                || !replacements_equal(&current_replacements, &input.replacements);
            let next_version = if semantic_changed {
                existing.version + 1
            } else {
                existing.version
            };

            sqlx::query(
                r#"
                UPDATE rules
                   SET keyword_id = $3,
                       source_ref = $4,
                       media_type = $5,
                       target_ref = $6,
                       action = $7,
                       priority = $8,
                       stop_processing = $9,
                       template = $10,
                       version = $11,
                       enabled = $12,
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1 AND id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(rule_id)
            .bind(input.keyword_id)
            .bind(input.source_ref)
            .bind(&input.media_type)
            .bind(input.target_ref)
            .bind(&input.action)
            .bind(input.priority)
            .bind(input.stop_processing)
            .bind(&input.template)
            .bind(next_version)
            .bind(input.enabled)
            .execute(&mut *conn)
            .await?;

            if semantic_changed {
                sqlx::query("DELETE FROM rule_conditions WHERE rule_id = $1")
                    .bind(rule_id)
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("DELETE FROM rule_replacements WHERE tenant_id = $1 AND rule_id = $2")
                    .bind(tenant.get())
                    .bind(rule_id)
                    .execute(&mut *conn)
                    .await?;
                insert_rule_children(
                    &mut *conn,
                    tenant,
                    rule_id,
                    &input.conditions,
                    &input.replacements,
                )
                .await?;
                insert_rule_snapshot(&mut *conn, tenant, rule_id, next_version, &input).await?;
            }

            Ok(RuleWrite {
                id: rule_id,
                version: next_version,
                semantic_changed,
            })
        })
    })
    .await;

    match result {
        Ok(write) => Ok(write),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) if is_rule_conflict(&error) => {
            match find_conflicting_rule(pool, tenant, &conflict_key, Some(rule_id)).await? {
                Some(existing_id) => Err(tgm_core::error::AppError::RuleConflict { existing_id }),
                None => Err(tgm_core::error::AppError::Db(error)),
            }
        }
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn copy_rule_replacements(
    pool: &PgPool,
    tenant: TenantId,
    from_rule: i64,
    to_rules: Vec<i64>,
) -> Result<Vec<RuleReplacementCopyWrite>, tgm_core::error::AppError> {
    if to_rules.is_empty() {
        return Err(tgm_core::error::AppError::ConditionInvalid(
            "to_rules 不能为空".to_owned(),
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(to_rules.len());
    if to_rules.iter().any(|rule_id| !seen.insert(*rule_id)) {
        return Err(tgm_core::error::AppError::ConditionInvalid(
            "to_rules 不得重复".to_owned(),
        ));
    }

    let result = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let mut lock_ids = to_rules.clone();
            lock_ids.push(from_rule);
            lock_ids.sort_unstable();
            lock_ids.dedup();
            let targets = sqlx::query_as::<_, ReplacementCopyTarget>(
                r#"
                SELECT id, keyword_id, source_ref, media_type, target_ref,
                       action, priority, stop_processing, template, version, enabled
                  FROM rules
                 WHERE tenant_id = $1
                   AND id = ANY($2::bigint[])
                 ORDER BY id
                 FOR UPDATE
                "#,
            )
            .bind(tenant.get())
            .bind(&lock_ids)
            .fetch_all(&mut *conn)
            .await?;
            let required_rule_count = to_rules
                .iter()
                .chain(std::iter::once(&from_rule))
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len();
            if targets.len() != required_rule_count {
                return Err(sqlx::Error::RowNotFound);
            }
            if !targets.iter().any(|target| target.id == from_rule) {
                return Err(sqlx::Error::RowNotFound);
            }

            let source_replacements = sqlx::query_as::<_, ExistingReplacement>(
                "SELECT seq, pattern, replacement, enabled FROM rule_replacements WHERE tenant_id = $1 AND rule_id = $2 ORDER BY seq",
            )
            .bind(tenant.get())
            .bind(from_rule)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|replacement| RuleReplacementInput {
                seq: replacement.seq,
                pattern: replacement.pattern,
                replacement: replacement.replacement,
                enabled: replacement.enabled,
            })
            .collect::<Vec<_>>();

            let targets_by_id: std::collections::HashMap<_, _> = targets
                .into_iter()
                .map(|target| (target.id, target))
                .collect();
            let mut writes = Vec::with_capacity(to_rules.len());
            for rule_id in to_rules {
                let target = targets_by_id
                    .get(&rule_id)
                    .ok_or(sqlx::Error::RowNotFound)?;
                let current_replacements = sqlx::query_as::<_, ExistingReplacement>(
                    "SELECT seq, pattern, replacement, enabled FROM rule_replacements WHERE tenant_id = $1 AND rule_id = $2 ORDER BY seq",
                )
                .bind(tenant.get())
                .bind(rule_id)
                .fetch_all(&mut *conn)
                .await?;
                let semantic_changed =
                    !replacements_equal(&current_replacements, &source_replacements);
                let next_version = if semantic_changed {
                    target.version + 1
                } else {
                    target.version
                };

                if semantic_changed {
                    sqlx::query(
                        "UPDATE rules SET version = $3, updated_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2",
                    )
                    .bind(tenant.get())
                    .bind(rule_id)
                    .bind(next_version)
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(
                        "DELETE FROM rule_replacements WHERE tenant_id = $1 AND rule_id = $2",
                    )
                    .bind(tenant.get())
                    .bind(rule_id)
                    .execute(&mut *conn)
                    .await?;
                    insert_rule_replacements(&mut *conn, tenant, rule_id, &source_replacements)
                        .await?;
                    let conditions = sqlx::query_as::<_, ExistingCondition>(
                        "SELECT kind, value FROM rule_conditions WHERE rule_id = $1 ORDER BY kind, value",
                    )
                    .bind(rule_id)
                    .fetch_all(&mut *conn)
                    .await?
                    .into_iter()
                    .map(|condition| RuleConditionInput {
                        kind: condition.kind,
                        value: condition.value,
                    })
                    .collect();
                    let input = RuleUpdateInput {
                        keyword_id: target.keyword_id,
                        source_ref: target.source_ref,
                        media_type: target.media_type.clone(),
                        target_ref: target.target_ref,
                        action: target.action.clone(),
                        priority: target.priority,
                        stop_processing: target.stop_processing,
                        template: target.template.clone(),
                        enabled: target.enabled,
                        conditions,
                        replacements: source_replacements.clone(),
                    };
                    insert_rule_snapshot(&mut *conn, tenant, rule_id, next_version, &input).await?;
                }
                writes.push(RuleReplacementCopyWrite {
                    rule_id,
                    version: next_version,
                    semantic_changed,
                });
            }
            Ok(writes)
        })
    })
    .await;

    match result {
        Ok(writes) => Ok(writes),
        Err(sqlx::Error::RowNotFound) => Err(tgm_core::error::AppError::NotFound),
        Err(error) => Err(tgm_core::error::AppError::Db(error)),
    }
}

pub async fn delete_rule(
    pool: &PgPool,
    tenant: TenantId,
    rule_id: i64,
) -> Result<bool, tgm_core::error::AppError> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            Ok(sqlx::query_scalar::<_, i64>(
                "DELETE FROM rules WHERE tenant_id = $1 AND id = $2 RETURNING id",
            )
            .bind(tenant.get())
            .bind(rule_id)
            .fetch_optional(&mut *conn)
            .await?
            .is_some())
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)
}

async fn insert_rule_children(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    rule_id: i64,
    conditions: &[RuleConditionInput],
    replacements: &[RuleReplacementInput],
) -> Result<(), sqlx::Error> {
    for condition in conditions {
        sqlx::query("INSERT INTO rule_conditions (rule_id, kind, value) VALUES ($1, $2, $3)")
            .bind(rule_id)
            .bind(&condition.kind)
            .bind(&condition.value)
            .execute(&mut *conn)
            .await?;
    }
    insert_rule_replacements(conn, tenant, rule_id, replacements).await?;
    Ok(())
}

async fn insert_rule_replacements(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    rule_id: i64,
    replacements: &[RuleReplacementInput],
) -> Result<(), sqlx::Error> {
    for replacement in replacements {
        sqlx::query(
            "INSERT INTO rule_replacements (tenant_id, rule_id, seq, pattern, replacement, enabled) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(tenant.get())
        .bind(rule_id)
        .bind(replacement.seq)
        .bind(&replacement.pattern)
        .bind(&replacement.replacement)
        .bind(replacement.enabled)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_rule_snapshot(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    rule_id: i64,
    version: i32,
    input: &impl RuleSnapshotInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot) VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant.get())
    .bind(rule_id)
    .bind(version)
    .bind(input.snapshot(rule_id, tenant, version))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

trait RuleSnapshotInput {
    fn snapshot(&self, rule_id: i64, tenant: TenantId, version: i32) -> Value;
}

impl RuleSnapshotInput for RuleCreateInput {
    fn snapshot(&self, rule_id: i64, tenant: TenantId, version: i32) -> Value {
        RuleSnapshot {
            rule_id,
            tenant,
            version,
            keyword_id: self.keyword_id,
            source_ref: self.source_ref,
            media_type: self.media_type.clone(),
            target_ref: self.target_ref,
            action: &self.action,
            priority: self.priority,
            stop_processing: self.stop_processing,
            template: self.template.clone(),
            enabled: self.enabled,
            conditions: &self.conditions,
            replacements: &self.replacements,
        }
        .into_value()
    }
}

impl RuleSnapshotInput for RuleUpdateInput {
    fn snapshot(&self, rule_id: i64, tenant: TenantId, version: i32) -> Value {
        RuleSnapshot {
            rule_id,
            tenant,
            version,
            keyword_id: self.keyword_id,
            source_ref: self.source_ref,
            media_type: self.media_type.clone(),
            target_ref: self.target_ref,
            action: &self.action,
            priority: self.priority,
            stop_processing: self.stop_processing,
            template: self.template.clone(),
            enabled: self.enabled,
            conditions: &self.conditions,
            replacements: &self.replacements,
        }
        .into_value()
    }
}

struct RuleSnapshot<'a> {
    rule_id: i64,
    tenant: TenantId,
    version: i32,
    keyword_id: i64,
    source_ref: Option<i64>,
    media_type: Option<String>,
    target_ref: i64,
    action: &'a str,
    priority: i32,
    stop_processing: bool,
    template: Option<String>,
    enabled: bool,
    conditions: &'a [RuleConditionInput],
    replacements: &'a [RuleReplacementInput],
}

impl RuleSnapshot<'_> {
    fn into_value(self) -> Value {
        let mut sorted_conditions: Vec<_> = self.conditions.iter().collect();
        sorted_conditions.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then(left.value.cmp(&right.value))
        });
        let mut sorted_replacements: Vec<_> = self.replacements.iter().collect();
        sorted_replacements.sort_by_key(|replacement| replacement.seq);
        json!({
            "rule": {
                "id": self.rule_id,
                "tenant_id": self.tenant.get(),
                "keyword_id": self.keyword_id,
                "source_ref": self.source_ref,
                "media_type": self.media_type,
                "target_ref": self.target_ref,
                "action": self.action,
                "priority": self.priority,
                "stop_processing": self.stop_processing,
                "template": self.template,
                "version": self.version,
                "enabled": self.enabled,
            },
            "conditions": sorted_conditions
                .into_iter()
                .map(|condition| json!({"kind": condition.kind, "value": condition.value}))
                .collect::<Vec<_>>(),
            "replacements": sorted_replacements
                .into_iter()
                .map(|replacement| json!({
                    "seq": replacement.seq,
                    "pattern": replacement.pattern,
                    "replacement": replacement.replacement,
                    "enabled": replacement.enabled,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

fn validate_rule_inputs(
    conditions: &[RuleConditionInput],
    replacements: &[RuleReplacementInput],
) -> Result<(), tgm_core::error::AppError> {
    let mut size_min = false;
    let mut size_max = false;
    let mut seqs = std::collections::HashSet::with_capacity(replacements.len());
    for condition in conditions {
        match condition.kind.as_str() {
            "size_min" if size_min => {
                return Err(tgm_core::error::AppError::ConditionInvalid(
                    "size_min 只能出现一次".to_string(),
                ));
            }
            "size_min" => size_min = true,
            "size_max" if size_max => {
                return Err(tgm_core::error::AppError::ConditionInvalid(
                    "size_max 只能出现一次".to_string(),
                ));
            }
            "size_max" => size_max = true,
            "exclude_word" | "file_name" | "sender" | "ext_allow" | "ext_deny" => {}
            other => {
                return Err(tgm_core::error::AppError::ConditionInvalid(format!(
                    "未知规则条件: {other}"
                )));
            }
        }
    }
    for replacement in replacements {
        if !seqs.insert(replacement.seq) {
            return Err(tgm_core::error::AppError::ConditionInvalid(
                "替换规则 seq 不得重复".to_string(),
            ));
        }
    }
    Ok(())
}

fn conditions_equal(current: &[ExistingCondition], incoming: &[RuleConditionInput]) -> bool {
    let mut left: Vec<_> = current
        .iter()
        .map(|condition| (&condition.kind, &condition.value))
        .collect();
    let mut right: Vec<_> = incoming
        .iter()
        .map(|condition| (&condition.kind, &condition.value))
        .collect();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

fn replacements_equal(current: &[ExistingReplacement], incoming: &[RuleReplacementInput]) -> bool {
    current.len() == incoming.len()
        && current.iter().zip(incoming).all(|(left, right)| {
            left.seq == right.seq
                && left.pattern == right.pattern
                && left.replacement == right.replacement
                && left.enabled == right.enabled
        })
}

#[derive(Debug, Clone)]
struct ConflictKey {
    keyword_id: i64,
    source_ref: Option<i64>,
    media_type: Option<String>,
    target_ref: i64,
}

impl ConflictKey {
    fn from_create(input: &RuleCreateInput) -> Self {
        Self {
            keyword_id: input.keyword_id,
            source_ref: input.source_ref,
            media_type: input.media_type.clone(),
            target_ref: input.target_ref,
        }
    }

    fn from_update(input: &RuleUpdateInput) -> Self {
        Self {
            keyword_id: input.keyword_id,
            source_ref: input.source_ref,
            media_type: input.media_type.clone(),
            target_ref: input.target_ref,
        }
    }
}

fn is_rule_conflict(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.constraint() == Some("uq_rule_conflict"))
}

async fn find_conflicting_rule(
    pool: &PgPool,
    tenant: TenantId,
    key: &ConflictKey,
    exclude_rule_id: Option<i64>,
) -> Result<Option<i64>, tgm_core::error::AppError> {
    let keyword_id = key.keyword_id;
    let source_ref = key.source_ref;
    let media_type = key.media_type.clone();
    let target_ref = key.target_ref;
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT id
                  FROM rules
                 WHERE tenant_id = $1
                   AND keyword_id = $2
                   AND COALESCE(source_ref, 0) = COALESCE($3, 0)
                   AND COALESCE(media_type, 'ANY') = COALESCE($4, 'ANY')
                   AND target_ref = $5
                   AND enabled
                   AND ($6 IS NULL OR id <> $6)
                 ORDER BY id
                 LIMIT 1
                "#,
            )
            .bind(tenant.get())
            .bind(keyword_id)
            .bind(source_ref)
            .bind(&media_type)
            .bind(target_ref)
            .bind(exclude_rule_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
    .map_err(tgm_core::error::AppError::Db)
}

pub async fn load_tenant_event(
    pool: &PgPool,
    tenant: TenantId,
    tenant_event_id: i64,
) -> Result<Option<TenantEventRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, TenantEventRecord>(
                r#"
                SELECT te.id,
                       te.canonical_event_id,
                       te.source_ref,
                       ce.receiver_identity_id,
                       ce.tg_chat_id,
                       ce.tg_message_id,
                       ce.event_type,
                       ce.sender_id,
                       ce.text,
                       ce.caption,
                       ce.file_name,
                       ce.file_size,
                       ss.title AS chat_title,
                       ss.username,
                       ce.media_type,
                       te.processed_at IS NOT NULL AS processed
                  FROM tenant_events AS te
                  JOIN canonical_events AS ce
                    ON ce.id = te.canonical_event_id
                  JOIN source_subscriptions AS ss
                    ON ss.tenant_id = te.tenant_id
                   AND ss.id = te.source_ref
                 WHERE te.tenant_id = $1
                   AND te.id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(tenant_event_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn list_active_tenants(pool: &PgPool, limit: i64) -> Result<Vec<TenantId>, sqlx::Error> {
    sqlx::query_scalar::<_, TenantId>(
        "SELECT id FROM tenants WHERE status = 'active' ORDER BY id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn list_all_active_tenants(pool: &PgPool) -> Result<Vec<TenantId>, sqlx::Error> {
    sqlx::query_scalar::<_, TenantId>("SELECT id FROM tenants WHERE status = 'active' ORDER BY id")
        .fetch_all(pool)
        .await
}

pub async fn list_unfanned_canonical_event_ids(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT event.id
           FROM canonical_events AS event
          WHERE NOT event.fanned_out
            AND (event.event_type <> 'MESSAGE_CREATED' OR event.media_group_id IS NULL
                 OR (EXISTS (
                     SELECT 1 FROM canonical_events AS completed
                      WHERE completed.receiver_identity_id = event.receiver_identity_id
                        AND completed.tg_chat_id = event.tg_chat_id
                        AND completed.media_group_id = event.media_group_id
                        AND completed.event_type = 'MEDIA_GROUP_COMPLETED')
                     AND NOT EXISTS (
                         SELECT 1 FROM media_group_members AS member
                          WHERE member.member_event_id = event.id)))
          ORDER BY event.created_at, event.id
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn aggregate_ready_media_groups(pool: &PgPool, limit: i64) -> Result<(), sqlx::Error> {
    let candidates = sqlx::query_scalar::<_, i64>(
        "SELECT min(event.id)
           FROM canonical_events AS event
          WHERE event.event_type = 'MESSAGE_CREATED'
            AND event.media_group_id IS NOT NULL
            AND NOT event.fanned_out
            AND NOT EXISTS (
                SELECT 1 FROM canonical_events AS completed
                 WHERE completed.receiver_identity_id = event.receiver_identity_id
                   AND completed.tg_chat_id = event.tg_chat_id
                   AND completed.media_group_id = event.media_group_id
                   AND completed.event_type = 'MEDIA_GROUP_COMPLETED')
          GROUP BY event.receiver_identity_id, event.tg_chat_id, event.media_group_id
         HAVING max(event.created_at) <= clock_timestamp() - interval '2 seconds'
          ORDER BY min(event.created_at), min(event.id)
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    for candidate in candidates {
        let mut tx = pool.begin().await?;
        let Some((identity, chat, group)) = sqlx::query_as::<_, (i64, i64, String)>(
            "SELECT receiver_identity_id, tg_chat_id, media_group_id
               FROM canonical_events WHERE id = $1",
        )
        .bind(candidate)
        .fetch_optional(&mut *tx)
        .await?
        else {
            continue;
        };
        crate::shared_retention::lock_writer(&mut tx, identity).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("media-group:{identity}:{chat}:{group}"))
            .execute(&mut *tx)
            .await?;
        let members = sqlx::query_as::<_, MediaGroupMemberRecord>(
            "SELECT event.id, event.tg_message_id, event.media_type, event.file_size
               FROM canonical_events AS event
              WHERE event.receiver_identity_id = $1 AND event.tg_chat_id = $2
                AND event.media_group_id = $3 AND event.event_type = 'MESSAGE_CREATED'
                AND NOT event.fanned_out
                AND NOT EXISTS (
                    SELECT 1 FROM canonical_events AS completed
                     WHERE completed.receiver_identity_id = $1 AND completed.tg_chat_id = $2
                       AND completed.media_group_id = $3
                       AND completed.event_type = 'MEDIA_GROUP_COMPLETED')
                AND NOT EXISTS (
                    SELECT 1 FROM canonical_events AS recent
                     WHERE recent.receiver_identity_id = $1 AND recent.tg_chat_id = $2
                       AND recent.media_group_id = $3 AND recent.event_type = 'MESSAGE_CREATED'
                       AND recent.created_at > clock_timestamp() - interval '2 seconds')
              ORDER BY event.tg_message_id, event.id
              FOR UPDATE OF event",
        )
        .bind(identity)
        .bind(chat)
        .bind(&group)
        .fetch_all(&mut *tx)
        .await?;
        let Some(first) = members.first() else {
            tx.commit().await?;
            continue;
        };
        let member_ids: Vec<_> = members.iter().map(|member| member.id).collect();
        let group_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO canonical_events (
                 adapter, receiver_identity_id, tg_chat_id, tg_message_id, event_type,
                 media_group_id, sender_id, message_date, text, caption, media_type, file_size)
             SELECT adapter, receiver_identity_id, tg_chat_id, tg_message_id,
                    'MEDIA_GROUP_COMPLETED', media_group_id, sender_id, message_date,
                    (SELECT text FROM canonical_events WHERE id = ANY($2) AND text <> ''
                      ORDER BY tg_message_id, id LIMIT 1),
                    (SELECT caption FROM canonical_events WHERE id = ANY($2) AND caption <> ''
                      ORDER BY tg_message_id, id LIMIT 1),
                    media_type,
                    (SELECT max(file_size) FROM canonical_events WHERE id = ANY($2))
               FROM canonical_events WHERE id = $1
             ON CONFLICT (receiver_identity_id, tg_chat_id, tg_message_id, event_type, edit_version)
             DO NOTHING RETURNING id",
        )
        .bind(first.id)
        .bind(&member_ids)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(group_id) = group_id {
            sqlx::query(
                "INSERT INTO media_group_members (group_event_id, member_event_id, position)
                 SELECT $1, member.id, (member.position - 1)::integer
                   FROM unnest($2::bigint[]) WITH ORDINALITY AS member(id, position)",
            )
            .bind(group_id)
            .bind(&member_ids)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE canonical_events SET fanned_out = TRUE WHERE id = ANY($1)")
                .bind(&member_ids)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MediaGroupMemberRecord {
    pub id: i64,
    pub tg_message_id: i64,
    pub media_type: Option<String>,
    pub file_size: Option<i64>,
}

pub async fn load_media_group_members(
    pool: &PgPool,
    tenant: TenantId,
    tenant_event_id: i64,
) -> Result<Vec<MediaGroupMemberRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, MediaGroupMemberRecord>(
                "SELECT event.id, event.tg_message_id, event.media_type, event.file_size
                   FROM tenant_events AS te
                   JOIN media_group_members AS member ON member.group_event_id = te.canonical_event_id
                   JOIN canonical_events AS event ON event.id = member.member_event_id
                  WHERE te.tenant_id = $1 AND te.id = $2 ORDER BY member.position",
            )
            .bind(tenant.get())
            .bind(tenant_event_id)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn mark_canonical_event_fanned_out(
    pool: &PgPool,
    canonical_event_id: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if let Some(identity) = sqlx::query_scalar::<_, i64>(
        "SELECT receiver_identity_id FROM canonical_events WHERE id = $1",
    )
    .bind(canonical_event_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        crate::shared_retention::lock_writer(&mut tx, identity).await?;
    }
    let result = sqlx::query(
        "UPDATE canonical_events
            SET fanned_out = TRUE
          WHERE id = $1
            AND NOT fanned_out",
    )
    .bind(canonical_event_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn list_unprocessed_tenant_event_ids(
    pool: &PgPool,
    tenant: TenantId,
    limit: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT id
                   FROM tenant_events
                  WHERE tenant_id = $1
                    AND processed_at IS NULL
                  ORDER BY created_at, id
                  LIMIT $2",
            )
            .bind(tenant.get())
            .bind(limit)
            .fetch_all(conn)
            .await
        })
    })
    .await
}

pub async fn load_enabled_rules(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<Vec<(RuleRecord, Vec<RuleConditionRecord>)>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let rules = sqlx::query_as::<_, RuleRecord>(
                r#"
                SELECT r.id,
                       r.tenant_id,
                       k.id AS keyword_id,
                       k.word AS keyword,
                       k.match_type AS keyword_match_type,
                       r.source_ref,
                       r.media_type,
                       r.target_ref,
                       tc.tg_chat_id AS target_tg_chat_id,
                       tc.sender_identity_id,
                       r.action,
                       r.priority,
                       r.stop_processing,
                       r.template,
                       r.version,
                       r.enabled
                  FROM rules AS r
                  JOIN keywords AS k
                    ON k.tenant_id = r.tenant_id
                   AND k.id = r.keyword_id
                  JOIN target_channels AS tc
                    ON tc.tenant_id = r.tenant_id
                   AND tc.id = r.target_ref
                 WHERE r.tenant_id = $1
                   AND r.enabled
                   AND k.enabled
                   AND tc.enabled
                 ORDER BY r.priority DESC, r.id ASC
                "#,
            )
            .bind(tenant.get())
            .fetch_all(&mut *conn)
            .await?;

            let conditions = sqlx::query_as::<_, RuleConditionRecord>(
                r#"
                SELECT rc.rule_id, rc.kind, rc.value
                  FROM rule_conditions AS rc
                  JOIN rules AS r
                    ON r.id = rc.rule_id
                   AND r.tenant_id = $1
                 WHERE r.enabled
                 ORDER BY rc.rule_id ASC, rc.id ASC
                "#,
            )
            .bind(tenant.get())
            .fetch_all(&mut *conn)
            .await?;

            let mut grouped = Vec::with_capacity(rules.len());
            for rule in rules {
                let rule_conditions = conditions
                    .iter()
                    .filter(|condition| condition.rule_id == rule.id)
                    .cloned()
                    .collect();
                grouped.push((rule, rule_conditions));
            }
            Ok(grouped)
        })
    })
    .await
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("tenant event is no longer available")]
    EventUnavailable,
    #[error("tenant event has already been processed")]
    EventProcessed,
    #[error("rule version is no longer available: {rule_id}/{version}")]
    VersionUnavailable { rule_id: i64, version: i32 },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub async fn enqueue_delivery_tasks(
    pool: &PgPool,
    tenant: TenantId,
    tenant_event_id: i64,
    matched_keywords: &[String],
    tasks: &[DeliveryTaskInput],
) -> Result<Vec<DeliveryTaskWrite>, EnqueueError> {
    let matched_keywords = matched_keywords.to_vec();
    let tasks = tasks.to_vec();
    crate::tenant::tenant_tx_with_error(pool, tenant, |conn| {
        Box::pin(async move {
            crate::tenant_retention::lock_rule_versions(conn, tenant).await?;
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            let identity = sqlx::query_scalar::<_, i64>(
                "SELECT event.receiver_identity_id FROM tenant_events projection
                   JOIN canonical_events event ON event.id = projection.canonical_event_id
                  WHERE projection.tenant_id = $1 AND projection.id = $2",
            )
            .bind(tenant.get())
            .bind(tenant_event_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(EnqueueError::EventUnavailable)?;
            crate::shared_retention::lock_writer(conn, identity).await?;
            let processed = sqlx::query_scalar::<_, bool>(
                "SELECT processed_at IS NOT NULL FROM tenant_events WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(tenant.get())
            .bind(tenant_event_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(EnqueueError::EventUnavailable)?;

            let mut writes = Vec::with_capacity(tasks.len());
            for task in &tasks {
                let idem_key = delivery_idem_key(
                    tenant,
                    task.rule_id,
                    task.rule_version,
                    tenant_event_id,
                    task.target_tg_chat_id,
                );
                let existing_id = sqlx::query_scalar::<_, i64>(
                    "SELECT id FROM delivery_tasks WHERE tenant_id = $1 AND idem_key = $2",
                )
                .bind(tenant.get())
                .bind(&idem_key)
                .fetch_optional(&mut *conn)
                .await?;
                if let Some(id) = existing_id {
                    writes.push(DeliveryTaskWrite { id, inserted: false });
                    continue;
                }
                if processed {
                    continue;
                }
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM rule_versions WHERE tenant_id = $1 AND rule_id = $2 AND version = $3)",
                )
                .bind(tenant.get())
                .bind(task.rule_id)
                .bind(task.rule_version)
                .fetch_one(&mut *conn)
                .await?;
                if !exists {
                    return Err(EnqueueError::VersionUnavailable {
                        rule_id: task.rule_id,
                        version: task.rule_version,
                    });
                }
                let mut write = None;
                for _ in 0..16 {
                    let telegram_random_id = delivery_random_id()?;
                    let inserted_id = sqlx::query_scalar::<_, i64>(
                        r#"
                        INSERT INTO delivery_tasks (
                            tenant_id,
                            tenant_event_id,
                            rule_id,
                            rule_version,
                            target_tg_chat_id,
                            sender_identity_id,
                            action,
                            telegram_random_id,
                            payload_snapshot,
                            idem_key,
                            status,
                            last_error_code,
                            completed_at
                        )
                        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                                CASE WHEN $11::text IS NULL THEN 'PENDING' ELSE 'DEAD' END,
                                $11,
                                CASE WHEN $11::text IS NULL THEN NULL ELSE clock_timestamp() END)
                        ON CONFLICT DO NOTHING
                        RETURNING id
                        "#,
                    )
                    .bind(tenant.get())
                    .bind(tenant_event_id)
                    .bind(task.rule_id)
                    .bind(task.rule_version)
                    .bind(task.target_tg_chat_id)
                    .bind(task.sender_identity_id)
                    .bind(&task.action)
                    .bind(telegram_random_id)
                    .bind(&task.payload_snapshot)
                    .bind(&idem_key)
                    .bind(task.dead_error_code.as_deref())
                    .fetch_optional(&mut *conn)
                    .await?;

                    if let Some(id) = inserted_id {
                        write = Some((id, true));
                        break;
                    }

                    let existing_id = sqlx::query_scalar::<_, i64>(
                        "SELECT id FROM delivery_tasks WHERE tenant_id = $1 AND idem_key = $2",
                    )
                    .bind(tenant.get())
                    .bind(&idem_key)
                    .fetch_optional(&mut *conn)
                    .await?;
                    if let Some(id) = existing_id {
                        write = Some((id, false));
                        break;
                    }
                }
                let Some((id, inserted)) = write else {
                    return Err(sqlx::Error::Protocol(
                        "could not allocate unique Telegram random_id after 16 attempts".to_owned(),
                    ).into());
                };

                writes.push(DeliveryTaskWrite { id, inserted });
            }

            if processed {
                if writes.is_empty() {
                    return Err(EnqueueError::EventProcessed);
                }
                return Ok(writes);
            }

            // 一条 canonical event 一个 job，多租户命中同一条消息只下载一次。
            // 只有 MTProto 采集的事件能由 mtproto-worker 下载；Bot 采集的没有 job，
            // delivery 查不到 job 时直接判死，不空等 30 分钟。
            if tasks.iter().any(|task| task.media_relay) {
                sqlx::query(
                    r#"
                    INSERT INTO media_relay_jobs (canonical_event_id, receiver_identity_id)
                    SELECT member_event.id, member_event.receiver_identity_id
                      FROM tenant_events AS te
                      JOIN canonical_events AS ce ON ce.id = te.canonical_event_id
                      LEFT JOIN media_group_members AS member ON member.group_event_id = ce.id
                      JOIN canonical_events AS member_event
                        ON member_event.id = COALESCE(member.member_event_id, ce.id)
                     WHERE te.tenant_id = $1
                       AND te.id = $2
                       AND ce.adapter = 'mtproto'
                    ON CONFLICT (canonical_event_id) DO NOTHING
                    "#,
                )
                .bind(tenant.get())
                .bind(tenant_event_id)
                .execute(&mut *conn)
                .await?;
            }

            sqlx::query(
                r#"
                UPDATE tenant_events
                   SET matched_keywords = $3,
                       matched = $4,
                       processed_at = clock_timestamp()
                 WHERE tenant_id = $1
                   AND id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(tenant_event_id)
            .bind(&matched_keywords)
            .bind(!tasks.is_empty())
            .execute(&mut *conn)
            .await?;

            sqlx::query(
                r#"
                UPDATE canonical_events AS ce
                   SET matched_any = ce.matched_any OR $2
                  FROM tenant_events AS te
                 WHERE te.tenant_id = $1
                   AND te.id = $3
                   AND (ce.id = te.canonical_event_id OR ce.id IN (
                       SELECT member_event_id FROM media_group_members
                        WHERE group_event_id = te.canonical_event_id))
                "#,
            )
            .bind(tenant.get())
            .bind(!tasks.is_empty())
            .bind(tenant_event_id)
            .execute(&mut *conn)
            .await?;

            Ok(writes)
        })
    })
    .await
}

pub async fn claim_due_delivery_task(
    pool: &PgPool,
    tenant: TenantId,
    sender_identity_id: IdentityId,
) -> Result<Option<DeliveryTaskRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            sqlx::query_as::<_, DeliveryTaskRecord>(
                r#"
                WITH candidate AS (
                    SELECT id
                      FROM delivery_tasks
                     WHERE tenant_id = $1
                       AND sender_identity_id = $2
                       AND tenant_event_id IS NOT NULL
                       AND telegram_random_id IS NOT NULL
                       AND payload_snapshot IS NOT NULL
                       AND status IN ('PENDING', 'FAILED')
                       AND (next_retry_at IS NULL OR next_retry_at <= clock_timestamp())
                     ORDER BY created_at ASC, id ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT 1
                )
                UPDATE delivery_tasks AS task
                   SET status = 'SENDING',
                       updated_at = clock_timestamp()
                  FROM candidate
                 WHERE task.id = candidate.id
                RETURNING task.id,
                          task.tenant_event_id,
                          task.rule_id,
                          task.rule_version,
                          task.target_tg_chat_id,
                          task.sender_identity_id,
                          task.action,
                          task.status,
                          task.attempt_count,
                          task.next_retry_at,
                          task.idem_key,
                          task.telegram_random_id
                "#,
            )
            .bind(tenant.get())
            .bind(sender_identity_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
}

/// delivery 要起 worker 的发送身份：`kind = 'bot'`、未失效或封禁、已存 Token，
/// 且被启用的目标引用或还有未完成任务（目标停用后积压的任务仍要发完或判死）。
/// 非 Bot 身份即使在历史数据里被填成发送身份也不在此列（spec/08 §1.4）。
pub async fn list_delivery_sender_candidates(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<Vec<IdentityId>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, IdentityId>(
                "SELECT i.id
                   FROM identities i
                   JOIN identity_secrets s ON s.identity_id = i.id
                  WHERE i.tenant_id = $1
                    AND i.kind = 'bot'
                    AND i.status NOT IN ('banned', 'unauthorized')
                    AND (EXISTS (SELECT 1 FROM target_channels t
                                  WHERE t.tenant_id = $1 AND t.sender_identity_id = i.id
                                    AND t.enabled)
                         OR EXISTS (SELECT 1 FROM delivery_tasks d
                                     WHERE d.tenant_id = $1 AND d.sender_identity_id = i.id
                                       AND d.status IN ('PENDING', 'FAILED', 'SENDING')))
                  ORDER BY i.id",
            )
            .bind(tenant.get())
            .fetch_all(conn)
            .await
        })
    })
    .await
}

/// Bot Token 被 Telegram 拒绝（吊销或重置）：同一事务内转 `unauthorized`、写审计、入队租户通知。
///
/// 发送身份没有租约（形态 A 单实例，ADR-0025），所以不校验 fencing。
/// 已是 `unauthorized` 时不重复写审计，返回 `false`。
pub async fn mark_bot_sender_unauthorized(
    pool: &PgPool,
    tenant: TenantId,
    identity: IdentityId,
) -> Result<bool, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let before = sqlx::query_scalar::<_, String>(
                "UPDATE identities AS i
                    SET status = 'unauthorized', updated_at = clock_timestamp()
                   FROM (SELECT id, status FROM identities
                          WHERE tenant_id = $1 AND id = $2 FOR UPDATE) AS old
                  WHERE i.id = old.id
                    AND i.kind = 'bot'
                    AND old.status <> 'unauthorized'
                 RETURNING old.status",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .fetch_optional(&mut *conn)
            .await?;
            let Some(before) = before else {
                return Ok(false);
            };
            sqlx::query(
                "INSERT INTO audit_logs
                    (tenant_id, actor_user_id, action, resource, resource_id, before, after, source)
                 VALUES ($1, NULL, 'identity.unauthorized', 'identity', $2, $3, $4, 'system')",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .bind(json!({ "status": before }))
            .bind(json!({ "status": "unauthorized" }))
            .execute(&mut *conn)
            .await?;
            enqueue_tenant_notification(
                &mut *conn,
                tenant,
                identity,
                LostAuthorization::Unauthorized,
            )
            .await?;
            Ok(true)
        })
    })
    .await
}

/// 把进程崩溃遗留的 `SENDING` 任务放回 outbox。
///
/// `updated_at` 是领取与状态变更共用的心跳点；只有超过调用方给定阈值的任务
/// 才会被回收，避免把仍在 Telegram 请求中的任务提前交给另一个 worker。
pub async fn recover_stale_delivery_tasks(
    pool: &PgPool,
    tenant: TenantId,
    sender_identity_id: IdentityId,
    stale_after_seconds: i64,
) -> Result<u64, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            let updated = sqlx::query(
                r#"
                UPDATE delivery_tasks
                   SET status = 'PENDING',
                       next_retry_at = clock_timestamp(),
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1
                   AND sender_identity_id = $2
                   AND tenant_event_id IS NOT NULL
                   AND telegram_random_id IS NOT NULL
                   AND payload_snapshot IS NOT NULL
                   AND status = 'SENDING'
                   AND updated_at <= clock_timestamp() - ($3 * interval '1 second')
                "#,
            )
            .bind(tenant.get())
            .bind(sender_identity_id)
            .bind(stale_after_seconds)
            .execute(&mut *conn)
            .await?;
            Ok(updated.rows_affected())
        })
    })
    .await
}

pub async fn load_delivery_task_payload(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
) -> Result<Option<DeliveryTaskPayloadRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, DeliveryTaskPayloadRecord>(
                r#"
                SELECT task.id,
                       task.target_tg_chat_id,
                       task.sender_identity_id,
                       task.action,
                       task.telegram_random_id,
                       task.payload_snapshot
                  FROM delivery_tasks AS task
                 WHERE task.tenant_id = $1
                   AND task.id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn list_delivery_tasks(
    pool: &PgPool,
    tenant: TenantId,
    status: Option<&str>,
    target_ref: Option<i64>,
    since: Option<DateTime<Utc>>,
    limit: i64,
    offset: i64,
) -> Result<Vec<DeliveryTaskListRecord>, sqlx::Error> {
    let status = status.map(str::to_owned);
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, DeliveryTaskListRecord>(
                r#"
                SELECT task.id,
                       task.tenant_event_id,
                       task.rule_id,
                       task.rule_version,
                       NULLIF(rv.snapshot->>'target_ref', '')::bigint AS target_ref,
                       task.target_tg_chat_id,
                       task.sender_identity_id,
                       task.action,
                       task.status,
                       task.attempt_count,
                       task.next_retry_at,
                       task.last_error_code,
                       task.telegram_message_id,
                       task.created_at,
                       task.completed_at,
                       task.updated_at
                  FROM delivery_tasks AS task
                  LEFT JOIN rule_versions AS rv
                    ON rv.tenant_id = task.tenant_id
                   AND rv.rule_id = task.rule_id
                   AND rv.version = task.rule_version
                 WHERE task.tenant_id = $1
                   AND ($2::text IS NULL OR task.status = $2)
                   AND ($3::bigint IS NULL OR NULLIF(rv.snapshot->>'target_ref', '')::bigint = $3)
                   AND ($4::timestamptz IS NULL OR task.created_at >= $4)
                 ORDER BY task.created_at DESC, task.id DESC
                 LIMIT $5 OFFSET $6
                "#,
            )
            .bind(tenant.get())
            .bind(status.as_deref())
            .bind(target_ref)
            .bind(since)
            .bind(limit)
            .bind(offset)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn load_delivery_task_detail(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
) -> Result<Option<(DeliveryTaskDetailRecord, Vec<DeliveryLogRecord>)>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let Some(task) = sqlx::query_as::<_, DeliveryTaskDetailRecord>(
                r#"
                SELECT task.id,
                       task.tenant_event_id,
                       task.rule_id,
                       task.rule_version,
                       NULLIF(rv.snapshot->>'target_ref', '')::bigint AS target_ref,
                       task.target_tg_chat_id,
                       task.sender_identity_id,
                       task.action,
                       task.status,
                       task.attempt_count,
                       task.next_retry_at,
                       task.last_error_code,
                       task.telegram_message_id,
                       task.created_at,
                       task.completed_at,
                       task.updated_at,
                       rv.snapshot AS rule_snapshot
                  FROM delivery_tasks AS task
                  LEFT JOIN rule_versions AS rv
                    ON rv.tenant_id = task.tenant_id
                   AND rv.rule_id = task.rule_id
                   AND rv.version = task.rule_version
                 WHERE task.tenant_id = $1
                   AND task.id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await?
            else {
                return Ok(None);
            };

            let logs = sqlx::query_as::<_, DeliveryLogRecord>(
                r#"
                SELECT log.id,
                       log.task_id,
                       log.attempt,
                       log.ok,
                       log.error_code,
                       log.error_msg,
                       log.latency_ms,
                       log.created_at
                  FROM delivery_logs AS log
                  JOIN delivery_tasks AS task ON task.id = log.task_id
                 WHERE task.tenant_id = $1
                   AND log.task_id = $2
                 ORDER BY log.attempt ASC, log.id ASC
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .fetch_all(&mut *conn)
            .await?;

            Ok(Some((task, logs)))
        })
    })
    .await
}

pub async fn retry_delivery_task(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
) -> Result<bool, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            let updated = sqlx::query(
                r#"
                UPDATE delivery_tasks
                   SET status = 'PENDING',
                       next_retry_at = clock_timestamp(),
                       last_error_code = NULL,
                       completed_at = NULL,
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1
                   AND id = $2
                   AND tenant_event_id IS NOT NULL
                   AND status IN ('DEAD', 'FAILED')
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .execute(&mut *conn)
            .await?;
            Ok(updated.rows_affected() == 1)
        })
    })
    .await
}

pub async fn list_audit_logs(
    pool: &PgPool,
    tenant: TenantId,
    actor_user_id: Option<i64>,
    source: Option<&str>,
    since: Option<DateTime<Utc>>,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditLogRecord>, sqlx::Error> {
    let source = source.map(str::to_owned);
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, AuditLogRecord>(
                r#"
                SELECT id,
                       actor_user_id,
                       action,
                       resource,
                       resource_id,
                       before,
                       after,
                       source,
                       ip::text AS ip,
                       created_at
                  FROM audit_logs
                 WHERE tenant_id = $1
                   AND ($2::bigint IS NULL OR actor_user_id = $2)
                   AND ($3::text IS NULL OR source = $3)
                   AND ($4::timestamptz IS NULL OR created_at >= $4)
                 ORDER BY created_at DESC, id DESC
                 LIMIT $5 OFFSET $6
                "#,
            )
            .bind(tenant.get())
            .bind(actor_user_id)
            .bind(source.as_deref())
            .bind(since)
            .bind(limit)
            .bind(offset)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn list_system_logs(
    pool: &PgPool,
    tenant: TenantId,
    level: Option<&str>,
    module: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<SystemLogRecord>, sqlx::Error> {
    let level = level.map(str::to_owned);
    let module = module.map(str::to_owned);
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, SystemLogRecord>(
                r#"
                SELECT id, level, module, event, context, created_at
                  FROM system_logs
                 WHERE tenant_id = $1
                   AND ($2::text IS NULL OR level = $2)
                   AND ($3::text IS NULL OR module = $3)
                 ORDER BY created_at DESC, id DESC
                 LIMIT $4 OFFSET $5
                "#,
            )
            .bind(tenant.get())
            .bind(level.as_deref())
            .bind(module.as_deref())
            .bind(limit)
            .bind(offset)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn complete_delivery_task(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
    telegram_message_id: i64,
    latency_ms: Option<i32>,
) -> Result<bool, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            let attempt = sqlx::query_scalar::<_, i32>(
                r#"
                UPDATE delivery_tasks
                   SET status = 'DONE',
                       attempt_count = attempt_count + 1,
                       telegram_message_id = $3,
                       completed_at = clock_timestamp(),
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1
                   AND id = $2
                   AND status = 'SENDING'
                RETURNING attempt_count
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .bind(telegram_message_id)
            .fetch_optional(&mut *conn)
            .await?;

            let Some(attempt) = attempt else {
                return Ok(false);
            };
            sqlx::query(
                r#"
                INSERT INTO delivery_logs (task_id, attempt, ok, latency_ms)
                VALUES ($1, $2, TRUE, $3)
                "#,
            )
            .bind(task_id)
            .bind(attempt)
            .bind(latency_ms)
            .execute(&mut *conn)
            .await?;
            Ok(true)
        })
    })
    .await
}

pub async fn record_delivery_failure(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
    failure: DeliveryTaskFailureInput,
) -> Result<Option<DeliveryTaskFailureWrite>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        let status = failure.status.as_str();
        Box::pin(async move {
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            let updated = sqlx::query_as::<_, DeliveryTaskFailureWrite>(
                r#"
                UPDATE delivery_tasks
                   SET status = $3,
                       attempt_count = $4,
                       next_retry_at = $5,
                       last_error_code = $6,
                       completed_at = CASE
                           WHEN $3 = 'DEAD' THEN clock_timestamp()
                           ELSE completed_at
                       END,
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1
                   AND id = $2
                   AND status = 'SENDING'
                RETURNING id, status, attempt_count, next_retry_at
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .bind(status)
            .bind(failure.attempt_count)
            .bind(failure.next_retry_at)
            .bind(&failure.error_code)
            .fetch_optional(&mut *conn)
            .await?;

            let Some(updated) = updated else {
                return Ok(None);
            };
            sqlx::query(
                r#"
                INSERT INTO delivery_logs (
                    task_id, attempt, ok, error_code, error_msg, latency_ms
                )
                VALUES ($1, $2, FALSE, $3, $4, $5)
                "#,
            )
            .bind(task_id)
            .bind(failure.attempt_count)
            .bind(&failure.error_code)
            .bind(failure.error_message)
            .bind(failure.latency_ms)
            .execute(&mut *conn)
            .await?;
            Ok(Some(updated))
        })
    })
    .await
}

/// 租户套餐（`tenants.plan`）。`tenants` 是丙类表，按主键读。
pub async fn load_tenant_plan(pool: &PgPool, tenant: TenantId) -> Result<String, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT plan FROM tenants WHERE id = $1")
        .bind(tenant.get())
        .fetch_one(pool)
        .await
}

/// 媒体中转 job 被 mtproto-worker 领到时带出的源消息信息。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MediaRelayJobRecord {
    pub canonical_event_id: i64,
    /// 已含本次领取。
    pub attempt_count: i32,
    pub tg_chat_id: i64,
    pub tg_message_id: i64,
    pub media_type: String,
    pub mime_type: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
}

/// 下载完成后写入 `media_archive` 的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaArchiveInput {
    pub content_hash: String,
    pub media_type: String,
    pub mime_type: Option<String>,
    pub file_name: Option<String>,
    pub file_size: i64,
    pub object_key: String,
}

/// 领取本身份一条到期的中转 job，转 `DOWNLOADING` 并计一次尝试。
///
/// `DOWNLOADING` 的 `next_retry_at` 是回收期限：worker 下载途中崩溃时，
/// 10 分钟后同一身份的下一任持有者能重新领到。租约不在手时返回 `None`。
pub async fn claim_media_relay_job(
    pool: &PgPool,
    lease: &LeaseGuard,
) -> Result<Option<MediaRelayJobRecord>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lease_is_held(&mut tx, lease).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    crate::shared_retention::lock_writer(&mut tx, lease.identity_id.get()).await?;
    let job = sqlx::query_as::<_, MediaRelayJobRecord>(
        r#"
        WITH candidate AS (
            SELECT canonical_event_id
              FROM media_relay_jobs
             WHERE receiver_identity_id = $1
               AND status IN ('PENDING', 'DOWNLOADING')
               AND (next_retry_at IS NULL OR next_retry_at <= clock_timestamp())
             ORDER BY created_at, canonical_event_id
             FOR UPDATE SKIP LOCKED
             LIMIT 1
        )
        UPDATE media_relay_jobs AS job
           SET status = 'DOWNLOADING',
               attempt_count = job.attempt_count + 1,
               next_retry_at = clock_timestamp() + interval '10 minutes',
               updated_at = clock_timestamp()
          FROM candidate, canonical_events AS ce
         WHERE job.canonical_event_id = candidate.canonical_event_id
           AND ce.id = job.canonical_event_id
        RETURNING job.canonical_event_id,
                  job.attempt_count,
                  ce.tg_chat_id,
                  ce.tg_message_id,
                  COALESCE(ce.media_type, 'DOCUMENT') AS media_type,
                  ce.mime_type,
                  ce.file_name,
                  ce.file_size
        "#,
    )
    .bind(lease.identity_id.get())
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(job)
}

/// 记一次下载失败：`dead` 时转 `DEAD`，否则回 `PENDING` 等 `next_retry_at`。
/// 返回 `false` 表示租约已不在手或 job 已不在 `DOWNLOADING`。
pub async fn fail_media_relay_job(
    pool: &PgPool,
    lease: &LeaseGuard,
    canonical_event_id: i64,
    dead: bool,
    next_retry_at: Option<DateTime<Utc>>,
    error_code: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lease_is_held(&mut tx, lease).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    crate::shared_retention::lock_writer(&mut tx, lease.identity_id.get()).await?;
    let updated = sqlx::query(
        r#"
        UPDATE media_relay_jobs
           SET status = CASE WHEN $2 THEN 'DEAD' ELSE 'PENDING' END,
               next_retry_at = $3,
               last_error_code = $4,
               updated_at = clock_timestamp()
         WHERE canonical_event_id = $1
           AND receiver_identity_id = $5
           AND status = 'DOWNLOADING'
        "#,
    )
    .bind(canonical_event_id)
    .bind(dead)
    .bind(next_retry_at)
    .bind(error_code)
    .bind(lease.identity_id.get())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(updated == 1)
}

/// 按 `content_hash` upsert 档案并把 job 转 `READY`，同一事务、校验租约。
///
/// 档案已有仍有效的对象时保留旧对象：返回的 `object_key` 与传入的不同，
/// 调用方要删掉自己刚写的那份。租约不在手时返回 `None`。
pub async fn complete_media_relay_job(
    pool: &PgPool,
    lease: &LeaseGuard,
    canonical_event_id: i64,
    archive: &MediaArchiveInput,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lease_is_held(&mut tx, lease).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    crate::shared_retention::lock_writer(&mut tx, lease.identity_id.get()).await?;
    let (archive_id, object_key) = sqlx::query_as::<_, (i64, String)>(
        r#"
        INSERT INTO media_archive (
            content_hash, media_type, mime_type, file_name, file_size,
            object_key, object_expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp() + interval '3 days')
        ON CONFLICT (content_hash) DO UPDATE SET
            object_key = CASE
                WHEN media_archive.object_key IS NOT NULL
                 AND media_archive.object_expires_at > clock_timestamp() + interval '1 hour'
                THEN media_archive.object_key
                ELSE EXCLUDED.object_key
            END,
            object_expires_at = CASE
                WHEN media_archive.object_key IS NOT NULL
                 AND media_archive.object_expires_at > clock_timestamp() + interval '1 hour'
                THEN media_archive.object_expires_at
                ELSE EXCLUDED.object_expires_at
            END
        RETURNING id, object_key
        "#,
    )
    .bind(&archive.content_hash)
    .bind(&archive.media_type)
    .bind(archive.mime_type.as_deref())
    .bind(archive.file_name.as_deref())
    .bind(archive.file_size)
    .bind(&archive.object_key)
    .fetch_one(&mut *tx)
    .await?;
    if !mark_ready_in(&mut tx, lease, canonical_event_id, archive_id).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some((archive_id, object_key)))
}

async fn mark_ready_in(
    conn: &mut sqlx::PgConnection,
    lease: &LeaseGuard,
    canonical_event_id: i64,
    media_archive_id: i64,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        r#"
        UPDATE media_relay_jobs
           SET status = 'READY',
               media_archive_id = $2,
               next_retry_at = NULL,
               last_error_code = NULL,
               updated_at = clock_timestamp()
         WHERE canonical_event_id = $1
           AND receiver_identity_id = $3
           AND status = 'DOWNLOADING'
        "#,
    )
    .bind(canonical_event_id)
    .bind(media_archive_id)
    .bind(lease.identity_id.get())
    .execute(conn)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// delivery 看到的中转状态。`job_status = None` 表示该事件没有 job。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MediaRelayStateRecord {
    pub job_status: Option<String>,
    pub job_error_code: Option<String>,
    pub object_key: Option<String>,
    pub media_type: Option<String>,
    pub mime_type: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
    /// 任务创建已超过 30 分钟（`MEDIA_RELAY_TIMEOUT` 的判据）。
    pub timed_out: bool,
}

/// 经本租户的 `delivery_tasks → tenant_events` 找到 canonical event，再读它的 job。
///
/// `media_relay_jobs` 与 `media_archive` 是丙类表、不开 RLS，这里是 delivery 读它们的
/// 唯一入口：任务与事件都在 `tenant_tx` 下按 `tenant_id` 过滤，别的租户的任务 id
/// 传进来得到 `None`。
pub async fn load_media_relay_for_task(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
) -> Result<Option<MediaRelayStateRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, MediaRelayStateRecord>(
                r#"
                SELECT job.status AS job_status,
                       job.last_error_code AS job_error_code,
                       archive.object_key,
                       archive.media_type,
                       archive.mime_type,
                       archive.file_name,
                       archive.file_size,
                       task.created_at < clock_timestamp() - interval '30 minutes' AS timed_out
                  FROM delivery_tasks AS task
                  JOIN tenant_events AS te
                    ON te.tenant_id = task.tenant_id
                   AND te.id = task.tenant_event_id
                  LEFT JOIN media_relay_jobs AS job
                    ON job.canonical_event_id = te.canonical_event_id
                  LEFT JOIN media_archive AS archive
                    ON archive.id = job.media_archive_id
                 WHERE task.tenant_id = $1
                   AND task.id = $2
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await
        })
    })
    .await
}

pub async fn load_media_group_relay_for_task(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
) -> Result<Vec<MediaRelayStateRecord>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, MediaRelayStateRecord>(
                "SELECT job.status AS job_status, job.last_error_code AS job_error_code,
                        archive.object_key, archive.media_type, archive.mime_type,
                        archive.file_name, archive.file_size,
                        task.created_at < clock_timestamp() - interval '30 minutes' AS timed_out
                   FROM delivery_tasks AS task
                   JOIN tenant_events AS te
                     ON te.tenant_id = task.tenant_id AND te.id = task.tenant_event_id
                   JOIN media_group_members AS member ON member.group_event_id = te.canonical_event_id
                   LEFT JOIN media_relay_jobs AS job ON job.canonical_event_id = member.member_event_id
                   LEFT JOIN media_archive AS archive ON archive.id = job.media_archive_id
                  WHERE task.tenant_id = $1 AND task.id = $2 ORDER BY member.position",
            )
            .bind(tenant.get())
            .bind(task_id)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

/// 对象已过期、等待删对象的档案（spec/04 §一 `media_archive` 行：对象存储文件 3 天）。
///
/// 只返回 `object_key`：调用方先删对象，删成功后再用 [`clear_media_archive_object`] 置空。
/// 顺序不能反 —— 先置空再删对象，删除失败就留下再也查不到的孤儿对象。
pub async fn list_expired_media_objects(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String)>(
        r#"
        SELECT id, object_key
          FROM media_archive
         WHERE object_key IS NOT NULL
           AND object_expires_at < clock_timestamp()
         ORDER BY object_expires_at, id
         LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 对象已删：把档案的 `object_key` 置空。
///
/// 条件带上 key 与过期判定：清理期间如果中转把同 hash 档案改指向了新对象
/// （`complete_media_relay_job` 的 upsert 在旧对象过期时换 key），这里不覆盖新 key。
pub async fn clear_media_archive_object(
    pool: &PgPool,
    archive_id: i64,
    object_key: &str,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        r#"
        UPDATE media_archive
           SET object_key = NULL,
               object_expires_at = NULL
         WHERE id = $1
           AND object_key = $2
           AND object_expires_at < clock_timestamp()
        "#,
    )
    .bind(archive_id)
    .bind(object_key)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// 物理删除超过保留期、且已没有对象和 job 引用的档案记录（spec/04 §一：记录 180 天）。
///
/// 仍挂着对象的不删（要先走对象清理），仍被 `media_relay_jobs` 引用的不删
/// （job 随 `canonical_events` 级联消失后再清）。单批不超过 `limit` 行（spec/04 §1.1）。
pub async fn delete_stale_media_archive(
    pool: &PgPool,
    retention_days: i32,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        r#"
        DELETE FROM media_archive
         WHERE id IN (
               SELECT archive.id
                 FROM media_archive AS archive
                WHERE archive.first_seen_at < clock_timestamp() - make_interval(days => $1)
                  AND archive.object_key IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM media_relay_jobs AS job
                       WHERE job.media_archive_id = archive.id)
                ORDER BY archive.first_seen_at, archive.id
                LIMIT $2)
        "#,
    )
    .bind(retention_days)
    .bind(limit)
    .execute(pool)
    .await?
    .rows_affected())
}

/// 媒体还没下载好：把 `SENDING` 的任务放回 `PENDING` 推后再看，不计尝试、不写投递日志。
pub async fn defer_delivery_task(
    pool: &PgPool,
    tenant: TenantId,
    task_id: i64,
    delay_seconds: i64,
) -> Result<bool, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            let updated = sqlx::query(
                r#"
                UPDATE delivery_tasks
                   SET status = 'PENDING',
                       next_retry_at = clock_timestamp() + make_interval(secs => $3),
                       updated_at = clock_timestamp()
                 WHERE tenant_id = $1
                   AND id = $2
                   AND status = 'SENDING'
                "#,
            )
            .bind(tenant.get())
            .bind(task_id)
            .bind(delay_seconds as f64)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            Ok(updated == 1)
        })
    })
    .await
}

fn delivery_idem_key(
    tenant: TenantId,
    rule_id: i64,
    rule_version: i32,
    tenant_event_id: i64,
    target_tg_chat_id: i64,
) -> String {
    let raw = format!(
        "{}|{}|{}|{}|{}",
        tenant.get(),
        rule_id,
        rule_version,
        tenant_event_id,
        target_tg_chat_id
    );
    Sha256::digest(raw.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn delivery_random_id() -> Result<i64, sqlx::Error> {
    let mut bytes = [0_u8; std::mem::size_of::<u64>()];
    getrandom::fill(&mut bytes).map_err(|error| sqlx::Error::Io(std::io::Error::other(error)))?;
    let value = i64::from_le_bytes(bytes) & i64::MAX;
    Ok(value.max(1))
}

/// 写入第一层规范化事件，并按数据库唯一键完成幂等去重。
///
/// 只把同一事件的重复写入视为幂等成功；若 `update_id` 与另一条事件冲突，
/// PostgreSQL 会保留唯一约束错误，避免把 Bot 重投异常伪装成正常重复。
pub async fn insert_canonical_event(
    pool: &PgPool,
    event: &CanonicalEventInput,
) -> Result<CanonicalEventWrite, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let written = insert_canonical_event_in(&mut tx, event).await?;
    tx.commit().await?;
    Ok(written)
}

/// 持租约写入规范化事件（spec/08 §4.3「所有写操作都带 token」）。
///
/// 租约行加 `FOR SHARE` 后再写：接管方的 `UPDATE` 要等本事务提交，提交后旧 token
/// 不再匹配。返回 `None` 表示调用方已不是持有者，必须立刻停止消费。
pub async fn insert_canonical_event_leased(
    pool: &PgPool,
    lease: &LeaseGuard,
    event: &CanonicalEventInput,
) -> Result<Option<CanonicalEventWrite>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lease_is_held(&mut tx, lease).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    let written = insert_canonical_event_in(&mut tx, event).await?;
    tx.commit().await?;
    Ok(Some(written))
}

async fn insert_canonical_event_in(
    conn: &mut sqlx::PgConnection,
    event: &CanonicalEventInput,
) -> Result<CanonicalEventWrite, sqlx::Error> {
    crate::shared_retention::lock_writer(conn, event.receiver_identity_id.get()).await?;
    if let Some(group) = &event.media_group_id {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "media-group:{}:{}:{group}",
                event.receiver_identity_id.get(),
                event.tg_chat_id
            ))
            .execute(&mut *conn)
            .await?;
    }
    let inserted_id = sqlx::query_scalar!(
        r#"
        INSERT INTO canonical_events (
            adapter,
            receiver_identity_id,
            tg_chat_id,
            tg_message_id,
            event_type,
            edit_version,
            update_id,
            media_group_id,
            sender_id,
            message_date,
            text,
            caption,
            media_type,
            content_hash,
            file_unique_id,
            file_name,
            file_size,
            mime_type,
            raw_file_id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
        ON CONFLICT (receiver_identity_id, tg_chat_id, tg_message_id, event_type, edit_version)
        DO NOTHING
        RETURNING id
        "#,
        event.adapter.as_str(),
        event.receiver_identity_id.get(),
        event.tg_chat_id,
        event.tg_message_id,
        event.event_type.as_str(),
        event.edit_version,
        event.update_id,
        event.media_group_id,
        event.sender_id,
        event.message_date,
        event.text,
        event.caption,
        event.media_type,
        event.content_hash,
        event.file_unique_id,
        event.file_name,
        event.file_size,
        event.mime_type,
        event.raw_file_id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    if let Some(id) = inserted_id {
        return Ok(CanonicalEventWrite { id, inserted: true });
    }

    let id = sqlx::query_scalar!(
        r#"
        SELECT id
          FROM canonical_events
         WHERE receiver_identity_id = $1
           AND tg_chat_id = $2
           AND tg_message_id = $3
           AND event_type = $4
           AND edit_version = $5
        "#,
        event.receiver_identity_id.get(),
        event.tg_chat_id,
        event.tg_message_id,
        event.event_type.as_str(),
        event.edit_version,
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(CanonicalEventWrite {
        id,
        inserted: false,
    })
}

/// 将一条规范化事件投影到一个租户的事件层。
///
/// 只有该租户启用、且同时匹配聊天与接收身份的订阅才会生成投影；
/// `(tenant_id, canonical_event_id)` 唯一键保证重试不会重复创建租户事件。
pub async fn fanout_canonical_event(
    pool: &PgPool,
    tenant: TenantId,
    canonical_event_id: i64,
) -> Result<Option<TenantEventWrite>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            crate::tenant_retention::lock_event_lifecycle_shared(conn, tenant).await?;
            if let Some(identity) = sqlx::query_scalar::<_, i64>(
                "SELECT receiver_identity_id FROM canonical_events WHERE id = $1",
            )
            .bind(canonical_event_id)
            .fetch_optional(&mut *conn)
            .await?
            {
                crate::shared_retention::lock_writer(conn, identity).await?;
            }
            let inserted_id = sqlx::query_scalar!(
                r#"
                INSERT INTO tenant_events (tenant_id, canonical_event_id, source_ref)
                SELECT $1, ce.id, ss.id
                  FROM canonical_events AS ce
                  JOIN source_subscriptions AS ss
                    ON ss.tenant_id = $1
                   AND ss.tg_chat_id = ce.tg_chat_id
                   AND ss.receiver_identity_id = ce.receiver_identity_id
                   AND ss.enabled
                 WHERE ce.id = $2
                   AND NOT ce.fanned_out
                ON CONFLICT (tenant_id, canonical_event_id) DO NOTHING
                RETURNING id
                "#,
                tenant.get(),
                canonical_event_id,
            )
            .fetch_optional(&mut *conn)
            .await?;

            if let Some(id) = inserted_id {
                return Ok(Some(TenantEventWrite { id, inserted: true }));
            }

            let existing_id = sqlx::query_scalar!(
                r#"
                SELECT id
                  FROM tenant_events
                 WHERE tenant_id = $1
                   AND canonical_event_id = $2
                "#,
                tenant.get(),
                canonical_event_id,
            )
            .fetch_optional(&mut *conn)
            .await?;

            Ok(existing_id.map(|id| TenantEventWrite {
                id,
                inserted: false,
            }))
        })
    })
    .await
}

/// 一次租约持有：写入前用它校验调用方仍是持有者。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseGuard {
    pub identity_id: IdentityId,
    pub worker_id: String,
    pub fencing_token: i64,
}

/// 在当前事务里锁住并校验租约。`FOR SHARE` 让并发的接管或续租等本事务结束。
pub async fn lease_is_held(
    conn: &mut sqlx::PgConnection,
    lease: &LeaseGuard,
) -> Result<bool, sqlx::Error> {
    let held = sqlx::query_scalar::<_, i32>(
        "SELECT 1
           FROM account_leases
          WHERE identity_id = $1
            AND worker_id = $2
            AND fencing_token = $3
            AND expires_at > clock_timestamp()
          FOR SHARE",
    )
    .bind(lease.identity_id.get())
    .bind(&lease.worker_id)
    .bind(lease.fencing_token)
    .fetch_optional(conn)
    .await?;
    Ok(held.is_some())
}

/// mtproto-worker 可接管的身份：租户自有、未失效或封禁、已有 session 与 `api_id`。
///
/// 平台演示身份（`tenant_id IS NULL`）不在此列：它的凭据不属于任何租户，
/// 现有凭据读取路径按租户连接，接不上。
pub async fn list_mtproto_worker_candidates(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<Vec<(IdentityId, i32)>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, (IdentityId, i32)>(
                "SELECT i.id, s.api_id
                   FROM identities i
                   JOIN identity_secrets s ON s.identity_id = i.id
                  WHERE i.tenant_id = $1
                    AND i.kind = 'mtproto'
                    AND i.status NOT IN ('banned', 'unauthorized')
                    AND s.api_id IS NOT NULL
                  ORDER BY i.id",
            )
            .bind(tenant.get())
            .fetch_all(conn)
            .await
        })
    })
    .await
}

/// 持租约改写 MTProto 身份的运行状态（`online` / `offline` / `flood_wait`）。
///
/// 已转入 `banned` / `unauthorized` 的身份不被改回，那两个状态只能由重新授权退出
/// （spec/08 §五）。返回 `false` 表示租约已丢失或身份已不可改。
pub async fn set_leased_identity_status(
    pool: &PgPool,
    tenant: TenantId,
    lease: &LeaseGuard,
    status: &'static str,
) -> Result<bool, sqlx::Error> {
    let lease = lease.clone();
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            if !lease_is_held(&mut *conn, &lease).await? {
                return Ok(false);
            }
            let updated = sqlx::query(
                "UPDATE identities
                    SET status = $3, updated_at = clock_timestamp()
                  WHERE tenant_id = $1
                    AND id = $2
                    AND kind = 'mtproto'
                    AND status NOT IN ('banned', 'unauthorized')",
            )
            .bind(tenant.get())
            .bind(lease.identity_id.get())
            .bind(status)
            .execute(&mut *conn)
            .await?;
            Ok(updated.rows_affected() == 1)
        })
    })
    .await
}

/// 身份失去授权的两种终态（spec/08 §五）。两者都不自动重试；
/// 区别只在通知文案：`unauthorized` 让用户重新扫码，`banned` 无法自助恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostAuthorization {
    Unauthorized,
    Banned,
}

impl LostAuthorization {
    pub fn status(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Banned => "banned",
        }
    }

    /// 审计 action 与 `tenant_notifications.kind` 共用同一个值。
    pub fn action(self) -> &'static str {
        match self {
            Self::Unauthorized => "identity.unauthorized",
            Self::Banned => "identity.banned",
        }
    }
}

/// 入队一条租户通知（`tenant_notifications`）。只在转状态的同一事务里调用：
/// 转状态成功而通知没入队，就是 spec/08 §五 第 2 条静默丢失。
async fn enqueue_tenant_notification(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    identity: IdentityId,
    lost: LostAuthorization,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tenant_notifications (tenant_id, kind, identity_id) VALUES ($1, $2, $3)",
    )
    .bind(tenant.get())
    .bind(lost.action())
    .bind(identity.get())
    .execute(conn)
    .await?;
    Ok(())
}

/// Telegram 判定授权失效：同一事务内转 `unauthorized`、释放租约、写审计、入队通知（spec/08 §五）。
pub async fn mark_leased_identity_unauthorized(
    pool: &PgPool,
    tenant: TenantId,
    lease: &LeaseGuard,
) -> Result<bool, sqlx::Error> {
    mark_leased_identity_lost(pool, tenant, lease, LostAuthorization::Unauthorized).await
}

/// 持租约把 MTProto 身份转入 `unauthorized` 或 `banned`（spec/08 §五 三件事）。
///
/// 一个事务完成：转状态、释放租约、写审计、入队租户通知。释放只把 `expires_at`
/// 置为当前时间，保留行与 token。返回 `false` 表示租约已丢失，状态由新持有者处理。
pub async fn mark_leased_identity_lost(
    pool: &PgPool,
    tenant: TenantId,
    lease: &LeaseGuard,
    lost: LostAuthorization,
) -> Result<bool, sqlx::Error> {
    let lease = lease.clone();
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            if !lease_is_held(&mut *conn, &lease).await? {
                return Ok(false);
            }
            let before = sqlx::query_scalar::<_, String>(
                "UPDATE identities AS i
                    SET status = $3, updated_at = clock_timestamp()
                   FROM (SELECT id, status FROM identities
                          WHERE tenant_id = $1 AND id = $2 FOR UPDATE) AS old
                  WHERE i.id = old.id
                    AND i.kind = 'mtproto'
                 RETURNING old.status",
            )
            .bind(tenant.get())
            .bind(lease.identity_id.get())
            .bind(lost.status())
            .fetch_optional(&mut *conn)
            .await?;
            let Some(before) = before else {
                return Ok(false);
            };
            sqlx::query(
                "UPDATE account_leases
                    SET expires_at = clock_timestamp()
                  WHERE identity_id = $1 AND worker_id = $2 AND fencing_token = $3",
            )
            .bind(lease.identity_id.get())
            .bind(&lease.worker_id)
            .bind(lease.fencing_token)
            .execute(&mut *conn)
            .await?;
            sqlx::query(
                "INSERT INTO audit_logs
                    (tenant_id, actor_user_id, action, resource, resource_id, before, after, source)
                 VALUES ($1, NULL, $5, 'identity', $2, $3, $4, 'system')",
            )
            .bind(tenant.get())
            .bind(lease.identity_id.get())
            .bind(json!({ "status": before }))
            .bind(json!({ "status": lost.status() }))
            .bind(lost.action())
            .execute(&mut *conn)
            .await?;
            enqueue_tenant_notification(&mut *conn, tenant, lease.identity_id, lost).await?;
            Ok(true)
        })
    })
    .await
}

/// 一条待发的租户通知，连同收件人（`tenants.owner_user_id`）。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DueTenantNotification {
    pub id: i64,
    pub kind: String,
    pub identity_id: IdentityId,
    pub owner_user_id: i64,
    pub attempt_count: i32,
}

/// 取本租户到期的 PENDING 通知，按入队顺序。
pub async fn list_due_tenant_notifications(
    pool: &PgPool,
    tenant: TenantId,
    limit: i64,
) -> Result<Vec<DueTenantNotification>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, DueTenantNotification>(
                "SELECT n.id, n.kind, n.identity_id, t.owner_user_id, n.attempt_count
                   FROM tenant_notifications n
                   JOIN tenants t ON t.id = n.tenant_id
                  WHERE n.tenant_id = $1
                    AND n.status = 'PENDING'
                    AND n.next_retry_at <= clock_timestamp()
                  ORDER BY n.id
                  LIMIT $2",
            )
            .bind(tenant.get())
            .bind(limit)
            .fetch_all(&mut *conn)
            .await
        })
    })
    .await
}

/// 通知一次发送尝试的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantNotificationOutcome {
    Sent,
    /// 可重试：`attempt_count` 加一，`retry_after` 之后再发。
    Retry {
        error_code: String,
        retry_after_seconds: i64,
    },
    /// 不可恢复（收件人从未私聊 Bot、已拉黑、重试用尽）：不再发送。
    Dead {
        error_code: String,
    },
}

/// 记录一次发送结局。只推进仍是 PENDING 的行，返回是否推进了：
/// 并发的两个发送循环里只有一个能把同一条通知置 SENT。
pub async fn record_tenant_notification(
    pool: &PgPool,
    tenant: TenantId,
    id: i64,
    outcome: TenantNotificationOutcome,
) -> Result<bool, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            if sqlx::query_scalar::<_, i64>(
                "SELECT id FROM tenant_notifications WHERE tenant_id = $1 AND id = $2 AND status = 'PENDING' FOR UPDATE",
            )
            .bind(tenant.get())
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
            .is_none()
            {
                return Ok(false);
            }
            let updated = match outcome {
                TenantNotificationOutcome::Sent => {
                    sqlx::query(
                        "UPDATE tenant_notifications
                            SET status = 'SENT', sent_at = statement_timestamp(),
                                terminal_at = statement_timestamp(),
                                attempt_count = attempt_count + 1, last_error_code = NULL
                          WHERE tenant_id = $1 AND id = $2 AND status = 'PENDING'",
                    )
                    .bind(tenant.get())
                    .bind(id)
                    .execute(&mut *conn)
                    .await?
                }
                TenantNotificationOutcome::Retry {
                    error_code,
                    retry_after_seconds,
                } => {
                    sqlx::query(
                        "UPDATE tenant_notifications
                            SET attempt_count = attempt_count + 1, last_error_code = $3,
                                next_retry_at = clock_timestamp() + make_interval(secs => $4)
                          WHERE tenant_id = $1 AND id = $2 AND status = 'PENDING'",
                    )
                    .bind(tenant.get())
                    .bind(id)
                    .bind(error_code)
                    .bind(retry_after_seconds as f64)
                    .execute(&mut *conn)
                    .await?
                }
                TenantNotificationOutcome::Dead { error_code } => {
                    sqlx::query(
                        "UPDATE tenant_notifications
                            SET status = 'DEAD', terminal_at = statement_timestamp(),
                                attempt_count = attempt_count + 1,
                                last_error_code = $3
                          WHERE tenant_id = $1 AND id = $2 AND status = 'PENDING'",
                    )
                    .bind(tenant.get())
                    .bind(id)
                    .bind(error_code)
                    .execute(&mut *conn)
                    .await?
                }
            };
            Ok(updated.rows_affected() == 1)
        })
    })
    .await
}

/// 获取身份租约。
///
/// 首次获取从 token 1 开始；有效租约由同一 worker 再次获取时只刷新时间，
/// 不改变 token；过期租约由任意 worker 接管时递增 token。有效租约由其他
/// worker 持有时返回 `None`，不等待也不报错。
pub async fn acquire_lease(
    pool: &PgPool,
    identity_id: IdentityId,
    worker_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO account_leases
            (identity_id, worker_id, fencing_token, heartbeat_at, expires_at)
        VALUES ($1, $2, 1, clock_timestamp(), clock_timestamp() + interval '30 seconds')
        ON CONFLICT (identity_id) DO NOTHING
        RETURNING fencing_token
        "#,
    )
    .bind(identity_id.get())
    .bind(worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(token) = inserted {
        tx.commit().await?;
        return Ok(Some(token));
    }

    let existing = sqlx::query_as::<_, (String, i64, DateTime<Utc>)>(
        "SELECT worker_id, fencing_token, expires_at
           FROM account_leases
          WHERE identity_id = $1
          FOR UPDATE",
    )
    .bind(identity_id.get())
    .fetch_one(&mut *tx)
    .await?;
    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let can_acquire = existing.2 < now || existing.0 == worker_id;
    let result = if can_acquire {
        let next_token = if existing.2 < now {
            existing.1 + 1
        } else {
            existing.1
        };
        sqlx::query_scalar::<_, i64>(
            r#"
            UPDATE account_leases
               SET worker_id = $2,
                   fencing_token = $3,
                   heartbeat_at = $4,
                   expires_at = $4 + interval '30 seconds'
             WHERE identity_id = $1
            RETURNING fencing_token
            "#,
        )
        .bind(identity_id.get())
        .bind(worker_id)
        .bind(next_token)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?
    } else {
        existing.1
    };
    tx.commit().await?;
    Ok(can_acquire.then_some(result))
}

/// 续租身份。
///
/// 续租必须同时匹配身份、worker 和 fencing token，且旧租约已经过期时不允许
/// 复活。返回 `false` 表示当前 worker 已失去租约，调用方不得重试续租。
pub async fn renew_lease(
    pool: &PgPool,
    identity_id: IdentityId,
    worker_id: &str,
    fencing_token: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let existing = sqlx::query_as::<_, (String, i64, DateTime<Utc>)>(
        "SELECT worker_id, fencing_token, expires_at
           FROM account_leases
          WHERE identity_id = $1
          FOR UPDATE",
    )
    .bind(identity_id.get())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(existing) = existing else {
        tx.commit().await?;
        return Ok(false);
    };
    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let valid = existing.0 == worker_id && existing.1 == fencing_token && existing.2 > now;
    if valid {
        sqlx::query(
            "UPDATE account_leases
                SET heartbeat_at = $2,
                    expires_at = $2 + interval '30 seconds'
              WHERE identity_id = $1",
        )
        .bind(identity_id.get())
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(valid)
}

/// 主动释放身份租约。
///
/// 不删除行，而是立即过期并保留 fencing token。这样下一次接管仍会递增 token，
/// 不会因为删除后重新插入而让旧 worker 的 token 再次有效。
pub async fn release_lease(
    pool: &PgPool,
    identity_id: IdentityId,
    worker_id: &str,
    fencing_token: i64,
) -> Result<bool, sqlx::Error> {
    let released = sqlx::query!(
        r#"
        UPDATE account_leases
           SET expires_at = clock_timestamp()
         WHERE identity_id = $1
           AND worker_id = $2
           AND fencing_token = $3
        RETURNING fencing_token
        "#,
        identity_id.get(),
        worker_id,
        fencing_token
    )
    .fetch_optional(pool)
    .await?;

    Ok(released.is_some())
}
