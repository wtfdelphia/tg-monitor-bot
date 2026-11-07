//! G3/G4 真实 Telegram 端到端验收。用例为 `#[ignore]`，默认测试不外呼。
//!
//! 同一数据库与进程内先经正式控制面 QR 登录，再串联生产 mtproto-worker、fanout、
//! rule 和 delivery，避免向另一进程交接已过时的 session 快照。
//! 只使用专用测试 Bot、源频道和目标，不打印 secret、session、密码或消息正文。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tgm_app::control::{CUSTODY_CONSENT_TEXT_VERSION, ControlPlaneState, CredentialIntake, router};
use tgm_app::delivery_runtime::{DeliveryRuntimeConfig, PlatformApp};
use tgm_app::media_relay::{MediaStore, S3Config};
use tgm_app::mtproto_worker::{IdentityExit, MtprotoTarget, MtprotoWorkerConfig, run_identity};
use tgm_app::pipeline::{FanoutWorker, FanoutWorkerConfig, RuleWorker, RuleWorkerConfig};
use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::store_identity_secret;
use tgm_tg::TelegramClient;
use tgm_tg::client::TelegramClientConfig;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower::ServiceExt;

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";
const OWNER: i64 = 9601;

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("真机验收缺少环境变量 {name}"))
}

fn allow_live() {
    assert_eq!(
        std::env::var("TGM_LIVE_E2E_ALLOW").as_deref(),
        Ok("1"),
        "必须显式设置 TGM_LIVE_E2E_ALLOW=1，且确认使用专用测试账号、Bot 与频道"
    );
}

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

fn api_id() -> i32 {
    required("TGM_PLATFORM_API_ID")
        .parse()
        .expect("TGM_PLATFORM_API_ID 必须是整数")
}

fn chat_id() -> i64 {
    required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数")
}

fn nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos();
    format!("{:x}{:x}", std::process::id(), nanos)
}

fn write_private(path: &PathBuf, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .expect("写私有文件失败");
    file.write_all(bytes).expect("写私有文件失败");
}

struct PrivateFile(PathBuf);

impl Drop for PrivateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn qr_page(svg: &str, generation: u32) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"3\">\
         <title>tgm QR</title></head><body style=\"background:#fff\">\
         <p>generation {generation}：Telegram 手机端 设置 → 设备 → 扫描二维码</p>\
         <div style=\"width:320px\">{svg}</div></body></html>"
    )
}

fn json_request(method: &str, uri: String, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("user-agent", "tgm-g34-live-acceptance")
        .body(Body::from(body.to_string()))
        .expect("构造请求失败")
}

async fn call(
    state: &ControlPlaneState,
    auth: AuthContext,
    request: Request<Body>,
) -> (StatusCode, Value) {
    let mut request = request;
    request.extensions_mut().insert(auth);
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("执行控制面请求失败");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1_048_576)
        .await
        .expect("读取控制面响应失败");
    (
        status,
        serde_json::from_slice(&bytes).expect("控制面响应不是 JSON"),
    )
}

/// 单进程登录并串联真实采集、规则与投递，避免把已过时的会话快照交给另一进程。
#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要专用测试用户扫码，可能要求 2FA"]
async fn live_g34_source_rule_delivery(admin: PgPool) {
    allow_live();
    let _ = tracing_subscriber::fmt()
        .with_env_filter("tgm_app::control=debug,tgm_tg::qr_login=debug")
        .with_test_writer()
        .try_init();
    let api_hash = required("TGM_PLATFORM_API_HASH");
    let html_path = PathBuf::from(required("TGM_LIVE_QR_HTML_FILE"));
    let password_path = PathBuf::from(required("TGM_LIVE_2FA_PASSWORD_FILE"));
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "g34-session-capture", OWNER).await;
    sqlx::query("UPDATE tenants SET plan = 'pro' WHERE id = $1")
        .bind(tenant)
        .execute(admin.raw())
        .await
        .expect("设置测试套餐失败");
    sqlx::query("INSERT INTO tenant_members (tenant_id, user_id) VALUES ($1, $2)")
        .bind(tenant)
        .bind(OWNER)
        .execute(admin.raw())
        .await
        .expect("添加测试成员失败");
    let state = ControlPlaneState::new(app.clone()).with_credential_intake(CredentialIntake {
        kek: Arc::new(kek()),
        bot_api_base_url: "http://127.0.0.1:9".to_owned(),
    });
    let owner = AuthContext::new(tenant, OWNER, TenantRole::Owner, AuthSource::Web);
    let (status, started) = call(
        &state,
        owner,
        json_request(
            "POST",
            "/api/v1/identities/login/qr".to_owned(),
            json!({
                "api_id": api_id(),
                "api_hash": api_hash,
                "consent_text_version": CUSTODY_CONSENT_TEXT_VERSION,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "QR 发起失败");
    assert!(!started.to_string().contains(&api_hash));
    let login_id = started["login_id"]
        .as_str()
        .expect("缺 login_id")
        .to_owned();
    let mut svg = started["qr_svg"].as_str().expect("缺 qr_svg").to_owned();
    write_private(&html_path, qr_page(&svg, 1).as_bytes());
    println!("[G34-QR] started html=written");
    let qr_file = PrivateFile(html_path.clone());

    let deadline = Instant::now() + Duration::from_secs(9 * 60);
    let mut generation = 1_u32;
    let mut awaiting_password = false;
    let confirmed = loop {
        assert!(Instant::now() < deadline, "等待扫码超时");
        tokio::time::sleep(Duration::from_secs(3)).await;
        let (status, body) = call(
            &state,
            owner,
            Request::builder()
                .uri(format!("/api/v1/identities/login/qr/{login_id}"))
                .body(Body::empty())
                .expect("构造轮询失败"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "QR 轮询失败");
        match body["status"].as_str() {
            Some("pending") => {
                let next = body["qr_svg"].as_str().expect("pending 缺 qr_svg");
                if next != svg {
                    svg = next.to_owned();
                    generation += 1;
                    write_private(&html_path, qr_page(&svg, generation).as_bytes());
                    println!("[G34-QR] generation={generation}");
                }
            }
            Some("awaiting_password") => {
                if !awaiting_password {
                    println!("[G34-QR] awaiting_password");
                    awaiting_password = true;
                }
                let Ok(password) = std::fs::read_to_string(&password_path) else {
                    continue;
                };
                std::fs::remove_file(&password_path).expect("删除密码文件失败");
                let (status, checked) = call(
                    &state,
                    owner,
                    json_request(
                        "POST",
                        "/api/v1/identities/login/password".to_owned(),
                        json!({ "login_id": login_id, "password": password.trim_end_matches(['\r', '\n']) }),
                    ),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "密码提交失败");
                if checked["status"] == "confirmed" {
                    break checked;
                }
                assert_eq!(checked["status"], "awaiting_password", "密码流程异常");
                println!("[G34-QR] password=rejected");
            }
            Some("confirmed") => break body,
            Some("failed") => panic!(
                "QR 登录失败：reason={}",
                body["reason"].as_str().unwrap_or("unknown")
            ),
            other => panic!("QR 登录状态异常：{other:?}"),
        }
    };
    drop(qr_file);
    let identity_id = confirmed["identity"]["id"]
        .as_i64()
        .expect("缺 identity.id");
    let identity: IdentityId = sqlx::query_scalar("SELECT id FROM identities WHERE id = $1")
        .bind(identity_id)
        .fetch_one(admin.raw())
        .await
        .expect("读取登录身份失败");

    let (stop, shutdown) = watch::channel(false);
    let pool = app.clone();
    let worker = tokio::spawn(async move {
        run_identity(
            &pool,
            &kek(),
            MtprotoTarget {
                tenant,
                identity,
                api_id: api_id(),
            },
            "g34-session-capture",
            MtprotoWorkerConfig::default(),
            None,
            shutdown,
        )
        .await
    });
    let online_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status: String = sqlx::query_scalar("SELECT status FROM identities WHERE id = $1")
            .bind(identity)
            .fetch_one(admin.raw())
            .await
            .expect("读取身份状态失败");
        if status == "online" {
            break;
        }
        assert!(
            Instant::now() < online_deadline,
            "登录后 worker 未上线：{status}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    stop.send(true).expect("停止采集 worker 失败");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(30), worker)
            .await
            .expect("采集 worker 退出超时")
            .expect("采集 worker join 失败"),
        Ok(IdentityExit::Shutdown)
    ));
    tgm_app::mtproto_session::load_session(&app, &kek(), tenant, identity)
        .await
        .expect("恢复已保存 session 失败")
        .expect("登录后应有 session");
    println!("[G34-QR] confirmed worker=shutdown");
    let keyword = format!("hit-g34-{}", nonce());
    let setup = setup(app, admin, tenant, identity, &keyword).await;
    let messages = std::sync::Arc::new(tokio::sync::Mutex::new(MessageCleanup::new()));
    let body = tokio::spawn({
        let messages = std::sync::Arc::clone(&messages);
        async move { run_delivery(setup, keyword, messages).await }
    })
    .await;
    let deleted = std::sync::Arc::try_unwrap(messages)
        .unwrap_or_else(|_| panic!("测试消息清理仍有引用"))
        .into_inner()
        .cleanup()
        .await;
    println!(
        "[G34-E2E] cleanup_message_count={deleted_count}",
        deleted_count = deleted.len()
    );
    assert!(deleted.iter().all(|deleted| *deleted), "测试消息清理失败");
    if let Err(error) = body {
        std::panic::resume_unwind(error.into_panic());
    }
}

struct BotApi {
    http: reqwest::Client,
    base: String,
    chat: i64,
}

struct MessageCleanup {
    bot: BotApi,
    ids: Vec<i64>,
}

impl MessageCleanup {
    fn new() -> Self {
        Self {
            bot: BotApi::new(),
            ids: Vec::new(),
        }
    }

    fn track(&mut self, id: i64) {
        self.ids.push(id);
    }

    async fn cleanup(mut self) -> Vec<bool> {
        let mut results = Vec::with_capacity(self.ids.len());
        for id in self.ids.drain(..) {
            results.push(self.bot.delete(id).await);
        }
        results
    }
}

impl Drop for MessageCleanup {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.ids);
        let bot = BotApi::new();
        tokio::spawn(async move {
            for id in ids {
                let _ = bot.delete(id).await;
            }
        });
    }
}

impl BotApi {
    fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base: format!(
                "https://api.telegram.org/bot{}",
                required("TGM_LIVE_BOT_TOKEN")
            ),
            chat: chat_id(),
        }
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value, String> {
        let response = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|_| format!("Bot API {method} 网络失败"))?;
        let body: Value = response
            .json()
            .await
            .map_err(|_| format!("Bot API {method} 响应无效"))?;
        if body["ok"] == true {
            Ok(body["result"].clone())
        } else {
            Err(format!(
                "Bot API {method} 拒绝：status={} retry_after={}",
                body["error_code"].as_i64().unwrap_or_default(),
                body["parameters"]["retry_after"]
                    .as_i64()
                    .unwrap_or_default()
            ))
        }
    }

    async fn send_source(&self, text: &str) -> i64 {
        self.call("sendMessage", json!({"chat_id": self.chat, "text": text}))
            .await
            .expect("发送测试源消息失败")["message_id"]
            .as_i64()
            .expect("源消息缺 message_id")
    }

    async fn delete(&self, message_id: i64) -> bool {
        self.call(
            "deleteMessage",
            json!({"chat_id": self.chat, "message_id": message_id}),
        )
        .await
        .is_ok()
    }
}

struct Setup {
    app: PgPool,
    admin: support::AdminOnlyPool,
    tenant: TenantId,
    receiver: IdentityId,
    sender: IdentityId,
}

async fn setup(
    app: PgPool,
    admin: support::AdminOnlyPool,
    tenant: TenantId,
    receiver: IdentityId,
    keyword: &str,
) -> Setup {
    let raw = admin.raw();

    let sender: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('bot', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(raw)
    .await
    .expect("创建发送身份失败");
    store_identity_secret(
        &app,
        &kek(),
        tenant,
        sender,
        &Secret::new(required("TGM_LIVE_BOT_TOKEN").into_bytes()),
    )
    .await
    .expect("加密保存 Bot Token 失败");
    let source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, receiver_identity_id, enabled)
         VALUES ($1, $2, 'G34 isolated source', $3, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(chat_id())
    .bind(receiver)
    .fetch_one(raw)
    .await
    .expect("创建来源失败");
    let target_ref: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, $2, $3, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(chat_id())
    .bind(sender)
    .fetch_one(raw)
    .await
    .expect("创建目标失败");
    let keyword_id: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, $2, 'exact', TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(keyword)
    .fetch_one(raw)
    .await
    .expect("创建关键词失败");
    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rules
            (tenant_id, keyword_id, source_ref, media_type, target_ref, action, priority, version, enabled)
         VALUES ($1, $2, $3, NULL, $4, 'COPY', 10, 1, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(keyword_id)
    .bind(source_ref)
    .bind(target_ref)
    .fetch_one(raw)
    .await
    .expect("创建规则失败");
    sqlx::query("INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot) VALUES ($1, $2, 1, '{\"rule\": {}}')")
        .bind(tenant)
        .bind(rule_id)
        .execute(raw)
        .await
        .expect("创建规则版本失败");
    Setup {
        app,
        admin,
        tenant,
        receiver,
        sender,
    }
}

async fn delivery_task(
    setup: &Setup,
    source_message_id: i64,
) -> Option<(i64, String, Option<i64>, i32)> {
    sqlx::query_as(
        "SELECT id, status, telegram_message_id, attempt_count FROM delivery_tasks
          WHERE tenant_id = $1 AND sender_identity_id = $2
            AND payload_snapshot->'source'->>'tg_message_id' = $3",
    )
    .bind(setup.tenant)
    .bind(setup.sender)
    .bind(source_message_id.to_string())
    .fetch_optional(setup.admin.raw())
    .await
    .expect("读取端到端投递任务失败")
}

async fn pipeline_counts(setup: &Setup) -> (i64, i64, i64, i64, Vec<Option<String>>) {
    let (canonical, tenant_events, tasks, processed) = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT
            (SELECT count(*) FROM canonical_events WHERE receiver_identity_id = $2),
            (SELECT count(*) FROM tenant_events WHERE tenant_id = $1),
            (SELECT count(*) FROM delivery_tasks WHERE tenant_id = $1),
            (SELECT count(*) FROM tenant_events WHERE tenant_id = $1 AND processed_at IS NOT NULL)",
    )
    .bind(setup.tenant)
    .bind(setup.receiver)
    .fetch_one(setup.admin.raw())
    .await
    .expect("读取端到端阶段计数失败");
    let media_types = sqlx::query_scalar(
        "SELECT DISTINCT media_type FROM canonical_events WHERE receiver_identity_id = $1",
    )
    .bind(setup.receiver)
    .fetch_all(setup.admin.raw())
    .await
    .expect("读取事件媒体类型失败");
    (canonical, tenant_events, tasks, processed, media_types)
}

struct Workers {
    stops: Vec<watch::Sender<bool>>,
    handles: Vec<JoinHandle<()>>,
}

impl Workers {
    fn new() -> Self {
        Self {
            stops: Vec::new(),
            handles: Vec::new(),
        }
    }

    fn add<T: Send + 'static>(&mut self, stop: watch::Sender<bool>, worker: JoinHandle<T>) {
        self.stops.push(stop);
        self.handles.push(tokio::spawn(async move {
            let _ = worker.await;
        }));
    }

    async fn stop(mut self) {
        for stop in self.stops.drain(..) {
            let _ = stop.send(true);
        }
        for handle in self.handles.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(40), handle).await;
        }
    }

    fn is_empty(&self) -> bool {
        self.stops.is_empty() && self.handles.is_empty()
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        for stop in self.stops.drain(..) {
            let _ = stop.send(true);
        }
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

async fn run_delivery(
    setup: Setup,
    keyword: String,
    messages: std::sync::Arc<tokio::sync::Mutex<MessageCleanup>>,
) {
    let unmatched = keyword.replacen("hit-", "miss-", 1);
    let bot = BotApi::new();

    let media_store = MediaStore::new(&S3Config::from_env().expect("缺少本地 S3 配置"));
    let platform = PlatformApp {
        api_id: api_id(),
        api_hash: Arc::new(Secret::new(required("TGM_PLATFORM_API_HASH"))),
        media_store: media_store.clone(),
    };
    let (mtproto_stop, mtproto_shutdown) = watch::channel(false);
    let mtproto_pool = setup.app.clone();
    let mtproto = tokio::spawn(async move {
        run_identity(
            &mtproto_pool,
            &kek(),
            MtprotoTarget {
                tenant: setup.tenant,
                identity: setup.receiver,
                api_id: api_id(),
            },
            "g34-e2e",
            MtprotoWorkerConfig::default(),
            Some(&media_store),
            mtproto_shutdown,
        )
        .await
    });
    let online_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status: String = sqlx::query_scalar("SELECT status FROM identities WHERE id = $1")
            .bind(setup.receiver)
            .fetch_one(setup.admin.raw())
            .await
            .expect("读取采集身份状态失败");
        if status == "online" {
            break;
        }
        assert!(
            Instant::now() < online_deadline,
            "采集 worker 未上线：{status}"
        );
        assert!(!mtproto.is_finished(), "采集 worker 启动后提前退出");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let source_hit = bot.send_source(&format!("{keyword} synthetic")).await;
    let source_miss = bot.send_source(&format!("{unmatched} synthetic")).await;
    messages.lock().await.track(source_hit);
    messages.lock().await.track(source_miss);
    println!("[G34-E2E] source_messages_sent=2 worker_started=true");
    let (fanout_stop, fanout_shutdown) = watch::channel(false);
    let fanout =
        FanoutWorker::new(setup.app.clone(), FanoutWorkerConfig::default()).run(fanout_shutdown);
    let fanout = tokio::spawn(fanout);
    let (rule_stop, rule_shutdown) = watch::channel(false);
    let rule = RuleWorker::new(setup.app.clone(), RuleWorkerConfig::default()).run(rule_shutdown);
    let rule = tokio::spawn(rule);
    let (delivery_stop, delivery_shutdown) = watch::channel(false);
    let delivery_pool = setup.app.clone();
    let delivery = tokio::spawn(async move {
        tgm_app::delivery_runtime::run_sender(
            &delivery_pool,
            &kek(),
            &platform,
            setup.tenant,
            setup.sender,
            DeliveryRuntimeConfig::default(),
            delivery_shutdown,
        )
        .await
    });

    let deadline = Instant::now() + Duration::from_secs(120);
    let task = loop {
        if let Some(task) = delivery_task(&setup, source_hit).await
            && task.1 == "DONE"
        {
            break task;
        }
        assert!(
            Instant::now() < deadline,
            "真实端到端投递未在 120 秒内完成：task={:?} pipeline_counts={:?} workers_running={}/{}/{}/{}",
            delivery_task(&setup, source_hit).await,
            pipeline_counts(&setup).await,
            !mtproto.is_finished(),
            !fanout.is_finished(),
            !rule.is_finished(),
            !delivery.is_finished(),
        );
        assert!(!mtproto.is_finished(), "采集 worker 提前退出");
        assert!(!fanout.is_finished(), "扇出 worker 提前退出");
        assert!(!rule.is_finished(), "规则 worker 提前退出");
        assert!(!delivery.is_finished(), "投递 worker 提前退出");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let mut workers = Workers::new();
    workers.add(mtproto_stop, mtproto);
    workers.add(fanout_stop, fanout);
    workers.add(rule_stop, rule);
    workers.add(delivery_stop, delivery);
    assert!(!workers.is_empty(), "worker 清理保护不能为空");
    let observer = TelegramClient::connect_bot_sender(
        TelegramClientConfig::new(api_id()).expect("api_id 无效"),
        &Secret::new(required("TGM_PLATFORM_API_HASH")),
        &Secret::new(required("TGM_LIVE_BOT2_TOKEN")),
    )
    .await
    .expect("只读验收 Bot 建连失败")
    .0;
    let target_reader = observer.media_reader();
    let target_message_id = task.2.expect("DONE 任务必须有 message id");
    messages.lock().await.track(target_message_id);
    let target_message = target_reader
        .read_messages(chat_id(), &[target_message_id])
        .await
        .expect("读取目标消息失败");
    assert_eq!(target_message.len(), 1);
    assert!(
        target_message[0]
            .text
            .as_deref()
            .is_some_and(|text| text.contains(&keyword)),
        "目标消息与本次随机标签不一致"
    );
    println!("[G34-E2E] 人工核对：测试频道中本次随机标签只出现一次。");
    let (hit_events, miss_events): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE tg_message_id = $2),
                count(*) FILTER (WHERE tg_message_id = $3)
           FROM canonical_events WHERE receiver_identity_id = $1",
    )
    .bind(setup.receiver)
    .bind(source_hit)
    .bind(source_miss)
    .fetch_one(setup.admin.raw())
    .await
    .expect("读取源事件失败");
    let matched_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenant_events te
          JOIN canonical_events ce ON ce.id = te.canonical_event_id
         WHERE te.tenant_id = $1 AND $2 = ANY(te.matched_keywords)
           AND ce.receiver_identity_id = $3 AND ce.tg_message_id = $4",
    )
    .bind(setup.tenant)
    .bind(&keyword)
    .bind(setup.receiver)
    .bind(source_hit)
    .fetch_one(setup.admin.raw())
    .await
    .expect("读取规则命中失败");
    assert_eq!(hit_events, 1, "命中源消息采集次数不正确");
    assert_eq!(miss_events, 1, "未命中源消息采集次数不正确");
    assert_eq!(matched_rows, 1, "规则命中次数不正确");
    assert_eq!(task.1, "DONE");
    assert!(task.2.is_some(), "DONE 任务缺 Telegram message id");

    assert!(
        delivery_task(&setup, source_miss).await.is_none(),
        "未命中消息不应入队"
    );

    workers.stop().await;
    observer.shutdown().await.expect("关闭只读验收 Bot 失败");
    println!(
        "[G34-E2E] events_hit=1 events_miss=1 matched=1 tasks=1 task_status=DONE target_message_label_match=true \
         target_message_id_present=true"
    );
}
