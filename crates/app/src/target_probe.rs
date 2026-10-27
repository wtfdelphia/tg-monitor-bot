use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use sqlx::PgPool;
use tgm_core::ids::TenantId;
use tgm_core::kek::Kek;
use tgm_core::secret::Secret;
use tgm_db::credentials::load_bot_credentials;
use tgm_db::target_probe::{ProbeClaim, ProbeOutcome, ProbeSuccess, claim_probe, finish_probe};
use tokio::sync::watch;

use crate::bot_console::{TelegramBotApiClient, TelegramBotApiError};
use crate::delivery_runtime::bot_id_from_token;

#[derive(Deserialize)]
pub struct ProbeChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub has_protected_content: bool,
    pub permissions: Option<ProbePermissions>,
}

#[derive(Deserialize)]
pub struct ProbePermissions {
    pub can_send_messages: Option<bool>,
}

#[derive(Deserialize)]
pub struct ProbeMember {
    pub user: ProbeUser,
    pub status: String,
    pub can_post_messages: Option<bool>,
    pub is_member: Option<bool>,
    pub can_send_messages: Option<bool>,
}

#[derive(Deserialize)]
pub struct ProbeUser {
    pub id: i64,
    pub is_bot: bool,
}

pub fn evaluate(
    chat: ProbeChat,
    member: ProbeMember,
    chat_id: i64,
    bot_id: i64,
) -> Option<ProbeSuccess> {
    if chat.id != chat_id || member.user.id != bot_id || !member.user.is_bot {
        return None;
    }
    let can_send = match (chat.kind.as_str(), member.status.as_str()) {
        ("channel" | "group" | "supergroup", "left" | "kicked") => false,
        ("channel" | "group" | "supergroup", "creator") => true,
        ("channel", "administrator") => member.can_post_messages?,
        ("channel", "member") => false,
        ("group" | "supergroup", "administrator") => true,
        ("group" | "supergroup", "member") => chat.permissions?.can_send_messages.unwrap_or(false),
        ("supergroup", "restricted") => member.is_member? && member.can_send_messages?,
        _ => return None,
    };
    Some(ProbeSuccess {
        can_send,
        content_protected: chat.has_protected_content,
        bot_role: member.status,
    })
}

impl TelegramBotApiClient {
    pub async fn probe_target(
        &self,
        chat_id: i64,
        bot_id: i64,
    ) -> Result<Option<ProbeSuccess>, TelegramBotApiError> {
        self.probe_target_admitted(chat_id, bot_id, None).await
    }

    async fn probe_target_admitted(
        &self,
        chat_id: i64,
        bot_id: i64,
        lifecycle: Option<(&PgPool, TenantId)>,
    ) -> Result<Option<ProbeSuccess>, TelegramBotApiError> {
        let _first = probe_admission(lifecycle).await?;
        let chat = self
            .request("getChat", &serde_json::json!({"chat_id": chat_id}))
            .await?
            .ok_or_else(|| TelegramBotApiError::Api("result missing".to_owned()))?;
        let chat: ProbeChat =
            serde_json::from_value(chat).map_err(TelegramBotApiError::Response)?;
        if chat.id != chat_id || !matches!(chat.kind.as_str(), "channel" | "group" | "supergroup") {
            return Ok(None);
        }
        let _second = probe_admission(lifecycle).await?;
        let member = self
            .request(
                "getChatMember",
                &serde_json::json!({"chat_id": chat_id, "user_id": bot_id}),
            )
            .await?
            .ok_or_else(|| TelegramBotApiError::Api("result missing".to_owned()))?;
        let member = serde_json::from_value(member).map_err(TelegramBotApiError::Response)?;
        Ok(evaluate(chat, member, chat_id, bot_id))
    }
}

async fn probe_admission(
    lifecycle: Option<(&PgPool, TenantId)>,
) -> Result<Option<tgm_db::lifecycle::WorkerGuard>, TelegramBotApiError> {
    let Some((pool, tenant)) = lifecycle else {
        return Ok(None);
    };
    tgm_db::lifecycle::WorkerGuard::acquire(pool, tenant)
        .await
        .map_err(|_| TelegramBotApiError::Api("tenant admission unavailable".to_owned()))?
        .map(Some)
        .ok_or_else(|| TelegramBotApiError::Api("tenant inactive".to_owned()))
}

fn failure(code: &'static str) -> ProbeOutcome {
    ProbeOutcome::Failure {
        code,
        retry_seconds: None,
        rate_limited: false,
    }
}

pub async fn execute(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    claim: &ProbeClaim,
    base_url: &str,
) -> ProbeOutcome {
    if claim.kind != "bot" || matches!(claim.identity_status.as_str(), "banned" | "unauthorized") {
        return failure("IDENTITY_UNAVAILABLE");
    }
    let credentials = match load_bot_credentials(pool, kek, tenant, claim.sender_identity_id).await
    {
        Ok(Some(credentials)) => credentials,
        Ok(None) => return failure("CREDENTIAL_MISSING"),
        Err(_) => {
            return ProbeOutcome::Failure {
                code: "CREDENTIAL_UNAVAILABLE",
                retry_seconds: Some(30),
                rate_limited: false,
            };
        }
    };
    if claim.credential_version.as_deref() != Some(credentials.version()) {
        return failure("CREDENTIAL_CHANGED");
    }
    let Ok(token) = std::str::from_utf8(credentials.token.expose()) else {
        return failure("CREDENTIAL_INVALID");
    };
    let Some(bot_id) = bot_id_from_token(token) else {
        return failure("CREDENTIAL_INVALID");
    };
    if claim.tg_user_id != Some(bot_id) {
        return failure("IDENTITY_MISMATCH");
    }
    let client = match TelegramBotApiClient::new(Arc::new(Secret::new(token.to_owned())), base_url)
    {
        Ok(client) => client,
        Err(_) => return failure("CLIENT_UNAVAILABLE"),
    };
    match client
        .probe_target_admitted(claim.tg_chat_id, bot_id, Some((pool, tenant)))
        .await
    {
        Ok(Some(result)) => ProbeOutcome::Success(result),
        Ok(None) => failure("UNSUPPORTED_RESPONSE"),
        Err(TelegramBotApiError::Rejected {
            status: 429,
            retry_after,
            ..
        }) => ProbeOutcome::Failure {
            code: "RATE_LIMITED",
            retry_seconds: Some(
                i32::try_from(retry_after.unwrap_or(60))
                    .unwrap_or(i32::MAX)
                    .max(1),
            ),
            rate_limited: true,
        },
        Err(TelegramBotApiError::Rejected { .. }) => failure("TELEGRAM_REJECTED"),
        Err(TelegramBotApiError::Request | TelegramBotApiError::Api(_)) => ProbeOutcome::Failure {
            code: "TELEGRAM_UNAVAILABLE",
            retry_seconds: Some(30),
            rate_limited: false,
        },
        Err(_) => failure("INVALID_RESPONSE"),
    }
}

pub async fn run(pool: PgPool, kek: Arc<dyn Kek>, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return; } }
            _ = interval.tick() => {}
        }
        if *shutdown.borrow() {
            return;
        }
        let Ok(tenants) = tgm_db::repo::list_all_active_tenants(&pool).await else {
            continue;
        };
        for tenant in tenants {
            if *shutdown.borrow() {
                return;
            }
            let mut random = [0_u8; 16];
            if getrandom::fill(&mut random).is_err() {
                continue;
            }
            use base64::Engine as _;
            let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
            let claim = match claim_probe(&pool, tenant, token).await {
                Ok(Some(claim)) => claim,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(error = %error, "目标自检认领失败");
                    continue;
                }
            };
            let outcome = tokio::select! {
                changed = shutdown.changed() => { let _ = changed; return; }
                outcome = tokio::time::timeout(Duration::from_secs(40), execute(&pool, kek.as_ref(), tenant, &claim, crate::bot_console::TELEGRAM_BOT_API_DEFAULT_BASE_URL)) => {
                    outcome.unwrap_or(ProbeOutcome::Failure { code: "PROBE_TIMEOUT", retry_seconds: Some(30), rate_limited: false })
                },
            };
            if finish_probe(&pool, tenant, claim, outcome).await.is_err() {
                tracing::warn!("目标自检回写失败，等待租约恢复");
            }
        }
    }
}
