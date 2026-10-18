//! WP-5 控制面骨架：统一错误包络、健康探针与 OpenAPI 导出。

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{FromRequestParts, Path, Query, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use josekit::jwk::JwkSet;
use josekit::jws::{ES256, ES256K, EdDSA, RS256};
use josekit::jwt::{self, JwtPayload};
use regex::Regex;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize, de::Error as DeError};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
use tgm_core::error::AppError;
use tgm_core::kek::Kek;
use tgm_core::oidc::{
    OidcIdTokenClaims, OidcLoginMaterial, OidcLoginStateStore, OidcSigningAlgorithm,
    OidcTokenHeader, validate_id_token_claims,
};
use tgm_core::secret::Secret;
use tokio::net::TcpListener;
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::bot_console::BotConsoleConfig;
use crate::console_login::{ChallengeMaterial, decode_nonce, encode_nonce};
use crate::pipeline::to_rule;
use crate::rule::{KeywordMatchType, RuleKeyword, keyword_matches_text};

#[derive(Clone)]
pub struct ControlPlaneState {
    pool: PgPool,
    auth_pool: Option<PgPool>,
    bot_console_config: Option<BotConsoleConfig>,
    oidc_config: Option<OidcConfig>,
    oidc_states: Arc<OidcLoginStateStore>,
    credential_intake: Option<CredentialIntake>,
}

/// 凭据录入所需的 KEK 与 Bot API 地址（ADR-0032）。
///
/// control-plane 的 KEK 只用 `wrap`：生产上它的 KMS 权限只有 `kms:Encrypt`，
/// 启动期 `encrypt_only_self_test` 实测这一点。
#[derive(Clone)]
pub struct CredentialIntake {
    pub kek: Arc<dyn Kek>,
    pub bot_api_base_url: String,
}

#[derive(Clone)]
pub struct OidcConfig {
    pub bot_id: String,
    pub client_secret: Arc<Secret<String>>,
    pub signing_algorithm: OidcSigningAlgorithm,
    pub redirect_url: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub issuer: String,
    pub jwks_endpoint: String,
}

impl ControlPlaneState {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            auth_pool: None,
            bot_console_config: None,
            oidc_config: None,
            oidc_states: Arc::new(OidcLoginStateStore::default()),
            credential_intake: None,
        }
    }

    pub fn with_auth_pool(pool: PgPool, auth_pool: PgPool) -> Self {
        Self {
            pool,
            auth_pool: Some(auth_pool),
            bot_console_config: None,
            oidc_config: None,
            oidc_states: Arc::new(OidcLoginStateStore::default()),
            credential_intake: None,
        }
    }

    pub fn with_auth_pool_and_bot(
        pool: PgPool,
        auth_pool: PgPool,
        bot_console_config: BotConsoleConfig,
    ) -> Self {
        Self {
            pool,
            auth_pool: Some(auth_pool),
            bot_console_config: Some(bot_console_config),
            oidc_config: None,
            oidc_states: Arc::new(OidcLoginStateStore::default()),
            credential_intake: None,
        }
    }

    pub fn with_auth_pool_and_bot_and_oidc(
        pool: PgPool,
        auth_pool: PgPool,
        bot_console_config: BotConsoleConfig,
        oidc_config: OidcConfig,
    ) -> Self {
        Self {
            pool,
            auth_pool: Some(auth_pool),
            bot_console_config: Some(bot_console_config),
            oidc_config: Some(oidc_config),
            oidc_states: Arc::new(OidcLoginStateStore::default()),
            credential_intake: None,
        }
    }

    pub fn with_auth_pool_and_oidc(
        pool: PgPool,
        auth_pool: PgPool,
        oidc_config: OidcConfig,
    ) -> Self {
        Self {
            pool,
            auth_pool: Some(auth_pool),
            bot_console_config: None,
            oidc_config: Some(oidc_config),
            oidc_states: Arc::new(OidcLoginStateStore::default()),
            credential_intake: None,
        }
    }

    /// 启用 `POST /api/v1/identities/bot`。未调用时该端点 fail closed（500）。
    pub fn with_credential_intake(mut self, intake: CredentialIntake) -> Self {
        self.credential_intake = Some(intake);
        self
    }

    pub(crate) fn app_pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn auth_pool(&self) -> Option<&PgPool> {
        self.auth_pool.as_ref()
    }

    pub(crate) fn bot_console_config(&self) -> Option<&BotConsoleConfig> {
        self.bot_console_config.as_ref()
    }
}

pub const SESSION_COOKIE: &str = "tgm_session";
pub const CSRF_COOKIE: &str = "tgm_csrf";
pub const CSRF_HEADER: &str = "x-csrf-token";
const OIDC_STATE_COOKIE: &str = "tgm_oidc_state";
const OIDC_STATE_TTL_SECONDS: u64 = 600;
const OIDC_STATE_CLEAR_COOKIE: &str =
    "tgm_oidc_state=; Path=/auth/oidc/callback; Max-Age=0; HttpOnly; Secure; SameSite=Lax";
const SESSION_CLEAR_COOKIE: &str =
    "tgm_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax";
const CSRF_CLEAR_COOKIE: &str = "tgm_csrf=; Path=/; Max-Age=0; Secure; SameSite=Lax";
const OIDC_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const OIDC_MAX_TOKEN_BYTES: u64 = 256 * 1024;
const OIDC_MAX_JWKS_BYTES: u64 = 1024 * 1024;
const SESSION_TOKEN_LEN: usize = 32;
pub const OIDC_AUTHORIZATION_ENDPOINT: &str = "https://oauth.telegram.org/auth";
pub const OIDC_TOKEN_ENDPOINT: &str = "https://oauth.telegram.org/token";
pub const OIDC_ISSUER: &str = "https://oauth.telegram.org";
pub const OIDC_JWKS_ENDPOINT: &str = "https://oauth.telegram.org/.well-known/jwks.json";

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthzResponse {
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthResponse {
    pub pg: &'static str,
    pub redis: &'static str,
    pub mtproto: &'static str,
    pub bot: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DashboardIdentityStatus {
    pub online: i64,
    pub offline: i64,
    pub flood_wait: i64,
    pub banned: i64,
    pub unauthorized: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DashboardToday {
    pub received: i64,
    pub matched: i64,
    pub delivered: i64,
    pub failed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DashboardResponse {
    pub status: &'static str,
    pub identities: DashboardIdentityStatus,
    pub queue_depth: i64,
    pub today: DashboardToday,
    pub flood_wait_until: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QrChallengeResponse {
    pub nonce: String,
    pub confirm_code: String,
    pub expires_at: String,
    pub deep_link: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QrChallengeStatusResponse {
    pub state: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IdentityListQuery {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IdentityResponse {
    pub id: i64,
    pub kind: String,
    pub tg_user_id: Option<i64>,
    pub display_name: Option<String>,
    pub status: String,
}

/// 提交 Bot Token。刻意不派生 `Debug`：token 明文不能经任何格式化路径出去。
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BotIdentityCreateRequest {
    /// BotFather 发放的 token。只写不读：响应与审计都不回显。
    pub token: String,
}

#[derive(Debug, Clone, ToSchema)]
pub struct IdentityPatchRequest {
    pub proxy_url: Option<Option<String>>,
    pub display_name: Option<Option<String>>,
}

impl<'de> Deserialize<'de> for IdentityPatchRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("identity patch must be a JSON object"))?;
        let parse_field = |field: &str| -> Result<Option<Option<String>>, D::Error> {
            match object.get(field) {
                None => Ok(None),
                Some(serde_json::Value::Null) => Ok(Some(None)),
                Some(serde_json::Value::String(value)) => Ok(Some(Some(value.clone()))),
                Some(_) => Err(D::Error::custom(format!(
                    "{field} must be a string or null"
                ))),
            }
        };
        Ok(Self {
            proxy_url: parse_field("proxy_url")?,
            display_name: parse_field("display_name")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IdentityListResponse {
    pub items: Vec<IdentityResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RuleListQuery {
    pub enabled: Option<String>,
    pub target_ref: Option<String>,
    pub source_ref: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct KeywordListQuery {
    pub q: Option<String>,
    pub match_type: Option<String>,
    pub category: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeywordResponse {
    pub id: i64,
    pub word: String,
    pub match_type: String,
    pub category: Option<String>,
    pub priority: i32,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeywordListResponse {
    pub items: Vec<KeywordResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeywordCreateRequest {
    pub word: String,
    pub match_type: String,
    pub category: Option<String>,
    pub priority: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct KeywordPatchRequest {
    pub word: Option<String>,
    pub match_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub category: Option<Option<String>>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeywordTestRequest {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeywordTestMatch {
    pub id: i64,
    pub word: String,
    pub match_type: String,
    pub category: Option<String>,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeywordTestResponse {
    pub text: String,
    pub matches: Vec<KeywordTestMatch>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeywordImportRuleRequest {
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub enabled: bool,
    pub conditions: Vec<RuleConditionRequest>,
    pub replacements: Vec<RuleReplacementRequest>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeywordImportRequest {
    pub keywords: Vec<KeywordCreateRequest>,
    pub create_rules_with: Option<KeywordImportRuleRequest>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeywordImportResponse {
    pub items: Vec<KeywordResponse>,
    pub rule_ids: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct KeywordExportQuery {
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeywordExportResponse {
    pub keywords: Vec<KeywordResponse>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleConditionRequest {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleReplacementRequest {
    pub seq: i16,
    pub pattern: String,
    pub replacement: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleReplacementsRequest {
    pub replacements: Vec<RuleReplacementRequest>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleReplacementTestRequest {
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleReplacementCopyRequest {
    pub from_rule: i64,
    pub to_rules: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleReplacementCopyItem {
    pub rule_id: i64,
    pub version: i32,
    pub semantic_changed: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleReplacementCopyResponse {
    pub items: Vec<RuleReplacementCopyItem>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleCreateRequest {
    pub keyword_id: i64,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub enabled: bool,
    pub conditions: Vec<RuleConditionRequest>,
    pub replacements: Vec<RuleReplacementRequest>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct RulePatchRequest {
    pub keyword_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub source_ref: Option<Option<i64>>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub media_type: Option<Option<String>>,
    pub target_ref: Option<i64>,
    pub action: Option<String>,
    pub priority: Option<i32>,
    pub stop_processing: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub template: Option<Option<String>>,
    pub enabled: Option<bool>,
    pub conditions: Option<Vec<RuleConditionRequest>>,
    pub replacements: Option<Vec<RuleReplacementRequest>>,
}

fn deserialize_nullable_patch<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleCopyRequest {
    pub target_ref: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleResponse {
    pub id: i64,
    pub keyword_id: i64,
    pub keyword: String,
    pub keyword_match_type: String,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub target_ref: i64,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: i64,
    pub action: String,
    pub priority: i32,
    pub stop_processing: bool,
    pub template: Option<String>,
    pub version: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleConditionResponse {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleReplacementResponse {
    pub seq: i16,
    pub pattern: String,
    pub replacement: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleReplacementListResponse {
    pub items: Vec<RuleReplacementResponse>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleReplacementStepResponse {
    pub seq: i16,
    pub pattern: String,
    pub replacement: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleReplacementTestResponse {
    pub original: String,
    pub final_text: String,
    pub steps: Vec<RuleReplacementStepResponse>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleDetailResponse {
    pub rule: RuleResponse,
    pub conditions: Vec<RuleConditionResponse>,
    pub replacements: Vec<RuleReplacementResponse>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RuleTestRequest {
    pub text: String,
    pub source_ref: Option<i64>,
    pub media_type: Option<String>,
    pub sender: Option<i64>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleTestTraceResponse {
    pub stage: String,
    pub field: String,
    pub pass: bool,
    pub detail: Option<String>,
    pub matched_at: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleTestRouteResponse {
    pub target_ref: i64,
    pub target_tg_chat_id: i64,
    pub action: String,
    pub rule_version: i32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleTestResponse {
    pub matched: bool,
    pub trace: Vec<RuleTestTraceResponse>,
    pub route: Option<RuleTestRouteResponse>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleListResponse {
    pub items: Vec<RuleResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleVersionResponse {
    pub rule_id: i64,
    pub version: i32,
    pub snapshot: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleVersionListResponse {
    pub items: Vec<RuleVersionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorDetails {
    pub existing_id: Option<i64>,
    pub reason: Option<String>,
    pub affected_rule_ids: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorEnvelope {
    pub code: String,
    pub message: String,
    pub details: Option<ErrorDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorEnvelope,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuthMeResponse {
    pub user_id: i64,
    pub tenant_id: i64,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WebSessionResponse {
    pub id: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub login_method: String,
    pub created_at: String,
    pub expires_at: String,
    pub is_current: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WebSessionListResponse {
    pub items: Vec<WebSessionResponse>,
}

#[derive(Debug, Clone, Copy)]
pub struct Authenticated(pub AuthContext);

impl<S> FromRequestParts<S> for Authenticated
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthContext>()
            .copied()
            .map(Self)
            .ok_or_else(|| AppError::Unauthenticated.into())
    }
}

#[utoipa::path(
    get,
    path = "/auth/me",
    responses(
        (status = 200, description = "Current authenticated user", body = AuthMeResponse),
        (status = 401, description = "Authentication is required", body = ErrorResponse)
    )
)]
async fn auth_me(Authenticated(auth): Authenticated) -> Json<AuthMeResponse> {
    Json(AuthMeResponse {
        user_id: auth.actor_user_id(),
        tenant_id: auth.tenant_id().get(),
        role: auth.role().as_str().to_owned(),
    })
}

#[utoipa::path(
    post,
    path = "/auth/logout",
    responses(
        (status = 204, description = "Current session revoked"),
        (status = 401, description = "Authentication is required", body = ErrorResponse),
        (status = 403, description = "CSRF token is invalid", body = ErrorResponse),
        (status = 500, description = "Session revocation failed", body = ErrorResponse)
    )
)]
async fn auth_logout(
    State(state): State<ControlPlaneState>,
    Authenticated(auth): Authenticated,
) -> Result<Response, ApiError> {
    let session_id = auth
        .web_session_id()
        .ok_or_else(|| AppError::Unauthenticated)?;
    let auth_pool = state.auth_pool.as_ref().ok_or_else(|| {
        AppError::internal(std::io::Error::other("auth lookup pool is not configured"))
    })?;
    tgm_db::repo::delete_web_session(auth_pool, &session_id)
        .await
        .map_err(AppError::Db)?;

    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(SESSION_CLEAR_COOKIE),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(CSRF_CLEAR_COOKIE),
    );
    Ok(response)
}

#[utoipa::path(
    get,
    path = "/auth/sessions",
    responses(
        (status = 200, description = "Own active sessions; ids are non-bearer references", body = WebSessionListResponse),
        (status = 401, description = "Authentication is required", body = ErrorResponse),
        (status = 500, description = "Session lookup failed", body = ErrorResponse)
    )
)]
async fn auth_sessions(
    State(state): State<ControlPlaneState>,
    Authenticated(auth): Authenticated,
) -> Result<Json<WebSessionListResponse>, ApiError> {
    let session_id = auth.web_session_id().ok_or(AppError::Unauthenticated)?;
    let auth_pool = state.auth_pool.as_ref().ok_or_else(|| {
        AppError::internal(std::io::Error::other("auth lookup pool is not configured"))
    })?;
    let sessions = tgm_db::repo::list_web_sessions(auth_pool, auth.actor_user_id(), &session_id)
        .await
        .map_err(AppError::Db)?;
    Ok(Json(WebSessionListResponse {
        items: sessions
            .into_iter()
            .map(|session| WebSessionResponse {
                id: URL_SAFE_NO_PAD.encode(session.reference),
                ip: session.ip,
                user_agent: session.user_agent,
                login_method: session.login_method,
                created_at: session.created_at.to_rfc3339(),
                expires_at: session.expires_at.to_rfc3339(),
                is_current: session.is_current,
            })
            .collect(),
    }))
}

#[utoipa::path(
    delete,
    path = "/auth/sessions/{id}",
    params(("id" = String, Path, description = "URL-safe non-bearer session reference from the session list")),
    responses(
        (status = 204, description = "Session revoked or unavailable"),
        (status = 400, description = "Invalid session reference", body = ErrorResponse),
        (status = 401, description = "Authentication is required", body = ErrorResponse),
        (status = 403, description = "CSRF token is invalid", body = ErrorResponse),
        (status = 500, description = "Session revocation failed", body = ErrorResponse)
    )
)]
async fn auth_revoke_session(
    State(state): State<ControlPlaneState>,
    Authenticated(auth): Authenticated,
    Path(reference): Path<String>,
) -> Result<Response, ApiError> {
    let session_id = auth.web_session_id().ok_or(AppError::Unauthenticated)?;
    if reference.len() != SESSION_TOKEN_LEN * 4 / 3 + 1 {
        return Err(AppError::ConditionInvalid("invalid session reference".to_owned()).into());
    }
    let reference = decode_token(&reference)
        .and_then(|value| <[u8; SESSION_TOKEN_LEN]>::try_from(value.as_slice()).ok())
        .ok_or_else(|| AppError::ConditionInvalid("invalid session reference".to_owned()))?;
    let auth_pool = state.auth_pool.as_ref().ok_or_else(|| {
        AppError::internal(std::io::Error::other("auth lookup pool is not configured"))
    })?;
    tgm_db::repo::revoke_web_session(auth_pool, auth.actor_user_id(), &reference)
        .await
        .map_err(AppError::Db)?;

    let mut response = StatusCode::NO_CONTENT.into_response();
    if reference == tgm_db::repo::web_session_reference(&session_id) {
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static(SESSION_CLEAR_COOKIE),
        );
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static(CSRF_CLEAR_COOKIE),
        );
    }
    Ok(response)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

fn decode_token(value: &str) -> Option<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD.decode(value).ok()?;
    (decoded.len() == SESSION_TOKEN_LEN).then_some(decoded)
}

fn unsafe_method(method: &axum::http::Method) -> bool {
    !matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    )
}

fn request_origin_ip(headers: &HeaderMap) -> String {
    let candidate = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| value.parse::<IpAddr>().is_ok());
    candidate.unwrap_or("0.0.0.0").to_owned()
}

#[utoipa::path(
    get,
    path = "/auth/oidc/start",
    responses(
        (status = 302, description = "Redirect to Telegram OIDC authorization endpoint"),
        (status = 501, description = "OIDC is not configured"),
        (status = 500, description = "OIDC configuration or challenge generation failed", body = ErrorResponse)
    )
)]
async fn start_oidc_login(State(state): State<ControlPlaneState>) -> Result<Response, ApiError> {
    let Some(config) = state.oidc_config.as_ref() else {
        return Ok(StatusCode::NOT_IMPLEMENTED.into_response());
    };
    let material = OidcLoginMaterial::generate().map_err(AppError::internal)?;
    let authorization_url = material.authorization_url(
        &config.authorization_endpoint,
        &config.bot_id,
        &config.redirect_url,
    );
    let state_value = material.state().to_owned();
    state.oidc_states.remember(material);
    let location = HeaderValue::from_str(&authorization_url).map_err(|error| {
        AppError::internal(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            error.to_string(),
        ))
    })?;
    let mut response = StatusCode::FOUND.into_response();
    response.headers_mut().insert(header::LOCATION, location);
    let state_cookie = format!(
        "{OIDC_STATE_COOKIE}={}; Path=/auth/oidc/callback; Max-Age={OIDC_STATE_TTL_SECONDS}; HttpOnly; Secure; SameSite=Lax",
        state_value
    );
    let state_cookie = HeaderValue::from_str(&state_cookie).map_err(|error| {
        AppError::internal(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            error.to_string(),
        ))
    })?;
    response
        .headers_mut()
        .insert(header::SET_COOKIE, state_cookie);
    Ok(response)
}

#[utoipa::path(
    get,
    path = "/auth/oidc/callback",
    params(OidcCallbackQuery),
    responses(
        (status = 403, description = "OIDC state binding is invalid", body = ErrorResponse),
        (status = 400, description = "OIDC callback parameters are invalid", body = ErrorResponse),
        (status = 403, description = "OIDC identity is not an unambiguous tenant member", body = ErrorResponse),
        (status = 503, description = "OIDC provider is unavailable; retry the login", body = ErrorResponse),
        (status = 500, description = "OIDC session issuance failed", body = ErrorResponse)
    )
)]
async fn oidc_callback(
    State(state): State<ControlPlaneState>,
    headers: HeaderMap,
    Query(query): Query<OidcCallbackQuery>,
) -> Result<Response, ApiError> {
    let browser_state = cookie_value(&headers, OIDC_STATE_COOKIE).ok_or(AppError::CsrfInvalid)?;
    let returned_state = query.state.as_deref().ok_or(AppError::CsrfInvalid)?;
    let material = state
        .oidc_states
        .take_if_bound(returned_state, &browser_state)
        .ok_or(AppError::CsrfInvalid)?;

    let clear_cookie = || OIDC_STATE_CLEAR_COOKIE;
    if query.error.is_some() {
        let mut response = ApiError::from(AppError::ConditionInvalid(
            "OIDC authorization was denied".to_owned(),
        ))
        .into_response();
        response
            .headers_mut()
            .insert(header::SET_COOKIE, HeaderValue::from_static(clear_cookie()));
        return Ok(response);
    }
    if query.code.as_deref().is_none_or(str::is_empty) {
        let mut response = ApiError::from(AppError::ConditionInvalid(
            "OIDC callback code is missing".to_owned(),
        ))
        .into_response();
        response
            .headers_mut()
            .insert(header::SET_COOKIE, HeaderValue::from_static(clear_cookie()));
        return Ok(response);
    }

    let config = match state.oidc_config.as_ref() {
        Some(config) => config,
        None => {
            return oidc_error_response(AppError::internal(std::io::Error::other(
                "OIDC configuration is not configured",
            )));
        }
    };
    let code = query
        .code
        .as_deref()
        .ok_or_else(|| AppError::ConditionInvalid("OIDC callback code is missing".to_owned()))?;
    let id_token = match exchange_oidc_code(config, code, &material).await {
        Ok(id_token) => id_token,
        Err(error) => return oidc_error_response(error),
    };
    if OidcTokenHeader::parse_and_validate(&id_token, config.signing_algorithm).is_err() {
        return oidc_error_response(oidc_authentication_failed());
    }
    let jwks = match fetch_oidc_jwks(config).await {
        Ok(jwks) => jwks,
        Err(error) => return oidc_error_response(error),
    };
    let user_id = match validate_oidc_id_token(&id_token, config, &material, &jwks) {
        Ok(user_id) => user_id,
        Err(error) => return oidc_error_response(error),
    };
    let auth_pool = match state.auth_pool.as_ref() {
        Some(pool) => pool,
        None => {
            return oidc_error_response(AppError::internal(std::io::Error::other(
                "auth lookup pool is not configured",
            )));
        }
    };
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let issue = match tgm_db::repo::issue_web_session_for_user(
        auth_pool,
        user_id,
        &request_origin_ip(&headers),
        user_agent,
        tgm_db::repo::WebSessionLoginMethod::Oidc,
    )
    .await
    {
        Ok(issue) => issue,
        Err(error) => return oidc_error_response(AppError::internal(error)),
    };
    let Some(issue) = issue else {
        return oidc_error_response(AppError::NotInvited);
    };
    let session_cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; Max-Age=2592000; HttpOnly; Secure; SameSite=Lax",
        URL_SAFE_NO_PAD.encode(issue.session_id)
    );
    let csrf_cookie = format!(
        "{CSRF_COOKIE}={}; Path=/; Max-Age=2592000; Secure; SameSite=Lax",
        URL_SAFE_NO_PAD.encode(issue.csrf_token)
    );
    let mut response = StatusCode::FOUND.into_response();
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(OIDC_STATE_CLEAR_COOKIE),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie).map_err(|error| {
            AppError::internal(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                error.to_string(),
            ))
        })?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&csrf_cookie).map_err(|error| {
            AppError::internal(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                error.to_string(),
            ))
        })?,
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct OidcTokenResponse {
    id_token: Option<String>,
}

fn oidc_http_client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(OIDC_HTTP_TIMEOUT)
        .build()
        .map_err(AppError::internal)
}

fn oidc_authentication_failed() -> AppError {
    AppError::ConditionInvalid("OIDC authentication failed".to_owned())
}

fn oidc_provider_unavailable() -> AppError {
    AppError::OidcProviderUnavailable
}

fn oidc_error_response(error: AppError) -> Result<Response, ApiError> {
    let mut response = ApiError::from(error).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(OIDC_STATE_CLEAR_COOKIE),
    );
    Ok(response)
}

async fn exchange_oidc_code(
    config: &OidcConfig,
    code: &str,
    material: &OidcLoginMaterial,
) -> Result<String, AppError> {
    let client = oidc_http_client()?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", config.bot_id.as_str()),
        ("client_secret", config.client_secret.expose().as_str()),
        ("redirect_uri", config.redirect_url.as_str()),
        ("code_verifier", material.code_verifier()),
    ];
    let response = client
        .post(&config.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|_| oidc_provider_unavailable())?;
    if response.status().is_server_error()
        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return Err(oidc_provider_unavailable());
    }
    if !response.status().is_success() {
        return Err(oidc_authentication_failed());
    }
    let body = read_response_body(response, OIDC_MAX_TOKEN_BYTES)
        .await
        .map_err(|_| oidc_provider_unavailable())?;
    let token = serde_json::from_slice::<OidcTokenResponse>(&body)
        .map_err(|_| oidc_authentication_failed())?;
    token
        .id_token
        .filter(|value| !value.is_empty())
        .ok_or_else(oidc_authentication_failed)
}

async fn fetch_oidc_jwks(config: &OidcConfig) -> Result<JwkSet, AppError> {
    let client = oidc_http_client()?;
    let response = client
        .get(&config.jwks_endpoint)
        .send()
        .await
        .map_err(|_| oidc_provider_unavailable())?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > OIDC_MAX_JWKS_BYTES)
    {
        return Err(oidc_provider_unavailable());
    }
    let body = read_response_body(response, OIDC_MAX_JWKS_BYTES)
        .await
        .map_err(|_| oidc_provider_unavailable())?;
    JwkSet::from_bytes(body).map_err(|_| oidc_provider_unavailable())
}

async fn read_response_body(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(AppError::internal)? {
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(AppError::internal(std::io::Error::other(
                "OIDC provider response is too large",
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_jwk_metadata(
    jwk: &josekit::jwk::Jwk,
    algorithm: OidcSigningAlgorithm,
) -> Result<(), AppError> {
    if jwk.key_use().is_some_and(|value| value != "sig")
        || jwk
            .key_operations()
            .is_some_and(|operations| !operations.contains(&"verify"))
        || jwk
            .algorithm()
            .is_some_and(|value| value != algorithm.as_str())
    {
        return Err(oidc_authentication_failed());
    }
    Ok(())
}

fn decode_oidc_id_token(
    id_token: &str,
    config: &OidcConfig,
    jwks: &JwkSet,
) -> Result<JwtPayload, AppError> {
    let token_header = OidcTokenHeader::parse_and_validate(id_token, config.signing_algorithm)
        .map_err(|_| oidc_authentication_failed())?;
    let keys = jwks.get(token_header.key_id());
    let Some(jwk) = (keys.len() == 1).then(|| keys[0]) else {
        return Err(oidc_provider_unavailable());
    };
    validate_jwk_metadata(jwk, config.signing_algorithm)?;
    let payload = match config.signing_algorithm {
        OidcSigningAlgorithm::Rs256 => {
            let verifier = RS256
                .verifier_from_jwk(jwk)
                .map_err(|_| oidc_authentication_failed())?;
            jwt::decode_with_verifier(id_token, &verifier)
        }
        OidcSigningAlgorithm::Es256 => {
            let verifier = ES256
                .verifier_from_jwk(jwk)
                .map_err(|_| oidc_authentication_failed())?;
            jwt::decode_with_verifier(id_token, &verifier)
        }
        OidcSigningAlgorithm::EdDsa => {
            let verifier = EdDSA
                .verifier_from_jwk(jwk)
                .map_err(|_| oidc_authentication_failed())?;
            jwt::decode_with_verifier(id_token, &verifier)
        }
        OidcSigningAlgorithm::Es256k => {
            let verifier = ES256K
                .verifier_from_jwk(jwk)
                .map_err(|_| oidc_authentication_failed())?;
            jwt::decode_with_verifier(id_token, &verifier)
        }
    }
    .map_err(|_| oidc_authentication_failed())?;
    Ok(payload.0)
}

fn validate_oidc_id_token(
    id_token: &str,
    config: &OidcConfig,
    material: &OidcLoginMaterial,
    jwks: &JwkSet,
) -> Result<i64, AppError> {
    let payload = decode_oidc_id_token(id_token, config, jwks)?;
    let issuer = payload.issuer().ok_or_else(oidc_authentication_failed)?;
    let audiences = payload.audience().ok_or_else(oidc_authentication_failed)?;
    if payload
        .claim("azp")
        .is_some_and(|value| value.as_str() != Some(config.bot_id.as_str()))
        || (audiences.len() > 1
            && payload.claim("azp").and_then(serde_json::Value::as_str)
                != Some(config.bot_id.as_str()))
    {
        return Err(oidc_authentication_failed());
    }
    let subject = payload.subject().ok_or_else(oidc_authentication_failed)?;
    let nonce = payload
        .claim("nonce")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(oidc_authentication_failed)?;
    let exp = payload
        .claim("exp")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(oidc_authentication_failed)?;
    let claims = OidcIdTokenClaims::new(issuer, &audiences, subject, nonce, exp);
    validate_id_token_claims(
        &claims,
        &config.issuer,
        &config.bot_id,
        Utc::now().timestamp(),
        material,
    )
    .map_err(|_| oidc_authentication_failed())
}

async fn no_store_sensitive_auth(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct OidcCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[utoipa::path(
    post,
    path = "/auth/qr/challenge",
    responses(
        (status = 201, description = "Console QR challenge created", body = QrChallengeResponse),
        (status = 500, description = "Challenge creation failed", body = ErrorResponse)
    )
)]
async fn create_qr_challenge(
    State(state): State<ControlPlaneState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<QrChallengeResponse>), ApiError> {
    let auth_pool = state.auth_pool.as_ref().ok_or_else(|| {
        AppError::internal(std::io::Error::other("auth lookup pool is not configured"))
    })?;
    let config = state.bot_console_config.as_ref().ok_or_else(|| {
        AppError::internal(std::io::Error::other("control bot is not configured"))
    })?;
    let material = ChallengeMaterial::generate().map_err(AppError::internal)?;
    let expires_at = Utc::now() + Duration::from_secs(120);
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    tgm_db::repo::create_console_login_challenge(
        auth_pool,
        material.nonce(),
        material.confirm_code(),
        &request_origin_ip(&headers),
        user_agent.as_deref(),
        expires_at,
    )
    .await
    .map_err(AppError::internal)?;

    let encoded_nonce = encode_nonce(material.nonce());
    Ok((
        StatusCode::CREATED,
        Json(QrChallengeResponse {
            nonce: encoded_nonce.clone(),
            confirm_code: material.confirm_code().to_owned(),
            expires_at: expires_at.to_rfc3339(),
            deep_link: format!(
                "https://t.me/{}?start=login_{encoded_nonce}",
                config.bot_username
            ),
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/auth/qr/challenge/{nonce}",
    params(("nonce" = String, Path, description = "URL-safe base64 challenge nonce")),
    responses(
        (status = 200, description = "Challenge status", body = QrChallengeStatusResponse),
        (status = 404, description = "Challenge not found", body = ErrorResponse),
        (status = 500, description = "Challenge lookup failed", body = ErrorResponse)
    )
)]
async fn get_qr_challenge(
    State(state): State<ControlPlaneState>,
    Path(encoded_nonce): Path<String>,
) -> Result<Response, ApiError> {
    let auth_pool = state.auth_pool.as_ref().ok_or_else(|| {
        AppError::internal(std::io::Error::other("auth lookup pool is not configured"))
    })?;
    let nonce = decode_nonce(&encoded_nonce).ok_or(AppError::NotFound)?;
    let Some(mut challenge) = tgm_db::repo::load_console_login_challenge(auth_pool, &nonce)
        .await
        .map_err(AppError::internal)?
    else {
        return Err(AppError::NotFound.into());
    };
    if matches!(challenge.state.as_str(), "pending" | "awaiting_confirm")
        && challenge.expires_at <= Utc::now()
    {
        let _ = tgm_db::repo::expire_console_login_challenge(auth_pool, &nonce)
            .await
            .map_err(AppError::internal)?;
        if let Some(expired) = tgm_db::repo::load_console_login_challenge(auth_pool, &nonce)
            .await
            .map_err(AppError::internal)?
        {
            challenge = expired;
        }
    }
    let mut issued = None;
    if challenge.state == "confirmed" {
        issued = tgm_db::repo::consume_confirmed_console_login_challenge(auth_pool, &nonce)
            .await
            .map_err(AppError::internal)?;
        if issued.is_none() {
            if let Some(current) = tgm_db::repo::load_console_login_challenge(auth_pool, &nonce)
                .await
                .map_err(AppError::internal)?
            {
                challenge = current;
            } else {
                challenge.state = "rejected".to_owned();
            }
        }
    }
    let mut response = Json(QrChallengeStatusResponse {
        state: challenge.state,
        expires_at: challenge.expires_at.to_rfc3339(),
    })
    .into_response();
    if let Some(issue) = issued {
        let session_cookie = format!(
            "{SESSION_COOKIE}={}; Path=/; Max-Age=2592000; HttpOnly; Secure; SameSite=Lax",
            URL_SAFE_NO_PAD.encode(issue.session_id)
        );
        let csrf_cookie = format!(
            "{CSRF_COOKIE}={}; Path=/; Max-Age=2592000; Secure; SameSite=Lax",
            URL_SAFE_NO_PAD.encode(issue.csrf_token)
        );
        let headers = response.headers_mut();
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_str(&session_cookie).map_err(|error| {
                AppError::internal(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    error.to_string(),
                ))
            })?,
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_str(&csrf_cookie).map_err(|error| {
                AppError::internal(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    error.to_string(),
                ))
            })?,
        );
    }
    Ok(response)
}

async fn authenticate_request(
    State(state): State<ControlPlaneState>,
    mut request: axum::http::Request<Body>,
    next: Next,
) -> Response {
    if request.extensions().get::<AuthContext>().is_some() {
        return next.run(request).await;
    }

    let Some(auth_pool) = state.auth_pool.as_ref() else {
        return next.run(request).await;
    };
    let Some(session_cookie) = cookie_value(request.headers(), SESSION_COOKIE) else {
        return next.run(request).await;
    };
    let Some(session_id) = decode_token(&session_cookie) else {
        return next.run(request).await;
    };

    let session = match tgm_db::repo::load_web_session(auth_pool, &session_id).await {
        Ok(session) => session,
        Err(error) => return ApiError::from(AppError::Db(error)).into_response(),
    };
    let Some(session) = session else {
        return next.run(request).await;
    };

    if unsafe_method(request.method()) {
        let Some(csrf_cookie) =
            cookie_value(request.headers(), CSRF_COOKIE).and_then(|value| decode_token(&value))
        else {
            return ApiError::from(AppError::CsrfInvalid).into_response();
        };
        let Some(csrf_header) = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(decode_token)
        else {
            return ApiError::from(AppError::CsrfInvalid).into_response();
        };
        if csrf_cookie != session.csrf_token || csrf_header != session.csrf_token {
            return ApiError::from(AppError::CsrfInvalid).into_response();
        }
    }

    let Some(role) = TenantRole::parse(&session.role) else {
        return ApiError::from(AppError::Unauthenticated).into_response();
    };
    let Ok(session_id) = <[u8; SESSION_TOKEN_LEN]>::try_from(session_id.as_slice()) else {
        return next.run(request).await;
    };
    request.extensions_mut().insert(
        AuthContext::new(session.tenant_id, session.user_id, role, AuthSource::Web)
            .with_web_session_id(session_id),
    );
    next.run(request).await
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SourceListQuery {
    pub status: Option<String>,
    pub identity_id: Option<String>,
    pub q: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TargetChannelListQuery {
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SourceSubscriptionResponse {
    pub id: i64,
    pub tg_chat_id: i64,
    pub title: Option<String>,
    pub username: Option<String>,
    pub receiver_identity_id: i64,
    pub enabled: bool,
    pub discovered: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SourceListResponse {
    pub items: Vec<SourceSubscriptionResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SourceCreateRequest {
    pub chat: String,
    pub receiver_identity_id: i64,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SourceUpdateRequest {
    pub enabled: Option<bool>,
    pub receiver_identity_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TargetChannelResponse {
    pub id: i64,
    pub tg_chat_id: i64,
    pub title: Option<String>,
    pub sender_identity_id: i64,
    pub can_send: Option<bool>,
    pub checked_at: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TargetChannelListResponse {
    pub items: Vec<TargetChannelResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TargetChannelCreateRequest {
    pub chat: String,
    pub title: Option<String>,
    pub sender_identity_id: i64,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct TargetChannelPatchRequest {
    pub chat: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub title: Option<Option<String>>,
    pub sender_identity_id: Option<i64>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TaskListQuery {
    pub status: Option<String>,
    pub target_ref: Option<String>,
    pub since: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeliveryTaskResponse {
    pub id: i64,
    pub tenant_event_id: Option<i64>,
    pub rule_id: Option<i64>,
    pub rule_version: Option<i32>,
    pub target_ref: Option<i64>,
    pub target_tg_chat_id: i64,
    pub sender_identity_id: i64,
    pub action: String,
    pub status: String,
    pub attempt_count: i32,
    pub next_retry_at: Option<String>,
    pub last_error_code: Option<String>,
    pub telegram_message_id: Option<i64>,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeliveryLogResponse {
    pub id: i64,
    pub task_id: i64,
    pub attempt: i32,
    pub ok: bool,
    pub error_code: Option<String>,
    pub error_msg: Option<String>,
    pub latency_ms: Option<i32>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaskListResponse {
    pub items: Vec<DeliveryTaskResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaskDetailResponse {
    pub task: DeliveryTaskResponse,
    pub delivery_logs: Vec<DeliveryLogResponse>,
    pub rule_snapshot: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuditLogListQuery {
    pub actor: Option<String>,
    pub source: Option<String>,
    pub since: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SystemLogListQuery {
    pub level: Option<String>,
    pub module: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditLogResponse {
    pub id: i64,
    pub actor_user_id: Option<i64>,
    pub action: String,
    pub resource: String,
    pub resource_id: Option<i64>,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub source: String,
    pub ip: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditLogListResponse {
    pub items: Vec<AuditLogResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SystemLogResponse {
    pub id: i64,
    pub level: String,
    pub module: String,
    pub event: String,
    pub context: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SystemLogListResponse {
    pub items: Vec<SystemLogResponse>,
    pub page: i64,
    pub per_page: i64,
    pub has_more: bool,
}

#[derive(Debug)]
pub struct ApiError(AppError);

impl From<AppError> for ApiError {
    fn from(error: AppError) -> Self {
        Self(error)
    }
}

impl ApiError {
    fn response_body(&self) -> ErrorResponse {
        let details = match &self.0 {
            AppError::RuleConflict { existing_id } => Some(ErrorDetails {
                existing_id: Some(*existing_id),
                reason: None,
                affected_rule_ids: None,
            }),
            AppError::KeywordConflict { existing_id } => Some(ErrorDetails {
                existing_id: Some(*existing_id),
                reason: None,
                affected_rule_ids: None,
            }),
            AppError::TargetConflict { existing_id } => Some(ErrorDetails {
                existing_id: Some(*existing_id),
                reason: None,
                affected_rule_ids: None,
            }),
            AppError::KeywordReferenced { rule_ids } => Some(ErrorDetails {
                existing_id: None,
                reason: None,
                affected_rule_ids: Some(rule_ids.clone()),
            }),
            AppError::ConditionInvalid(reason) | AppError::RegexInvalid(reason) => {
                Some(ErrorDetails {
                    existing_id: None,
                    reason: Some(reason.clone()),
                    affected_rule_ids: None,
                })
            }
            _ => None,
        };
        ErrorResponse {
            error: ErrorEnvelope {
                code: self.0.code().to_string(),
                message: self.0.public_message(),
                details,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is_internal() {
            tracing::error!(error = %self.0, code = self.0.code(), "control plane request failed");
        }
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(self.response_body())).into_response();
        if let Ok(value) = HeaderValue::from_str(self.0.code()) {
            response.headers_mut().insert("x-tgm-error-code", value);
        }
        response
    }
}

#[utoipa::path(
    get,
    path = "/healthz",
    responses((status = 200, description = "Process is alive", body = HealthzResponse))
)]
async fn healthz() -> Json<HealthzResponse> {
    Json(HealthzResponse { status: "ok" })
}

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Health status", body = HealthResponse),
        (status = 503, description = "At least one component is unhealthy", body = HealthResponse)
    )
)]
async fn health(State(state): State<ControlPlaneState>) -> Response {
    let pg_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
    let response = HealthResponse {
        pg: if pg_ok { "ok" } else { "down" },
        redis: "not_configured",
        mtproto: "not_configured",
        bot: "not_configured",
    };
    let status = if pg_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(response)).into_response()
}

#[utoipa::path(
    get,
    path = "/api/v1/dashboard/summary",
    responses(
        (status = 200, description = "Dashboard summary", body = DashboardResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn dashboard(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
) -> Result<Json<DashboardResponse>, ApiError> {
    let summary = tgm_db::repo::load_dashboard_summary(&state.pool, auth.tenant_id())
        .await
        .map_err(AppError::from)?;
    Ok(Json(DashboardResponse {
        status: "ok",
        identities: DashboardIdentityStatus {
            online: summary.identity_online,
            offline: summary.identity_offline,
            flood_wait: summary.identity_flood_wait,
            banned: summary.identity_banned,
            unauthorized: summary.identity_unauthorized,
        },
        queue_depth: summary.queue_depth,
        today: DashboardToday {
            received: summary.received_today,
            matched: summary.matched_today,
            delivered: summary.delivered_today,
            failed: summary.failed_today,
        },
        flood_wait_until: summary.flood_wait_until,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/identities",
    params(IdentityListQuery),
    responses(
        (status = 200, description = "Tenant identities", body = IdentityListResponse),
        (status = 400, description = "Invalid filters", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_identities(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<IdentityListQuery>,
) -> Result<Json<IdentityListResponse>, ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    let kind = match query.kind.as_deref() {
        None => None,
        Some("bot") | Some("mtproto") => query.kind.as_deref(),
        Some(_) => {
            return Err(
                AppError::ConditionInvalid("kind must be bot or mtproto".to_owned()).into(),
            );
        }
    };
    let status = match query.status.as_deref() {
        None => None,
        Some("online") | Some("offline") | Some("flood_wait") | Some("banned")
        | Some("unauthorized") => query.status.as_deref(),
        Some(_) => {
            return Err(AppError::ConditionInvalid(
                "status must be online, offline, flood_wait, banned, or unauthorized".to_owned(),
            )
            .into());
        }
    };
    let mut records = tgm_db::repo::list_tenant_identities(
        &state.pool,
        auth.tenant_id(),
        kind,
        status,
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(IdentityListResponse {
        items: records.into_iter().map(identity_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/identities/bot",
    request_body = BotIdentityCreateRequest,
    responses(
        (status = 201, description = "Bot identity created; token is never echoed", body = IdentityResponse),
        (status = 400, description = "Token rejected by Telegram or bot already registered", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required, or credential submitted via Bot", body = ErrorResponse)
    )
)]
async fn create_bot_identity(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Json(input): Json<BotIdentityCreateRequest>,
) -> Result<(StatusCode, Json<IdentityResponse>), ApiError> {
    let token = Arc::new(Secret::new(input.token));
    // spec/02 §6.3：凭据经 Bot 入口提交会留在 Telegram 聊天记录里，永久拒绝。
    if auth.source() != AuthSource::Web {
        return Err(AppError::CredentialViaWebOnly.into());
    }
    require_identity_write(auth)?;
    let bot_id = validate_bot_token(token.expose())?;
    let Some(intake) = state.credential_intake.as_ref() else {
        return Err(
            AppError::internal(std::io::Error::other("control-plane 未配置凭据录入 KEK")).into(),
        );
    };

    let client =
        crate::bot_console::TelegramBotApiClient::new(Arc::clone(&token), &intake.bot_api_base_url)
            .map_err(AppError::internal)?;
    let me = client.get_me().await.map_err(|error| match error {
        crate::bot_console::TelegramBotApiError::Api(_) => {
            AppError::ConditionInvalid("Telegram 拒绝了该 Bot Token".to_owned())
        }
        other => AppError::internal(other),
    })?;
    if !me.is_bot || me.id != bot_id {
        return Err(AppError::ConditionInvalid("该 token 不属于一个 Bot".to_owned()).into());
    }

    let plaintext = Secret::new(token.expose().as_bytes().to_vec());
    let display_name = Some(me.first_name.clone()).filter(|name| !name.trim().is_empty());
    let identity_id = tgm_db::credentials::create_bot_identity(
        &state.pool,
        intake.kek.as_ref(),
        auth.tenant_id(),
        tgm_db::credentials::NewBotIdentity {
            tg_user_id: me.id,
            username: me.username,
            display_name: display_name.clone(),
            actor_user_id: auth.actor_user_id(),
            source: auth.source().as_str().to_owned(),
        },
        &plaintext,
    )
    .await
    .map_err(|error| match error {
        tgm_db::credentials::CredentialError::BotAlreadyRegistered => {
            AppError::ConditionInvalid("该 Bot 已接入".to_owned())
        }
        tgm_db::credentials::CredentialError::Db(error) => AppError::Db(error),
        other => AppError::internal(other),
    })?;
    Ok((
        StatusCode::CREATED,
        Json(IdentityResponse {
            id: identity_id.get(),
            kind: "bot".to_owned(),
            tg_user_id: Some(me.id),
            display_name,
            status: "offline".to_owned(),
        }),
    ))
}

/// Bot Token 形如 `<bot_id>:<secret>`。先在本地拒掉明显不合法的输入，
/// 免得把任意字符串拼进 Bot API 的 URL 路径；返回 bot_id 供与 getMe 结果比对。
fn validate_bot_token(token: &str) -> Result<i64, ApiError> {
    let invalid = || ApiError::from(AppError::ConditionInvalid("Bot Token 格式无效".to_owned()));
    let (id, secret) = token.split_once(':').ok_or_else(invalid)?;
    if id.is_empty()
        || id.len() > 20
        || !id.bytes().all(|byte| byte.is_ascii_digit())
        || secret.len() < 20
        || secret.len() > 128
        || !secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(invalid());
    }
    id.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(invalid)
}

#[utoipa::path(
    patch,
    path = "/api/v1/identities/{id}",
    params(("id" = i64, Path, description = "Identity id")),
    request_body = IdentityPatchRequest,
    responses(
        (status = 200, description = "Identity updated", body = IdentityResponse),
        (status = 400, description = "Invalid identity update", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Identity not found", body = ErrorResponse)
    )
)]
async fn update_identity(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<IdentityPatchRequest>,
) -> Result<Json<IdentityResponse>, ApiError> {
    require_identity_write(auth)?;
    validate_identity_patch(&input)?;
    let record = tgm_db::repo::update_identity(
        &state.pool,
        auth.tenant_id(),
        id,
        tgm_db::repo::IdentityUpdateInput {
            proxy_url: input.proxy_url,
            display_name: input.display_name,
            actor_user_id: auth.actor_user_id(),
            source: auth.source().as_str().to_owned(),
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(identity_response(record)))
}

#[utoipa::path(
    post,
    path = "/api/v1/identities/{id}/reset-status",
    params(("id" = i64, Path, description = "Identity id")),
    responses(
        (status = 200, description = "Identity status reset", body = IdentityResponse),
        (status = 400, description = "Identity cannot be reset", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Identity not found", body = ErrorResponse)
    )
)]
async fn reset_identity_status(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<Json<IdentityResponse>, ApiError> {
    require_identity_write(auth)?;
    let record = tgm_db::repo::reset_identity_status(
        &state.pool,
        auth.tenant_id(),
        id,
        auth.actor_user_id(),
        auth.source().as_str(),
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(identity_response(record)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/identities/{id}",
    params(("id" = i64, Path, description = "Identity id")),
    responses(
        (status = 204, description = "Identity deleted"),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Identity not found", body = ErrorResponse),
        (status = 409, description = "Identity is still referenced", body = ErrorResponse)
    )
)]
async fn delete_identity(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_identity_write(auth)?;
    tgm_db::repo::delete_identity(
        &state.pool,
        auth.tenant_id(),
        id,
        auth.actor_user_id(),
        auth.source().as_str(),
    )
    .await
    .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

fn identity_response(record: tgm_db::repo::IdentityRecord) -> IdentityResponse {
    IdentityResponse {
        id: record.id.get(),
        kind: record.kind,
        tg_user_id: record.tg_user_id,
        display_name: record.display_name,
        status: record.status,
    }
}

fn require_identity_write(auth: AuthContext) -> Result<(), ApiError> {
    match auth.role() {
        TenantRole::Owner | TenantRole::Admin => Ok(()),
        TenantRole::Member | TenantRole::Viewer | TenantRole::SuperAdmin => {
            Err(AppError::Forbidden.into())
        }
    }
}

fn validate_identity_patch(input: &IdentityPatchRequest) -> Result<(), ApiError> {
    if input.proxy_url.is_none() && input.display_name.is_none() {
        return Err(AppError::ConditionInvalid("至少提供一个身份字段".to_owned()).into());
    }
    if let Some(Some(proxy_url)) = &input.proxy_url {
        validate_proxy_url(proxy_url)?;
    }
    if let Some(Some(display_name)) = &input.display_name
        && display_name.trim().is_empty()
    {
        return Err(AppError::ConditionInvalid("display_name 不能为空".to_owned()).into());
    }
    Ok(())
}

fn validate_proxy_url(value: &str) -> Result<(), ApiError> {
    let value = value.trim();
    let Some((scheme, authority_and_path)) = value.split_once("://") else {
        return Err(AppError::ConditionInvalid(
            "proxy_url 必须使用 http、https、socks5 或 socks5h scheme".to_owned(),
        )
        .into());
    };
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h")
        || value.chars().any(char::is_whitespace)
    {
        return Err(AppError::ConditionInvalid(
            "proxy_url 必须使用 http、https、socks5 或 socks5h scheme".to_owned(),
        )
        .into());
    }
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host_port.is_empty() {
        return Err(AppError::ConditionInvalid("proxy_url 必须包含代理主机".to_owned()).into());
    }
    let port = if host_port.starts_with('[') {
        let end = host_port.find(']').ok_or_else(|| {
            AppError::ConditionInvalid("proxy_url 的 IPv6 主机格式无效".to_owned())
        })?;
        match host_port.as_bytes().get(end + 1) {
            None => None,
            Some(b':') => Some(&host_port[end + 2..]),
            Some(_) => {
                return Err(
                    AppError::ConditionInvalid("proxy_url 的端口格式无效".to_owned()).into(),
                );
            }
        }
    } else {
        host_port.rsplit_once(':').map(|(_, port)| port)
    };
    if let Some(port) = port
        && (port.is_empty()
            || !port.chars().all(|character| character.is_ascii_digit())
            || port.parse::<u16>().is_err())
    {
        return Err(AppError::ConditionInvalid("proxy_url 的端口格式无效".to_owned()).into());
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/keywords",
    params(KeywordListQuery),
    responses(
        (status = 200, description = "Tenant keywords", body = KeywordListResponse),
        (status = 400, description = "Invalid keyword filters", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_keywords(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<KeywordListQuery>,
) -> Result<Json<KeywordListResponse>, ApiError> {
    let (page, per_page) = keyword_pagination(&query)?;
    let match_type = keyword_match_type(query.match_type.as_deref())?;
    let mut records = tgm_db::repo::list_keywords(
        &state.pool,
        auth.tenant_id(),
        query.q.as_deref(),
        match_type,
        query.category.as_deref(),
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(KeywordListResponse {
        items: records.into_iter().map(keyword_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/keywords/test",
    request_body = KeywordTestRequest,
    responses(
        (status = 200, description = "Enabled keywords matched against sample text", body = KeywordTestResponse),
        (status = 400, description = "Invalid sample text or unsupported match type", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn test_keywords(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Json(input): Json<KeywordTestRequest>,
) -> Result<Json<KeywordTestResponse>, ApiError> {
    let text = input.text.trim().to_owned();
    if text.is_empty() {
        return Err(AppError::ConditionInvalid("text 不能为空".to_owned()).into());
    }

    let keywords = tgm_db::repo::list_enabled_keywords(&state.pool, auth.tenant_id())
        .await
        .map_err(AppError::from)?;
    let mut matches = Vec::new();
    for keyword in keywords {
        let match_type = match keyword.match_type.as_str() {
            "exact" => KeywordMatchType::Exact,
            "regex" => KeywordMatchType::Regex,
            "fuzzy" => {
                return Err(
                    AppError::ConditionInvalid("暂不支持关键词匹配类型: fuzzy".to_owned()).into(),
                );
            }
            other => {
                return Err(
                    AppError::ConditionInvalid(format!("暂不支持关键词匹配类型: {other}")).into(),
                );
            }
        };
        if keyword_matches_text(
            &text,
            &RuleKeyword {
                id: keyword.id,
                word: keyword.word.clone(),
                match_type,
            },
        )? {
            matches.push(KeywordTestMatch {
                id: keyword.id,
                word: keyword.word,
                match_type: keyword.match_type,
                category: keyword.category,
                priority: keyword.priority,
            });
        }
    }

    Ok(Json(KeywordTestResponse { text, matches }))
}

#[utoipa::path(
    post,
    path = "/api/v1/keywords/import",
    request_body = KeywordImportRequest,
    responses(
        (status = 201, description = "Keywords imported", body = KeywordImportResponse),
        (status = 400, description = "Invalid keyword import", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 409, description = "Keyword or rule conflict", body = ErrorResponse)
    )
)]
async fn import_keywords(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<KeywordImportResponse>), ApiError> {
    require_keyword_write(auth)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let input = if content_type.starts_with("text/csv") {
        parse_keyword_csv(&body)?
    } else if content_type.starts_with("application/json") || content_type.is_empty() {
        serde_json::from_slice::<KeywordImportRequest>(&body)
            .map_err(|error| AppError::ConditionInvalid(format!("导入 JSON 无效: {error}")))?
    } else {
        return Err(AppError::ConditionInvalid(
            "Content-Type 必须是 application/json 或 text/csv".to_owned(),
        )
        .into());
    };

    let mut keyword_inputs = Vec::with_capacity(input.keywords.len());
    for keyword in input.keywords {
        keyword_inputs.push(keyword_create_input(keyword)?);
    }
    let rule_template = input
        .create_rules_with
        .map(keyword_import_rule_input)
        .transpose()?;
    let write =
        tgm_db::repo::import_keywords(&state.pool, auth.tenant_id(), keyword_inputs, rule_template)
            .await
            .map_err(ApiError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(KeywordImportResponse {
            items: write.keywords.into_iter().map(keyword_response).collect(),
            rule_ids: write.rule_ids,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/keywords/export",
    params(KeywordExportQuery),
    responses(
        (status = 200, description = "Tenant keywords export", body = KeywordExportResponse),
        (status = 400, description = "Invalid export format", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn export_keywords(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<KeywordExportQuery>,
) -> Result<Response, ApiError> {
    let format = query.format.as_deref().unwrap_or("json");
    if !matches!(format, "json" | "csv") {
        return Err(AppError::ConditionInvalid("format must be json or csv".to_owned()).into());
    }
    let records =
        tgm_db::repo::list_keywords(&state.pool, auth.tenant_id(), None, None, None, i64::MAX, 0)
            .await
            .map_err(AppError::from)?;
    let items: Vec<_> = records.into_iter().map(keyword_response).collect();
    if format == "csv" {
        let body = keyword_csv(&items);
        return Ok((
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=keywords.csv",
                ),
            ],
            body,
        )
            .into_response());
    }

    let body = serde_json::to_vec(&KeywordExportResponse { keywords: items })
        .map_err(AppError::internal)?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=keywords.json",
            ),
        ],
        body,
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/api/v1/keywords",
    request_body = KeywordCreateRequest,
    responses(
        (status = 201, description = "Keyword created", body = KeywordResponse),
        (status = 400, description = "Invalid keyword", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 409, description = "Keyword already exists", body = ErrorResponse)
    )
)]
async fn create_keyword(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Json(input): Json<KeywordCreateRequest>,
) -> Result<(StatusCode, Json<KeywordResponse>), ApiError> {
    require_keyword_write(auth)?;
    let input = keyword_create_input(input)?;
    let record = tgm_db::repo::create_keyword(&state.pool, auth.tenant_id(), input)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(keyword_response(record))))
}

#[utoipa::path(
    patch,
    path = "/api/v1/keywords/{id}",
    params(("id" = i64, Path, description = "Keyword id")),
    request_body = KeywordPatchRequest,
    responses(
        (status = 200, description = "Keyword updated", body = KeywordResponse),
        (status = 400, description = "Invalid keyword", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Keyword not found", body = ErrorResponse),
        (status = 409, description = "Keyword already exists", body = ErrorResponse)
    )
)]
async fn update_keyword(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<KeywordPatchRequest>,
) -> Result<Json<KeywordResponse>, ApiError> {
    require_keyword_write(auth)?;
    let current = tgm_db::repo::load_keyword(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(AppError::from)?
        .ok_or(AppError::NotFound)?;
    let word = input.word.as_deref().unwrap_or(current.word.as_str());
    let match_type = input
        .match_type
        .as_deref()
        .unwrap_or(current.match_type.as_str());
    validate_keyword_word(word, match_type)?;
    let record = tgm_db::repo::update_keyword(
        &state.pool,
        auth.tenant_id(),
        id,
        tgm_db::repo::KeywordUpdateInput {
            word: input.word.map(|word| word.trim().to_owned()),
            match_type: input.match_type,
            category: input.category,
            priority: input.priority,
            enabled: input.enabled,
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(keyword_response(record)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/keywords/{id}",
    params(("id" = i64, Path, description = "Keyword id")),
    responses(
        (status = 204, description = "Keyword deleted"),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Keyword not found", body = ErrorResponse),
        (status = 409, description = "Keyword is referenced by rules", body = ErrorResponse)
    )
)]
async fn delete_keyword(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_keyword_write(auth)?;
    let (deleted, _) = tgm_db::repo::delete_keyword(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(ApiError::from)?;
    if !deleted {
        return Err(AppError::NotFound.into());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/rules",
    params(RuleListQuery),
    responses(
        (status = 200, description = "Tenant rules", body = RuleListResponse),
        (status = 400, description = "Invalid rule filters", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_rules(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<RuleListQuery>,
) -> Result<Json<RuleListResponse>, ApiError> {
    let (page, per_page) = rule_pagination(&query)?;
    let enabled = rule_enabled(&query)?;
    let target_ref = rule_ref(query.target_ref.as_deref(), "target_ref")?;
    let source_ref = rule_ref(query.source_ref.as_deref(), "source_ref")?;
    let mut records = tgm_db::repo::list_rules(
        &state.pool,
        auth.tenant_id(),
        enabled,
        target_ref,
        source_ref,
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(RuleListResponse {
        items: records.into_iter().map(rule_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/rules/{id}",
    params(("id" = i64, Path, description = "Rule id")),
    responses(
        (status = 200, description = "Rule detail", body = RuleDetailResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn get_rule(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<Json<RuleDetailResponse>, ApiError> {
    let (rule, conditions, replacements) =
        tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    Ok(Json(rule_detail_response(rule, conditions, replacements)))
}

#[utoipa::path(
    post,
    path = "/api/v1/rules/{id}/test",
    params(("id" = i64, Path, description = "Rule id")),
    request_body = RuleTestRequest,
    responses(
        (status = 200, description = "Rule evaluation with trace", body = RuleTestResponse),
        (status = 400, description = "Invalid rule test input", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn test_rule(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<RuleTestRequest>,
) -> Result<Json<RuleTestResponse>, ApiError> {
    let (record, conditions, _replacements) =
        tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    let rule = to_rule(record.clone(), conditions)?;
    let evaluation = crate::rule::evaluate_rule(
        &crate::rule::RuleEvent {
            source_ref: input.source_ref.unwrap_or_default(),
            media_type: input.media_type,
            sender_id: input.sender,
            text: Some(input.text),
            caption: None,
            file_name: input.file_name,
            file_size: input.file_size,
        },
        &rule,
    )?;
    let route = evaluation.matched.then_some(RuleTestRouteResponse {
        target_ref: record.target_ref,
        target_tg_chat_id: record.target_tg_chat_id,
        action: record.action,
        rule_version: record.version,
    });
    Ok(Json(RuleTestResponse {
        matched: evaluation.matched,
        trace: evaluation
            .trace
            .into_iter()
            .map(|step| RuleTestTraceResponse {
                stage: step.stage.to_owned(),
                field: step.field,
                pass: step.pass,
                detail: step.detail,
                matched_at: step.matched_at.map(|values| {
                    values
                        .into_iter()
                        .map(|value| i64::try_from(value).unwrap_or(i64::MAX))
                        .collect()
                }),
            })
            .collect(),
        route,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/rules/{id}/replacements",
    params(("id" = i64, Path, description = "Rule id")),
    responses(
        (status = 200, description = "Rule replacements", body = RuleReplacementListResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn list_rule_replacements(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<Json<RuleReplacementListResponse>, ApiError> {
    let (_rule, _conditions, replacements) =
        tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    Ok(Json(RuleReplacementListResponse {
        items: replacements
            .into_iter()
            .map(rule_replacement_response)
            .collect(),
    }))
}

#[utoipa::path(
    put,
    path = "/api/v1/rules/{id}/replacements",
    params(("id" = i64, Path, description = "Rule id")),
    request_body = RuleReplacementsRequest,
    responses(
        (status = 200, description = "Rule replacements replaced", body = RuleReplacementListResponse),
        (status = 400, description = "Invalid replacements", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn replace_rule_replacements(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<RuleReplacementsRequest>,
) -> Result<Json<RuleReplacementListResponse>, ApiError> {
    require_rule_write(auth)?;
    let (current, conditions, _replacements) =
        tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    tgm_db::repo::update_rule(
        &state.pool,
        auth.tenant_id(),
        id,
        tgm_db::repo::RuleUpdateInput {
            keyword_id: current.keyword_id,
            source_ref: current.source_ref,
            media_type: current.media_type,
            target_ref: current.target_ref,
            action: current.action,
            priority: current.priority,
            stop_processing: current.stop_processing,
            template: current.template,
            enabled: current.enabled,
            conditions: conditions
                .into_iter()
                .map(rule_condition_record_input)
                .collect(),
            replacements: input
                .replacements
                .into_iter()
                .map(rule_replacement_input)
                .collect(),
        },
    )
    .await
    .map_err(ApiError::from)?;
    let (_rule, _conditions, replacements) =
        tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    Ok(Json(RuleReplacementListResponse {
        items: replacements
            .into_iter()
            .map(rule_replacement_response)
            .collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/rules/{id}/replacements/test",
    params(("id" = i64, Path, description = "Rule id")),
    request_body = RuleReplacementTestRequest,
    responses(
        (status = 200, description = "Replacement preview with intermediate steps", body = RuleReplacementTestResponse),
        (status = 400, description = "Invalid replacement pattern", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn test_rule_replacements(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<RuleReplacementTestRequest>,
) -> Result<Json<RuleReplacementTestResponse>, ApiError> {
    let (_rule, _conditions, mut replacements) =
        tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    replacements.retain(|replacement| replacement.enabled);
    replacements.sort_by_key(|replacement| replacement.seq);

    let original = input.text;
    let mut current = original.clone();
    let mut steps = Vec::with_capacity(replacements.len());
    for replacement in replacements {
        let regex = Regex::new(&replacement.pattern)
            .map_err(|error| AppError::RegexInvalid(error.to_string()))?;
        current = regex
            .replace_all(&current, replacement.replacement.as_str())
            .into_owned();
        steps.push(RuleReplacementStepResponse {
            seq: replacement.seq,
            pattern: replacement.pattern,
            replacement: replacement.replacement,
            text: current.clone(),
        });
    }

    Ok(Json(RuleReplacementTestResponse {
        original,
        final_text: current,
        steps,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/replacements/copy",
    request_body = RuleReplacementCopyRequest,
    responses(
        (status = 200, description = "Replacement rules copied", body = RuleReplacementCopyResponse),
        (status = 400, description = "Invalid replacement copy request", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn copy_rule_replacements(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Json(input): Json<RuleReplacementCopyRequest>,
) -> Result<Json<RuleReplacementCopyResponse>, ApiError> {
    require_rule_write(auth)?;
    let writes = tgm_db::repo::copy_rule_replacements(
        &state.pool,
        auth.tenant_id(),
        input.from_rule,
        input.to_rules,
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(RuleReplacementCopyResponse {
        items: writes
            .into_iter()
            .map(|write| RuleReplacementCopyItem {
                rule_id: write.rule_id,
                version: write.version,
                semantic_changed: write.semantic_changed,
            })
            .collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/rules/{id}/versions",
    params(("id" = i64, Path, description = "Rule id")),
    responses(
        (status = 200, description = "Rule version history", body = RuleVersionListResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn list_rule_versions(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<Json<RuleVersionListResponse>, ApiError> {
    let items = tgm_db::repo::list_rule_versions(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(AppError::from)?;
    if items.is_empty()
        && tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .is_none()
    {
        return Err(AppError::NotFound.into());
    }
    Ok(Json(RuleVersionListResponse {
        items: items.into_iter().map(rule_version_response).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/rules/{id}/versions/{version}",
    params(
        ("id" = i64, Path, description = "Rule id"),
        ("version" = i32, Path, description = "Rule version")
    ),
    responses(
        (status = 200, description = "Rule version snapshot", body = RuleVersionResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Rule or version not found", body = ErrorResponse)
    )
)]
async fn get_rule_version(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path((id, version)): Path<(i64, i32)>,
) -> Result<Json<RuleVersionResponse>, ApiError> {
    let record = tgm_db::repo::load_rule_version(&state.pool, auth.tenant_id(), id, version)
        .await
        .map_err(AppError::from)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(rule_version_response(record)))
}

#[utoipa::path(
    post,
    path = "/api/v1/rules",
    request_body = RuleCreateRequest,
    responses(
        (status = 201, description = "Rule created", body = RuleDetailResponse),
        (status = 400, description = "Invalid rule", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 409, description = "Conflicting rule", body = ErrorResponse)
    )
)]
async fn create_rule(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Json(input): Json<RuleCreateRequest>,
) -> Result<(StatusCode, Json<RuleDetailResponse>), ApiError> {
    require_rule_write(auth)?;
    validate_rule_fields(&input.action, input.media_type.as_deref())?;
    let write = tgm_db::repo::create_rule(
        &state.pool,
        auth.tenant_id(),
        tgm_db::repo::RuleCreateInput {
            keyword_id: input.keyword_id,
            source_ref: input.source_ref,
            media_type: input.media_type,
            target_ref: input.target_ref,
            action: input.action,
            priority: input.priority,
            stop_processing: input.stop_processing,
            template: input.template,
            enabled: input.enabled,
            conditions: input
                .conditions
                .into_iter()
                .map(rule_condition_input)
                .collect(),
            replacements: input
                .replacements
                .into_iter()
                .map(rule_replacement_input)
                .collect(),
        },
    )
    .await
    .map_err(ApiError::from)?;
    let detail = load_rule_detail_response(&state.pool, auth.tenant_id(), write.id).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

#[utoipa::path(
    post,
    path = "/api/v1/rules/{id}/copy",
    params(("id" = i64, Path, description = "Source rule id")),
    request_body = RuleCopyRequest,
    responses(
        (status = 201, description = "Rule copied", body = RuleDetailResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Rule or target not found", body = ErrorResponse),
        (status = 409, description = "Conflicting rule", body = ErrorResponse)
    )
)]
async fn copy_rule(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<RuleCopyRequest>,
) -> Result<(StatusCode, Json<RuleDetailResponse>), ApiError> {
    require_rule_write(auth)?;
    let write = tgm_db::repo::copy_rule(&state.pool, auth.tenant_id(), id, input.target_ref)
        .await
        .map_err(ApiError::from)?;
    let detail = load_rule_detail_response(&state.pool, auth.tenant_id(), write.id).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/rules/{id}",
    params(("id" = i64, Path, description = "Rule id")),
    request_body = RulePatchRequest,
    responses(
        (status = 200, description = "Rule updated", body = RuleDetailResponse),
        (status = 400, description = "Invalid rule", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse),
        (status = 409, description = "Conflicting rule", body = ErrorResponse)
    )
)]
async fn update_rule(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<RulePatchRequest>,
) -> Result<Json<RuleDetailResponse>, ApiError> {
    require_rule_write(auth)?;
    let (current, conditions, replacements) =
        tgm_db::repo::load_rule_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    let action = input.action.unwrap_or(current.action);
    let media_type = input.media_type.unwrap_or(current.media_type);
    validate_rule_fields(&action, media_type.as_deref())?;
    let update = tgm_db::repo::update_rule(
        &state.pool,
        auth.tenant_id(),
        id,
        tgm_db::repo::RuleUpdateInput {
            keyword_id: input.keyword_id.unwrap_or(current.keyword_id),
            source_ref: input.source_ref.unwrap_or(current.source_ref),
            media_type,
            target_ref: input.target_ref.unwrap_or(current.target_ref),
            action,
            priority: input.priority.unwrap_or(current.priority),
            stop_processing: input.stop_processing.unwrap_or(current.stop_processing),
            template: input.template.unwrap_or(current.template),
            enabled: input.enabled.unwrap_or(current.enabled),
            conditions: input
                .conditions
                .map(|items| items.into_iter().map(rule_condition_input).collect())
                .unwrap_or_else(|| {
                    conditions
                        .into_iter()
                        .map(rule_condition_record_input)
                        .collect()
                }),
            replacements: input
                .replacements
                .map(|items| items.into_iter().map(rule_replacement_input).collect())
                .unwrap_or_else(|| {
                    replacements
                        .into_iter()
                        .map(rule_replacement_record_input)
                        .collect()
                }),
        },
    )
    .await
    .map_err(ApiError::from)?;
    let _ = update;
    Ok(Json(
        load_rule_detail_response(&state.pool, auth.tenant_id(), id).await?,
    ))
}

#[utoipa::path(
    delete,
    path = "/api/v1/rules/{id}",
    params(("id" = i64, Path, description = "Rule id")),
    responses(
        (status = 204, description = "Rule deleted"),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Rule not found", body = ErrorResponse)
    )
)]
async fn delete_rule(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_rule_write(auth)?;
    if !tgm_db::repo::delete_rule(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(AppError::NotFound.into());
    }
    Ok(StatusCode::NO_CONTENT)
}

fn pagination(query: &SourceListQuery) -> Result<(i64, i64), ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    Ok((page, per_page))
}

fn keyword_pagination(query: &KeywordListQuery) -> Result<(i64, i64), ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    Ok((page, per_page))
}

fn keyword_match_type(value: Option<&str>) -> Result<Option<&str>, ApiError> {
    match value {
        None => Ok(None),
        Some("exact") | Some("regex") | Some("fuzzy") => Ok(value),
        Some(_) => Err(AppError::ConditionInvalid(
            "match_type must be exact, regex, or fuzzy".to_owned(),
        )
        .into()),
    }
}

fn validate_keyword_word(word: &str, match_type: &str) -> Result<String, ApiError> {
    let word = word.trim();
    if word.is_empty() {
        return Err(AppError::ConditionInvalid("word 不能为空".to_owned()).into());
    }
    if !matches!(match_type, "exact" | "regex" | "fuzzy") {
        return Err(AppError::ConditionInvalid(
            "match_type must be exact, regex, or fuzzy".to_owned(),
        )
        .into());
    }
    if match_type == "regex" {
        Regex::new(word).map_err(|error| AppError::RegexInvalid(error.to_string()))?;
    }
    Ok(word.to_owned())
}

fn keyword_create_input(
    input: KeywordCreateRequest,
) -> Result<tgm_db::repo::KeywordCreateInput, ApiError> {
    let match_type = input.match_type.trim().to_owned();
    let word = validate_keyword_word(&input.word, &match_type)?;
    Ok(tgm_db::repo::KeywordCreateInput {
        word,
        match_type,
        category: input.category.map(|value| value.trim().to_owned()),
        priority: input.priority,
        enabled: input.enabled,
    })
}

fn keyword_import_rule_input(
    input: KeywordImportRuleRequest,
) -> Result<tgm_db::repo::KeywordImportRuleInput, ApiError> {
    validate_rule_fields(&input.action, input.media_type.as_deref())?;
    Ok(tgm_db::repo::KeywordImportRuleInput {
        source_ref: input.source_ref,
        media_type: input.media_type,
        target_ref: input.target_ref,
        action: input.action,
        priority: input.priority,
        stop_processing: input.stop_processing,
        template: input.template,
        enabled: input.enabled,
        conditions: input
            .conditions
            .into_iter()
            .map(rule_condition_input)
            .collect(),
        replacements: input
            .replacements
            .into_iter()
            .map(rule_replacement_input)
            .collect(),
    })
}

fn parse_keyword_csv(body: &[u8]) -> Result<KeywordImportRequest, ApiError> {
    let text = std::str::from_utf8(body)
        .map_err(|error| AppError::ConditionInvalid(format!("CSV 编码无效: {error}")))?;
    let mut rows = csv_rows(text)?;
    let header = rows
        .next()
        .ok_or_else(|| AppError::ConditionInvalid("CSV 不能为空".to_owned()))?;
    let header: Vec<_> = header
        .into_iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .collect();
    if header.first().map(String::as_str) != Some("word") {
        return Err(AppError::ConditionInvalid("CSV 第一列必须是 word".to_owned()).into());
    }
    let column = |name: &str| header.iter().position(|value| value == name);
    let match_type_column = column("match_type");
    let category_column = column("category");
    let priority_column = column("priority");
    let enabled_column = column("enabled");
    let mut keywords = Vec::new();
    for row in rows {
        if row.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let value_at = |position: Option<usize>| {
            position
                .and_then(|index| row.get(index))
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let word = row
            .first()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppError::ConditionInvalid("CSV 关键词不能为空".to_owned()))?;
        let priority = value_at(priority_column)
            .map(|value| {
                value
                    .parse::<i32>()
                    .map_err(|_| AppError::ConditionInvalid("CSV priority 必须是整数".to_owned()))
            })
            .transpose()?
            .unwrap_or(0);
        let enabled = value_at(enabled_column)
            .map(|value| match value.as_str() {
                "true" | "TRUE" | "1" => Ok(true),
                "false" | "FALSE" | "0" => Ok(false),
                _ => Err(AppError::ConditionInvalid(
                    "CSV enabled 必须是 true/false".to_owned(),
                )),
            })
            .transpose()?
            .unwrap_or(true);
        keywords.push(KeywordCreateRequest {
            word,
            match_type: value_at(match_type_column).unwrap_or_else(|| "exact".to_owned()),
            category: value_at(category_column),
            priority,
            enabled,
        });
    }
    Ok(KeywordImportRequest {
        keywords,
        create_rules_with: None,
    })
}

fn csv_rows(text: &str) -> Result<impl Iterator<Item = Vec<String>> + '_, ApiError> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match (quoted, character) {
            (true, '"') if chars.peek() == Some(&'"') => {
                field.push('"');
                let _ = chars.next();
            }
            (true, '"') => quoted = false,
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => {
                row.push(std::mem::take(&mut field));
            }
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (_, '\r') => {}
            (_, character) => field.push(character),
        }
    }
    if quoted {
        return Err(AppError::ConditionInvalid("CSV 引号未闭合".to_owned()).into());
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows.into_iter())
}

fn keyword_csv(items: &[KeywordResponse]) -> String {
    let mut output =
        String::from("word,match_type,category,priority,enabled,created_at,updated_at\n");
    for item in items {
        output.push_str(
            &[
                csv_field(&item.word),
                csv_field(&item.match_type),
                csv_field(item.category.as_deref().unwrap_or("")),
                item.priority.to_string(),
                item.enabled.to_string(),
                csv_field(&item.created_at),
                csv_field(&item.updated_at),
            ]
            .join(","),
        );
        output.push('\n');
    }
    output
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn keyword_response(record: tgm_db::repo::KeywordRecord) -> KeywordResponse {
    KeywordResponse {
        id: record.id,
        word: record.word,
        match_type: record.match_type,
        category: record.category,
        priority: record.priority,
        enabled: record.enabled,
        created_at: format_timestamp(record.created_at),
        updated_at: format_timestamp(record.updated_at),
    }
}

fn target_pagination(query: &TargetChannelListQuery) -> Result<(i64, i64), ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    Ok((page, per_page))
}

fn target_response(record: tgm_db::repo::TargetChannelRecord) -> TargetChannelResponse {
    TargetChannelResponse {
        id: record.id,
        tg_chat_id: record.tg_chat_id,
        title: record.title,
        sender_identity_id: record.sender_identity_id.get(),
        can_send: record.can_send,
        checked_at: record.checked_at.map(format_timestamp),
        enabled: record.enabled,
    }
}

fn task_pagination(query: &TaskListQuery) -> Result<(i64, i64), ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    Ok((page, per_page))
}

fn rule_pagination(query: &RuleListQuery) -> Result<(i64, i64), ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    Ok((page, per_page))
}

fn rule_enabled(query: &RuleListQuery) -> Result<Option<bool>, ApiError> {
    query
        .enabled
        .as_deref()
        .map(|value| match value {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(AppError::ConditionInvalid(
                "enabled must be true or false".to_owned(),
            )),
        })
        .transpose()
        .map_err(Into::into)
}

fn rule_ref(value: Option<&str>, name: &str) -> Result<Option<i64>, ApiError> {
    value
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| AppError::ConditionInvalid(format!("{name} must be an integer")))
        })
        .transpose()
        .map_err(Into::into)
}

fn rule_response(record: tgm_db::repo::RuleRecord) -> RuleResponse {
    RuleResponse {
        id: record.id,
        keyword_id: record.keyword_id,
        keyword: record.keyword,
        keyword_match_type: record.keyword_match_type,
        source_ref: record.source_ref,
        media_type: record.media_type,
        target_ref: record.target_ref,
        target_tg_chat_id: record.target_tg_chat_id,
        sender_identity_id: record.sender_identity_id.get(),
        action: record.action,
        priority: record.priority,
        stop_processing: record.stop_processing,
        template: record.template,
        version: record.version,
        enabled: record.enabled,
    }
}

fn rule_detail_response(
    rule: tgm_db::repo::RuleRecord,
    conditions: Vec<tgm_db::repo::RuleConditionRecord>,
    replacements: Vec<tgm_db::repo::RuleReplacementRecord>,
) -> RuleDetailResponse {
    RuleDetailResponse {
        rule: rule_response(rule),
        conditions: conditions
            .into_iter()
            .map(|condition| RuleConditionResponse {
                kind: condition.kind,
                value: condition.value,
            })
            .collect(),
        replacements: replacements
            .into_iter()
            .map(rule_replacement_response)
            .collect(),
    }
}

fn rule_replacement_response(
    replacement: tgm_db::repo::RuleReplacementRecord,
) -> RuleReplacementResponse {
    RuleReplacementResponse {
        seq: replacement.seq,
        pattern: replacement.pattern,
        replacement: replacement.replacement,
        enabled: replacement.enabled,
    }
}

fn rule_version_response(record: tgm_db::repo::RuleVersionRecord) -> RuleVersionResponse {
    RuleVersionResponse {
        rule_id: record.rule_id,
        version: record.version,
        snapshot: record.snapshot,
        created_at: format_timestamp(record.created_at),
    }
}

async fn load_rule_detail_response(
    pool: &PgPool,
    tenant: tgm_core::ids::TenantId,
    rule_id: i64,
) -> Result<RuleDetailResponse, ApiError> {
    let (rule, conditions, replacements) = tgm_db::repo::load_rule_detail(pool, tenant, rule_id)
        .await
        .map_err(AppError::from)?
        .ok_or(AppError::NotFound)?;
    Ok(rule_detail_response(rule, conditions, replacements))
}

fn rule_condition_input(condition: RuleConditionRequest) -> tgm_db::repo::RuleConditionInput {
    tgm_db::repo::RuleConditionInput {
        kind: condition.kind,
        value: condition.value,
    }
}

fn rule_condition_record_input(
    condition: tgm_db::repo::RuleConditionRecord,
) -> tgm_db::repo::RuleConditionInput {
    tgm_db::repo::RuleConditionInput {
        kind: condition.kind,
        value: condition.value,
    }
}

fn rule_replacement_input(
    replacement: RuleReplacementRequest,
) -> tgm_db::repo::RuleReplacementInput {
    tgm_db::repo::RuleReplacementInput {
        seq: replacement.seq,
        pattern: replacement.pattern,
        replacement: replacement.replacement,
        enabled: replacement.enabled,
    }
}

fn rule_replacement_record_input(
    replacement: tgm_db::repo::RuleReplacementRecord,
) -> tgm_db::repo::RuleReplacementInput {
    tgm_db::repo::RuleReplacementInput {
        seq: replacement.seq,
        pattern: replacement.pattern,
        replacement: replacement.replacement,
        enabled: replacement.enabled,
    }
}

fn validate_rule_fields(action: &str, media_type: Option<&str>) -> Result<(), ApiError> {
    if !matches!(action, "COPY" | "FORWARD" | "RESEND") {
        return Err(AppError::ConditionInvalid(
            "action must be COPY, FORWARD, or RESEND".to_owned(),
        )
        .into());
    }
    if let Some(media_type) = media_type
        && !matches!(
            media_type,
            "ANY"
                | "TEXT"
                | "PHOTO"
                | "VIDEO"
                | "AUDIO"
                | "VOICE"
                | "VIDEO_NOTE"
                | "DOCUMENT"
                | "STICKER"
                | "ANIMATION"
                | "OTHER"
        )
    {
        return Err(AppError::ConditionInvalid("media_type is not supported".to_owned()).into());
    }
    Ok(())
}

fn require_rule_write(auth: AuthContext) -> Result<(), ApiError> {
    match auth.role() {
        TenantRole::Owner | TenantRole::Admin => Ok(()),
        TenantRole::Member | TenantRole::Viewer | TenantRole::SuperAdmin => {
            Err(AppError::Forbidden.into())
        }
    }
}

fn require_keyword_write(auth: AuthContext) -> Result<(), ApiError> {
    match auth.role() {
        TenantRole::Owner | TenantRole::Admin => Ok(()),
        TenantRole::Member | TenantRole::Viewer | TenantRole::SuperAdmin => {
            Err(AppError::Forbidden.into())
        }
    }
}

fn require_target_write(auth: AuthContext) -> Result<(), ApiError> {
    match auth.role() {
        TenantRole::Owner | TenantRole::Admin => Ok(()),
        TenantRole::Member | TenantRole::Viewer | TenantRole::SuperAdmin => {
            Err(AppError::Forbidden.into())
        }
    }
}

fn task_status(query: &TaskListQuery) -> Result<Option<&str>, ApiError> {
    match query.status.as_deref() {
        None => Ok(None),
        Some("PENDING") | Some("SENDING") | Some("DONE") | Some("FAILED") | Some("DEAD")
        | Some("CANCELLED") => Ok(query.status.as_deref()),
        Some(_) => Err(AppError::ConditionInvalid(
            "status must be PENDING, SENDING, DONE, FAILED, DEAD, or CANCELLED".to_owned(),
        )
        .into()),
    }
}

fn task_target_ref(query: &TaskListQuery) -> Result<Option<i64>, ApiError> {
    query
        .target_ref
        .as_deref()
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| AppError::ConditionInvalid("target_ref must be an integer".to_owned()))
        })
        .transpose()
        .map_err(Into::into)
}

fn task_since(query: &TaskListQuery) -> Result<Option<DateTime<Utc>>, ApiError> {
    query
        .since
        .as_deref()
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|parsed| parsed.with_timezone(&Utc))
                .map_err(|_| {
                    AppError::ConditionInvalid("since must be an RFC3339 timestamp".to_owned())
                })
        })
        .transpose()
        .map_err(Into::into)
}

fn audit_pagination(query: &AuditLogListQuery) -> Result<(i64, i64), ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    Ok((page, per_page))
}

fn system_log_pagination(query: &SystemLogListQuery) -> Result<(i64, i64), ApiError> {
    let page = query
        .page
        .as_deref()
        .unwrap_or("1")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("page must be an integer".to_owned()))?
        .max(1);
    let per_page = query
        .per_page
        .as_deref()
        .unwrap_or("20")
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("per_page must be an integer".to_owned()))?
        .clamp(1, 100);
    page.checked_sub(1)
        .and_then(|value| value.checked_mul(per_page))
        .ok_or_else(|| AppError::ConditionInvalid("page is too large".to_owned()))?;
    Ok((page, per_page))
}

fn audit_actor(query: &AuditLogListQuery) -> Result<Option<i64>, ApiError> {
    query
        .actor
        .as_deref()
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| AppError::ConditionInvalid("actor must be an integer".to_owned()))
        })
        .transpose()
        .map_err(Into::into)
}

fn audit_source(query: &AuditLogListQuery) -> Result<Option<&str>, ApiError> {
    match query.source.as_deref() {
        None => Ok(None),
        Some("web") | Some("bot") | Some("system") => Ok(query.source.as_deref()),
        Some(_) => {
            Err(AppError::ConditionInvalid("source must be web, bot, or system".to_owned()).into())
        }
    }
}

fn audit_since(query: &AuditLogListQuery) -> Result<Option<DateTime<Utc>>, ApiError> {
    query
        .since
        .as_deref()
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|parsed| parsed.with_timezone(&Utc))
                .map_err(|_| {
                    AppError::ConditionInvalid("since must be an RFC3339 timestamp".to_owned())
                })
        })
        .transpose()
        .map_err(Into::into)
}

fn format_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}

fn task_response(record: tgm_db::repo::DeliveryTaskListRecord) -> DeliveryTaskResponse {
    DeliveryTaskResponse {
        id: record.id,
        tenant_event_id: record.tenant_event_id,
        rule_id: record.rule_id,
        rule_version: record.rule_version,
        target_ref: record.target_ref,
        target_tg_chat_id: record.target_tg_chat_id,
        sender_identity_id: record.sender_identity_id.get(),
        action: record.action,
        status: record.status,
        attempt_count: record.attempt_count,
        next_retry_at: record.next_retry_at.map(format_timestamp),
        last_error_code: record.last_error_code,
        telegram_message_id: record.telegram_message_id,
        created_at: format_timestamp(record.created_at),
        completed_at: record.completed_at.map(format_timestamp),
        updated_at: format_timestamp(record.updated_at),
    }
}

fn task_detail_response(
    task: tgm_db::repo::DeliveryTaskDetailRecord,
    logs: Vec<tgm_db::repo::DeliveryLogRecord>,
) -> TaskDetailResponse {
    let task_response = DeliveryTaskResponse {
        id: task.id,
        tenant_event_id: task.tenant_event_id,
        rule_id: task.rule_id,
        rule_version: task.rule_version,
        target_ref: task.target_ref,
        target_tg_chat_id: task.target_tg_chat_id,
        sender_identity_id: task.sender_identity_id.get(),
        action: task.action,
        status: task.status,
        attempt_count: task.attempt_count,
        next_retry_at: task.next_retry_at.map(format_timestamp),
        last_error_code: task.last_error_code,
        telegram_message_id: task.telegram_message_id,
        created_at: format_timestamp(task.created_at),
        completed_at: task.completed_at.map(format_timestamp),
        updated_at: format_timestamp(task.updated_at),
    };
    TaskDetailResponse {
        task: task_response,
        delivery_logs: logs
            .into_iter()
            .map(|log| DeliveryLogResponse {
                id: log.id,
                task_id: log.task_id,
                attempt: log.attempt,
                ok: log.ok,
                error_code: log.error_code,
                error_msg: log.error_msg,
                latency_ms: log.latency_ms,
                created_at: format_timestamp(log.created_at),
            })
            .collect(),
        rule_snapshot: task.rule_snapshot,
    }
}

fn audit_log_response(record: tgm_db::repo::AuditLogRecord) -> AuditLogResponse {
    AuditLogResponse {
        id: record.id,
        actor_user_id: record.actor_user_id,
        action: record.action,
        resource: record.resource,
        resource_id: record.resource_id,
        before: record.before,
        after: record.after,
        source: record.source,
        ip: record.ip,
        created_at: format_timestamp(record.created_at),
    }
}

fn system_log_response(record: tgm_db::repo::SystemLogRecord) -> SystemLogResponse {
    SystemLogResponse {
        id: record.id,
        level: record.level,
        module: record.module,
        event: record.event,
        context: record.context,
        created_at: format_timestamp(record.created_at),
    }
}

fn source_status(query: &SourceListQuery) -> Result<Option<&str>, ApiError> {
    match query.status.as_deref() {
        None => Ok(None),
        Some("enabled") | Some("disabled") => Ok(query.status.as_deref()),
        Some(_) => {
            Err(AppError::ConditionInvalid("status must be enabled or disabled".to_owned()).into())
        }
    }
}

fn source_identity_id(query: &SourceListQuery) -> Result<Option<i64>, ApiError> {
    query
        .identity_id
        .as_deref()
        .map(|value| {
            value.parse::<i64>().map_err(|_| {
                AppError::ConditionInvalid("identity_id must be an integer".to_owned())
            })
        })
        .transpose()
        .map_err(Into::into)
}

fn source_response(record: tgm_db::repo::SourceSubscriptionRecord) -> SourceSubscriptionResponse {
    SourceSubscriptionResponse {
        id: record.id,
        tg_chat_id: record.tg_chat_id,
        title: record.title,
        username: record.username,
        receiver_identity_id: record.receiver_identity_id,
        enabled: record.enabled,
        discovered: record.discovered,
    }
}

fn require_source_write(auth: AuthContext) -> Result<(), ApiError> {
    match auth.role() {
        TenantRole::Owner | TenantRole::Admin => Ok(()),
        TenantRole::Member | TenantRole::Viewer | TenantRole::SuperAdmin => {
            Err(AppError::Forbidden.into())
        }
    }
}

fn parse_source_chat(value: &str) -> Result<i64, ApiError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::ConditionInvalid("chat 不能为空".to_owned()).into());
    }
    if value.starts_with('@') {
        return Err(AppError::ConditionInvalid(
            "chat 当前必须是已解析的 Telegram chat_id；@username 解析将在 probe 链路接入"
                .to_owned(),
        )
        .into());
    }
    let chat_id = value
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("chat 必须是 Telegram chat_id".to_owned()))?;
    if chat_id == 0 {
        return Err(AppError::ConditionInvalid("chat 不能为 0".to_owned()).into());
    }
    Ok(chat_id)
}

fn parse_target_chat(value: &str) -> Result<i64, ApiError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::ConditionInvalid("chat 不能为空".to_owned()).into());
    }
    if value.starts_with('@') {
        return Err(AppError::ConditionInvalid(
            "chat 当前必须是已解析的 Telegram chat_id；@username 解析将在 probe 链路接入"
                .to_owned(),
        )
        .into());
    }
    let chat_id = value
        .parse::<i64>()
        .map_err(|_| AppError::ConditionInvalid("chat 必须是 Telegram chat_id".to_owned()))?;
    if chat_id == 0 {
        return Err(AppError::ConditionInvalid("chat 不能为 0".to_owned()).into());
    }
    Ok(chat_id)
}

#[utoipa::path(
    post,
    path = "/api/v1/sources",
    request_body = SourceCreateRequest,
    responses(
        (status = 201, description = "Source subscription created or reused", body = SourceSubscriptionResponse),
        (status = 400, description = "Invalid source", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Identity not found", body = ErrorResponse)
    )
)]
async fn create_source(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Json(input): Json<SourceCreateRequest>,
) -> Result<(StatusCode, Json<SourceSubscriptionResponse>), ApiError> {
    require_source_write(auth)?;
    let tg_chat_id = parse_source_chat(&input.chat)?;
    let receiver_identity_id = tgm_db::repo::load_source_identity(
        &state.pool,
        auth.tenant_id(),
        input.receiver_identity_id,
    )
    .await
    .map_err(ApiError::from)?;
    let record = tgm_db::repo::create_source_subscription(
        &state.pool,
        auth.tenant_id(),
        tgm_db::repo::SourceSubscriptionCreateInput {
            tg_chat_id,
            receiver_identity_id,
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(source_response(record))))
}

#[utoipa::path(
    patch,
    path = "/api/v1/sources/{id}",
    params(("id" = i64, Path, description = "Source subscription id")),
    request_body = SourceUpdateRequest,
    responses(
        (status = 200, description = "Source subscription updated", body = SourceSubscriptionResponse),
        (status = 400, description = "Invalid source update", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Source or identity not found", body = ErrorResponse)
    )
)]
async fn update_source(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<SourceUpdateRequest>,
) -> Result<Json<SourceSubscriptionResponse>, ApiError> {
    require_source_write(auth)?;
    if let Some(receiver_identity_id) = input.receiver_identity_id {
        tgm_db::repo::load_source_subscription(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
        tgm_db::repo::load_source_identity(&state.pool, auth.tenant_id(), receiver_identity_id)
            .await
            .map_err(ApiError::from)?;
    }
    let record = tgm_db::repo::update_source_subscription(
        &state.pool,
        auth.tenant_id(),
        id,
        tgm_db::repo::SourceSubscriptionUpdateInput {
            enabled: input.enabled,
            receiver_identity_id: input.receiver_identity_id,
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(source_response(record)))
}

#[utoipa::path(
    get,
    path = "/api/v1/sources",
    params(SourceListQuery),
    responses(
        (status = 200, description = "Tenant source subscriptions", body = SourceListResponse),
        (status = 400, description = "Invalid filters", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_sources(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<SourceListQuery>,
) -> Result<Json<SourceListResponse>, ApiError> {
    let (page, per_page) = pagination(&query)?;
    let status = source_status(&query)?;
    let identity_id = source_identity_id(&query)?;
    let mut records = tgm_db::repo::list_source_subscriptions(
        &state.pool,
        auth.tenant_id(),
        status,
        identity_id,
        query.q.as_deref(),
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(SourceListResponse {
        items: records.into_iter().map(source_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}",
    params(("id" = i64, Path, description = "Source subscription id")),
    responses(
        (status = 200, description = "Tenant source subscription", body = SourceSubscriptionResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Source not found", body = ErrorResponse)
    )
)]
async fn get_source(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<Json<SourceSubscriptionResponse>, ApiError> {
    let record = tgm_db::repo::load_source_subscription(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(AppError::from)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(source_response(record)))
}

#[utoipa::path(
    get,
    path = "/api/v1/targets",
    params(TargetChannelListQuery),
    responses(
        (status = 200, description = "Tenant target channels", body = TargetChannelListResponse),
        (status = 400, description = "Invalid pagination", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_targets(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<TargetChannelListQuery>,
) -> Result<Json<TargetChannelListResponse>, ApiError> {
    let (page, per_page) = target_pagination(&query)?;
    let mut records = tgm_db::repo::list_target_channels(
        &state.pool,
        auth.tenant_id(),
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(TargetChannelListResponse {
        items: records.into_iter().map(target_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/targets",
    request_body = TargetChannelCreateRequest,
    responses(
        (status = 201, description = "Target channel created; Telegram probe pending", body = TargetChannelResponse),
        (status = 400, description = "Invalid target", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Sender identity not found", body = ErrorResponse),
        (status = 409, description = "Target channel already exists", body = ErrorResponse)
    )
)]
async fn create_target(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Json(input): Json<TargetChannelCreateRequest>,
) -> Result<(StatusCode, Json<TargetChannelResponse>), ApiError> {
    require_target_write(auth)?;
    let tg_chat_id = parse_target_chat(&input.chat)?;
    let sender_identity_id =
        tgm_db::repo::load_tenant_identity(&state.pool, auth.tenant_id(), input.sender_identity_id)
            .await
            .map_err(AppError::from)?
            .ok_or(AppError::NotFound)?;
    let record = tgm_db::repo::create_target_channel(
        &state.pool,
        auth.tenant_id(),
        tgm_db::repo::TargetChannelCreateInput {
            tg_chat_id,
            title: input.title.map(|value| value.trim().to_owned()),
            sender_identity_id,
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(target_response(record))))
}

#[utoipa::path(
    patch,
    path = "/api/v1/targets/{id}",
    params(("id" = i64, Path, description = "Target channel id")),
    request_body = TargetChannelPatchRequest,
    responses(
        (status = 200, description = "Target channel updated; probe state cleared when routing fields change", body = TargetChannelResponse),
        (status = 400, description = "Invalid target update", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Target or sender identity not found", body = ErrorResponse),
        (status = 409, description = "Target channel already exists", body = ErrorResponse)
    )
)]
async fn update_target(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
    Json(input): Json<TargetChannelPatchRequest>,
) -> Result<Json<TargetChannelResponse>, ApiError> {
    require_target_write(auth)?;
    let chat = input.chat.as_deref().map(parse_target_chat).transpose()?;
    let record = tgm_db::repo::update_target_channel(
        &state.pool,
        auth.tenant_id(),
        id,
        tgm_db::repo::TargetChannelUpdateInput {
            tg_chat_id: chat,
            title: input
                .title
                .map(|value| value.map(|title| title.trim().to_owned())),
            sender_identity_id: input.sender_identity_id,
            enabled: input.enabled,
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(target_response(record)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/targets/{id}",
    params(("id" = i64, Path, description = "Target channel id")),
    responses(
        (status = 204, description = "Target channel disabled"),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Target channel not found", body = ErrorResponse)
    )
)]
async fn delete_target(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_target_write(auth)?;
    if !tgm_db::repo::delete_target_channel(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(AppError::NotFound.into());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/tasks",
    params(TaskListQuery),
    responses(
        (status = 200, description = "Tenant delivery tasks", body = TaskListResponse),
        (status = 400, description = "Invalid task filters", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_tasks(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<TaskListResponse>, ApiError> {
    let (page, per_page) = task_pagination(&query)?;
    let status = task_status(&query)?;
    let target_ref = task_target_ref(&query)?;
    let since = task_since(&query)?;
    let mut records = tgm_db::repo::list_delivery_tasks(
        &state.pool,
        auth.tenant_id(),
        status,
        target_ref,
        since,
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(TaskListResponse {
        items: records.into_iter().map(task_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/tasks/{id}",
    params(("id" = i64, Path, description = "Delivery task id")),
    responses(
        (status = 200, description = "Delivery task with logs and rule snapshot", body = TaskDetailResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 404, description = "Task not found", body = ErrorResponse)
    )
)]
async fn get_task(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<Json<TaskDetailResponse>, ApiError> {
    let (task, logs) = tgm_db::repo::load_delivery_task_detail(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(AppError::from)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(task_detail_response(task, logs)))
}

#[utoipa::path(
    get,
    path = "/api/v1/audit-logs",
    params(AuditLogListQuery),
    responses(
        (status = 200, description = "Tenant audit logs", body = AuditLogListResponse),
        (status = 400, description = "Invalid audit log filters", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_audit_logs(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<AuditLogListQuery>,
) -> Result<Json<AuditLogListResponse>, ApiError> {
    let (page, per_page) = audit_pagination(&query)?;
    let actor_user_id = audit_actor(&query)?;
    let source = audit_source(&query)?;
    let since = audit_since(&query)?;
    let mut records = tgm_db::repo::list_audit_logs(
        &state.pool,
        auth.tenant_id(),
        actor_user_id,
        source,
        since,
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(AuditLogListResponse {
        items: records.into_iter().map(audit_log_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/logs",
    params(SystemLogListQuery),
    responses(
        (status = 200, description = "Tenant system logs", body = SystemLogListResponse),
        (status = 400, description = "Invalid log pagination", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse)
    )
)]
async fn list_system_logs(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Query(query): Query<SystemLogListQuery>,
) -> Result<Json<SystemLogListResponse>, ApiError> {
    let (page, per_page) = system_log_pagination(&query)?;
    let mut records = tgm_db::repo::list_system_logs(
        &state.pool,
        auth.tenant_id(),
        query.level.as_deref(),
        query.module.as_deref(),
        per_page + 1,
        (page - 1) * per_page,
    )
    .await
    .map_err(AppError::from)?;
    let has_more = records.len() as i64 > per_page;
    if has_more {
        records.truncate(per_page as usize);
    }
    Ok(Json(SystemLogListResponse {
        items: records.into_iter().map(system_log_response).collect(),
        page,
        per_page,
        has_more,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/tasks/{id}/retry",
    params(("id" = i64, Path, description = "Delivery task id")),
    responses(
        (status = 200, description = "Delivery task queued for retry", body = TaskDetailResponse),
        (status = 400, description = "Task is not retryable", body = ErrorResponse),
        (status = 401, description = "Authentication required", body = ErrorResponse),
        (status = 403, description = "Owner or admin required", body = ErrorResponse),
        (status = 404, description = "Task not found", body = ErrorResponse)
    )
)]
async fn retry_task(
    Authenticated(auth): Authenticated,
    State(state): State<ControlPlaneState>,
    Path(id): Path<i64>,
) -> Result<Json<TaskDetailResponse>, ApiError> {
    require_source_write(auth)?;
    let Some((task, _logs)) =
        tgm_db::repo::load_delivery_task_detail(&state.pool, auth.tenant_id(), id)
            .await
            .map_err(AppError::from)?
    else {
        return Err(AppError::NotFound.into());
    };
    if task.status != "DEAD" && task.status != "FAILED" {
        return Err(AppError::ConditionInvalid(
            "only DEAD or FAILED tasks can be retried".to_owned(),
        )
        .into());
    }
    if !tgm_db::repo::retry_delivery_task(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(AppError::from)?
    {
        return Err(
            AppError::ConditionInvalid("task changed state before retry".to_owned()).into(),
        );
    }
    let (task, logs) = tgm_db::repo::load_delivery_task_detail(&state.pool, auth.tenant_id(), id)
        .await
        .map_err(AppError::from)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(task_detail_response(task, logs)))
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Telegram Monitor Control Plane",
        version = "0.1.0",
        description = "Tenant-scoped control plane contract"
    ),
    paths(
        healthz,
        health,
        dashboard,
        list_identities,
        create_bot_identity,
        update_identity,
        reset_identity_status,
        delete_identity,
        list_keywords,
        test_keywords,
        import_keywords,
        export_keywords,
        create_keyword,
        update_keyword,
        delete_keyword,
        list_targets,
        create_target,
        update_target,
        delete_target,
        list_rules,
        get_rule,
        test_rule,
        create_rule,
        update_rule,
        delete_rule,
        list_rule_versions,
        get_rule_version,
        copy_rule,
        list_rule_replacements,
        replace_rule_replacements,
        test_rule_replacements,
        copy_rule_replacements,
        list_sources,
        get_source,
        create_source,
        update_source,
        list_tasks,
        get_task,
        retry_task,
        list_audit_logs,
        list_system_logs,
        create_qr_challenge,
        get_qr_challenge,
        auth_me,
        auth_logout,
        auth_sessions,
        auth_revoke_session,
        start_oidc_login,
        oidc_callback
    ),
    components(schemas(
        HealthzResponse,
        HealthResponse,
        DashboardResponse,
        DashboardIdentityStatus,
        DashboardToday,
        IdentityResponse,
        IdentityListResponse,
        BotIdentityCreateRequest,
        IdentityPatchRequest,
        KeywordResponse,
        KeywordListResponse,
        KeywordCreateRequest,
        KeywordPatchRequest,
        KeywordTestRequest,
        KeywordTestMatch,
        KeywordTestResponse,
        KeywordImportRuleRequest,
        KeywordImportRequest,
        KeywordImportResponse,
        KeywordExportResponse,
        TargetChannelResponse,
        TargetChannelListResponse,
        TargetChannelCreateRequest,
        TargetChannelPatchRequest,
        RuleResponse,
        RuleListResponse,
        RuleConditionRequest,
        RuleReplacementRequest,
        RuleReplacementsRequest,
        RuleReplacementTestRequest,
        RuleReplacementCopyRequest,
        RuleReplacementCopyItem,
        RuleReplacementCopyResponse,
        RuleCreateRequest,
        RulePatchRequest,
        RuleCopyRequest,
        RuleConditionResponse,
        RuleReplacementResponse,
        RuleReplacementListResponse,
        RuleReplacementStepResponse,
        RuleReplacementTestResponse,
        RuleDetailResponse,
        RuleTestRequest,
        RuleTestTraceResponse,
        RuleTestRouteResponse,
        RuleTestResponse,
        RuleVersionResponse,
        RuleVersionListResponse,
        SourceSubscriptionResponse,
        SourceListResponse,
        SourceCreateRequest,
        SourceUpdateRequest,
        DeliveryTaskResponse,
        DeliveryLogResponse,
        TaskListResponse,
        TaskDetailResponse,
        AuditLogResponse,
        AuditLogListResponse,
        SystemLogResponse,
        SystemLogListResponse,
        ErrorDetails,
        ErrorEnvelope,
        ErrorResponse,
        AuthMeResponse,
        WebSessionResponse,
        WebSessionListResponse,
        QrChallengeResponse,
        QrChallengeStatusResponse
    ))
)]
pub struct PublicApiDoc;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Telegram Monitor Platform Admin Control Plane",
        version = "0.1.0",
        description = "Platform-only contract; tenant routes are intentionally excluded"
    ),
    components(schemas(ErrorDetails, ErrorEnvelope, ErrorResponse))
)]
pub struct AdminApiDoc;

pub fn public_openapi_json() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&PublicApiDoc::openapi())
}

pub fn admin_openapi_json() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&AdminApiDoc::openapi())
}

pub fn router(state: ControlPlaneState) -> Router {
    Router::new()
        .merge(crate::console::route())
        .merge(crate::bot_console::route(state.clone()))
        .route(
            "/auth/oidc/start",
            get(start_oidc_login).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route(
            "/auth/oidc/callback",
            get(oidc_callback).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route(
            "/auth/qr/challenge",
            post(create_qr_challenge).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route(
            "/auth/qr/challenge/{nonce}",
            get(get_qr_challenge).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route(
            "/auth/me",
            get(auth_me).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route(
            "/auth/logout",
            post(auth_logout).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route(
            "/auth/sessions",
            get(auth_sessions).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route(
            "/auth/sessions/{id}",
            delete(auth_revoke_session).route_layer(middleware::from_fn(no_store_sensitive_auth)),
        )
        .route("/healthz", get(healthz))
        .route("/health", get(health))
        .route("/api/v1/dashboard/summary", get(dashboard))
        .route("/api/v1/identities", get(list_identities))
        .route("/api/v1/identities/bot", post(create_bot_identity))
        .route(
            "/api/v1/identities/{id}",
            patch(update_identity).delete(delete_identity),
        )
        .route(
            "/api/v1/identities/{id}/reset-status",
            post(reset_identity_status),
        )
        .route("/api/v1/keywords", get(list_keywords).post(create_keyword))
        .route("/api/v1/keywords/test", post(test_keywords))
        .route("/api/v1/keywords/import", post(import_keywords))
        .route("/api/v1/keywords/export", get(export_keywords))
        .route(
            "/api/v1/keywords/{id}",
            patch(update_keyword).delete(delete_keyword),
        )
        .route("/api/v1/targets", get(list_targets).post(create_target))
        .route(
            "/api/v1/targets/{id}",
            patch(update_target).delete(delete_target),
        )
        .route("/api/v1/rules", get(list_rules).post(create_rule))
        .route(
            "/api/v1/rules/{id}",
            get(get_rule).patch(update_rule).delete(delete_rule),
        )
        .route("/api/v1/rules/{id}/copy", post(copy_rule))
        .route("/api/v1/rules/{id}/test", post(test_rule))
        .route(
            "/api/v1/rules/{id}/replacements",
            get(list_rule_replacements).put(replace_rule_replacements),
        )
        .route(
            "/api/v1/rules/{id}/replacements/test",
            post(test_rule_replacements),
        )
        .route("/api/v1/replacements/copy", post(copy_rule_replacements))
        .route("/api/v1/rules/{id}/versions", get(list_rule_versions))
        .route(
            "/api/v1/rules/{id}/versions/{version}",
            get(get_rule_version),
        )
        .route("/api/v1/sources", get(list_sources).post(create_source))
        .route("/api/v1/sources/{id}", get(get_source).patch(update_source))
        .route("/api/v1/tasks", get(list_tasks))
        .route("/api/v1/tasks/{id}", get(get_task))
        .route("/api/v1/tasks/{id}/retry", post(retry_task))
        .route("/api/v1/audit-logs", get(list_audit_logs))
        .route("/api/v1/logs", get(list_system_logs))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_request,
        ))
        .with_state(state)
}

pub async fn serve(pool: PgPool, listener: TcpListener) -> Result<(), std::io::Error> {
    let app = router(ControlPlaneState::new(pool));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

pub async fn serve_with_auth(
    pool: PgPool,
    auth_pool: PgPool,
    listener: TcpListener,
) -> Result<(), std::io::Error> {
    let app = router(ControlPlaneState::with_auth_pool(pool, auth_pool));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

pub async fn serve_with_auth_and_bot(
    pool: PgPool,
    auth_pool: PgPool,
    bot_console_config: BotConsoleConfig,
    listener: TcpListener,
) -> Result<(), std::io::Error> {
    let app = router(ControlPlaneState::with_auth_pool_and_bot(
        pool,
        auth_pool,
        bot_console_config,
    ));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

pub async fn serve_with_auth_and_bot_and_oidc(
    pool: PgPool,
    auth_pool: PgPool,
    bot_console_config: BotConsoleConfig,
    oidc_config: OidcConfig,
    credential_intake: CredentialIntake,
    listener: TcpListener,
) -> Result<(), std::io::Error> {
    let app = router(
        ControlPlaneState::with_auth_pool_and_bot_and_oidc(
            pool,
            auth_pool,
            bot_console_config,
            oidc_config,
        )
        .with_credential_intake(credential_intake),
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{
        ApiError, ControlPlaneState, ErrorResponse, KeywordPatchRequest,
        OIDC_AUTHORIZATION_ENDPOINT, OIDC_ISSUER, OIDC_JWKS_ENDPOINT, OIDC_STATE_COOKIE,
        OIDC_TOKEN_ENDPOINT, OidcConfig, RulePatchRequest, TargetChannelPatchRequest,
        exchange_oidc_code, fetch_oidc_jwks, router, validate_oidc_id_token,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use josekit::jwk::alg::rsa::RsaKeyPair;
    use josekit::jwk::{JwkSet, KeyPair};
    use josekit::jws::{JwsHeader, RS256};
    use josekit::jwt::{self, JwtPayload};
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use tgm_core::error::AppError;
    use tgm_core::oidc::{OidcLoginMaterial, OidcSigningAlgorithm};
    use tgm_core::secret::Secret;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    #[test]
    fn metadata_patch_distinguishes_omitted_null_and_strings() {
        for (value, expected) in [
            (None, None),
            (Some(json!(null)), Some(None)),
            (Some(json!("ops")), Some(Some("ops".to_owned()))),
            (Some(json!("")), Some(Some(String::new()))),
        ] {
            let mut keyword = json!({});
            let mut target = json!({});
            if let Some(value) = value {
                keyword["category"] = value.clone();
                target["title"] = value;
            }
            let keyword: KeywordPatchRequest =
                serde_json::from_value(keyword).expect("解析关键词分类 PATCH 失败");
            let target: TargetChannelPatchRequest =
                serde_json::from_value(target).expect("解析目标名称 PATCH 失败");
            assert_eq!(keyword.category, expected);
            assert_eq!(target.title, expected);
        }
    }

    #[test]
    fn metadata_patch_rejects_invalid_types() {
        for value in [json!(42), json!(false), json!([]), json!({})] {
            assert!(
                serde_json::from_value::<KeywordPatchRequest>(json!({"category": value})).is_err()
            );
            assert!(
                serde_json::from_value::<TargetChannelPatchRequest>(json!({"title": value}))
                    .is_err()
            );
        }
    }

    #[test]
    fn metadata_patch_schema_keeps_nullable_strings_optional() {
        let document: serde_json::Value = serde_json::from_str(
            &super::public_openapi_json().expect("生成分类和名称 PATCH 契约失败"),
        )
        .expect("解析分类和名称 PATCH 契约失败");
        for (schema_name, field) in [
            ("KeywordPatchRequest", "category"),
            ("TargetChannelPatchRequest", "title"),
        ] {
            let schema = &document["components"]["schemas"][schema_name];
            assert_eq!(
                schema["properties"][field]["type"],
                json!(["string", "null"])
            );
            assert!(
                schema["required"]
                    .as_array()
                    .is_none_or(|fields| !fields.contains(&json!(field)))
            );
        }
    }

    #[test]
    fn rule_patch_distinguishes_omitted_null_and_values() {
        let omitted: RulePatchRequest =
            serde_json::from_value(json!({})).expect("解析省略字段的规则 PATCH 失败");
        assert_eq!(omitted.source_ref, None);
        assert_eq!(omitted.media_type, None);
        assert_eq!(omitted.template, None);

        let cleared: RulePatchRequest = serde_json::from_value(json!({
            "source_ref": null, "media_type": null, "template": null
        }))
        .expect("解析清空字段的规则 PATCH 失败");
        assert_eq!(cleared.source_ref, Some(None));
        assert_eq!(cleared.media_type, Some(None));
        assert_eq!(cleared.template, Some(None));

        let replaced: RulePatchRequest = serde_json::from_value(json!({
            "source_ref": 42, "media_type": "TEXT", "template": ""
        }))
        .expect("解析赋值字段的规则 PATCH 失败");
        assert_eq!(replaced.source_ref, Some(Some(42)));
        assert_eq!(replaced.media_type, Some(Some("TEXT".to_owned())));
        assert_eq!(replaced.template, Some(Some(String::new())));
    }

    #[test]
    fn rule_patch_rejects_invalid_nullable_field_types() {
        for payload in [
            json!({"source_ref": "42"}),
            json!({"source_ref": 1.5}),
            json!({"source_ref": true}),
            json!({"source_ref": []}),
            json!({"source_ref": {}}),
            json!({"media_type": 42}),
            json!({"media_type": false}),
            json!({"media_type": []}),
            json!({"media_type": {}}),
            json!({"template": 42}),
            json!({"template": false}),
            json!({"template": []}),
            json!({"template": {}}),
        ] {
            assert!(
                serde_json::from_value::<RulePatchRequest>(payload.clone()).is_err(),
                "无效字段类型未被拒绝：{payload}"
            );
        }
    }

    #[test]
    fn rule_patch_schema_keeps_nullable_fields_optional() {
        let document: serde_json::Value =
            serde_json::from_str(&super::public_openapi_json().expect("生成规则 PATCH 契约失败"))
                .expect("解析规则 PATCH 契约失败");
        let schema = &document["components"]["schemas"]["RulePatchRequest"];
        for (field, kind) in [
            ("source_ref", "integer"),
            ("media_type", "string"),
            ("template", "string"),
        ] {
            assert_eq!(schema["properties"][field]["type"], json!([kind, "null"]));
            assert!(
                schema["required"]
                    .as_array()
                    .is_none_or(|fields| !fields.contains(&json!(field)))
            );
        }
        assert_eq!(schema["properties"]["source_ref"]["format"], "int64");
    }

    fn oidc_test_config(token_endpoint: String, jwks_endpoint: String) -> OidcConfig {
        OidcConfig {
            bot_id: "12345".to_owned(),
            client_secret: Arc::new(Secret::new("test-client-secret".to_owned())),
            signing_algorithm: OidcSigningAlgorithm::Rs256,
            redirect_url: "https://console.example/auth/oidc/callback".to_owned(),
            authorization_endpoint: OIDC_AUTHORIZATION_ENDPOINT.to_owned(),
            token_endpoint,
            issuer: OIDC_ISSUER.to_owned(),
            jwks_endpoint,
        }
    }

    fn signed_test_id_token(material: &OidcLoginMaterial) -> (String, JwkSet) {
        let mut key_pair = RsaKeyPair::generate(2048).expect("生成测试 RSA 密钥失败");
        key_pair.set_algorithm(Some("RS256"));
        key_pair.set_key_id(Some("test-kid"));
        let private_jwk = key_pair.to_jwk_key_pair();
        let mut public_jwk = key_pair.to_jwk_public_key();
        public_jwk.set_key_id("test-kid");
        public_jwk.set_algorithm("RS256");
        public_jwk.set_key_use("sig");
        public_jwk.set_key_operations(vec!["verify"]);

        let mut payload = JwtPayload::new();
        payload.set_issuer(OIDC_ISSUER);
        payload.set_subject("7001");
        payload.set_audience(vec!["12345"]);
        payload.set_expires_at(
            &(SystemTime::UNIX_EPOCH
                + Duration::from_secs(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .expect("读取测试时间失败")
                        .as_secs()
                        + 600,
                )),
        );
        payload
            .set_claim("nonce", Some(json!(material.nonce())))
            .expect("写入测试 nonce 失败");

        let mut header = JwsHeader::new();
        header.set_algorithm("RS256");
        header.set_key_id("test-kid");
        let signer = RS256
            .signer_from_jwk(&private_jwk)
            .expect("创建测试 RSA signer 失败");
        let token =
            jwt::encode_with_signer(&payload, &header, &signer).expect("签发测试 id_token 失败");

        let mut jwks = JwkSet::new();
        jwks.push_key(public_jwk);
        (token, jwks)
    }

    async fn read_http_request_body(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = socket
                .read(&mut chunk)
                .await
                .expect("读取 HTTP 测试请求失败");
            assert!(read > 0, "HTTP 测试请求意外提前结束");
            buffer.extend_from_slice(&chunk[..read]);
            let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let header_text = String::from_utf8_lossy(&buffer[..header_end]);
            let content_length = header_text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .or_else(|| line.strip_prefix("content-length:"))
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("测试请求缺少 Content-Length");
            let body_start = header_end + 4;
            if buffer.len() >= body_start + content_length {
                return buffer[body_start..body_start + content_length].to_vec();
            }
        }
    }

    #[test]
    fn internal_error_response_is_opaque_but_input_details_survive() {
        let internal = ApiError::from(AppError::internal(sqlx::Error::ColumnNotFound(
            "private_constraint".into(),
        )))
        .response_body();
        assert_eq!(internal.error.code, "INTERNAL");
        assert_eq!(internal.error.message, "internal error");
        assert!(internal.error.details.is_none());

        let invalid =
            ApiError::from(AppError::RegexInvalid("unclosed group".into())).response_body();
        assert_eq!(invalid.error.code, "REGEX_INVALID");
        assert_eq!(
            invalid.error.details.expect("details").reason.as_deref(),
            Some("unclosed group")
        );
    }

    #[test]
    fn error_response_keeps_single_outer_envelope() {
        let body = ApiError::from(AppError::NotFound).response_body();
        let encoded = serde_json::to_value(ErrorResponse { error: body.error }).expect("json");
        assert_eq!(encoded["error"]["code"], "NOT_FOUND");
        assert!(encoded.get("tenant_id").is_none());
    }

    #[tokio::test]
    async fn oidc_token_exchange_sends_client_secret_and_pkce_fields() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("启动 OIDC token 测试服务失败");
        let address = listener.local_addr().expect("读取 token 测试地址失败");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("接受 token 测试请求失败");
            let body = read_http_request_body(&mut socket).await;
            let body = String::from_utf8(body).expect("解析 token 测试请求体失败");
            assert!(body.contains("grant_type=authorization_code"));
            assert!(body.contains("code=auth-code"));
            assert!(body.contains("client_id=12345"));
            assert!(body.contains("client_secret=test-client-secret"));
            assert!(
                body.contains(
                    "redirect_uri=https%3A%2F%2Fconsole.example%2Fauth%2Foidc%2Fcallback"
                )
            );
            assert!(body.contains("code_verifier="));
            let response_body = br#"{"id_token":"signed-token"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                std::str::from_utf8(response_body).expect("构造 token 测试响应失败")
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("写入 token 测试响应失败");
        });

        let material = OidcLoginMaterial::generate().expect("生成 OIDC 测试材料失败");
        let config = oidc_test_config(
            format!("http://{address}/token"),
            "http://127.0.0.1:9/jwks".to_owned(),
        );
        let token = exchange_oidc_code(&config, "auth-code", &material)
            .await
            .expect("OIDC token exchange 测试失败");
        assert_eq!(token, "signed-token");
        server.await.expect("token 测试服务任务失败");
    }

    #[tokio::test]
    async fn oidc_token_exchange_maps_provider_overload_to_503() {
        for status in ["429 Too Many Requests", "503 Service Unavailable"] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("启动 OIDC provider 错误测试服务失败");
            let address = listener
                .local_addr()
                .expect("读取 OIDC provider 错误测试地址失败");
            let status = status.to_owned();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("接受 OIDC provider 错误测试请求失败");
                let _ = read_http_request_body(&mut socket).await;
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("写入 OIDC provider 错误测试响应失败");
            });

            let material = OidcLoginMaterial::generate().expect("生成 OIDC 测试材料失败");
            let config = oidc_test_config(
                format!("http://{address}/token"),
                "http://127.0.0.1:9/jwks".to_owned(),
            );
            let error = exchange_oidc_code(&config, "auth-code", &material)
                .await
                .expect_err("provider 过载不应被当作认证失败");
            assert_eq!(error.http_status(), 503);
            assert_eq!(error.code(), "OIDC_PROVIDER_UNAVAILABLE");
            server.await.expect("OIDC provider 错误测试服务任务失败");
        }
    }

    #[tokio::test]
    async fn oidc_token_exchange_keeps_provider_rejected_code_as_authentication_failure() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("启动 OIDC token 拒绝测试服务失败");
        let address = listener
            .local_addr()
            .expect("读取 OIDC token 拒绝测试地址失败");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("接受 OIDC token 拒绝测试请求失败");
            let _ = read_http_request_body(&mut socket).await;
            let response =
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            socket
                .write_all(response.as_bytes())
                .await
                .expect("写入 OIDC token 拒绝测试响应失败");
        });

        let material = OidcLoginMaterial::generate().expect("生成 OIDC 测试材料失败");
        let config = oidc_test_config(
            format!("http://{address}/token"),
            "http://127.0.0.1:9/jwks".to_owned(),
        );
        let error = exchange_oidc_code(&config, "auth-code", &material)
            .await
            .expect_err("provider 拒绝授权码不应变成 503");
        assert_eq!(error.http_status(), 400);
        assert_eq!(error.code(), "CONDITION_INVALID");
        server.await.expect("OIDC token 拒绝测试服务任务失败");
    }

    #[tokio::test]
    async fn oidc_jwks_provider_statuses_are_unavailable() {
        for status in ["429 Too Many Requests", "503 Service Unavailable"] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("启动 OIDC JWKS provider 错误测试服务失败");
            let address = listener
                .local_addr()
                .expect("读取 OIDC JWKS provider 错误测试地址失败");
            let status = status.to_owned();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("接受 OIDC JWKS provider 错误测试请求失败");
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("写入 OIDC JWKS provider 错误测试响应失败");
            });

            let config = oidc_test_config(
                "http://127.0.0.1:9/token".to_owned(),
                format!("http://{address}/jwks"),
            );
            let error = fetch_oidc_jwks(&config)
                .await
                .expect_err("JWKS provider 过载不应被当作空密钥集");
            assert_eq!(error.http_status(), 503);
            assert_eq!(error.code(), "OIDC_PROVIDER_UNAVAILABLE");
            server
                .await
                .expect("OIDC JWKS provider 错误测试服务任务失败");
        }
    }

    #[tokio::test]
    async fn oidc_jwks_provider_rejects_invalid_or_oversized_payloads() {
        let responses = [
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"not\":\"jwks\"}",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n",
        ];
        for response in responses {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("启动 OIDC JWKS payload 测试服务失败");
            let address = listener
                .local_addr()
                .expect("读取 OIDC JWKS payload 测试地址失败");
            let response = response.to_owned();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("接受 OIDC JWKS payload 测试请求失败");
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("写入 OIDC JWKS payload 测试响应失败");
            });

            let config = oidc_test_config(
                "http://127.0.0.1:9/token".to_owned(),
                format!("http://{address}/jwks"),
            );
            let error = fetch_oidc_jwks(&config)
                .await
                .expect_err("非法或超大 JWKS 不应被接受");
            assert_eq!(error.http_status(), 503);
            assert_eq!(error.code(), "OIDC_PROVIDER_UNAVAILABLE");
            server.await.expect("OIDC JWKS payload 测试服务任务失败");
        }
    }

    #[test]
    fn oidc_rs256_jwks_verification_rejects_tampered_algorithm() {
        let material = OidcLoginMaterial::generate().expect("生成 OIDC 测试材料失败");
        let (token, jwks) = signed_test_id_token(&material);
        let config = oidc_test_config(
            "http://127.0.0.1:9/token".to_owned(),
            "http://127.0.0.1:9/jwks".to_owned(),
        );
        let jwk = jwks
            .get("test-kid")
            .into_iter()
            .next()
            .expect("测试 JWKS 缺少 kid");
        let verifier = RS256
            .verifier_from_jwk(jwk)
            .expect("创建测试 verifier 失败");
        let (payload, header) =
            jwt::decode_with_verifier(&token, &verifier).expect("josekit 直接验签失败");
        assert_eq!(header.algorithm(), Some("RS256"));
        assert_eq!(payload.subject(), Some("7001"));
        assert_eq!(
            validate_oidc_id_token(&token, &config, &material, &jwks)
                .expect("RS256 测试 token 验签失败"),
            7001
        );
        let missing_kid_error = validate_oidc_id_token(&token, &config, &material, &JwkSet::new())
            .expect_err("JWKS 缺少 kid 时不应继续验签");
        assert_eq!(missing_kid_error.http_status(), 503);
        assert_eq!(missing_kid_error.code(), "OIDC_PROVIDER_UNAVAILABLE");

        let mut parts = token.split('.');
        let _header = parts.next().expect("测试 token 缺少 header");
        let payload = parts.next().expect("测试 token 缺少 payload");
        let signature = parts.next().expect("测试 token 缺少 signature");
        let tampered_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"test-kid"}"#);
        let tampered = format!("{tampered_header}.{payload}.{signature}");
        assert!(validate_oidc_id_token(&tampered, &config, &material, &jwks).is_err());
    }

    #[tokio::test]
    async fn oidc_start_returns_server_side_pkce_redirect() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://app_user:apw@127.0.0.1:55432/tgm_test_control")
            .expect("构造惰性测试连接池失败");
        let state = ControlPlaneState::with_auth_pool_and_oidc(
            pool.clone(),
            pool,
            OidcConfig {
                bot_id: "12345".to_owned(),
                client_secret: Arc::new(Secret::new("test-client-secret".to_owned())),
                signing_algorithm: OidcSigningAlgorithm::Rs256,
                redirect_url: "https://console.example/auth/oidc/callback".to_owned(),
                authorization_endpoint: OIDC_AUTHORIZATION_ENDPOINT.to_owned(),
                token_endpoint: "http://127.0.0.1:9/token".to_owned(),
                issuer: OIDC_ISSUER.to_owned(),
                jwks_endpoint: "http://127.0.0.1:9/jwks".to_owned(),
            },
        );

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/auth/oidc/start")
                    .body(Body::empty())
                    .expect("构造 OIDC 起始请求失败"),
            )
            .await
            .expect("执行 OIDC 起始请求失败");

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("OIDC 起始响应缺少 Location");
        assert!(location.starts_with("https://oauth.telegram.org/auth?"));
        assert!(location.contains("response_type=code"));
        assert!(location.contains("client_id=12345"));
        assert!(location.contains("code_challenge="));
        assert!(location.contains("code_challenge_method=S256"));
        assert!(location.contains("nonce="));
        assert!(location.contains("state="));
        let state_value = location
            .split("state=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("OIDC 起始 URL 缺少 state 值");
        let state_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("OIDC 起始响应缺少 state cookie");
        assert!(state_cookie.contains(&format!("{OIDC_STATE_COOKIE}={state_value}")));
        assert!(state_cookie.contains("Path=/auth/oidc/callback"));
        assert!(state_cookie.contains("Max-Age=600"));
        assert!(state_cookie.contains("HttpOnly"));
        assert!(state_cookie.contains("Secure"));
        assert!(state_cookie.contains("SameSite=Lax"));
    }

    #[tokio::test]
    async fn oidc_callback_binds_state_to_browser_and_consumes_it_once() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("启动 OIDC 拒绝测试服务失败");
        let address = listener.local_addr().expect("读取 OIDC 拒绝测试地址失败");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("接受 OIDC 拒绝测试请求失败");
            let _ = read_http_request_body(&mut socket).await;
            let response =
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            socket
                .write_all(response.as_bytes())
                .await
                .expect("写入 OIDC 拒绝测试响应失败");
        });
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://app_user:apw@127.0.0.1:55432/tgm_test_control")
            .expect("构造惰性测试连接池失败");
        let state = ControlPlaneState::with_auth_pool_and_oidc(
            pool.clone(),
            pool,
            OidcConfig {
                bot_id: "12345".to_owned(),
                client_secret: Arc::new(Secret::new("test-client-secret".to_owned())),
                signing_algorithm: OidcSigningAlgorithm::Rs256,
                redirect_url: "https://console.example/auth/oidc/callback".to_owned(),
                authorization_endpoint: OIDC_AUTHORIZATION_ENDPOINT.to_owned(),
                token_endpoint: format!("http://{address}/token"),
                issuer: OIDC_ISSUER.to_owned(),
                jwks_endpoint: format!("http://{address}/jwks"),
            },
        );
        let app = router(state);
        let start = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/auth/oidc/start")
                    .body(Body::empty())
                    .expect("构造 OIDC 起始请求失败"),
            )
            .await
            .expect("执行 OIDC 起始请求失败");
        let location = start
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("OIDC 起始响应缺少 Location")
            .to_owned();
        let state_value = location
            .split("state=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("OIDC 起始 URL 缺少 state 值")
            .to_owned();
        let state_cookie = start
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("OIDC 起始响应缺少 state cookie")
            .split(';')
            .next()
            .expect("OIDC state cookie 为空")
            .to_owned();

        let mismatch = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/auth/oidc/callback?code=code-1&state=attacker-state")
                    .header(header::COOKIE, &state_cookie)
                    .body(Body::empty())
                    .expect("构造错误 state 回调请求失败"),
            )
            .await
            .expect("执行错误 state 回调请求失败");
        assert_eq!(mismatch.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            mismatch
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );

        let accepted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/auth/oidc/callback?code=code-1&state={state_value}"
                    ))
                    .header(header::COOKIE, &state_cookie)
                    .body(Body::empty())
                    .expect("构造正确 state 回调请求失败"),
            )
            .await
            .expect("执行正确 state 回调请求失败");
        assert_eq!(accepted.status(), StatusCode::BAD_REQUEST);
        server.await.expect("OIDC 拒绝测试服务任务失败");
        assert_eq!(
            accepted
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let cleared_cookie = accepted
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("回调响应缺少清理 state cookie");
        assert!(cleared_cookie.contains("Max-Age=0"));

        let replay = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/auth/oidc/callback?code=code-1&state={state_value}"
                    ))
                    .header(header::COOKIE, &state_cookie)
                    .body(Body::empty())
                    .expect("构造重放回调请求失败"),
            )
            .await
            .expect("执行重放回调请求失败");
        assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn oidc_callback_missing_code_clears_cookie_after_consuming_state() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://app_user:apw@127.0.0.1:55432/tgm_test_control")
            .expect("构造惰性测试连接池失败");
        let state = ControlPlaneState::with_auth_pool_and_oidc(
            pool.clone(),
            pool,
            OidcConfig {
                bot_id: "12345".to_owned(),
                client_secret: Arc::new(Secret::new("test-client-secret".to_owned())),
                signing_algorithm: OidcSigningAlgorithm::Rs256,
                redirect_url: "https://console.example/auth/oidc/callback".to_owned(),
                authorization_endpoint: OIDC_AUTHORIZATION_ENDPOINT.to_owned(),
                token_endpoint: OIDC_TOKEN_ENDPOINT.to_owned(),
                issuer: OIDC_ISSUER.to_owned(),
                jwks_endpoint: OIDC_JWKS_ENDPOINT.to_owned(),
            },
        );
        let app = router(state);
        let start = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/auth/oidc/start")
                    .body(Body::empty())
                    .expect("构造 OIDC 起始请求失败"),
            )
            .await
            .expect("执行 OIDC 起始请求失败");
        let location = start
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("OIDC 起始响应缺少 Location")
            .to_owned();
        let state_value = location
            .split("state=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("OIDC 起始 URL 缺少 state 值")
            .to_owned();
        let state_cookie = start
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("OIDC 起始响应缺少 state cookie")
            .split(';')
            .next()
            .expect("OIDC state cookie 为空")
            .to_owned();

        let missing_code = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/auth/oidc/callback?state={state_value}"))
                    .header(header::COOKIE, &state_cookie)
                    .body(Body::empty())
                    .expect("构造缺少 code 的回调请求失败"),
            )
            .await
            .expect("执行缺少 code 的回调请求失败");
        assert_eq!(missing_code.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            missing_code
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let cleared_cookie = missing_code
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("缺少 code 的回调响应缺少清理 state cookie");
        assert!(cleared_cookie.contains("Max-Age=0"));

        let replay = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/auth/oidc/callback?code=code-1&state={state_value}"
                    ))
                    .header(header::COOKIE, &state_cookie)
                    .body(Body::empty())
                    .expect("构造重放回调请求失败"),
            )
            .await
            .expect("执行重放回调请求失败");
        assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    }
}
