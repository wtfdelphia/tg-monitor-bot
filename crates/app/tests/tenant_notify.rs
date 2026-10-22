//! WP-2-remaining R2-A：身份失去授权 → 通知租户（spec/08 §五 第 2 条），以受限角色 app_user 连接。
//!
//! 入队与转状态同事务；发送循环经 mock 的 Bot API，验 SENT / DEAD / 退避与只发一次。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use sqlx::PgPool;
use tgm_app::bot_console::TelegramBotApiClient;
use tgm_app::tenant_notify::{MAX_ATTEMPTS, deliver_once, deliver_tenant};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::secret::Secret;
use tgm_db::repo::{
    LeaseGuard, LostAuthorization, acquire_lease, mark_bot_sender_unauthorized,
    mark_leased_identity_lost, mark_leased_identity_unauthorized,
};

async fn seed(admin: &support::AdminOnlyPool, tenant: TenantId, kind: &str) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ($1, $2, 'online') RETURNING id",
    )
    .bind(kind)
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建身份失败")
}

async fn lease(app: &PgPool, identity: IdentityId, worker: &str) -> LeaseGuard {
    let fencing_token = acquire_lease(app, identity, worker)
        .await
        .expect("抢租约失败")
        .expect("租约应可获取");
    LeaseGuard {
        identity_id: identity,
        worker_id: worker.to_owned(),
        fencing_token,
    }
}

/// (kind, status, attempt_count, last_error_code)，以 admin 读，按 id 排序。
async fn rows(
    admin: &support::AdminOnlyPool,
    identity: IdentityId,
) -> Vec<(String, String, i32, Option<String>)> {
    sqlx::query_as(
        "SELECT kind, status, attempt_count, last_error_code FROM tenant_notifications
          WHERE identity_id = $1 ORDER BY id",
    )
    .bind(identity)
    .fetch_all(admin.raw())
    .await
    .expect("读通知失败")
}

#[derive(Clone, Default)]
struct Mock {
    sent: Arc<Mutex<Vec<serde_json::Value>>>,
    reply: Arc<Mutex<(u16, &'static str)>>,
}

async fn handle(State(mock): State<Mock>, body: String) -> (StatusCode, String) {
    let payload: serde_json::Value = serde_json::from_str(&body).expect("请求体不是 JSON");
    mock.sent.lock().expect("锁").push(payload);
    let (status, body) = *mock.reply.lock().expect("锁");
    (
        StatusCode::from_u16(status).expect("状态码"),
        body.to_owned(),
    )
}

const OK: (u16, &str) = (200, r#"{"ok":true,"result":{}}"#);
const FORBIDDEN: (u16, &str) = (
    403,
    r#"{"ok":false,"description":"Forbidden: bot can't initiate conversation with a user"}"#,
);
const TOO_MANY: (u16, &str) = (429, r#"{"ok":false,"description":"Too Many Requests"}"#);

async fn mock_bot_api(reply: (u16, &'static str)) -> (TelegramBotApiClient, Mock) {
    let mock = Mock::default();
    *mock.reply.lock().expect("锁") = reply;
    let app = Router::new()
        .route("/{*path}", post(handle))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定 mock 失败");
    let address = listener.local_addr().expect("mock 地址");
    tokio::spawn(async move { axum::serve(listener, app).await });
    let client = TelegramBotApiClient::new(
        Arc::new(Secret::new("123456:test".to_owned())),
        format!("http://{address}"),
    )
    .expect("建 Bot API client 失败");
    (client, mock)
}

/// 转 unauthorized 的同一事务里入队恰好一条；租约已丢失的僵尸 worker 什么都不写。
#[sqlx::test(migrations = "../../migrations-test")]
async fn lost_authorization_enqueues_exactly_one_in_same_transaction(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "tn-a", 7101).await;
    let identity = seed(&admin, tenant, "mtproto").await;
    let zombie = lease(&app, identity, "worker-a").await;
    sqlx::query("UPDATE account_leases SET expires_at = clock_timestamp() - interval '1 second'")
        .execute(admin.raw())
        .await
        .expect("让租约过期失败");
    let holder = lease(&app, identity, "worker-b").await;

    assert!(
        !mark_leased_identity_unauthorized(&app, tenant, &zombie)
            .await
            .expect("标记失败")
    );
    assert!(rows(&admin, identity).await.is_empty(), "僵尸不得入队");

    assert!(
        mark_leased_identity_unauthorized(&app, tenant, &holder)
            .await
            .expect("标记失败")
    );
    assert_eq!(
        rows(&admin, identity).await,
        vec![(
            "identity.unauthorized".to_owned(),
            "PENDING".to_owned(),
            0,
            None
        )]
    );
}

/// banned 走同一形状：状态、审计 action、通知 kind 都是 banned。
#[sqlx::test(migrations = "../../migrations-test")]
async fn banned_marks_status_audit_and_notification(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "tn-b", 7102).await;
    let identity = seed(&admin, tenant, "mtproto").await;
    let holder = lease(&app, identity, "worker-a").await;

    assert!(
        mark_leased_identity_lost(&app, tenant, &holder, LostAuthorization::Banned)
            .await
            .expect("标记失败")
    );
    let (status, audits): (String, i64) = sqlx::query_as(
        "SELECT i.status,
                (SELECT count(*) FROM audit_logs a
                  WHERE a.resource_id = i.id AND a.action = 'identity.banned' AND a.source = 'system')
           FROM identities i WHERE i.id = $1",
    )
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("读状态失败");
    assert_eq!((status.as_str(), audits), ("banned", 1));
    assert_eq!(rows(&admin, identity).await[0].0, "identity.banned");
    assert!(
        acquire_lease(&app, identity, "worker-b")
            .await
            .expect("抢租约失败")
            .is_some(),
        "封禁后租约必须立即可接管"
    );
}

/// 发送 Bot 的 Token 被拒同样通知；已是 unauthorized 时不重复入队。
#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_sender_unauthorized_enqueues_once(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "tn-c", 7103).await;
    let bot = seed(&admin, tenant, "bot").await;
    assert!(
        mark_bot_sender_unauthorized(&app, tenant, bot)
            .await
            .expect("标记失败")
    );
    assert!(
        !mark_bot_sender_unauthorized(&app, tenant, bot)
            .await
            .expect("标记失败")
    );
    assert_eq!(rows(&admin, bot).await.len(), 1);
}

/// 发给 owner，成功置 SENT；第二轮不再发。
#[sqlx::test(migrations = "../../migrations-test")]
async fn delivery_sends_to_owner_once(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "tn-d", 7104).await;
    let identity = seed(&admin, tenant, "mtproto").await;
    let holder = lease(&app, identity, "worker-a").await;
    mark_leased_identity_unauthorized(&app, tenant, &holder)
        .await
        .expect("标记失败");

    let (client, mock) = mock_bot_api(OK).await;
    assert_eq!(deliver_once(&app, &client).await.expect("发送失败"), 1);
    assert_eq!(deliver_once(&app, &client).await.expect("发送失败"), 0);
    let sent = mock.sent.lock().expect("锁").clone();
    assert_eq!(sent.len(), 1, "只发一次");
    assert_eq!(sent[0]["chat_id"], 7104);
    let text = sent[0]["text"].as_str().expect("text");
    assert!(text.contains(&format!("#{}", identity.get())) && text.contains("重新扫码"));
    let state = rows(&admin, identity).await;
    assert_eq!(state[0].1, "SENT");
    assert_eq!(state[0].2, 1);
}

/// 403（owner 没私聊过 Bot）直接 DEAD，不重试。
#[sqlx::test(migrations = "../../migrations-test")]
async fn forbidden_is_dead_and_not_retried(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "tn-e", 7105).await;
    let identity = seed(&admin, tenant, "mtproto").await;
    let holder = lease(&app, identity, "worker-a").await;
    mark_leased_identity_unauthorized(&app, tenant, &holder)
        .await
        .expect("标记失败");

    let (client, mock) = mock_bot_api(FORBIDDEN).await;
    deliver_tenant(&app, &client, tenant)
        .await
        .expect("发送失败");
    sqlx::query("UPDATE tenant_notifications SET next_retry_at = clock_timestamp()")
        .execute(admin.raw())
        .await
        .expect("改时间失败");
    deliver_tenant(&app, &client, tenant)
        .await
        .expect("发送失败");
    assert_eq!(mock.sent.lock().expect("锁").len(), 1);
    assert_eq!(
        rows(&admin, identity).await[0],
        (
            "identity.unauthorized".to_owned(),
            "DEAD".to_owned(),
            1,
            Some("HTTP_403".to_owned())
        )
    );
}

/// 429 退避：未到期不发；用尽 MAX_ATTEMPTS 后 DEAD。
#[sqlx::test(migrations = "../../migrations-test")]
async fn rate_limited_backs_off_then_dead(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "tn-f", 7106).await;
    let identity = seed(&admin, tenant, "mtproto").await;
    let holder = lease(&app, identity, "worker-a").await;
    mark_leased_identity_unauthorized(&app, tenant, &holder)
        .await
        .expect("标记失败");

    let (client, mock) = mock_bot_api(TOO_MANY).await;
    deliver_tenant(&app, &client, tenant)
        .await
        .expect("发送失败");
    let waited: bool = sqlx::query_scalar(
        "SELECT next_retry_at > clock_timestamp() + interval '50 seconds' FROM tenant_notifications",
    )
    .fetch_one(admin.raw())
    .await
    .expect("读重试时间失败");
    assert!(waited, "第一次失败应退避约 60s");
    deliver_tenant(&app, &client, tenant)
        .await
        .expect("发送失败");
    assert_eq!(mock.sent.lock().expect("锁").len(), 1, "未到期不得重发");

    for _ in 1..MAX_ATTEMPTS {
        sqlx::query("UPDATE tenant_notifications SET next_retry_at = clock_timestamp()")
            .execute(admin.raw())
            .await
            .expect("改时间失败");
        deliver_tenant(&app, &client, tenant)
            .await
            .expect("发送失败");
    }
    assert_eq!(
        mock.sent.lock().expect("锁").len(),
        usize::try_from(MAX_ATTEMPTS).expect("正数")
    );
    let state = rows(&admin, identity).await;
    assert_eq!((state[0].1.as_str(), state[0].2), ("DEAD", MAX_ATTEMPTS));
}

/// RLS：租户 B 的上下文里读不到、也改不动 A 的通知；不设上下文直接报错。
#[sqlx::test(migrations = "../../migrations-test")]
async fn notifications_are_tenant_isolated(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "tn-g", 7107).await;
    let tenant_b = support::seed_tenant(&admin, "tn-h", 7108).await;
    let identity = seed(&admin, tenant_a, "mtproto").await;
    let holder = lease(&app, identity, "worker-a").await;
    mark_leased_identity_unauthorized(&app, tenant_a, &holder)
        .await
        .expect("标记失败");

    let mut tx = app.begin().await.expect("开事务失败");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_b.get().to_string())
        .execute(&mut *tx)
        .await
        .expect("设上下文失败");
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_notifications")
        .fetch_one(&mut *tx)
        .await
        .expect("查询失败");
    assert_eq!(visible, 0);
    let updated = sqlx::query("UPDATE tenant_notifications SET status = 'SENT', sent_at = now()")
        .execute(&mut *tx)
        .await
        .expect("更新失败");
    assert_eq!(updated.rows_affected(), 0);
    let forged = sqlx::query(
        "INSERT INTO tenant_notifications (tenant_id, kind, identity_id) VALUES ($1, 'identity.banned', $2)",
    )
    .bind(tenant_a)
    .bind(identity)
    .execute(&mut *tx)
    .await;
    assert!(forged.is_err(), "WITH CHECK 必须拒绝跨租户写");
    drop(tx);

    let denied = sqlx::query("DELETE FROM tenant_notifications")
        .execute(&app)
        .await;
    assert!(denied.is_err(), "app_user 不得 DELETE");
    let no_context = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenant_notifications")
        .fetch_one(&app)
        .await;
    assert!(
        no_context.is_err(),
        "不设 app.tenant_id 必须报错而不是 0 行"
    );

    let (client, mock) = mock_bot_api(OK).await;
    deliver_tenant(&app, &client, tenant_b)
        .await
        .expect("发送失败");
    assert!(
        mock.sent.lock().expect("锁").is_empty(),
        "B 的轮次不得发 A 的通知"
    );
    assert_eq!(rows(&admin, identity).await[0].1, "PENDING");
}
