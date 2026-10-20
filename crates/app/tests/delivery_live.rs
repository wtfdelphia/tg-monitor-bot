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
//! ```
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use sqlx::PgPool;
use tgm_app::delivery::{
    DeliveryPayload, DeliverySendError, DeliveryTransport, TelegramDeliveryTransport,
    run_delivery_once,
};
use tgm_app::delivery_runtime::{
    DeliveryRuntimeConfig, PlatformApp, SenderExit, bot_id_from_token, load_bot_token, run_sender,
};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::store_identity_secret;
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{StoredSession, TelegramClient};
use tokio::sync::watch;

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";
const CHILD_TIMEOUT: Duration = Duration::from_secs(90);

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
                let code = error.delivery_error().code;
                println!("[V1] round={round} login=err code={code}");
                failures.push(format!("round {round}: {code}"));
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
    let (client, _) = TelegramClient::connect_bot_sender(config, &app.api_hash, &token)
        .await
        .expect("bot_sign_in 失败");
    drop(token);
    let directory = PathBuf::from(required("TGM_LIVE_CHILD_DIR"));
    let transport = GateTransport {
        inner: TelegramDeliveryTransport::new(Arc::new(client)),
        directory,
        phase: required("TGM_LIVE_CHILD_PHASE"),
    };
    let outcome = run_delivery_once(&pool, tenant, sender, &transport)
        .await
        .expect("运行投递失败");
    println!("[V2-child] outcome={outcome:?}");
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
            .env("TGM_LIVE_CHILD_PHASE", phase)
            .env("TGM_PLATFORM_API_ID", required("TGM_PLATFORM_API_ID"))
            .env("TGM_PLATFORM_API_HASH", required("TGM_PLATFORM_API_HASH"))
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
    let url = format!("postgres://app_user:apw@127.0.0.1:55432/{database}");
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "delivery-live", 9101).await;
    let bot = seed_bot(&app, &admin, tenant, &token).await;
    drop(token);

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
        let task_id: i64 = sqlx::query_scalar(
            "INSERT INTO delivery_tasks
                (tenant_id, sender_identity_id, target_tg_chat_id, action, telegram_random_id, payload_snapshot)
             VALUES ($1, $2, $3, 'COPY', $4, $5) RETURNING id",
        )
        .bind(tenant)
        .bind(bot)
        .bind(target)
        .bind(random_id)
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
        verdicts.push((phase, dedupe, status));
    }

    println!("[V2] 人工核对：测试频道里每个 label 应当只出现一次。");
    for (phase, dedupe, status) in &verdicts {
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
