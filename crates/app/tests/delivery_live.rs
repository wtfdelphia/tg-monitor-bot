//! delivery 真机验收工具（ADR-0033；提案 WP-5-delivery-sender-channel 的「验收」节）。
//!
//! 全部 `#[ignore]`：要真实 Bot、真实频道与平台 `api_id`，CI 与本地默认测试都不跑。
//! 凭据只经环境变量传入，本文件不打印、不落盘任何 Token、`api_hash` 或 session。
//!
//! ```text
//! V1 live_bot_login_repeat          连续 bot_sign_in N 次，看是否触发限流
//! V2 live_delivery_g4_kill_restart  请求前 / 平台已接受后两个时点强杀，重启（新 session）重发
//! V3 live_revoked_token             Token 在 BotFather 重置后，身份转 unauthorized 并写审计
//! V4a live_bot_session_capture      登录、存 session 到文件、确认此刻可复用（重置 Token 之前跑）
//! V4b live_bot_session_after_revoke 在 BotFather 重置 Token 之后，用 V4a 的旧 session 建连并试发
//! G4 live_delivery_flood_wait_recovery 真实投递限流、退避落库、到期恢复与幂等复跑
//! ```
//!
//! V4 回答 ADR-0034 代价节的未验证项：重置 Token 是否注销已有 auth key。两段之间由人去
//! BotFather 重置，所以拆成两个测试。session 文件含 auth key：权限 0600、放仓库外，
//! V4b 读完即删。
//!
//! 环境变量：
//!
//! ```text
//! TGM_PLATFORM_API_ID / TGM_PLATFORM_API_HASH   平台应用凭据（V1 V2 V3）
//! TGM_LIVE_BOT_TOKEN                            测试 Bot 的 Token（V1 V2）
//! TGM_LIVE_TARGET_CHAT                          测试频道的 Bot API chat id，如 -100...（V2）
//! TGM_LIVE_REVOKED_BOT_TOKEN                    已在 BotFather 重置作废的旧 Token（V3）
//! TGM_LIVE_LOGIN_REPEAT                         V1 次数，默认 5
//! TGM_LIVE_SESSION_FILE                         V4 的 session 文件路径（仓库外）
//! TGM_G4_FLOOD_WAIT_MAX_SECONDS                 G4 最长等待上限，默认 1800
//! TGM_G4_REPORT_FILE                            G4 脱敏报告路径（可选）
//! TGM_G4_SENDER_SESSION_FILE                    G4 预先捕获的测试发送 Bot session（0600）
//! TGM_G4_OBSERVER_SESSION_FILE                  G4 预先捕获的只读 Bot session（0600）
//! ```
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tgm_app::delivery::{
    DeliveryPayload, DeliveryRunOutcome, DeliverySendError, DeliveryTransport,
    TelegramDeliveryTransport, run_delivery_once,
};
use tgm_app::delivery_runtime::{
    DeliveryRuntimeConfig, PlatformApp, SenderExit, bot_id_from_token, load_bot_token, run_sender,
};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::store_identity_secret;
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, fanout_canonical_event,
    insert_canonical_event,
};
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{StoredSession, TelegramClient};
use tokio::sync::watch;

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";
const CHILD_TIMEOUT: Duration = Duration::from_secs(90);
const G4_PROBE_MARKER: &str = "tgm-g4-flood-wait-probe";

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

/// 缺变量直接失败：`--ignored` 跑起来却因为没配凭据而「通过」就是假绿。
fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("真机验收缺少环境变量 {name}"))
}

fn platform_app() -> PlatformApp {
    let api_id = required("TGM_PLATFORM_API_ID")
        .parse::<i32>()
        .expect("TGM_PLATFORM_API_ID 必须是整数");
    PlatformApp {
        api_id,
        api_hash: Arc::new(Secret::new(required("TGM_PLATFORM_API_HASH"))),
        media_store: tgm_app::media_relay::MediaStore::new(&tgm_app::media_relay::S3Config {
            endpoint: "http://127.0.0.1:59000".to_owned(),
            bucket: "tgm-live".to_owned(),
            region: "us-east-1".to_owned(),
            access_key_id: "minioadmin".to_owned(),
            secret_access_key: Secret::new("minioadmin".to_owned()),
        }),
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos()
}

/// 正的 63 位随机数。用时间与进程号拼，不引新依赖；同一秒内重复跑也不撞。
fn fresh_random_id() -> i64 {
    let mixed = now_nanos() ^ (u128::from(std::process::id()) << 64);
    i64::try_from(mixed & 0x3fff_ffff_ffff_ffff).expect("截断后必为正")
}

async fn seed_bot(
    app: &PgPool,
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    token: &str,
) -> IdentityId {
    let bot: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status, tg_user_id) VALUES ('bot', $1, 'offline', $2) RETURNING id",
    )
    .bind(tenant)
    .bind(bot_id_from_token(token).expect("Token 格式无效"))
    .fetch_one(admin.raw())
    .await
    .expect("创建 Bot 身份失败");
    store_identity_secret(
        app,
        &kek(),
        tenant,
        bot,
        &Secret::new(token.as_bytes().to_vec()),
    )
    .await
    .expect("写入 Token 失败");
    bot
}

async fn identity_state(admin: &support::AdminOnlyPool, identity: IdentityId) -> (String, i64) {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT i.status,
                (SELECT count(*) FROM audit_logs a
                  WHERE a.resource = 'identity' AND a.resource_id = i.id
                    AND a.action = 'identity.unauthorized')
           FROM identities i WHERE i.id = $1",
    )
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("读取身份状态失败")
}

// ───────────────────────── V1 ─────────────────────────

#[tokio::test]
#[ignore = "真机：需要测试 Bot 与平台 api_id"]
async fn live_bot_login_repeat() {
    let app = platform_app();
    let token = Secret::new(required("TGM_LIVE_BOT_TOKEN"));
    let expected = bot_id_from_token(token.expose()).expect("Token 格式无效");
    let repeat: u32 = std::env::var("TGM_LIVE_LOGIN_REPEAT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);
    let mut failures = Vec::new();
    for round in 1..=repeat {
        let config = TelegramClientConfig::new(app.api_id).expect("api_id 无效");
        match TelegramClient::connect_bot_sender(config, &app.api_hash, &token).await {
            Ok((client, bot_id)) => {
                println!(
                    "[V1] round={round} login=ok bot_id_matches={}",
                    bot_id == expected
                );
                if bot_id != expected {
                    failures.push(format!("round {round}: bot id 不符"));
                }
                client.shutdown().await.expect("关闭客户端失败");
            }
            Err(error) => {
                let delivery = error.delivery_error();
                println!(
                    "[V1] round={round} login=err code={} flood_wait_seconds={:?}",
                    delivery.code, delivery.flood_wait_seconds
                );
                failures.push(format!("round {round}: {}", delivery.code));
                if delivery.code == "FLOOD_WAIT" {
                    println!("[V1] stop=first-flood-wait");
                    break;
                }
            }
        }
    }
    assert!(failures.is_empty(), "[V1] 登录失败：{failures:?}");
}

// ───────────────────────── V2 ─────────────────────────

/// 包住真实 transport，在两个时点留检查点并挂起，等父进程强杀。
struct GateTransport {
    inner: TelegramDeliveryTransport,
    directory: PathBuf,
    phase: String,
}

#[async_trait::async_trait]
impl DeliveryTransport for GateTransport {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
        let marker = |name: &str, body: String| {
            std::fs::write(self.directory.join(name), body).expect("写检查点失败");
        };
        if self.phase == "before-send" {
            marker(
                "before-send.reached",
                payload.telegram_random_id.to_string(),
            );
            std::future::pending::<()>().await;
        }
        let result = self.inner.deliver(payload).await;
        let record = match &result {
            Ok(message_id) => format!("ok:{message_id}"),
            Err(error) => format!("err:{}", error.code),
        };
        marker(&format!("{}.result", self.phase), record);
        if self.phase == "after-send" {
            std::future::pending::<()>().await;
        }
        result
    }
}

/// 子进程：从库里解密 Token、全新登录（新 auth key / 新 session），跑一次投递。
#[tokio::test]
async fn live_delivery_child() {
    let Ok(url) = std::env::var("TGM_LIVE_CHILD_URL") else {
        return;
    };
    required("TGM_LIVE_TARGET_CHAT");
    eprintln!("[V2-child] stage=connect-database");
    let pool = PgPool::connect(&url).await.expect("连接测试库失败");
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&pool)
        .await
        .expect("读取角色失败");
    assert_eq!(role, "app_user");
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants WHERE id = $1")
        .bind(
            required("TGM_LIVE_CHILD_TENANT")
                .parse::<i64>()
                .expect("租户无效"),
        )
        .fetch_one(&pool)
        .await
        .expect("读取租户失败");
    let sender: IdentityId = tgm_db::tenant::tenant_tx(&pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar(
                "SELECT sender_identity_id FROM delivery_tasks WHERE tenant_id = $1 ORDER BY id LIMIT 1",
            )
            .bind(tenant)
            .fetch_one(conn)
            .await
        })
    })
    .await
    .expect("读取发送身份失败");
    let token = load_bot_token(&pool, &kek(), tenant, sender)
        .await
        .expect("解密 Token 失败")
        .flatten()
        .expect("库里没有可用 Token");
    let app = platform_app();
    let config = TelegramClientConfig::new(app.api_id).expect("api_id 无效");
    eprintln!("[V2-child] stage=bot-sign-in");
    let (client, _) = tokio::time::timeout(
        Duration::from_secs(20),
        TelegramClient::connect_bot_sender(config, &app.api_hash, &token),
    )
    .await
    .expect("bot_sign_in 超时")
    .expect("bot_sign_in 失败");
    drop(token);
    let directory = PathBuf::from(required("TGM_LIVE_CHILD_DIR"));
    let transport = GateTransport {
        inner: TelegramDeliveryTransport::new(
            Arc::new(client),
            app.media_store.clone(),
            pool.clone(),
            tenant,
        ),
        directory,
        phase: required("TGM_LIVE_CHILD_PHASE"),
    };
    eprintln!("[V2-child] stage=run-delivery");
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_delivery_once(&pool, tenant, sender, &transport),
    )
    .await
    .expect("运行投递超时")
    .expect("运行投递失败");
    println!("[V2-child] outcome={outcome:?}");
    assert!(
        !matches!(outcome, DeliveryRunOutcome::Idle),
        "子进程没有领取投递任务"
    );
}

struct LiveChild(Child);

impl LiveChild {
    fn start(url: &str, tenant: TenantId, directory: &Path, phase: &str) -> Self {
        let child = Command::new(std::env::current_exe().expect("定位测试二进制失败"))
            .args(["--exact", "live_delivery_child", "--nocapture"])
            .env_clear()
            .env("TGM_LIVE_CHILD_URL", url)
            .env("TGM_LIVE_CHILD_TENANT", tenant.get().to_string())
            .env("TGM_LIVE_CHILD_DIR", directory)
            .env("TGM_LIVE_TARGET_CHAT", required("TGM_LIVE_TARGET_CHAT"))
            .env("TGM_LIVE_CHILD_PHASE", phase)
            .env("TGM_PLATFORM_API_ID", required("TGM_PLATFORM_API_ID"))
            .env("TGM_PLATFORM_API_HASH", required("TGM_PLATFORM_API_HASH"))
            .env("TGM_S3_ENDPOINT", "http://127.0.0.1:59000")
            .env("TGM_S3_BUCKET", "tgm-live")
            .env("TGM_S3_REGION", "us-east-1")
            .env("TGM_S3_ACCESS_KEY_ID", "minioadmin")
            .env("TGM_S3_SECRET_ACCESS_KEY", "minioadmin")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("启动子进程失败");
        Self(child)
    }

    async fn wait_for(&mut self, path: &Path) {
        let deadline = tokio::time::Instant::now() + CHILD_TIMEOUT;
        while !path.exists() {
            assert!(
                self.0.try_wait().expect("检查子进程失败").is_none(),
                "子进程在检查点前退出"
            );
            assert!(tokio::time::Instant::now() < deadline, "等待检查点超时");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn kill(&mut self) {
        self.0.kill().expect("强杀子进程失败");
        let _ = self.0.wait();
    }

    async fn wait_exit(&mut self) {
        let deadline = tokio::time::Instant::now() + CHILD_TIMEOUT;
        loop {
            if let Some(status) = self.0.try_wait().expect("等待子进程失败") {
                assert!(status.success(), "重启子进程失败退出");
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "重启子进程超时");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read(directory: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(directory.join(name)).ok()
}

struct CapturedDelivery {
    inner: TelegramDeliveryTransport,
    error: Arc<std::sync::Mutex<Option<DeliverySendError>>>,
}

fn g4_error_category(error: &DeliverySendError) -> &'static str {
    if error.code != "TELEGRAM_CLIENT_ERROR" {
        return "telegram_rpc_or_delivery_code";
    }
    match error.message.as_deref() {
        Some(message) if message.starts_with("无法解析 Telegram peer:") => "peer_resolution",
        Some(message) if message.starts_with("Telegram 调用失败: request error: rpc error ") => {
            "invocation_rpc"
        }
        Some(message) if message.starts_with("Telegram 调用失败: request error: IO failed:") => {
            "invocation_io"
        }
        Some(message)
            if message.starts_with("Telegram 调用失败: request error: transport-level:") =>
        {
            "invocation_transport"
        }
        Some(message) if message.starts_with("Telegram 调用失败: request error: bad response:") => {
            "invocation_deserialize"
        }
        Some(message)
            if message.starts_with("Telegram 调用失败: request error: dropped (cancelled)") =>
        {
            "invocation_dropped"
        }
        Some(message) if message.starts_with("Telegram 调用失败: request error: invalid dc") => {
            "invocation_invalid_dc"
        }
        Some(message) if message.starts_with("Telegram 调用失败: session error") => {
            "invocation_session"
        }
        Some(message)
            if message.starts_with("Telegram 调用失败: request error: authentication") =>
        {
            "invocation_authentication"
        }
        Some(message) if message.starts_with("Telegram 调用失败:") => "invocation_unclassified",
        Some(message) if message.starts_with("无法同步 Telegram 更新状态:") => {
            "session_sync"
        }
        Some(message) if message.starts_with("无法建立 Telegram 更新流:") => "update_stream",
        Some(message) if message.starts_with("session 快照失败:") => "session_snapshot",
        Some(message) if message.starts_with("Telegram runner 关闭失败:") => "runner_shutdown",
        Some(message) if message.starts_with("媒体上传失败:") => "media_upload",
        _ => "unclassified_client_error",
    }
}

#[test]
fn g4_error_category_does_not_expose_error_text() {
    let error = DeliverySendError {
        code: "TELEGRAM_CLIENT_ERROR".to_owned(),
        message: Some("Telegram 调用失败: request error: rpc error 420: FLOOD_WAIT_12".to_owned()),
        flood_wait_seconds: None,
    };
    assert_eq!(g4_error_category(&error), "invocation_rpc");

    let transport = DeliverySendError {
        code: "TELEGRAM_CLIENT_ERROR".to_owned(),
        message: Some(
            "Telegram 调用失败: request error: transport-level: private details".to_owned(),
        ),
        flood_wait_seconds: None,
    };
    assert_eq!(g4_error_category(&transport), "invocation_transport");

    let io = DeliverySendError {
        code: "TELEGRAM_CLIENT_ERROR".to_owned(),
        message: Some("Telegram 调用失败: request error: IO failed: private details".to_owned()),
        flood_wait_seconds: None,
    };
    assert_eq!(g4_error_category(&io), "invocation_io");

    let unknown = DeliverySendError {
        code: "TELEGRAM_CLIENT_ERROR".to_owned(),
        message: Some("unexpected private details".to_owned()),
        flood_wait_seconds: None,
    };
    assert_eq!(g4_error_category(&unknown), "unclassified_client_error");
}

struct BlockedProbe {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl DeliveryTransport for BlockedProbe {
    async fn deliver(&self, _payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(DeliverySendError {
            code: "G4_LOCAL_PROBE_BLOCKED".to_owned(),
            message: None,
            flood_wait_seconds: None,
        })
    }
}

#[async_trait::async_trait]
impl DeliveryTransport for CapturedDelivery {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
        let result = self.inner.deliver(payload).await;
        if let Err(error) = &result {
            *self.error.lock().expect("读取限流错误捕获器失败") = Some(error.clone());
        }
        result
    }
}

fn g4_report_path() -> Option<PathBuf> {
    std::env::var_os("TGM_G4_REPORT_FILE").map(PathBuf::from)
}

fn write_g4_report(lines: &[String]) {
    let Some(path) = g4_report_path() else {
        return;
    };
    let body = format!("{}\n", lines.join("\n"));
    std::fs::write(path, body).expect("写 G4 脱敏报告失败");
}

async fn g4_delete_message(token: &str, target: i64, message_id: i64) -> Result<(), String> {
    let response = reqwest::Client::new()
        .post(format!("https://api.telegram.org/bot{token}/deleteMessage"))
        .json(&json!({"chat_id": target, "message_id": message_id}))
        .send()
        .await
        .map_err(|_| "G4 清理测试消息网络失败".to_owned())?
        .json::<serde_json::Value>()
        .await
        .map_err(|_| "G4 清理测试消息响应无效".to_owned())?;
    if response["ok"] == true {
        Ok(())
    } else {
        Err("G4 清理测试消息被拒绝".to_owned())
    }
}

async fn g4_task_state(
    admin: &support::AdminOnlyPool,
    task_id: i64,
) -> (
    String,
    i32,
    Option<sqlx::types::chrono::DateTime<Utc>>,
    Option<String>,
    Option<i64>,
) {
    sqlx::query_as(
        "SELECT status, attempt_count, next_retry_at, last_error_code, telegram_message_id
           FROM delivery_tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_one(admin.raw())
    .await
    .expect("读取 G4 投递任务状态失败")
}

async fn g4_identity_status(admin: &support::AdminOnlyPool, identity: IdentityId) -> String {
    sqlx::query_scalar("SELECT status FROM identities WHERE id = $1")
        .bind(identity)
        .fetch_one(admin.raw())
        .await
        .expect("读取 G4 发送身份状态失败")
}

fn g4_max_wait() -> Duration {
    let seconds = match std::env::var("TGM_G4_FLOOD_WAIT_MAX_SECONDS") {
        Ok(value) => value
            .parse::<u64>()
            .expect("TGM_G4_FLOOD_WAIT_MAX_SECONDS 必须是正整数"),
        Err(_) => 1800,
    };
    assert!(seconds > 0, "TGM_G4_FLOOD_WAIT_MAX_SECONDS 必须大于零");
    Duration::from_secs(seconds)
}

fn g4_session(name: &str) -> Secret<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;

    let path = PathBuf::from(required(name));
    let metadata = std::fs::symlink_metadata(&path).expect("读取 G4 session 元数据失败");
    assert!(metadata.is_file(), "G4 session 必须是普通文件");
    assert_eq!(metadata.mode() & 0o777, 0o600, "G4 session 权限必须是 0600");
    Secret::new(std::fs::read(path).expect("读取 G4 session 失败"))
}

async fn g4_resume_bot(
    config: TelegramClientConfig,
    session: &Secret<Vec<u8>>,
) -> Result<(TelegramClient, i64), String> {
    let stored =
        StoredSession::restore(session.expose()).map_err(|_| "G4 session 快照无效".to_owned())?;
    TelegramClient::resume_bot_sender(config, stored)
        .await
        .map_err(|_| "G4 复用预先捕获的 Bot session 失败".to_owned())
}

async fn g4_wait_until(deadline: sqlx::types::chrono::DateTime<Utc>) {
    loop {
        let now = Utc::now();
        if now >= deadline {
            return;
        }
        let remaining = (deadline - now).num_seconds().max(0);
        println!("[G4] wait_expiry=pending remaining_seconds={remaining}");
        tokio::time::sleep(Duration::from_secs(remaining.min(30) as u64)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：只使用专用测试 Bot/频道；不会自动制造限流"]
async fn live_delivery_flood_wait_recovery(admin: PgPool) {
    assert_eq!(
        std::env::var("TGM_G4_FLOOD_WAIT_ALLOW").as_deref(),
        Ok("1"),
        "必须显式设置 TGM_G4_FLOOD_WAIT_ALLOW=1，并确认使用专用测试对象"
    );
    let token = required("TGM_LIVE_BOT_TOKEN");
    let observer_token = required("TGM_LIVE_BOT2_TOKEN");
    let sender_session = g4_session("TGM_G4_SENDER_SESSION_FILE");
    let observer_session = g4_session("TGM_G4_OBSERVER_SESSION_FILE");
    let target: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数");
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-g4-flood-wait", 9301).await;
    let bot = seed_bot(&app, &admin, tenant, &token).await;
    sqlx::query(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, receiver_identity_id, enabled)
         VALUES ($1, $2, 'G4 flood wait probe', $3, TRUE)",
    )
    .bind(tenant)
    .bind(target)
    .bind(bot)
    .execute(admin.raw())
    .await
    .expect("创建 G4 临时来源失败");
    let random_id = fresh_random_id();
    let canonical = insert_canonical_event(
        &app,
        &CanonicalEventInput {
            receiver_tenant_id: Some(tenant),
            adapter: CanonicalAdapter::Bot,
            receiver_identity_id: bot,
            tg_chat_id: target,
            tg_message_id: random_id,
            event_type: CanonicalEventType::MessageCreated,
            edit_version: 0,
            update_id: None,
            media_group_id: None,
            sender_id: None,
            message_date: Utc::now(),
            text: Some(G4_PROBE_MARKER.to_owned()),
            caption: None,
            media_type: None,
            content_hash: None,
            file_unique_id: None,
            file_name: None,
            file_size: None,
            mime_type: None,
            raw_file_id: None,
        },
    )
    .await
    .expect("创建 G4 规范事件失败");
    let tenant_event = fanout_canonical_event(&app, tenant, canonical.id)
        .await
        .expect("扇出 G4 事件失败")
        .expect("G4 事件应匹配来源");
    let task_id: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, tenant_event_id, sender_identity_id, target_tg_chat_id, action,
             telegram_random_id, payload_snapshot)
         VALUES ($1, $5, $2, $3, 'COPY', $4, $6) RETURNING id",
    )
    .bind(tenant)
    .bind(bot)
    .bind(target)
    .bind(random_id)
    .bind(tenant_event.id)
    .bind(json!({
        "source": {"tg_chat_id": target, "tg_message_id": random_id, "media_type": "TEXT"},
        "message": {"text": G4_PROBE_MARKER, "parse_mode": "HTML"},
        "text_only": true
    }))
    .fetch_one(admin.raw())
    .await
    .expect("创建 G4 投递任务失败");
    let platform = platform_app();
    let config = TelegramClientConfig::new(platform.api_id).expect("api_id 无效");
    let (client, bot_id) = g4_resume_bot(config.clone(), &sender_session)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(bot_id, bot_id_from_token(&token).expect("Token 格式无效"));
    let capture = Arc::new(std::sync::Mutex::new(None));
    let sender_client = Arc::new(client);
    let transport = CapturedDelivery {
        inner: TelegramDeliveryTransport::new(
            Arc::clone(&sender_client),
            platform.media_store.clone(),
            app.clone(),
            tenant,
        ),
        error: Arc::clone(&capture),
    };
    let first = tgm_app::delivery::run_delivery_once(&app, tenant, bot, &transport)
        .await
        .expect("G4 首次真实投递执行失败");
    let observed_at = Utc::now();
    let (status, attempt_count, next_retry_at, last_error_code, message_id) =
        g4_task_state(&admin, task_id).await;
    let captured = capture.lock().expect("读取 G4 限流错误捕获器失败").clone();
    println!(
        "[G4] first_outcome={first:?} task_status={status} attempt_count={attempt_count} \
         last_error_code={} next_retry_at_present={} identity_status={}",
        last_error_code.as_deref().unwrap_or("-"),
        next_retry_at.is_some(),
        g4_identity_status(&admin, bot).await,
    );
    let Some(error) = captured else {
        let cleanup = match message_id {
            Some(message_id) => g4_delete_message(&token, target, message_id).await,
            None => Err("没有可用于确认清理结果的 Telegram message id".to_owned()),
        };
        write_g4_report(&[
            "source_message: not-run".to_owned(),
            "flood_wait_observed: not-run".to_owned(),
            "retry_at_persisted: not-run".to_owned(),
            "identity_state: not-run".to_owned(),
            "retry_suppressed: not-run".to_owned(),
            "wait_expiry: not-run".to_owned(),
            "single_delivery_after_recovery: not-run".to_owned(),
            "duplicate_delivery: not-run".to_owned(),
            "journal_redaction: not-run".to_owned(),
            "production_credentials: not-used".to_owned(),
            "production_services: untouched".to_owned(),
            format!(
                "cleanup: {}",
                if cleanup.is_ok() { "pass" } else { "partial" }
            ),
        ]);
        if let Err(error) = cleanup {
            panic!("{error}");
        }
        println!("[G4] flood_wait_observed=not-run reason=first_delivery_was_not_rate_limited");
        return;
    };
    if error.code != "FLOOD_WAIT" {
        let error_category = g4_error_category(&error);
        drop(transport);
        drop(sender_client);
        let cleanup = match message_id {
            Some(message_id) => g4_delete_message(&token, target, message_id).await,
            None => Err("非限流响应没有可核对的消息 id".to_owned()),
        };
        write_g4_report(&[
            "source_message: not-run".to_owned(),
            "flood_wait_observed: fail".to_owned(),
            "retry_at_persisted: not-run".to_owned(),
            "identity_state: not-run".to_owned(),
            "retry_suppressed: not-run".to_owned(),
            "wait_expiry: not-run".to_owned(),
            "single_delivery_after_recovery: not-run".to_owned(),
            "duplicate_delivery: not-run".to_owned(),
            "journal_redaction: not-run".to_owned(),
            format!("observed_error_code: {}", error.code),
            format!("observed_error_category: {error_category}"),
            "production_credentials: not-used".to_owned(),
            "production_services: untouched".to_owned(),
            format!(
                "cleanup: {}",
                if cleanup.is_ok() { "pass" } else { "partial" }
            ),
        ]);
        if let Err(error) = cleanup {
            panic!("{error}");
        }
        panic!("G4 首次真实投递返回非 FLOOD_WAIT：{}", error.code);
    }
    let Some(wait_seconds) = error.flood_wait_seconds.filter(|seconds| *seconds > 0) else {
        drop(transport);
        drop(sender_client);
        write_g4_report(&[
            "source_message: not-run".to_owned(),
            "flood_wait_observed: fail".to_owned(),
            "retry_at_persisted: not-run".to_owned(),
            "identity_state: not-run".to_owned(),
            "retry_suppressed: not-run".to_owned(),
            "wait_expiry: not-run".to_owned(),
            "single_delivery_after_recovery: not-run".to_owned(),
            "duplicate_delivery: not-run".to_owned(),
            "journal_redaction: not-run".to_owned(),
            "production_credentials: not-used".to_owned(),
            "production_services: untouched".to_owned(),
            "cleanup: partial".to_owned(),
        ]);
        panic!("G4 FLOOD_WAIT 响应缺少有效的服务端等待秒数");
    };
    let wait_seconds = wait_seconds as u64;
    write_g4_report(&[
        "source_message: not-run".to_owned(),
        "flood_wait_observed: pass".to_owned(),
        "retry_at_persisted: not-run".to_owned(),
        "identity_state: not-run".to_owned(),
        "retry_suppressed: not-run".to_owned(),
        "wait_expiry: not-run".to_owned(),
        "single_delivery_after_recovery: not-run".to_owned(),
        "duplicate_delivery: not-run".to_owned(),
        "journal_redaction: not-run".to_owned(),
        "production_credentials: not-used".to_owned(),
        "production_services: untouched".to_owned(),
        "cleanup: partial".to_owned(),
    ]);
    let retry_persisted = status == "PENDING"
        && last_error_code.as_deref() == Some("FLOOD_WAIT")
        && next_retry_at.is_some_and(|deadline| {
            deadline >= observed_at + Duration::from_secs(wait_seconds)
                && deadline <= observed_at + g4_max_wait()
        });
    let identity_status = g4_identity_status(&admin, bot).await;
    let identity_pass = identity_status == "flood_wait";
    let Some(deadline) = next_retry_at else {
        drop(transport);
        drop(sender_client);
        write_g4_report(&[
            "source_message: not-run".to_owned(),
            "flood_wait_observed: pass".to_owned(),
            "retry_at_persisted: fail".to_owned(),
            format!(
                "identity_state: {}",
                if identity_pass { "pass" } else { "fail" }
            ),
            "retry_suppressed: fail".to_owned(),
            "wait_expiry: not-run".to_owned(),
            "single_delivery_after_recovery: not-run".to_owned(),
            "duplicate_delivery: not-run".to_owned(),
            "journal_redaction: not-run".to_owned(),
            "production_credentials: not-used".to_owned(),
            "production_services: untouched".to_owned(),
            "cleanup: pass".to_owned(),
        ]);
        panic!("G4 FLOOD_WAIT 未写入 next_retry_at");
    };
    let probe_at = Utc::now();
    let remaining = (deadline - probe_at).num_seconds().max(0) as u64;
    let retry_suppressed = if deadline > probe_at + Duration::from_secs(5) {
        let probe_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe = BlockedProbe {
            calls: Arc::clone(&probe_calls),
        };
        let suppressed = tgm_app::delivery::run_delivery_once(&app, tenant, bot, &probe)
            .await
            .expect("G4 冷却期领取检查失败");
        matches!(suppressed, tgm_app::delivery::DeliveryRunOutcome::Idle)
            && probe_calls.load(std::sync::atomic::Ordering::SeqCst) == 0
    } else {
        false
    };
    write_g4_report(&[
        "source_message: not-run".to_owned(),
        "flood_wait_observed: pass".to_owned(),
        format!(
            "retry_at_persisted: {}",
            if retry_persisted { "pass" } else { "fail" }
        ),
        format!(
            "identity_state: {}",
            if identity_pass { "pass" } else { "fail" }
        ),
        format!(
            "retry_suppressed: {}",
            if remaining <= 5 && !retry_suppressed {
                "not-run"
            } else if retry_suppressed {
                "pass"
            } else {
                "fail"
            }
        ),
        "wait_expiry: not-run".to_owned(),
        "single_delivery_after_recovery: not-run".to_owned(),
        "duplicate_delivery: not-run".to_owned(),
        "journal_redaction: not-run".to_owned(),
        "production_credentials: not-used".to_owned(),
        "production_services: untouched".to_owned(),
        "cleanup: partial".to_owned(),
    ]);
    if !retry_persisted || !retry_suppressed || remaining > g4_max_wait().as_secs() {
        write_g4_report(&[
            "source_message: not-run".to_owned(),
            "flood_wait_observed: pass".to_owned(),
            format!(
                "retry_at_persisted: {}",
                if retry_persisted { "pass" } else { "fail" }
            ),
            format!(
                "identity_state: {}",
                if identity_pass { "pass" } else { "fail" }
            ),
            format!(
                "retry_suppressed: {}",
                if remaining <= 5 && !retry_suppressed {
                    "not-run"
                } else if retry_suppressed {
                    "pass"
                } else {
                    "fail"
                }
            ),
            "wait_expiry: not-run".to_owned(),
            "single_delivery_after_recovery: not-run".to_owned(),
            "duplicate_delivery: not-run".to_owned(),
            "journal_redaction: not-run".to_owned(),
            "production_credentials: not-used".to_owned(),
            "production_services: untouched".to_owned(),
            "cleanup: partial".to_owned(),
        ]);
        drop(transport);
        drop(sender_client);
        panic!(
            "G4 FLOOD_WAIT 退避不满足判据：retry_persisted={retry_persisted} \
             identity_pass={identity_pass} retry_suppressed={retry_suppressed} \
             remaining={remaining} wait_seconds={wait_seconds}"
        );
    }
    drop(transport);
    drop(sender_client);
    println!("[G4] flood_wait_observed=pass wait_seconds={wait_seconds}");
    println!(
        "[G4] retry_at_persisted={} identity_state={} retry_suppressed={retry_suppressed}",
        retry_persisted, identity_status,
    );
    assert!(retry_persisted, "G4 next_retry_at 未正确落库");
    assert!(retry_suppressed, "G4 冷却期仍领取了待发送任务");
    let _ = g4_wait_until(deadline).await;
    let (client, bot_id) = g4_resume_bot(config, &sender_session)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(bot_id, bot_id_from_token(&token).expect("Token 格式无效"));
    let recovery = TelegramDeliveryTransport::new(
        Arc::new(client),
        platform.media_store.clone(),
        app.clone(),
        tenant,
    );
    let recovered = tgm_app::delivery::run_delivery_once(&app, tenant, bot, &recovery)
        .await
        .expect("G4 等待到期恢复投递失败");
    let (status, _, _, _, message_id) = g4_task_state(&admin, task_id).await;
    assert!(matches!(
        recovered,
        tgm_app::delivery::DeliveryRunOutcome::Done { .. }
    ));
    assert_eq!(status, "DONE");
    let duplicate = tgm_app::delivery::run_delivery_once(&app, tenant, bot, &recovery)
        .await
        .expect("G4 重复恢复检查失败");
    drop(recovery);
    assert!(matches!(
        duplicate,
        tgm_app::delivery::DeliveryRunOutcome::Idle
    ));
    let observer_result = g4_resume_bot(
        TelegramClientConfig::new(platform.api_id).expect("api_id 无效"),
        &observer_session,
    )
    .await;
    let message_id = message_id.expect("G4 DONE 任务没有 message id");
    let observation = match observer_result {
        Ok((observer, observer_id)) => {
            let expected_id = bot_id_from_token(&observer_token).expect("只读 Bot Token 格式无效");
            let read_result = if observer_id != expected_id {
                Err("G4 只读 session 与 Bot Token 不一致".to_owned())
            } else {
                observer
                    .media_reader()
                    .read_messages(target, &[message_id])
                    .await
                    .and_then(|messages| {
                        if messages.len() == 1
                            && messages[0].tg_chat_id == target
                            && messages[0].text.as_deref() == Some(G4_PROBE_MARKER)
                        {
                            Ok(())
                        } else {
                            Err(tgm_tg::TelegramClientError::SourceMessageMissing)
                        }
                    })
                    .map_err(|_| "G4 只读 Bot 读取目标消息失败".to_owned())
            };
            let shutdown_result = observer
                .shutdown()
                .await
                .map_err(|_| "关闭 G4 只读 Bot 失败".to_owned());
            read_result.and(shutdown_result)
        }
        Err(error) => Err(error),
    };
    let cleanup = g4_delete_message(&token, target, message_id).await;
    let observation_pass = observation.is_ok();
    let cleanup_pass = cleanup.is_ok();
    write_g4_report(&[
        "source_message: not-run".to_owned(),
        "flood_wait_observed: pass".to_owned(),
        "retry_at_persisted: pass".to_owned(),
        format!(
            "identity_state: {}",
            if identity_pass { "pass" } else { "fail" }
        ),
        "retry_suppressed: pass".to_owned(),
        "wait_expiry: pass".to_owned(),
        format!(
            "single_delivery_after_recovery: {}",
            if observation_pass { "pass" } else { "fail" }
        ),
        "duplicate_delivery: pass".to_owned(),
        "journal_redaction: not-run".to_owned(),
        "production_credentials: not-used".to_owned(),
        "production_services: untouched".to_owned(),
        format!("cleanup: {}", if cleanup_pass { "pass" } else { "fail" }),
    ]);
    if let Err(error) = observation {
        panic!("{error}");
    }
    if let Err(error) = cleanup {
        panic!("{error}");
    }
    println!(
        "[G4] wait_expiry=pass single_delivery_after_recovery=pass duplicate_delivery=pass journal_redaction=not-run cleanup=pass"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要测试 Bot、测试频道与平台 api_id"]
async fn live_delivery_g4_kill_restart(admin: PgPool) {
    let token = required("TGM_LIVE_BOT_TOKEN");
    let target: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数");
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .expect("读取库名失败");
    let url = format!(
        "postgres://app_user:apw@127.0.0.1:{}/{database}",
        support::pg_port()
    );
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-live", 9101).await;
    let bot = seed_bot(&app, &admin, tenant, &token).await;
    drop(token);
    sqlx::query(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, receiver_identity_id, enabled)
         VALUES ($1, $2, 'V2 synthetic source', $3, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(target)
    .bind(bot)
    .fetch_one(admin.raw())
    .await
    .expect("创建临时来源失败");

    let mut verdicts = Vec::new();
    for phase in ["before-send", "after-send"] {
        let random_id = fresh_random_id();
        let label = format!("tgm-g4 {phase} {random_id}");
        let directory = TempDir(std::env::temp_dir().join(format!(
            "tgm-live-{}-{}-{phase}",
            std::process::id(),
            now_nanos()
        )));
        std::fs::create_dir(&directory.0).expect("创建检查点目录失败");
        let canonical = insert_canonical_event(
            &app,
            &CanonicalEventInput {
                receiver_tenant_id: Some(tenant),
                adapter: CanonicalAdapter::Bot,
                receiver_identity_id: bot,
                tg_chat_id: target,
                tg_message_id: random_id,
                event_type: CanonicalEventType::MessageCreated,
                edit_version: 0,
                update_id: None,
                media_group_id: None,
                sender_id: None,
                message_date: Utc::now(),
                text: Some(label.clone()),
                caption: None,
                media_type: None,
                content_hash: None,
                file_unique_id: None,
                file_name: None,
                file_size: None,
                mime_type: None,
                raw_file_id: None,
            },
        )
        .await
        .expect("创建临时规范事件失败");
        let tenant_event = fanout_canonical_event(&app, tenant, canonical.id)
            .await
            .expect("扇出临时事件失败")
            .expect("临时事件应匹配来源");
        let task_id: i64 = sqlx::query_scalar(
            "INSERT INTO delivery_tasks
                (tenant_id, tenant_event_id, sender_identity_id, target_tg_chat_id, action, telegram_random_id, payload_snapshot)
             VALUES ($1, $5, $2, $3, 'COPY', $4, $6) RETURNING id",
        )
        .bind(tenant)
        .bind(bot)
        .bind(target)
        .bind(random_id)
        .bind(tenant_event.id)
        .bind(json!({
            "source": {"tg_chat_id": target, "tg_message_id": 1, "media_type": "TEXT"},
            "message": {"text": label, "parse_mode": "HTML"}
        }))
        .fetch_one(admin.raw())
        .await
        .expect("创建投递任务失败");

        let reached = if phase == "before-send" {
            "before-send.reached"
        } else {
            "after-send.result"
        };
        let mut first = LiveChild::start(&url, tenant, &directory.0, phase);
        first.wait_for(&directory.0.join(reached)).await;
        first.kill();

        sqlx::query(
            "UPDATE delivery_tasks SET next_retry_at = clock_timestamp(),
                    updated_at = clock_timestamp() - interval '120 seconds' WHERE id = $1",
        )
        .bind(task_id)
        .execute(admin.raw())
        .await
        .expect("设置恢复条件失败");
        let mut restarted = LiveChild::start(&url, tenant, &directory.0, "restarted");
        restarted.wait_exit().await;

        let first_result = read(&directory.0, "after-send.result");
        let restarted_result = read(&directory.0, "restarted.result").unwrap_or_default();
        let (status, message_id): (String, Option<i64>) =
            sqlx::query_as("SELECT status, telegram_message_id FROM delivery_tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(admin.raw())
                .await
                .expect("读取任务状态失败");

        let dedupe = match (phase, first_result.as_deref(), restarted_result.as_str()) {
            ("before-send", _, result) if result.starts_with("ok:") => "single-send",
            ("after-send", Some(first), result) if first == result => "same-message-id",
            ("after-send", Some(first), result)
                if first.starts_with("ok:") && result == "err:RANDOM_ID_DUPLICATE" =>
            {
                "random-id-duplicate"
            }
            ("after-send", Some(first), result)
                if first.starts_with("ok:") && result.starts_with("ok:") =>
            {
                "DUPLICATE-SENT"
            }
            _ => "unexpected",
        };
        println!(
            "[V2] phase={phase} label=\"{label}\" first={} restarted={restarted_result} \
             task_status={status} telegram_message_id={message_id:?} dedupe={dedupe}",
            first_result.as_deref().unwrap_or("-"),
        );
        verdicts.push((phase, dedupe, status, message_id));
    }

    println!("[V2] 使用第二个 Bot 只读核对本轮两条消息，再删除对应消息。");
    let observer = TelegramClient::connect_bot_sender(
        TelegramClientConfig::new(
            required("TGM_PLATFORM_API_ID")
                .parse()
                .expect("api_id 无效"),
        )
        .expect("api_id 无效"),
        &Secret::new(required("TGM_PLATFORM_API_HASH")),
        &Secret::new(required("TGM_LIVE_BOT2_TOKEN")),
    )
    .await
    .expect("只读核对 Bot 建连失败")
    .0;
    for (phase, dedupe, status, _) in &verdicts {
        assert_ne!(
            *dedupe, "DUPLICATE-SENT",
            "[V2] {phase}：同一 random_id 发出了两条消息，G4 不成立"
        );
        assert_ne!(
            *dedupe, "unexpected",
            "[V2] {phase}：结果不在预期内，见上方输出"
        );
        assert_eq!(
            status, "DONE",
            "[V2] {phase}：未重复发送（{dedupe}），但任务没到 DONE —— 去重结果需要应用层收口"
        );
    }
    let ids: Vec<i64> = verdicts
        .iter()
        .map(|(_, _, _, id)| id.expect("DONE 任务缺 message id"))
        .collect();
    let messages = observer
        .media_reader()
        .read_messages(target, &ids)
        .await
        .expect("读取 V2 消息失败");
    assert_eq!(messages.len(), 2);
    for ((phase, _, _, _), message) in verdicts.iter().zip(messages) {
        let marker = format!("tgm-g4 {phase} ");
        assert!(
            message
                .text
                .as_deref()
                .is_some_and(|text| text.starts_with(&marker))
        );
        println!("[V2] phase={phase} readback_count=1 label_present=true");
    }
    observer.shutdown().await.expect("关闭只读核对 Bot 失败");
    let http = reqwest::Client::new();
    for id in ids {
        let response = http
            .post(format!(
                "https://api.telegram.org/bot{}/deleteMessage",
                required("TGM_LIVE_BOT_TOKEN")
            ))
            .json(&json!({"chat_id": target, "message_id": id}))
            .send()
            .await
            .expect("清理 V2 消息网络失败")
            .json::<serde_json::Value>()
            .await
            .expect("清理 V2 消息响应无效");
        assert_eq!(response["ok"], true, "清理 V2 消息被拒绝");
    }
    println!("[V2] cleanup_count=2 cleanup=true");
}

// ───────────────────────── V3 ─────────────────────────

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要已作废的旧 Bot Token 与平台 api_id"]
async fn live_revoked_token(admin: PgPool) {
    let revoked = required("TGM_LIVE_REVOKED_BOT_TOKEN");
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-live-revoked", 9201).await;
    let bot = seed_bot(&app, &admin, tenant, &revoked).await;
    drop(revoked);
    let (_tx, shutdown) = watch::channel(false);
    let exit = run_sender(
        &app,
        &kek(),
        &platform_app(),
        tenant,
        bot,
        DeliveryRuntimeConfig::default(),
        shutdown,
    )
    .await;
    let state = identity_state(&admin, bot).await;
    println!("[V3] exit={exit:?} status={} audits={}", state.0, state.1);
    assert!(
        matches!(exit, Ok(SenderExit::Unauthorized)),
        "[V3] 作废 Token 未被识别"
    );
    assert_eq!(state, ("unauthorized".to_owned(), 1));
}

// ───────────────────────── V4 ─────────────────────────

#[tokio::test]
#[ignore = "真机：需要测试 Bot 与平台 api_id；之后由人在 BotFather 重置 Token"]
async fn live_bot_session_capture() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let app = platform_app();
    let token = Secret::new(required("TGM_LIVE_BOT_TOKEN"));
    let expected = bot_id_from_token(token.expose()).expect("Token 格式无效");
    let path = PathBuf::from(required("TGM_LIVE_SESSION_FILE"));
    let config = TelegramClientConfig::new(app.api_id).expect("api_id 无效");
    let (client, bot_id) =
        TelegramClient::connect_bot_sender(config.clone(), &app.api_hash, &token)
            .await
            .expect("bot_sign_in 失败");
    drop(token);
    assert_eq!(bot_id, expected);
    let session = client.bot_session_snapshot().expect("取 session 失败");
    client.shutdown().await.expect("关闭客户端失败");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("创建 session 文件失败（已存在时先删掉旧的）");
    file.write_all(session.expose())
        .expect("写 session 文件失败");
    drop(file);

    let restored = StoredSession::restore(session.expose()).expect("session 快照无效");
    let (client, resumed_id) = TelegramClient::resume_bot_sender(config, restored)
        .await
        .expect("[V4a] 重置前复用 session 就失败了");
    client.shutdown().await.expect("关闭客户端失败");
    assert_eq!(resumed_id, expected);
    println!(
        "[V4a] session 已存，重置前复用成功 bot_id={resumed_id}；现在去 BotFather 重置 Token 再跑 V4b"
    );
}

#[tokio::test]
#[ignore = "真机：需要 V4a 留下的 session 文件，且 Token 已在 BotFather 重置"]
async fn live_bot_session_after_revoke() {
    let app = platform_app();
    let target: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数");
    let path = PathBuf::from(required("TGM_LIVE_SESSION_FILE"));
    let bytes = Secret::new(std::fs::read(&path).expect("读 session 文件失败"));
    std::fs::remove_file(&path).expect("删 session 文件失败");
    let restored = StoredSession::restore(bytes.expose()).expect("session 快照无效");
    drop(bytes);
    let config = TelegramClientConfig::new(app.api_id).expect("api_id 无效");

    let (client, bot_id) = match TelegramClient::resume_bot_sender(config, restored).await {
        Ok(connected) => connected,
        Err(error) => {
            println!(
                "[V4b] 重置后旧 session 建连失败 unauthorized={} error={error}",
                error.is_unauthorized()
            );
            assert!(
                error.is_unauthorized(),
                "[V4b] 失败但不是授权失效，结论不成立，检查网络后重做：{error}"
            );
            println!("[V4b] 结论：重置 Token 注销了已有 auth key");
            return;
        }
    };
    let sent = client
        .copy_text(
            target,
            fresh_random_id(),
            "tgm-revoke-probe：重置 Token 后旧 session 仍能发送",
        )
        .await;
    let _ = client.shutdown().await;
    match sent {
        Ok(message_id) => println!(
            "[V4b] 结论：重置 Token 没有注销已有 auth key，旧 session 仍能以 bot_id={bot_id} 发出 message_id={message_id}"
        ),
        Err(error) => println!(
            "[V4b] 旧 session 能建连（getMe 成功 bot_id={bot_id}），但发送失败 unauthorized={} error={error}",
            error.is_unauthorized()
        ),
    }
}
