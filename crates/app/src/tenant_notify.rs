//! 系统通知租户（spec/08 §五 第 2 条；`docs/plan/proposals/WP-2-remaining.md` R2-A）。
//!
//! 入队发生在身份转入 `unauthorized` / `banned` 的同一事务里（`tgm_db::repo`），
//! 本模块只负责发送：control-plane 周期扫描各租户到期的 PENDING 通知，
//! 经控制 Bot 私聊 `tenants.owner_user_id`。
//!
//! 结局分三种：
//! - 成功 → SENT，不再发送。
//! - 403 / 400（收件人从未私聊 Bot、已拉黑、chat 不存在）→ DEAD，不重试：
//!   重发只会得到同一个拒绝。
//! - 429、5xx、网络错误 → 指数退避，第 [`MAX_ATTEMPTS`] 次仍失败则 DEAD。

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tgm_core::ids::TenantId;
use tgm_db::repo::{
    DueTenantNotification, TenantNotificationOutcome, list_all_active_tenants,
    list_due_tenant_notifications, record_tenant_notification,
};
use tokio::sync::watch;

use crate::bot_console::{TelegramBotApiClient, TelegramBotApiError};

/// 单条通知的最多发送次数（含首次）。
pub const MAX_ATTEMPTS: i32 = 5;
/// 每租户每轮最多发送条数，防止一个租户的积压挡住其他租户。
const BATCH: i64 = 20;

#[derive(Debug, Clone, Copy)]
pub struct TenantNotifyConfig {
    pub interval: Duration,
}

impl Default for TenantNotifyConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
        }
    }
}

/// 通知正文。只含身份 id 与可做的动作，不含 session、手机号等任何敏感信息。
/// 首发单语种（中文）。走 `parse_mode=HTML`，正文里没有需要转义的字符。
pub fn notification_text(kind: &str, identity_id: i64) -> String {
    match kind {
        "identity.banned" => {
            format!("监控账号 #{identity_id} 已被 Telegram 封禁或注销，采集已停止，无法自动恢复。")
        }
        _ => format!(
            "监控账号 #{identity_id} 的 Telegram 授权已失效，采集已停止。请在控制台重新扫码登录。"
        ),
    }
}

/// 第 `attempt`（从 1 起）次失败后的等待：60s、120s、240s……
fn backoff_seconds(attempt: i32) -> i64 {
    60_i64 << (attempt.clamp(1, 10) - 1)
}

/// 把一次发送结果映射为落库结局。`attempt_count` 是本次之前已失败的次数。
pub fn classify(
    result: Result<(), TelegramBotApiError>,
    attempt_count: i32,
) -> TenantNotificationOutcome {
    let error = match result {
        Ok(()) => return TenantNotificationOutcome::Sent,
        Err(error) => error,
    };
    let error_code = match &error {
        TelegramBotApiError::Rejected { status, .. } => format!("HTTP_{status}"),
        TelegramBotApiError::Request => "REQUEST".to_owned(),
        TelegramBotApiError::ResponseTooLarge => "RESPONSE_TOO_LARGE".to_owned(),
        TelegramBotApiError::Response(_) => "RESPONSE".to_owned(),
        TelegramBotApiError::Api(_) => "API".to_owned(),
        TelegramBotApiError::EmptyToken | TelegramBotApiError::InvalidBaseUrl => {
            "CONFIG".to_owned()
        }
    };
    let permanent = matches!(
        error,
        TelegramBotApiError::Rejected { status, .. } if status != 429
    );
    let attempt = attempt_count.saturating_add(1);
    if permanent || attempt >= MAX_ATTEMPTS {
        TenantNotificationOutcome::Dead { error_code }
    } else {
        TenantNotificationOutcome::Retry {
            error_code,
            retry_after_seconds: backoff_seconds(attempt),
        }
    }
}

/// 发送本租户到期的通知，返回本轮推进的条数。
pub async fn deliver_tenant(
    pool: &PgPool,
    bot_api: &TelegramBotApiClient,
    tenant: TenantId,
) -> Result<usize, sqlx::Error> {
    let due = list_due_tenant_notifications(pool, tenant, BATCH).await?;
    let mut advanced = 0;
    for DueTenantNotification {
        id,
        kind,
        identity_id,
        owner_user_id,
        attempt_count,
    } in due
    {
        let text = notification_text(&kind, identity_id.get());
        let outcome = classify(
            bot_api.send_status_message(owner_user_id, &text).await,
            attempt_count,
        );
        if let TenantNotificationOutcome::Dead { error_code } = &outcome {
            tracing::warn!(
                tenant_id = tenant.get(),
                notification_id = id,
                error_code = %error_code,
                "租户通知无法送达，置 DEAD"
            );
        }
        if record_tenant_notification(pool, tenant, id, outcome).await? {
            advanced += 1;
        }
    }
    Ok(advanced)
}

/// 一轮：逐个活跃租户发送。单个租户出错只记日志，不挡其他租户。
pub async fn deliver_once(
    pool: &PgPool,
    bot_api: &TelegramBotApiClient,
) -> Result<usize, sqlx::Error> {
    let mut advanced = 0;
    for tenant in list_all_active_tenants(pool).await? {
        match deliver_tenant(pool, bot_api, tenant).await {
            Ok(count) => advanced += count,
            Err(error) => {
                tracing::warn!(tenant_id = tenant.get(), error = %error, "租户通知发送失败，下轮重试");
            }
        }
    }
    Ok(advanced)
}

/// control-plane 的后台循环，收到停止信号后退出。
// ponytail: 每轮遍历全部活跃租户各查一次，租户数上千时改为单条跨租户到期索引查询
pub async fn run(
    pool: PgPool,
    bot_api: Arc<TelegramBotApiClient>,
    config: TenantNotifyConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(config.interval);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            _ = tick.tick() => {
                if let Err(error) = deliver_once(&pool, &bot_api).await {
                    tracing::warn!(error = %error, "租户通知扫描失败，下轮重试");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(status: u16) -> Result<(), TelegramBotApiError> {
        Err(TelegramBotApiError::Rejected {
            status,
            description: "x".to_owned(),
            retry_after: None,
        })
    }

    #[test]
    fn forbidden_is_dead_without_retry() {
        assert_eq!(
            classify(rejected(403), 0),
            TenantNotificationOutcome::Dead {
                error_code: "HTTP_403".to_owned()
            }
        );
        assert!(matches!(
            classify(rejected(400), 0),
            TenantNotificationOutcome::Dead { .. }
        ));
    }

    #[test]
    fn rate_limit_and_network_back_off_until_exhausted() {
        assert_eq!(
            classify(rejected(429), 0),
            TenantNotificationOutcome::Retry {
                error_code: "HTTP_429".to_owned(),
                retry_after_seconds: 60
            }
        );
        assert_eq!(
            classify(Err(TelegramBotApiError::Request), 2),
            TenantNotificationOutcome::Retry {
                error_code: "REQUEST".to_owned(),
                retry_after_seconds: 240
            }
        );
        assert!(matches!(
            classify(Err(TelegramBotApiError::Request), MAX_ATTEMPTS - 1),
            TenantNotificationOutcome::Dead { .. }
        ));
        assert_eq!(classify(Ok(()), 3), TenantNotificationOutcome::Sent);
    }

    #[test]
    fn text_distinguishes_banned_and_has_no_secrets() {
        let banned = notification_text("identity.banned", 7);
        let unauthorized = notification_text("identity.unauthorized", 7);
        assert!(banned.contains("封禁") && !banned.contains("扫码"));
        assert!(unauthorized.contains("重新扫码"));
        for text in [banned, unauthorized] {
            assert!(!text.contains('<') && !text.contains('&'));
        }
    }
}
