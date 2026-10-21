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
