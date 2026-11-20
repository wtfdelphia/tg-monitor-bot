//! delivery 运行链的发送身份判据（ADR-0033、spec/08 §1.4），以受限角色 app_user 连接。
//!
//! 不连 Telegram：bot_sign_in 之后要真机，这里验的是登录之前的每一个数据库判据。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Arc;

use tgm_app::delivery_runtime::{
    DeliveryRuntimeConfig, PlatformApp, SenderExit, load_bot_token, run_sender,
};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::store_identity_secret;
use tgm_db::repo::{list_delivery_sender_candidates, mark_bot_sender_unauthorized};
use tokio::sync::watch;

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

async fn app_creds() -> PlatformApp {
    PlatformApp {
        api_id: 12_345,
        api_hash: Arc::new(Secret::new("0123456789abcdef0123456789abcdef".to_owned())),
        media_store: support::media_store("tgm-test-runtime").await,
    }
}

async fn seed(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    kind: &str,
    status: &str,
) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(kind)
    .bind(tenant)
    .bind(status)
    .fetch_one(admin.raw())
    .await
    .expect("创建身份失败")
}

/// 直接写目标频道：模拟应用层校验落地前的历史数据（mtproto 被填成发送身份）。
async fn seed_target(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    sender: IdentityId,
    chat: i64,
    enabled: bool,
) {
    sqlx::query(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant)
    .bind(chat)
    .bind(sender)
    .bind(enabled)
    .execute(admin.raw())
    .await
    .expect("创建目标频道失败");
}

async fn store(app: &sqlx::PgPool, tenant: TenantId, identity: IdentityId, plain: &[u8]) {
    store_identity_secret(app, &kek(), tenant, identity, &Secret::new(plain.to_vec()))
        .await
        .expect("写入凭据失败");
}

async fn status_and_audits(admin: &support::AdminOnlyPool, identity: IdentityId) -> (String, i64) {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT i.status,
                (SELECT count(*) FROM audit_logs a
                  WHERE a.resource = 'identity' AND a.resource_id = i.id
                    AND a.action = 'identity.unauthorized' AND a.source = 'system')
           FROM identities i WHERE i.id = $1",
    )
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("读取身份状态失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn sender_candidates_are_bots_with_token_and_live_work(admin: sqlx::PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-a", 8101).await;
    let other = support::seed_tenant(&admin, "delivery-b", 8102).await;

    let bot = seed(&admin, tenant, "bot", "offline").await;
    let user = seed(&admin, tenant, "mtproto", "online").await;
    let no_token = seed(&admin, tenant, "bot", "offline").await;
    let unauthorized = seed(&admin, tenant, "bot", "unauthorized").await;
    let disabled_only = seed(&admin, tenant, "bot", "offline").await;
    let backlog = seed(&admin, tenant, "bot", "offline").await;
    let foreign = seed(&admin, other, "bot", "offline").await;

    for identity in [bot, user, unauthorized, disabled_only, backlog] {
        store(&app, tenant, identity, b"123456:AAbbCCddEEffGGhhIIjjKK").await;
    }
    store(&app, other, foreign, b"123456:AAbbCCddEEffGGhhIIjjKK").await;

    seed_target(&admin, tenant, bot, -1_001, true).await;
    seed_target(&admin, tenant, user, -1_002, true).await;
    seed_target(&admin, tenant, no_token, -1_003, true).await;
    seed_target(&admin, tenant, unauthorized, -1_004, true).await;
    seed_target(&admin, tenant, disabled_only, -1_005, false).await;
    seed_target(&admin, tenant, backlog, -1_006, false).await;
    seed_target(&admin, other, foreign, -1_007, true).await;
    sqlx::query(
        "INSERT INTO delivery_tasks (tenant_id, sender_identity_id, target_tg_chat_id, action)
         VALUES ($1, $2, -1006, 'COPY')",
    )
    .bind(tenant)
    .bind(backlog)
    .execute(admin.raw())
    .await
    .expect("创建积压任务失败");

    let candidates = list_delivery_sender_candidates(&app, tenant)
        .await
        .expect("列出发送身份失败");
    assert_eq!(candidates, vec![bot, backlog]);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn malformed_token_marks_bot_unauthorized_once(admin: sqlx::PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-bad-token", 8201).await;
    let bot = seed(&admin, tenant, "bot", "online").await;
    store(&app, tenant, bot, b"not-a-bot-token").await;

    assert!(matches!(
        load_bot_token(&app, &kek(), tenant, bot).await,
        Ok(Some(None))
    ));
    let (_tx, shutdown) = watch::channel(false);
    let exit = run_sender(
        &app,
        &kek(),
        &app_creds().await,
        tenant,
        bot,
        DeliveryRuntimeConfig::default(),
        shutdown,
    )
    .await
    .expect("运行发送身份失败");
    assert_eq!(exit, SenderExit::TokenInvalid);
    assert_eq!(
        status_and_audits(&admin, bot).await,
        ("unauthorized".to_owned(), 1)
    );

    assert!(
        !mark_bot_sender_unauthorized(&app, tenant, bot)
            .await
            .expect("重复标记失败")
    );
    assert_eq!(
        status_and_audits(&admin, bot).await,
        ("unauthorized".to_owned(), 1)
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn unauthorized_mark_is_bot_only_and_tenant_scoped(admin: sqlx::PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-scope-a", 8301).await;
    let other = support::seed_tenant(&admin, "delivery-scope-b", 8302).await;
    let user = seed(&admin, tenant, "mtproto", "online").await;
    let foreign_bot = seed(&admin, other, "bot", "online").await;

    assert!(
        !mark_bot_sender_unauthorized(&app, tenant, user)
            .await
            .expect("标记 mtproto 失败")
    );
    assert!(
        !mark_bot_sender_unauthorized(&app, tenant, foreign_bot)
            .await
            .expect("跨租户标记失败")
    );
    assert_eq!(
        status_and_audits(&admin, user).await,
        ("online".to_owned(), 0)
    );
    assert_eq!(
        status_and_audits(&admin, foreign_bot).await,
        ("online".to_owned(), 0)
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn missing_token_exits_without_touching_status(admin: sqlx::PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-no-token", 8401).await;
    let bot = seed(&admin, tenant, "bot", "offline").await;
    let (_tx, shutdown) = watch::channel(false);
    let exit = run_sender(
        &app,
        &kek(),
        &app_creds().await,
        tenant,
        bot,
        DeliveryRuntimeConfig::default(),
        shutdown,
    )
    .await
    .expect("运行发送身份失败");
    assert_eq!(exit, SenderExit::NoToken);
    assert_eq!(
        status_and_audits(&admin, bot).await,
        ("offline".to_owned(), 0)
    );
}
