//! WP-2 QR 全分支真机实测的 T5 / T6（`docs/plan/proposals/WP-2-qr-login-live.md` 片 1）。
//!
//! `#[ignore]`：要扫码登录的测试用户 session、平台 `api_id` 与测试频道，CI 与默认测试都不跑。
//! session 只从仓库外文件读入，本文件不打印、不落盘 session 与 `api_hash`。
//!
//! ```text
//! TGM_PLATFORM_API_ID            平台应用 api_id（握手只需要它）
//! TGM_LIVE_USER_SESSION_FILE     qr_login 的 live_user_qr_login 写出的 session
//! TGM_LIVE_TARGET_CHAT           测试频道的 Bot API chat id（T5 读一条消息证明已授权）
//! TGM_LIVE_PROBE_MESSAGE_ID      T5 读取的频道消息 id
//! TGM_LIVE_T6_SECONDS            T6 等待 worker 识别失效的上限，默认 180
//! TGM_LIVE_R1_SECONDS            R1 上线后等人撤销并识别的上限，默认 600
//! ```
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::time::{Duration, Instant};

use sqlx::PgPool;
use tgm_app::mtproto_session::{load_session, save_session};
use tgm_app::mtproto_worker::{IdentityExit, MtprotoTarget, MtprotoWorkerConfig, run_identity};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_db::repo::acquire_lease;
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{StoredSession, TelegramClient};
use tokio::sync::watch;

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("真机实测缺少环境变量 {name}"))
}

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

fn api_id() -> i32 {
    required("TGM_PLATFORM_API_ID")
        .parse()
        .expect("TGM_PLATFORM_API_ID 必须是整数")
}

struct Seeded {
    app: PgPool,
    admin: support::AdminOnlyPool,
    tenant: TenantId,
    identity: IdentityId,
}

/// 建租户与 MTProto 身份，把文件里的 session 经生产路径 `save_session` 加密落库，
/// 再补上 `api_id`（ADR-0013：从 `identity_secrets.api_id` 取）。
async fn seed_with_session(admin: PgPool, name: &str, owner: i64) -> Seeded {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, name, owner).await;
    let identity: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建 MTProto 身份失败");
    let bytes = std::fs::read(required("TGM_LIVE_USER_SESSION_FILE")).expect("读 session 失败");
    let client = TelegramClient::connect_with_session(
        TelegramClientConfig::new(api_id())
            .expect("api_id 无效")
            .without_catch_up(),
        StoredSession::restore(&bytes).expect("session 快照无效"),
    )
    .await
    .expect("用户账号建连失败");
    drop(bytes);
    save_session(&app, &kek(), tenant, identity, &client)
        .await
        .expect("session 加密落库失败");
    client.shutdown().await.expect("客户端关闭失败");
    sqlx::query("UPDATE identity_secrets SET api_id = $2 WHERE identity_id = $1")
        .bind(identity)
        .bind(api_id())
        .execute(admin.raw())
        .await
        .expect("写入 api_id 失败");
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await
        .expect("读角色失败");
    assert_eq!(role, "app_user");
    Seeded {
        app,
        admin,
        tenant,
        identity,
    }
}

async fn status_of(seeded: &Seeded) -> String {
    sqlx::query_scalar("SELECT status FROM identities WHERE id = $1")
        .bind(seeded.identity)
        .fetch_one(seeded.admin.raw())
        .await
        .expect("读身份状态失败")
}

/// T5：落库后丢掉全部客户端，只凭库里的密文恢复，仍是已授权用户，不要求再扫码。
#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要扫码登录的测试用户 session"]
async fn live_t5_session_survives_restart_via_database(admin: PgPool) {
    let seeded = seed_with_session(admin, "qr-t5", 9301).await;
    let stored: (i32, i32) = sqlx::query_as(
        "SELECT octet_length(secret_ciphertext), api_id FROM identity_secrets WHERE identity_id = $1",
    )
    .bind(seeded.identity)
    .fetch_one(seeded.admin.raw())
    .await
    .expect("读凭据行失败");

    let session = load_session(&seeded.app, &kek(), seeded.tenant, seeded.identity)
        .await
        .expect("解密 session 失败")
        .expect("库里应有 session");
    let client = TelegramClient::connect_with_session(
        TelegramClientConfig::new(api_id())
            .expect("api_id 无效")
            .without_catch_up(),
        session,
    )
    .await
    .expect("恢复后建连失败");
    let chat: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("频道 id 无效");
    let probe: i64 = required("TGM_LIVE_PROBE_MESSAGE_ID")
        .parse()
        .expect("消息 id 无效");
    let read = client
        .media_reader()
        .read_messages(chat, &[probe])
        .await
        .expect("恢复后的 session 读不到频道消息");
    client.shutdown().await.expect("客户端关闭失败");

    // 生产循环同样只凭库里的 session 上线：run_identity 置 online，收到停止信号后落库退出。
    let (stop, shutdown) = watch::channel(false);
    let pool = seeded.app.clone();
    let tenant = seeded.tenant;
    let identity = seeded.identity;
    let worker = tokio::spawn(async move {
        run_identity(
            &pool,
            &kek(),
            MtprotoTarget {
                tenant,
                identity,
                api_id: api_id(),
            },
            "qr-t5",
            MtprotoWorkerConfig::default(),
            None,
            shutdown,
        )
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while status_of(&seeded).await != "online" {
        assert!(
            Instant::now() < deadline,
            "worker 未能用库里的 session 上线"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    stop.send(true).expect("发停止信号失败");
    let exit = worker.await.expect("worker 任务失败").expect("worker 出错");
    assert_eq!(exit, IdentityExit::Shutdown);
    println!(
        "[T5] ciphertext_bytes={} api_id_set={} restored_read={} worker_online=true exit={exit:?} status_after={}",
        stored.0,
        stored.1 > 0,
        read.len(),
        status_of(&seeded).await
    );
}

/// T6：手机端结束本会话后，worker 必须识别为 unauthorized、写审计、释放租约，且不自动重试。
#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要已在手机端撤销的测试用户 session"]
async fn live_t6_revoked_session_marks_identity_unauthorized(admin: PgPool) {
    let seeded = seed_with_session(admin, "qr-t6", 9302).await;
    let seconds: u64 = std::env::var("TGM_LIVE_T6_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(180);
    let (_stop, shutdown) = watch::channel(false);
    let started = Instant::now();
    let exit = tokio::time::timeout(
        Duration::from_secs(seconds),
        run_identity(
            &seeded.app,
            &kek(),
            MtprotoTarget {
                tenant: seeded.tenant,
                identity: seeded.identity,
                api_id: api_id(),
            },
            "qr-t6",
            MtprotoWorkerConfig::default(),
            None,
            shutdown,
        ),
    )
    .await;
    let took = started.elapsed();
    let exit = match exit {
        Ok(result) => result,
        Err(_) => panic!(
            "{seconds}s 内未识别授权失效，status={}",
            status_of(&seeded).await
        ),
    };
    let exit = match exit {
        Ok(exit) => exit,
        Err(error) => panic!("worker 以错误退出（未归入 unauthorized）：{error}"),
    };
    assert_eq!(exit, IdentityExit::Unauthorized);
    assert_eq!(status_of(&seeded).await, "unauthorized");
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.unauthorized' AND resource_id = $2",
    )
    .bind(seeded.tenant)
    .bind(seeded.identity)
    .fetch_one(seeded.admin.raw())
    .await
    .expect("读审计失败");
    assert_eq!(audits, 1);
    let next = acquire_lease(&seeded.app, seeded.identity, "qr-t6-next")
        .await
        .expect("抢租约失败");
    assert!(next.is_some(), "失效后租约必须立即可接管");
    let candidates = tgm_db::repo::list_mtproto_worker_candidates(&seeded.app, seeded.tenant)
        .await
        .expect("读候选失败");
    assert!(
        candidates
            .iter()
            .all(|(identity, _)| *identity != seeded.identity),
        "unauthorized 身份不得再进候选（不自动重试）"
    );
    println!(
        "[T6] exit={exit:?} detected_after={took:?} status=unauthorized audits={audits} lease_takeover=ok in_candidates=false"
    );
}

/// R1：在线期间被撤销（`docs/plan/proposals/WP-2-remaining.md`）。T6 是上线前那次校验识别的；
/// 这里先上线，再由人在手机端结束会话，必须由运行中的周期校验或更新流识别出来。
#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要扫码登录的测试用户 session，上线后由人在手机端撤销"]
async fn live_r1_revoked_while_online(admin: PgPool) {
    let seeded = seed_with_session(admin, "wp2-r1", 9303).await;
    let seconds: u64 = std::env::var("TGM_LIVE_R1_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(600);
    let (_stop, shutdown) = watch::channel(false);
    let pool = seeded.app.clone();
    let tenant = seeded.tenant;
    let identity = seeded.identity;
    let config = MtprotoWorkerConfig {
        auth_check_interval: Duration::from_secs(10),
        ..MtprotoWorkerConfig::default()
    };
    let worker = tokio::spawn(async move {
        run_identity(
            &pool,
            &kek(),
            MtprotoTarget {
                tenant,
                identity,
                api_id: api_id(),
            },
            "wp2-r1",
            config,
            None,
            shutdown,
        )
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while status_of(&seeded).await != "online" {
        assert!(Instant::now() < deadline, "worker 未能上线");
        assert!(!worker.is_finished(), "worker 在上线前就退出了");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let online_at = Instant::now();
    println!("[R1] online, revoke now");
    let exit = match tokio::time::timeout(Duration::from_secs(seconds), worker).await {
        Ok(joined) => joined.expect("worker 任务失败"),
        Err(_) => panic!("{seconds}s 内未识别在线期间的撤销"),
    };
    let online_for = online_at.elapsed();
    let exit = match exit {
        Ok(exit) => exit,
        Err(error) => panic!("worker 以错误退出（未归入 unauthorized）：{error}"),
    };
    assert_eq!(exit, IdentityExit::Unauthorized);
    assert_eq!(status_of(&seeded).await, "unauthorized");
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.unauthorized' AND resource_id = $2",
    )
    .bind(seeded.tenant)
    .bind(seeded.identity)
    .fetch_one(seeded.admin.raw())
    .await
    .expect("读审计失败");
    assert_eq!(audits, 1);
    let next = acquire_lease(&seeded.app, seeded.identity, "wp2-r1-next")
        .await
        .expect("抢租约失败");
    assert!(next.is_some(), "失效后租约必须立即可接管");
    println!(
        "[R1] exit={exit:?} online_until_detected={online_for:?} status=unauthorized audits={audits} lease_takeover=ok"
    );
}

// ───────────── WP-2-live-updates：U1–U3（docs/plan/proposals/WP-2-live-updates.md） ─────────────

/// 测试 Bot 经 Bot API 往测试频道发消息。只用于造真机输入；采集身份本身不发送（spec/08 §1.4）。
struct ChannelBot {
    http: reqwest::Client,
    base: String,
    chat: i64,
    sent: Vec<i64>,
}

impl ChannelBot {
    fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base: format!(
                "https://api.telegram.org/bot{}",
                required("TGM_LIVE_BOT_TOKEN")
            ),
            chat: required("TGM_LIVE_TARGET_CHAT")
                .parse()
                .expect("频道 id 无效"),
            sent: Vec::new(),
        }
    }

    async fn call(&self, method: &str, body: serde_json::Value) -> serde_json::Value {
        let response: serde_json::Value = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|_| panic!("Bot API {method} 请求失败"))
            .json()
            .await
            .unwrap_or_else(|_| panic!("Bot API {method} 响应无效"));
        assert_eq!(
            response["ok"],
            serde_json::Value::Bool(true),
            "Bot API {method} 失败：{}",
            response["description"]
        );
        response["result"].clone()
    }

    async fn send(&mut self, text: &str) -> i64 {
        let result = self
            .call(
                "sendMessage",
                serde_json::json!({ "chat_id": self.chat, "text": text }),
            )
            .await;
        let id = result["message_id"].as_i64().expect("缺 message_id");
        self.sent.push(id);
        id
    }

    /// 返回 Telegram 给的 `edit_date`。
    async fn edit(&self, message_id: i64, text: &str) -> i64 {
        let result = self
            .call(
                "editMessageText",
                serde_json::json!({ "chat_id": self.chat, "message_id": message_id, "text": text }),
            )
            .await;
        result["edit_date"].as_i64().expect("缺 edit_date")
    }

    async fn cleanup(&self) -> usize {
        let mut deleted = 0;
        for id in &self.sent {
            let ok = self
                .http
                .post(format!("{}/deleteMessage", self.base))
                .json(&serde_json::json!({ "chat_id": self.chat, "message_id": id }))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success());
            deleted += usize::from(ok);
        }
        deleted
    }
}

/// `(event_type, edit_version) -> 行数`，按 `uq_canonical` 的列统计。
async fn event_rows(seeded: &Seeded, chat: i64, message_id: i64) -> Vec<(String, i32, i64)> {
    sqlx::query_as(
        "SELECT event_type, edit_version, count(*) FROM canonical_events
          WHERE receiver_identity_id = $1 AND tg_chat_id = $2 AND tg_message_id = $3
          GROUP BY event_type, edit_version ORDER BY event_type, edit_version",
    )
    .bind(seeded.identity)
    .bind(chat)
    .bind(message_id)
    .fetch_all(seeded.admin.raw())
    .await
    .expect("读事件失败")
}

async fn text_of(seeded: &Seeded, chat: i64, message_id: i64, edit_version: i32) -> Option<String> {
    sqlx::query_scalar(
        "SELECT text FROM canonical_events
          WHERE receiver_identity_id = $1 AND tg_chat_id = $2 AND tg_message_id = $3 AND edit_version = $4",
    )
    .bind(seeded.identity)
    .bind(chat)
    .bind(message_id)
    .bind(edit_version)
    .fetch_optional(seeded.admin.raw())
    .await
    .expect("读事件正文失败")
    .flatten()
}

/// 只数测试频道：测试账号在别的对话里收到的消息同样落库，不能混进判据。
async fn total_rows(seeded: &Seeded, chat: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM canonical_events WHERE receiver_identity_id = $1 AND tg_chat_id = $2",
    )
    .bind(seeded.identity)
    .bind(chat)
    .fetch_one(seeded.admin.raw())
    .await
    .expect("读事件总数失败")
}

/// `updated_at` 的微秒时间戳；只做先后比较，不引 chrono。
async fn secret_updated_at(seeded: &Seeded) -> i64 {
    sqlx::query_scalar(
        "SELECT (extract(epoch FROM updated_at) * 1000000)::bigint FROM identity_secrets WHERE identity_id = $1",
    )
        .bind(seeded.identity)
        .fetch_one(seeded.admin.raw())
        .await
        .expect("读凭据行失败")
}

/// 等到 `expected` 中每个 `(message_id, event_type, edit_version)` 都恰好一行。
async fn wait_events(
    seeded: &Seeded,
    chat: i64,
    expected: &[(i64, &str, i32)],
    label: &str,
    seconds: u64,
) -> Duration {
    let started = Instant::now();
    loop {
        let mut all = true;
        for (message_id, kind, version) in expected {
            let rows = event_rows(seeded, chat, *message_id).await;
            let count = rows
                .iter()
                .find(|(event, edit, _)| event == kind && edit == version)
                .map_or(0, |row| row.2);
            assert!(
                count <= 1,
                "[{label}] message {message_id} {kind}/{version} 重复 {count} 行"
            );
            all &= count == 1;
        }
        if all {
            return started.elapsed();
        }
        assert!(
            started.elapsed() < Duration::from_secs(seconds),
            "[{label}] {seconds}s 内未落齐：{expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn spawn_worker(
    seeded: &Seeded,
    worker_id: &'static str,
) -> (
    watch::Sender<bool>,
    tokio::task::JoinHandle<Result<IdentityExit, tgm_app::mtproto_worker::MtprotoWorkerError>>,
) {
    let (stop, shutdown) = watch::channel(false);
    let pool = seeded.app.clone();
    let target = MtprotoTarget {
        tenant: seeded.tenant,
        identity: seeded.identity,
        api_id: api_id(),
    };
    let task = tokio::spawn(async move {
        run_identity(
            &pool,
            &kek(),
            target,
            worker_id,
            MtprotoWorkerConfig::default(),
            None,
            shutdown,
        )
        .await
    });
    (stop, task)
}

async fn wait_online(seeded: &Seeded, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while status_of(seeded).await != "online" {
        assert!(Instant::now() < deadline, "[{label}] worker 未上线");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// U1 实时收消息与编辑；U2 正常退出后 `catch_up` 补齐；U3 硬停后从定时落库续上。
#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要扫码登录的测试用户 session 与测试 Bot"]
async fn live_updates_realtime_catch_up_and_hard_stop(admin: PgPool) {
    let seeded = std::sync::Arc::new(seed_with_session(admin, "live-updates", 9303).await);
    let bot = std::sync::Arc::new(tokio::sync::Mutex::new(ChannelBot::new()));
    let nonce = format!("{:x}", now_nanos_u64());
    // 主体放进独立任务：断言失败时任务 panic 变成 JoinError，频道消息仍能清理。
    let body = tokio::spawn({
        let seeded = std::sync::Arc::clone(&seeded);
        let bot = std::sync::Arc::clone(&bot);
        async move { updates_body(&seeded, &bot, &nonce).await }
    })
    .await;
    let bot = bot.lock().await;
    let deleted = bot.cleanup().await;
    println!("[U] cleanup sent={} deleted={deleted}", bot.sent.len());
    if let Err(error) = body {
        std::panic::resume_unwind(error.into_panic());
    }
}

fn now_nanos_u64() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("时钟早于 1970")
            .as_nanos()
            % u128::from(u64::MAX),
    )
    .expect("取模后放得下")
}

async fn updates_body(seeded: &Seeded, bot: &tokio::sync::Mutex<ChannelBot>, nonce: &str) {
    let chat = bot.lock().await.chat;
    // U1：在线时实时收新消息与编辑。
    let (stop, worker) = spawn_worker(seeded, "live-updates-1");
    wait_online(seeded, "U1").await;
    let u1_text = format!("tgm-live-updates {nonce} U1");
    let u1 = bot.lock().await.send(&u1_text).await;
    let took_new = wait_events(seeded, chat, &[(u1, "MESSAGE_CREATED", 0)], "U1", 30).await;
    assert_eq!(
        text_of(seeded, chat, u1, 0).await.as_deref(),
        Some(u1_text.as_str())
    );
    let u1_edited = format!("{u1_text} edited");
    let u1_edit =
        i32::try_from(bot.lock().await.edit(u1, &u1_edited).await).expect("edit_date 越界");
    let took_edit = wait_events(seeded, chat, &[(u1, "MESSAGE_EDITED", u1_edit)], "U1", 30).await;
    assert_eq!(
        text_of(seeded, chat, u1, u1_edit).await.as_deref(),
        Some(u1_edited.as_str())
    );
    println!("[U1] new_after={took_new:?} edit_after={took_edit:?} edit_version=edit_date");

    // U2：正常退出（退出前落库），离线期间发 2 条、编辑 1 条，重启后补齐。
    stop.send(true).expect("发停止信号失败");
    let exit = worker.await.expect("worker 任务失败").expect("worker 出错");
    assert_eq!(exit, IdentityExit::Shutdown);
    let before_u2 = total_rows(seeded, chat).await;
    let u2a_edited = format!("tgm-live-updates {nonce} U2a edited");
    let u2a = bot
        .lock()
        .await
        .send(&format!("tgm-live-updates {nonce} U2a"))
        .await;
    let u2b = bot
        .lock()
        .await
        .send(&format!("tgm-live-updates {nonce} U2b"))
        .await;
    let u2_edit =
        i32::try_from(bot.lock().await.edit(u2a, &u2a_edited).await).expect("edit_date 越界");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (stop, worker) = spawn_worker(seeded, "live-updates-2");
    wait_online(seeded, "U2").await;
    let took_u2 = wait_events(
        seeded,
        chat,
        &[(u2a, "MESSAGE_CREATED", 0), (u2b, "MESSAGE_CREATED", 0)],
        "U2",
        60,
    )
    .await;
    // 离线期间的编辑有两种合法落法：单独一条 MESSAGE_EDITED，或服务端补齐时直接给出编辑后的正文。
    // 两种都不丢内容；如实记录是哪一种，不预设。
    tokio::time::sleep(Duration::from_secs(5)).await;
    let u2a_rows = event_rows(seeded, chat, u2a).await;
    assert!(
        u2a_rows.iter().all(|row| row.2 == 1),
        "[U2] U2a 有重复行：{u2a_rows:?}"
    );
    let edit_row = u2a_rows
        .iter()
        .any(|(kind, version, _)| kind == "MESSAGE_EDITED" && *version == u2_edit);
    let created_text = text_of(seeded, chat, u2a, 0).await;
    let edit_text = text_of(seeded, chat, u2a, u2_edit).await;
    let edit_captured = edit_text.as_deref() == Some(u2a_edited.as_str())
        || created_text.as_deref() == Some(u2a_edited.as_str());
    assert!(
        edit_captured,
        "[U2] 离线期间的编辑内容没有落库：edit_row={edit_row}"
    );
    let after_u2 = total_rows(seeded, chat).await;
    assert_eq!(
        event_rows(seeded, chat, u1).await.len(),
        2,
        "[U2] U1 消息不得新增行"
    );
    println!(
        "[U2] exit_before=Shutdown caught_up_after={took_u2:?} chat_rows_added={} edit_as_separate_row={edit_row} created_has_edited_text={}",
        after_u2 - before_u2,
        created_text.as_deref() == Some(u2a_edited.as_str())
    );

    // U3：在线超过一个落库周期后硬停（不做退出前落库），租约过期后重启。
    let saved_before = secret_updated_at(seeded).await;
    tokio::time::sleep(Duration::from_secs(75)).await;
    let saved_periodic = secret_updated_at(seeded).await;
    worker.abort();
    let _ = worker.await;
    drop(stop);
    sqlx::query(
        "UPDATE account_leases SET expires_at = clock_timestamp() - interval '1 second'
          WHERE identity_id = $1",
    )
    .bind(seeded.identity)
    .execute(seeded.admin.raw())
    .await
    .expect("让租约过期失败");
    let saved_after_abort = secret_updated_at(seeded).await;
    let before_u3 = total_rows(seeded, chat).await;
    let u3a = bot
        .lock()
        .await
        .send(&format!("tgm-live-updates {nonce} U3a"))
        .await;
    let u3b = bot
        .lock()
        .await
        .send(&format!("tgm-live-updates {nonce} U3b"))
        .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (stop, worker) = spawn_worker(seeded, "live-updates-3");
    wait_online(seeded, "U3").await;
    let took_u3 = wait_events(
        seeded,
        chat,
        &[(u3a, "MESSAGE_CREATED", 0), (u3b, "MESSAGE_CREATED", 0)],
        "U3",
        60,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let after_u3 = total_rows(seeded, chat).await;
    assert_eq!(
        event_rows(seeded, chat, u1).await.len(),
        2,
        "[U3] U1 消息不得新增行"
    );
    assert_eq!(
        event_rows(seeded, chat, u2a).await,
        u2a_rows,
        "[U3] U2a 不得新增行"
    );
    assert_eq!(
        event_rows(seeded, chat, u2b).await.len(),
        1,
        "[U3] U2b 不得新增行"
    );
    stop.send(true).expect("发停止信号失败");
    let exit = worker.await.expect("worker 任务失败").expect("worker 出错");
    assert_eq!(exit, IdentityExit::Shutdown);
    println!(
        "[U3] periodic_save={} unchanged_by_abort={} caught_up_after={took_u3:?} chat_rows_added={} expected_added=2 exit={exit:?}",
        saved_periodic > saved_before,
        saved_after_abort == saved_periodic,
        after_u3 - before_u3
    );
}
