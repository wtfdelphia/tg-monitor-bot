//! Bot Gateway 的无网络安全边界。
//!
//! Webhook 请求和 callback_data 都来自 Telegram 之外的可控输入：前者要先过
//! secret_token，后者只允许携带资源引用，租户范围必须从已认证上下文与数据库
//! 资源归属二次核对，不能从 callback_data 推导。

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Json, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use sqlx::PgPool;
use sqlx::types::chrono::{TimeZone, Utc};
use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
use tgm_core::error::AppError;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, insert_canonical_event,
};
use tgm_db::tenant::tenant_tx;

pub const TELEGRAM_SECRET_TOKEN_HEADER: &str = "x-telegram-bot-api-secret-token";

#[derive(Clone)]
pub struct BotGatewayState {
    pub pool: PgPool,
    pub auth_pool: PgPool,
    pub secret_token: String,
    pub receiver_identity_id: IdentityId,
}

pub fn router(state: BotGatewayState) -> Router {
    Router::new()
        .route("/webhook", post(webhook_ingest))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_webhook,
        ))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

pub async fn authenticate_webhook(
    State(state): State<BotGatewayState>,
    headers: HeaderMap,
    request: axum::http::Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    let provided = headers
        .get(TELEGRAM_SECRET_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if verify_webhook_secret(&state.secret_token, provided).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn webhook_ingest(
    State(state): State<BotGatewayState>,
    Json(update): Json<TelegramUpdate>,
) -> StatusCode {
    let Some((message, event_type)) = update.message() else {
        return StatusCode::NO_CONTENT;
    };

    match update_already_ingested(&state.pool, state.receiver_identity_id, update.update_id).await {
        Ok(true) => return StatusCode::NO_CONTENT,
        Ok(false) => {}
        Err(error) => {
            tracing::error!(error = %error, update_id = update.update_id, "读取 Telegram 幂等键失败");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }

    let message_date = match Utc.timestamp_opt(message.date, 0).single() {
        Some(value) => value,
        None => return StatusCode::BAD_REQUEST,
    };
    let edit_version =
        match next_edit_version(&state.pool, state.receiver_identity_id, message, event_type).await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(error = %error, "读取 Telegram 编辑版本失败");
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        };
    let media = message.media();
    let event = CanonicalEventInput {
        adapter: CanonicalAdapter::Bot,
        receiver_identity_id: state.receiver_identity_id,
        tg_chat_id: message.chat.id,
        tg_message_id: message.message_id,
        event_type,
        edit_version,
        update_id: Some(update.update_id),
        media_group_id: message.media_group_id.clone(),
        sender_id: message.sender_id(),
        message_date,
        text: message.text.clone(),
        caption: message.caption.clone(),
        media_type: media.media_type,
        content_hash: None,
        file_unique_id: media.file_unique_id,
        file_name: media.file_name,
        file_size: media.file_size,
        mime_type: media.mime_type,
        raw_file_id: media.raw_file_id,
    };

    match insert_canonical_event(&state.pool, &event).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(error) if is_constraint(&error, "uq_canonical_update") => {
            match same_webhook_event_exists(&state.pool, &event).await {
                Ok(true) => StatusCode::NO_CONTENT,
                Ok(false) => {
                    tracing::error!(
                        error = %error,
                        update_id = update.update_id,
                        "Telegram update_id 与另一条消息冲突"
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                }
                Err(check_error) => {
                    tracing::error!(
                        error = %error,
                        check_error = %check_error,
                        update_id = update.update_id,
                        "核对 Telegram 幂等冲突失败"
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }
        }
        Err(error) => {
            tracing::error!(error = %error, update_id = update.update_id, "写入 Telegram 规范化事件失败");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn same_webhook_event_exists(
    pool: &PgPool,
    event: &CanonicalEventInput,
) -> Result<bool, sqlx::Error> {
    let existing = sqlx::query_as::<_, (i64, i64, String, i32)>(
        "SELECT tg_chat_id, tg_message_id, event_type, edit_version
           FROM canonical_events
          WHERE receiver_identity_id = $1
            AND update_id = $2",
    )
    .bind(event.receiver_identity_id)
    .bind(event.update_id)
    .fetch_optional(pool)
    .await?;

    Ok(
        existing.is_some_and(|(chat_id, message_id, event_type, edit_version)| {
            chat_id == event.tg_chat_id
                && message_id == event.tg_message_id
                && event_type == canonical_event_type_name(event.event_type)
                && edit_version == event.edit_version
        }),
    )
}

fn canonical_event_type_name(event_type: CanonicalEventType) -> &'static str {
    match event_type {
        CanonicalEventType::MessageCreated => "MESSAGE_CREATED",
        CanonicalEventType::MessageEdited => "MESSAGE_EDITED",
        CanonicalEventType::MessageDeleted => "MESSAGE_DELETED",
        CanonicalEventType::MediaGroupCompleted => "MEDIA_GROUP_COMPLETED",
        CanonicalEventType::SourcePermissionChanged => "SOURCE_PERMISSION_CHANGED",
    }
}

fn is_constraint(error: &sqlx::Error, constraint: &str) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.constraint() == Some(constraint))
}

async fn update_already_ingested(
    pool: &PgPool,
    receiver_identity_id: IdentityId,
    update_id: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
              FROM canonical_events
             WHERE receiver_identity_id = $1
               AND update_id = $2
        )",
    )
    .bind(receiver_identity_id)
    .bind(update_id)
    .fetch_one(pool)
    .await
}

async fn next_edit_version(
    pool: &PgPool,
    receiver_identity_id: IdentityId,
    message: &TelegramMessage,
    event_type: CanonicalEventType,
) -> Result<i32, sqlx::Error> {
    if event_type == CanonicalEventType::MessageCreated {
        return Ok(0);
    }
    sqlx::query_scalar::<_, i32>(
        "SELECT COALESCE(MAX(edit_version), -1) + 1
           FROM canonical_events
          WHERE receiver_identity_id = $1
            AND tg_chat_id = $2
            AND tg_message_id = $3",
    )
    .bind(receiver_identity_id)
    .bind(message.chat.id)
    .bind(message.message_id)
    .fetch_one(pool)
    .await
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    channel_post: Option<TelegramMessage>,
    edited_channel_post: Option<TelegramMessage>,
}

impl TelegramUpdate {
    fn message(&self) -> Option<(&TelegramMessage, CanonicalEventType)> {
        if let Some(message) = self.channel_post.as_ref() {
            return Some((message, CanonicalEventType::MessageCreated));
        }
        self.edited_channel_post
            .as_ref()
            .map(|message| (message, CanonicalEventType::MessageEdited))
    }
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    date: i64,
    chat: TelegramChat,
    from: Option<TelegramUser>,
    sender_chat: Option<TelegramChat>,
    text: Option<String>,
    caption: Option<String>,
    media_group_id: Option<String>,
    photo: Option<Vec<TelegramFile>>,
    video: Option<TelegramFile>,
    audio: Option<TelegramFile>,
    document: Option<TelegramFile>,
    animation: Option<TelegramFile>,
    voice: Option<TelegramFile>,
    video_note: Option<TelegramFile>,
    sticker: Option<TelegramFile>,
}

impl TelegramMessage {
    fn sender_id(&self) -> Option<i64> {
        self.sender_chat
            .as_ref()
            .map(|chat| chat.id)
            .or_else(|| self.from.as_ref().map(|user| user.id))
    }

    fn media(&self) -> TelegramMedia {
        if let Some(photo) = self.photo.as_ref().and_then(|items| items.last()) {
            return photo.media("PHOTO");
        }
        for (file, media_type) in [
            (&self.video, "VIDEO"),
            (&self.audio, "AUDIO"),
            (&self.document, "DOCUMENT"),
            (&self.animation, "ANIMATION"),
            (&self.voice, "VOICE"),
            (&self.video_note, "VIDEO_NOTE"),
            (&self.sticker, "STICKER"),
        ] {
            if let Some(file) = file {
                return file.media(media_type);
            }
        }
        TelegramMedia::default()
    }
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    file_id: String,
    file_unique_id: String,
    file_size: Option<i64>,
    file_name: Option<String>,
    mime_type: Option<String>,
}

impl TelegramFile {
    fn media(&self, media_type: &str) -> TelegramMedia {
        TelegramMedia {
            media_type: Some(media_type.to_owned()),
            file_unique_id: Some(self.file_unique_id.clone()),
            file_name: self.file_name.clone(),
            file_size: self.file_size,
            mime_type: self.mime_type.clone(),
            raw_file_id: Some(self.file_id.clone()),
        }
    }
}

#[derive(Debug, Default)]
struct TelegramMedia {
    media_type: Option<String>,
    file_unique_id: Option<String>,
    file_name: Option<String>,
    file_size: Option<i64>,
    mime_type: Option<String>,
    raw_file_id: Option<String>,
}

pub async fn serve(
    pool: PgPool,
    auth_pool: PgPool,
    secret_token: String,
    receiver_identity_id: IdentityId,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let app = router(BotGatewayState {
        pool,
        auth_pool,
        secret_token,
        receiver_identity_id,
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

pub async fn list_member_contexts(
    pool: &PgPool,
    telegram_user_id: i64,
) -> Result<Vec<AuthContext>, AppError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.auth_user_id', $1, true)")
        .bind(telegram_user_id.to_string())
        .execute(&mut *tx)
        .await?;
    let records = sqlx::query_as::<_, (TenantId, String)>(
        "SELECT tenant_id, role
           FROM tenant_members
          WHERE user_id = $1
          ORDER BY tenant_id",
    )
    .bind(telegram_user_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    if records.is_empty() {
        return Err(AppError::NotInvited);
    }
    records
        .into_iter()
        .map(|(tenant_id, role)| {
            let role = TenantRole::parse(&role).ok_or(AppError::Unauthenticated)?;
            Ok(AuthContext::new(
                tenant_id,
                telegram_user_id,
                role,
                AuthSource::Bot,
            ))
        })
        .collect()
}

pub async fn authorize_callback(
    pool: &PgPool,
    auth: AuthContext,
    callback_data: &str,
) -> Result<CallbackReference, AppError> {
    let reference = parse_callback_reference(callback_data).map_err(|_| AppError::NotFound)?;

    let tenant = auth.tenant_id();
    let resource_exists = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let exists = match reference.resource {
                CallbackResource::Identity => {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(
                           SELECT 1 FROM identities
                            WHERE tenant_id = $1 AND id = $2
                         )",
                    )
                    .bind(tenant.get())
                    .bind(reference.resource_id)
                    .fetch_one(conn)
                    .await?
                }
                CallbackResource::Keyword => {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(
                           SELECT 1 FROM keywords
                            WHERE tenant_id = $1 AND id = $2
                         )",
                    )
                    .bind(tenant.get())
                    .bind(reference.resource_id)
                    .fetch_one(conn)
                    .await?
                }
                CallbackResource::Rule => {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(
                           SELECT 1 FROM rules
                            WHERE tenant_id = $1 AND id = $2
                         )",
                    )
                    .bind(tenant.get())
                    .bind(reference.resource_id)
                    .fetch_one(conn)
                    .await?
                }
                CallbackResource::Source => {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(
                           SELECT 1 FROM source_subscriptions
                            WHERE tenant_id = $1 AND id = $2
                         )",
                    )
                    .bind(tenant.get())
                    .bind(reference.resource_id)
                    .fetch_one(conn)
                    .await?
                }
                CallbackResource::Target => {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(
                           SELECT 1 FROM target_channels
                            WHERE tenant_id = $1 AND id = $2
                         )",
                    )
                    .bind(tenant.get())
                    .bind(reference.resource_id)
                    .fetch_one(conn)
                    .await?
                }
                CallbackResource::Task => {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(
                           SELECT 1 FROM delivery_tasks
                            WHERE tenant_id = $1 AND id = $2
                         )",
                    )
                    .bind(tenant.get())
                    .bind(reference.resource_id)
                    .fetch_one(conn)
                    .await?
                }
            };
            Ok(exists)
        })
    })
    .await?;
    if !resource_exists {
        return Err(AppError::NotFound);
    }
    Ok(reference)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookAuthError {
    InvalidExpectedSecret,
    MissingSecret,
    InvalidProvidedSecret,
    SecretMismatch,
}

pub fn verify_webhook_secret(
    expected: &str,
    provided: Option<&str>,
) -> Result<(), WebhookAuthError> {
    if !is_valid_secret_token(expected) {
        return Err(WebhookAuthError::InvalidExpectedSecret);
    }

    let provided = provided.ok_or(WebhookAuthError::MissingSecret)?;
    if !is_valid_secret_token(provided) {
        return Err(WebhookAuthError::InvalidProvidedSecret);
    }

    if constant_time_eq(expected.as_bytes(), provided.as_bytes()) {
        Ok(())
    } else {
        Err(WebhookAuthError::SecretMismatch)
    }
}

fn is_valid_secret_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = (left.len() ^ right.len()) as u8;
    let width = left.len().max(right.len());
    for index in 0..width {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= left_byte ^ right_byte;
    }
    difference == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackResource {
    Identity,
    Keyword,
    Rule,
    Source,
    Target,
    Task,
}

impl CallbackResource {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "identity" => Some(Self::Identity),
            "keyword" => Some(Self::Keyword),
            "rule" => Some(Self::Rule),
            "source" => Some(Self::Source),
            "target" => Some(Self::Target),
            "task" => Some(Self::Task),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackReference {
    pub resource: CallbackResource,
    pub resource_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackReferenceError {
    InvalidShape,
    UnknownResource,
    InvalidResourceId,
    TenantSelectorNotAllowed,
}

pub fn parse_callback_reference(
    callback_data: &str,
) -> Result<CallbackReference, CallbackReferenceError> {
    if callback_data.contains("tenant") || callback_data.contains('=') {
        return Err(CallbackReferenceError::TenantSelectorNotAllowed);
    }

    let mut parts = callback_data.split(':');
    let resource = parts.next().ok_or(CallbackReferenceError::InvalidShape)?;
    let resource_id = parts.next().ok_or(CallbackReferenceError::InvalidShape)?;
    if parts.next().is_some() {
        return Err(CallbackReferenceError::InvalidShape);
    }

    let resource =
        CallbackResource::parse(resource).ok_or(CallbackReferenceError::UnknownResource)?;
    let resource_id = resource_id
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(CallbackReferenceError::InvalidResourceId)?;

    Ok(CallbackReference {
        resource,
        resource_id,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        CallbackReferenceError, WebhookAuthError, parse_callback_reference, verify_webhook_secret,
    };

    #[test]
    fn webhook_requires_a_valid_secret_header() {
        assert_eq!(
            verify_webhook_secret("gateway-secret", None),
            Err(WebhookAuthError::MissingSecret)
        );
        assert_eq!(
            verify_webhook_secret("gateway-secret", Some("wrong-secret")),
            Err(WebhookAuthError::SecretMismatch)
        );
        assert_eq!(
            verify_webhook_secret("gateway-secret", Some("gateway-secret")),
            Ok(())
        );
    }

    #[test]
    fn webhook_rejects_invalid_secret_token_shapes() {
        assert_eq!(
            verify_webhook_secret("", Some("gateway-secret")),
            Err(WebhookAuthError::InvalidExpectedSecret)
        );
        assert_eq!(
            verify_webhook_secret("gateway-secret", Some("gateway secret")),
            Err(WebhookAuthError::InvalidProvidedSecret)
        );
        assert_eq!(
            verify_webhook_secret(&"a".repeat(257), Some("a")),
            Err(WebhookAuthError::InvalidExpectedSecret)
        );
    }

    #[test]
    fn callback_data_only_contains_a_resource_reference() {
        let reference = parse_callback_reference("rule:42").expect("callback 引用解析失败");
        assert_eq!(reference.resource_id, 42);

        assert_eq!(
            parse_callback_reference("rule:tenant_id=9001:42"),
            Err(CallbackReferenceError::TenantSelectorNotAllowed)
        );
        assert_eq!(
            parse_callback_reference("tenant_id:9001"),
            Err(CallbackReferenceError::TenantSelectorNotAllowed)
        );
    }

    #[test]
    fn callback_data_rejects_unknown_or_invalid_resources() {
        assert_eq!(
            parse_callback_reference("unknown:42"),
            Err(CallbackReferenceError::UnknownResource)
        );
        assert_eq!(
            parse_callback_reference("rule:0"),
            Err(CallbackReferenceError::InvalidResourceId)
        );
        assert_eq!(
            parse_callback_reference("rule:42:extra"),
            Err(CallbackReferenceError::InvalidShape)
        );
    }
}
