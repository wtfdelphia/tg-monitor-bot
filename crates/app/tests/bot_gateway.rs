#![allow(clippy::expect_used, clippy::panic)]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sqlx::PgPool;
use tgm_app::bot_gateway::{BotGatewayState, authorize_callback, list_member_contexts, router};
use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
use tgm_core::ids::IdentityId;
use tower::ServiceExt;

mod support;

#[sqlx::test(migrations = "../../migrations-test")]
async fn webhook_requires_secret_header(admin: PgPool) {
    let receiver_identity_id = sqlx::query_scalar(
        "INSERT INTO identities (kind, status) VALUES ('bot', 'offline') RETURNING id",
    )
    .fetch_one(&admin)
    .await
    .expect("创建 Webhook 身份失败");
    let app = router(BotGatewayState {
        pool: admin.clone(),
        auth_pool: admin.clone(),
        secret_token: "gateway-secret".to_owned(),
        receiver_identity_id,
    });

    let missing = app
        .clone()
        .oneshot(
            Request::post("/webhook")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"update_id":1}"#))
                .expect("构造请求失败"),
        )
        .await
        .expect("无 secret 请求失败");
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let accepted = app
        .oneshot(
            Request::post("/webhook")
                .header("x-telegram-bot-api-secret-token", "gateway-secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"update_id":1}"#))
                .expect("构造请求失败"),
        )
        .await
        .expect("有 secret 请求失败");
    assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn webhook_ingests_channel_post_and_deduplicates_update_id(admin: PgPool) {
    let (app_pool, admin) = support::pools(admin).await;
    let current_user: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app_pool)
        .await
        .expect("读取当前测试角色失败");
    assert_eq!(current_user, "app_user");
    let tenant = support::seed_tenant(&admin, "bot-webhook", 7301).await;
    let receiver_identity_id = support::seed_identity(&admin, tenant).await;
    let auth_pool = support::auth_pool(&admin).await;
    let app = router(BotGatewayState {
        pool: app_pool,
        auth_pool,
        secret_token: "gateway-secret".to_owned(),
        receiver_identity_id,
    });
    let update = r#"{
        "update_id": 73001,
        "channel_post": {
            "message_id": 44,
            "date": 1760000000,
            "chat": {"id": -1007301},
            "sender_chat": {"id": -1007301},
            "text": "hello from channel"
        }
    }"#;

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::post("/webhook")
                    .header("x-telegram-bot-api-secret-token", "gateway-secret")
                    .header("content-type", "application/json")
                    .body(Body::from(update))
                    .expect("构造 Webhook 请求失败"),
            )
            .await
            .expect("Webhook 请求失败");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let row = sqlx::query_as::<_, (i64, String, i64, Option<String>, Option<String>)>(
        "SELECT id, event_type, tg_chat_id, text, media_type
           FROM canonical_events
          WHERE receiver_identity_id = $1
            AND update_id = $2",
    )
    .bind(receiver_identity_id)
    .bind(73001_i64)
    .fetch_one(admin.raw())
    .await
    .expect("读取 canonical event 失败");
    assert_eq!(row.1, "MESSAGE_CREATED");
    assert_eq!(row.2, -1007301);
    assert_eq!(row.3.as_deref(), Some("hello from channel"));
    assert_eq!(row.4, None);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn callback_lookup_binds_resource_id_to_the_authenticated_tenant(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "bot-resource-a", 7101).await;
    let tenant_b = support::seed_tenant(&admin, "bot-resource-b", 7102).await;
    let identity_a: IdentityId = support::seed_identity(&admin, tenant_a).await;
    let identity_b: IdentityId = support::seed_identity(&admin, tenant_b).await;
    let auth = AuthContext::new(tenant_a, 7101, TenantRole::Admin, AuthSource::Bot);

    assert!(
        authorize_callback(&app, auth, &format!("identity:{}", identity_a.get()))
            .await
            .is_ok()
    );
    let error = authorize_callback(&app, auth, &format!("identity:{}", identity_b.get()))
        .await
        .expect_err("跨租户身份 callback 未被隐藏");
    assert_eq!(error.code(), "NOT_FOUND");
    assert_eq!(error.http_status(), 404);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_member_lookup_uses_auth_lookup_context(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "bot-members", 7201).await;
    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role)
         VALUES ($1, $2, 'TENANT_ADMIN')",
    )
    .bind(tenant)
    .bind(7201_i64)
    .execute(admin.raw())
    .await
    .expect("创建 Bot 成员关系失败");

    let contexts = list_member_contexts(&auth_pool, 7201)
        .await
        .expect("Bot 成员反查失败");
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].tenant_id(), tenant);
    assert_eq!(contexts[0].actor_user_id(), 7201);
    drop(app);
}
