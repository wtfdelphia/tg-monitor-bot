//! 控制 Bot Webhook 的安全入口、登录确认卡片与命令幂等边界。
//!
//! Webhook 先校验 secret token，再将扫码登录动作限制在控制 Bot 私聊内；
//! 挑战状态仍由数据库原子迁移负责，Bot API 只发送确认卡片与结果提示。

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Json, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tgm_core::auth::AuthContext;
use tgm_core::error::AppError;
use tgm_core::ids::IdentityId;
use tgm_core::secret::Secret;
use tgm_db::tenant::tenant_write_tx as tenant_tx;
use thiserror::Error;

use crate::bot_gateway::{list_member_contexts, verify_webhook_secret};
use crate::console_login::{
    ConsoleLoginAction, confirm_callback_data, parse_callback_data, parse_start_command,
    parse_start_command_for_bot, reject_callback_data,
};
use crate::control::ControlPlaneState;

pub const TELEGRAM_SECRET_TOKEN_HEADER: &str = "x-telegram-bot-api-secret-token";
const TELEGRAM_BOT_API_TIMEOUT: Duration = Duration::from_secs(10);
const TELEGRAM_BOT_API_MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const TELEGRAM_BOT_API_DEFAULT_BASE_URL: &str = "https://api.telegram.org";

#[derive(Debug, Clone)]
pub struct BotConsoleConfig {
    pub secret_token: String,
    pub bot_identity_id: IdentityId,
    pub bot_username: String,
    pub bot_api: Option<Arc<TelegramBotApiClient>>,
}

#[derive(Debug, Error)]
pub enum TelegramBotApiError {
    #[error("Telegram Bot API token 不能为空")]
    EmptyToken,
    #[error("Telegram Bot API 地址无效")]
    InvalidBaseUrl,
    #[error("Telegram Bot API 请求失败")]
    Request,
    #[error("Telegram Bot API 响应解析失败")]
    Response(#[source] serde_json::Error),
    #[error("Telegram Bot API 响应过大")]
    ResponseTooLarge,
    #[error("Telegram Bot API 调用失败：{0}")]
    Api(String),
    /// Telegram 明确拒绝了请求（HTTP 4xx，`ok=false`）。`status` 用于区分可重试的 429
    /// 与不可恢复的 403（收件人没私聊过 Bot 或已拉黑）。
    #[error("Telegram Bot API 拒绝请求（HTTP {status}）：{description}")]
    Rejected {
        status: u16,
        description: String,
        retry_after: Option<u32>,
    },
}

#[derive(Debug, Error)]
pub enum ConsoleLoginReplyError {
    #[error(transparent)]
    Repository(#[from] tgm_db::repo::ConsoleLoginRepoError),
    #[error(transparent)]
    BotApi(#[from] TelegramBotApiError),
}

#[derive(Debug)]
pub struct TelegramBotApiClient {
    http: reqwest::Client,
    token: Arc<Secret<String>>,
    base_url: String,
}

impl TelegramBotApiClient {
    pub fn new(
        token: Arc<Secret<String>>,
        base_url: impl Into<String>,
    ) -> Result<Self, TelegramBotApiError> {
        if token.expose().is_empty() {
            return Err(TelegramBotApiError::EmptyToken);
        }
        let base_url = base_url.into();
        let parsed = base_url
            .parse::<reqwest::Url>()
            .map_err(|_| TelegramBotApiError::InvalidBaseUrl)?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(TelegramBotApiError::InvalidBaseUrl);
        }
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(TELEGRAM_BOT_API_TIMEOUT)
            .build()
            .map_err(|_| TelegramBotApiError::Request)?;
        Ok(Self {
            http,
            token,
            base_url,
        })
    }

    pub fn from_token(token: String) -> Result<Self, TelegramBotApiError> {
        Self::new(
            Arc::new(Secret::new(token)),
            TELEGRAM_BOT_API_DEFAULT_BASE_URL,
        )
    }

    pub async fn send_login_card(
        &self,
        chat_id: i64,
        challenge: &tgm_db::repo::ConsoleLoginChallengeRecord,
    ) -> Result<(), TelegramBotApiError> {
        let payload = SendMessageRequest {
            chat_id,
            text: login_card_text(challenge),
            parse_mode: "HTML",
            reply_markup: InlineKeyboardMarkup {
                inline_keyboard: vec![vec![
                    InlineKeyboardButton {
                        text: "确认登录".to_owned(),
                        callback_data: confirm_callback_data(
                            &challenge.nonce,
                            &challenge.confirm_code,
                        ),
                    },
                    InlineKeyboardButton {
                        text: "拒绝登录".to_owned(),
                        callback_data: reject_callback_data(&challenge.nonce),
                    },
                ]],
            },
        };
        self.call("sendMessage", &payload).await
    }

    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: &str,
    ) -> Result<(), TelegramBotApiError> {
        let payload = AnswerCallbackQueryRequest {
            callback_query_id,
            text,
            show_alert: false,
        };
        self.call("answerCallbackQuery", &payload).await
    }

    pub async fn send_status_message(
        &self,
        chat_id: i64,
        text: &str,
    ) -> Result<(), TelegramBotApiError> {
        let payload = PlainMessageRequest {
            chat_id,
            text,
            parse_mode: "HTML",
        };
        self.call("sendMessage", &payload).await
    }

    /// `getMe`：租户提交 Bot Token 时校验它确实属于一个 Bot（spec/07 §3.3）。
    pub async fn get_me(&self) -> Result<TelegramBotUser, TelegramBotApiError> {
        let result = self
            .request("getMe", &serde_json::json!({}))
            .await?
            .ok_or_else(|| TelegramBotApiError::Api("getMe 缺少 result".to_owned()))?;
        serde_json::from_value(result).map_err(TelegramBotApiError::Response)
    }

    async fn call<T: Serialize>(
        &self,
        method: &str,
        payload: &T,
    ) -> Result<(), TelegramBotApiError> {
        self.request(method, payload).await.map(|_| ())
    }

    pub(crate) async fn request<T: Serialize>(
        &self,
        method: &str,
        payload: &T,
    ) -> Result<Option<serde_json::Value>, TelegramBotApiError> {
        let url = format!(
            "{}/bot{}/{}",
            self.base_url.trim_end_matches('/'),
            self.token.expose(),
            method
        );
        let response = self
            .http
            .post(url)
            .json(payload)
            .send()
            .await
            .map_err(|_| TelegramBotApiError::Request)?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > TELEGRAM_BOT_API_MAX_RESPONSE_BYTES as u64)
        {
            return Err(TelegramBotApiError::ResponseTooLarge);
        }
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| TelegramBotApiError::Request)?
        {
            if body.len().saturating_add(chunk.len()) > TELEGRAM_BOT_API_MAX_RESPONSE_BYTES {
                return Err(TelegramBotApiError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let envelope = match serde_json::from_slice::<BotApiResponse>(&body) {
            Ok(envelope) => envelope,
            Err(_) if status.is_client_error() => {
                return Err(TelegramBotApiError::Rejected {
                    status: status.as_u16(),
                    description: "Telegram 拒绝请求".to_owned(),
                    retry_after: None,
                });
            }
            Err(_) if !status.is_success() => {
                return Err(TelegramBotApiError::Api(format!(
                    "HTTP {}",
                    status.as_u16()
                )));
            }
            Err(error) => return Err(TelegramBotApiError::Response(error)),
        };
        if !status.is_success() || !envelope.ok {
            let description = envelope
                .description
                .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
            let error_status = envelope.error_code.unwrap_or(status.as_u16());
            if (400..500).contains(&error_status) {
                return Err(TelegramBotApiError::Rejected {
                    status: error_status,
                    description,
                    retry_after: envelope
                        .parameters
                        .and_then(|parameters| parameters.retry_after),
                });
            }
            return Err(TelegramBotApiError::Api(description));
        }
        Ok(envelope.result)
    }
}

#[derive(Debug, Deserialize)]
struct BotApiResponse {
    ok: bool,
    description: Option<String>,
    result: Option<serde_json::Value>,
    error_code: Option<u16>,
    parameters: Option<BotApiParameters>,
}

#[derive(Debug, Deserialize)]
struct BotApiParameters {
    retry_after: Option<u32>,
}

/// `getMe` 返回的 User 对象中本仓用到的字段。
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramBotUser {
    pub id: i64,
    pub is_bot: bool,
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    chat_id: i64,
    text: String,
    parse_mode: &'static str,
    reply_markup: InlineKeyboardMarkup,
}

#[derive(Debug, Serialize)]
struct PlainMessageRequest<'a> {
    chat_id: i64,
    text: &'a str,
    parse_mode: &'static str,
}

#[derive(Debug, Serialize)]
struct AnswerCallbackQueryRequest<'a> {
    callback_query_id: &'a str,
    text: &'a str,
    show_alert: bool,
}

#[derive(Debug, Serialize)]
struct InlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Serialize)]
struct InlineKeyboardButton {
    text: String,
    callback_data: String,
}

fn login_card_text(challenge: &tgm_db::repo::ConsoleLoginChallengeRecord) -> String {
    let user_agent = challenge
        .user_agent
        .as_deref()
        .map(escape_html)
        .unwrap_or_else(|| "(未提供)".to_owned());
    format!(
        "<b>控制台登录请求</b>\n来源 IP：<code>{}</code>\nUser-Agent：<code>{}</code>\n确认码：<code>{}</code>\n\n请核对网页上的确认码，确认无误后再点击“确认登录”。",
        escape_html(&challenge.origin_ip),
        user_agent,
        escape_html(&challenge.confirm_code),
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlUpdateKind {
    Message,
    CallbackQuery,
    MyChatMember,
}

#[derive(Debug, Deserialize)]
pub struct ControlUpdate {
    pub update_id: i64,
    pub message: Option<ControlMessage>,
    pub callback_query: Option<ControlCallbackQuery>,
    pub my_chat_member: Option<ControlChatMemberUpdated>,
}

impl ControlUpdate {
    pub fn actor_user_id(&self) -> Option<i64> {
        self.message
            .as_ref()
            .map(|message| message.from.id)
            .or_else(|| self.callback_query.as_ref().map(|query| query.from.id))
            .or_else(|| self.my_chat_member.as_ref().map(|event| event.from.id))
    }

    pub fn kind(&self) -> Option<ControlUpdateKind> {
        if self.message.is_some() {
            return Some(ControlUpdateKind::Message);
        }
        if self.callback_query.is_some() {
            return Some(ControlUpdateKind::CallbackQuery);
        }
        self.my_chat_member
            .as_ref()
            .map(|_| ControlUpdateKind::MyChatMember)
    }

    pub fn console_login_action(&self) -> Option<ConsoleLoginAction> {
        self.console_login_action_for_bot("")
    }

    pub fn console_login_action_for_bot(&self, bot_username: &str) -> Option<ConsoleLoginAction> {
        self.message
            .as_ref()
            .and_then(|message| message.text.as_deref())
            .and_then(|text| {
                if bot_username.is_empty() {
                    parse_start_command(text)
                } else {
                    parse_start_command_for_bot(text, bot_username)
                }
            })
            .or_else(|| {
                self.callback_query
                    .as_ref()
                    .and_then(|query| query.data.as_deref())
                    .and_then(parse_callback_data)
            })
    }

    pub fn private_chat_id(&self) -> Option<i64> {
        if let Some(message) = self.message.as_ref() {
            let chat = message.chat.as_ref()?;
            return (chat.kind == "private"
                && chat.id > 0
                && chat.id == message.from.id
                && message.from.id > 0)
                .then_some(chat.id);
        }
        let query = self.callback_query.as_ref()?;
        let message = query.message.as_ref()?;
        let chat = message.chat.as_ref()?;
        (chat.kind == "private" && chat.id > 0 && chat.id == query.from.id && query.from.id > 0)
            .then_some(chat.id)
    }

    pub fn callback_query_id(&self) -> Option<&str> {
        self.callback_query
            .as_ref()
            .and_then(|query| query.id.as_deref())
            .filter(|id| !id.is_empty())
    }
}

#[derive(Debug, Deserialize)]
pub struct ControlMessage {
    pub from: ControlUser,
    pub chat: Option<ControlChat>,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ControlCallbackQuery {
    #[serde(default)]
    pub id: Option<String>,
    pub from: ControlUser,
    pub message: Option<ControlCallbackMessage>,
    pub data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ControlCallbackMessage {
    pub chat: Option<ControlChat>,
}

#[derive(Debug, Deserialize)]
pub struct ControlChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct ControlChatMemberUpdated {
    pub from: ControlUser,
}

#[derive(Debug, Deserialize)]
pub struct ControlUser {
    pub id: i64,
}

pub fn route(state: ControlPlaneState) -> Router<ControlPlaneState> {
    Router::new()
        .route("/bot/webhook", post(webhook))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_webhook,
        ))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

pub async fn authenticate_webhook(
    State(state): State<ControlPlaneState>,
    headers: HeaderMap,
    request: axum::http::Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    let Some(config) = state.bot_console_config() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let provided = headers
        .get(TELEGRAM_SECRET_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if verify_webhook_secret(&config.secret_token, provided).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn webhook(
    State(state): State<ControlPlaneState>,
    Json(update): Json<ControlUpdate>,
) -> StatusCode {
    let Some(config) = state.bot_console_config() else {
        return StatusCode::NOT_FOUND;
    };
    let Some(actor_user_id) = update.actor_user_id() else {
        return StatusCode::NO_CONTENT;
    };

    if let Some(action) = update.console_login_action_for_bot(&config.bot_username) {
        let Some(chat_id) = update.private_chat_id() else {
            return StatusCode::NO_CONTENT;
        };
        let Some(auth_pool) = state.auth_pool() else {
            return StatusCode::NOT_FOUND;
        };
        return match process_console_login_action_and_reply(
            auth_pool,
            config.bot_api.as_deref(),
            actor_user_id,
            chat_id,
            update.callback_query_id(),
            action,
        )
        .await
        {
            Ok(()) => StatusCode::NO_CONTENT,
            Err(error) => {
                tracing::error!(error = %error, actor_user_id, "控制台扫码挑战处理失败");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
    }

    let Some(auth_pool) = state.auth_pool() else {
        return StatusCode::NOT_FOUND;
    };
    let contexts = match list_member_contexts(auth_pool, actor_user_id).await {
        Ok(contexts) => contexts,
        Err(AppError::NotInvited) => return StatusCode::NO_CONTENT,
        Err(error) => {
            tracing::error!(error = %error, actor_user_id, "控制 Bot 成员反查失败");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    let mut contexts = contexts.into_iter();
    let Some(auth) = contexts.next() else {
        return StatusCode::NO_CONTENT;
    };
    if !tgm_db::lifecycle::is_active(state.app_pool(), auth.tenant_id())
        .await
        .unwrap_or(false)
    {
        return StatusCode::NO_CONTENT;
    }
    if contexts.next().is_some() {
        tracing::warn!(
            actor_user_id,
            "控制 Bot 操作者属于多个租户，等待服务端会话选择租户"
        );
        return StatusCode::NO_CONTENT;
    }

    match claim_update(
        state.app_pool(),
        auth,
        config.bot_identity_id,
        update.update_id,
    )
    .await
    {
        Ok(true) => {
            tracing::debug!(
                actor_user_id,
                update_id = update.update_id,
                kind = ?update.kind(),
                "控制 Bot update 已认领"
            );
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NO_CONTENT,
        Err(error) => {
            tracing::error!(error = %error, update_id = update.update_id, "控制 Bot update 幂等记录失败");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn process_console_login_action_and_reply(
    pool: &PgPool,
    bot_api: Option<&TelegramBotApiClient>,
    actor_user_id: i64,
    chat_id: i64,
    callback_query_id: Option<&str>,
    action: ConsoleLoginAction,
) -> Result<(), ConsoleLoginReplyError> {
    let action_result = process_console_login_action(pool, actor_user_id, action.clone()).await?;
    let nonce = match &action {
        ConsoleLoginAction::Start { nonce }
        | ConsoleLoginAction::Confirm { nonce, .. }
        | ConsoleLoginAction::Reject { nonce } => nonce,
    };
    let Some(bot_api) = bot_api else {
        return Ok(());
    };
    let Some(challenge) = tgm_db::repo::load_console_login_challenge(pool, nonce).await? else {
        if let Some(callback_query_id) = callback_query_id {
            bot_api
                .answer_callback_query(callback_query_id, "登录请求已失效")
                .await?;
        }
        return Ok(());
    };

    match action {
        ConsoleLoginAction::Start { .. }
            if action_result
                || (challenge.state == "awaiting_confirm"
                    && challenge.tg_user_id == Some(actor_user_id)) =>
        {
            bot_api.send_login_card(chat_id, &challenge).await?;
        }
        ConsoleLoginAction::Confirm { .. } => {
            let message = if action_result {
                "控制台登录已确认，网页会继续完成会话签发。"
            } else {
                "登录确认未生效，请重新扫描二维码。"
            };
            if let Some(callback_query_id) = callback_query_id {
                bot_api
                    .answer_callback_query(callback_query_id, message)
                    .await?;
            }
            bot_api.send_status_message(chat_id, message).await?;
        }
        ConsoleLoginAction::Reject { .. } => {
            let message = if action_result {
                "已拒绝这次控制台登录请求。"
            } else {
                "登录请求已失效，请重新扫描二维码。"
            };
            if let Some(callback_query_id) = callback_query_id {
                bot_api
                    .answer_callback_query(callback_query_id, message)
                    .await?;
            }
            bot_api.send_status_message(chat_id, message).await?;
        }
        ConsoleLoginAction::Start { .. } => {}
    }
    Ok(())
}

pub async fn process_console_login_action(
    pool: &PgPool,
    actor_user_id: i64,
    action: ConsoleLoginAction,
) -> Result<bool, tgm_db::repo::ConsoleLoginRepoError> {
    match action {
        ConsoleLoginAction::Start { nonce } => {
            tgm_db::repo::claim_console_login_challenge(pool, &nonce, actor_user_id).await
        }
        ConsoleLoginAction::Confirm {
            nonce,
            confirm_code,
        } => {
            tgm_db::repo::confirm_console_login_challenge(
                pool,
                &nonce,
                actor_user_id,
                &confirm_code,
            )
            .await
        }
        ConsoleLoginAction::Reject { nonce } => {
            tgm_db::repo::reject_console_login_challenge(pool, &nonce, actor_user_id).await
        }
    }
}

pub async fn claim_update(
    pool: &PgPool,
    auth: AuthContext,
    bot_identity_id: IdentityId,
    update_id: i64,
) -> Result<bool, sqlx::Error> {
    let tenant = auth.tenant_id();
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let inserted = sqlx::query_scalar::<_, i64>(
                "INSERT INTO bot_console_updates (tenant_id, bot_id, update_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (tenant_id, bot_id, update_id) DO NOTHING
                 RETURNING update_id",
            )
            .bind(tenant.get())
            .bind(bot_identity_id.get())
            .bind(update_id)
            .fetch_optional(&mut *conn)
            .await?;
            Ok(inserted.is_some())
        })
    })
    .await
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{ControlUpdate, ControlUpdateKind, TelegramBotApiClient, TelegramBotApiError};
    use serde_json::Value;
    use sqlx::types::chrono::Utc;
    use std::sync::Arc;
    use std::time::Duration;
    use tgm_core::secret::Secret;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_bot_api(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定 Bot API mock 失败");
        let address = listener.local_addr().expect("读取 Bot API mock 地址失败");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("接受 Bot API mock 请求失败");
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = socket
                    .read(&mut chunk)
                    .await
                    .expect("读取 Bot API mock 请求失败");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers_end = bytes
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .expect("Bot API mock 请求缺少 headers 结束符")
                        + 4;
                    let content_length = String::from_utf8_lossy(&bytes[..headers_end])
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .or_else(|| line.strip_prefix("content-length:"))
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .expect("Bot API mock 请求缺少 Content-Length");
                    if bytes.len() >= headers_end + content_length {
                        break;
                    }
                }
            }
            let body_start = bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("Bot API mock 请求缺少 body")
                + 4;
            let body = String::from_utf8(bytes[body_start..].to_vec())
                .expect("Bot API mock 请求 body 不是 UTF-8");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("写入 Bot API mock 响应失败");
            body
        });
        (format!("http://{address}"), task)
    }

    #[test]
    fn control_update_extracts_actor_and_kind() -> Result<(), serde_json::Error> {
        let update: ControlUpdate = serde_json::from_str(
            r#"{
                "update_id": 9,
                "callback_query": {
                    "from": {"id": 42},
                    "data": "console_login:reject:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                }
            }"#,
        )?;
        assert_eq!(update.actor_user_id(), Some(42));
        assert_eq!(update.kind(), Some(ControlUpdateKind::CallbackQuery));
        assert!(update.console_login_action().is_some());
        Ok(())
    }

    #[test]
    fn empty_control_update_has_no_actor() -> Result<(), serde_json::Error> {
        let update: ControlUpdate = serde_json::from_str(r#"{"update_id": 9}"#)?;
        assert_eq!(update.actor_user_id(), None);
        assert_eq!(update.kind(), None);
        Ok(())
    }

    #[test]
    fn login_actions_require_a_private_chat_bound_to_the_actor() -> Result<(), serde_json::Error> {
        let private = serde_json::from_str::<ControlUpdate>(
            r#"{
                "update_id": 10,
                "message": {
                    "from": {"id": 42},
                    "chat": {"id": 42, "type": "private"},
                    "text": "/start login_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                }
            }"#,
        )?;
        assert_eq!(private.private_chat_id(), Some(42));

        let mismatched = serde_json::from_str::<ControlUpdate>(
            r#"{
                "update_id": 11,
                "callback_query": {
                    "id": "callback-1",
                    "from": {"id": 42},
                    "message": {"chat": {"id": 43, "type": "private"}},
                    "data": "cl:r:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                }
            }"#,
        )?;
        assert_eq!(mismatched.private_chat_id(), None);
        assert_eq!(mismatched.callback_query_id(), Some("callback-1"));
        Ok(())
    }

    #[tokio::test]
    async fn bot_api_sends_escaped_login_card_and_compact_buttons() {
        let (base_url, server) = mock_bot_api(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 23\r\nConnection: close\r\n\r\n{\"ok\":true,\"result\":{}}",
        )
        .await;
        let client =
            TelegramBotApiClient::new(Arc::new(Secret::new("test-token".to_owned())), base_url)
                .expect("创建 Bot API client 失败");
        let challenge = tgm_db::repo::ConsoleLoginChallengeRecord {
            nonce: vec![7_u8; 32],
            confirm_code: "A7K4".to_owned(),
            state: "awaiting_confirm".to_owned(),
            tg_user_id: Some(42),
            origin_ip: "203.0.113.7\"><script>".to_owned(),
            user_agent: Some("browser \"test\"".to_owned()),
            expires_at: Utc::now() + Duration::from_secs(60),
        };
        client
            .send_login_card(42, &challenge)
            .await
            .expect("发送控制台登录卡片失败");
        let body: Value = serde_json::from_str(&server.await.expect("等待 Bot API mock 失败"))
            .expect("解析 Bot API mock body 失败");
        assert_eq!(body["chat_id"], 42);
        assert!(
            body["text"]
                .as_str()
                .unwrap_or_default()
                .contains("&lt;script&gt;")
        );
        assert_eq!(
            body["reply_markup"]["inline_keyboard"][0][0]["text"],
            "确认登录"
        );
        assert!(
            body["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
                .as_str()
                .is_some_and(|value| value.len() <= 64)
        );
    }

    #[tokio::test]
    async fn bot_api_surfaces_api_failures() {
        let (base_url, server) = mock_bot_api(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 34\r\nConnection: close\r\n\r\n{\"ok\":false,\"description\":\"retry\"}",
        )
        .await;
        let client =
            TelegramBotApiClient::new(Arc::new(Secret::new("test-token".to_owned())), base_url)
                .expect("创建 Bot API client 失败");
        let error = client
            .send_status_message(42, "状态")
            .await
            .expect_err("Bot API 失败响应不应被视为成功");
        assert!(matches!(
            error,
            TelegramBotApiError::Rejected { status: 429, description, .. } if description == "retry"
        ));
        let _ = server.await.expect("等待 Bot API mock 失败");
    }

    #[tokio::test]
    async fn get_me_parses_bot_user() {
        let body = r#"{"ok":true,"result":{"id":123456,"is_bot":true,"first_name":"Data","username":"data_bot"}}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (base_url, server) = mock_bot_api(response).await;
        let client =
            TelegramBotApiClient::new(Arc::new(Secret::new("123456:abc".to_owned())), base_url)
                .expect("创建 Bot API client 失败");
        let me = client.get_me().await.expect("getMe 失败");
        assert_eq!(me.id, 123456);
        assert!(me.is_bot);
        assert_eq!(me.username.as_deref(), Some("data_bot"));
        let _ = server.await.expect("等待 Bot API mock 失败");
    }
}
