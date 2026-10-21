//! MTProto 用户账号 QR 登录真机工具（提案 `docs/plan/proposals/WP-5-delivery-media.md`
//! 片 2 批准记录第 6 条：真机验收用扫码登录的测试用户账号作为采集身份）。
//!
//! `#[ignore]`：需要人用 Telegram 手机端扫码，CI 与默认测试都不跑。不进生产代码。
//! WP-2 QR 全分支实测（`docs/plan/proposals/WP-2-qr-login-live.md` 片 1）复用本工具，
//! 按 T1–T4 打印可判定的行。登录走生产驱动 `tgm_tg::qr_login::QrLoginDriver`
//! （控制面端点同一实现），真机事件序列同时喂给 `tgm_tg::qr` 的纯状态机。
//! 走 `auth.exportLoginToken` → 轮询 → `auth.importLoginToken`（DC 迁移）→ 可选 2FA，
//! 把登录后的 session 快照写到仓库外的文件（0600）。session 含 auth key，等同登录凭据：
//! 本工具只打印 `tg://login?token=...` 链接与登录结果，不打印 session、`api_hash`、密码。
//! 链接本身 30 秒左右过期、只能被扫码确认一次。
//!
//! ```text
//! TGM_PLATFORM_API_ID / TGM_PLATFORM_API_HASH   平台应用凭据
//! TGM_LIVE_USER_SESSION_FILE                    session 输出路径（仓库外，必须不存在）
//! TGM_LIVE_QR_LINK_FILE                         每次导出新 token 时覆盖写入链接（供人渲染二维码）
//! TGM_LIVE_2FA_PASSWORD_FILE                    可选：2FA 密码文件；出现 SESSION_PASSWORD_NEEDED 时
//!                                               轮询等待该文件出现，读完即删
//! TGM_LIVE_QR_MINUTES                           等待扫码的总时长，默认 10 分钟
//! TGM_LIVE_QR_EXPIRY_PROBE                      可选：=1 时先跑 T4（首代 token 不展示，过期后导入必须不授权）
//! ```
#![allow(clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use grammers_client::{Client, InvocationError, SenderPool, tl};
use grammers_session::Session;
use tgm_core::secret::Secret;
use tgm_tg::StoredSession;
use tgm_tg::qr::QrLoginState;
use tgm_tg::qr_login::{QrLoginDriver, QrPassword, QrPoll};

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("缺少环境变量 {name}"))
}

fn write_private(path: &PathBuf, bytes: &[u8], overwrite: bool) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).mode(0o600);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(path)
        .unwrap_or_else(|error| panic!("写 {} 失败: {error}", path.display()));
    file.write_all(bytes).expect("写文件失败");
}

enum Step {
    Pending { token: Vec<u8>, expires: i64 },
    Migrate,
    Success,
    PasswordNeeded,
}

fn classify(result: Result<tl::enums::auth::LoginToken, InvocationError>) -> Step {
    match result {
        Ok(tl::enums::auth::LoginToken::Token(token)) => Step::Pending {
            token: token.token,
            expires: i64::from(token.expires),
        },
        Ok(tl::enums::auth::LoginToken::MigrateTo(_)) => Step::Migrate,
        Ok(tl::enums::auth::LoginToken::Success(_)) => Step::Success,
        Err(InvocationError::Rpc(error)) if error.is("SESSION_PASSWORD_NEEDED") => {
            Step::PasswordNeeded
        }
        Err(error) => panic!("QR 登录调用失败: {error}"),
    }
}

fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("读取时间失败")
            .as_secs(),
    )
    .expect("时间溢出")
}

/// T4：导出第一代 token 后不展示、等它过期，再用它 `importLoginToken`，必须不授权。
async fn expired_token_probe(client: &Client, export: &tl::functions::auth::ExportLoginToken) {
    let Step::Pending { token, expires } = classify(client.invoke(export).await) else {
        panic!("首次导出应返回待扫码 token")
    };
    let ttl = expires - now_unix();
    println!("[T4] probe generation=0 ttl_secs={ttl}（不展示，等过期）");
    tokio::time::sleep(Duration::from_secs(
        u64::try_from(ttl.max(0)).unwrap_or(0) + 3,
    ))
    .await;
    let result = client
        .invoke(&tl::functions::auth::ImportLoginToken {
            token: token.clone(),
        })
        .await;
    let outcome = match &result {
        Ok(tl::enums::auth::LoginToken::Token(fresh)) => {
            format!("token same_as_old={}", fresh.token == token)
        }
        Ok(tl::enums::auth::LoginToken::MigrateTo(_)) => "migrate_to".to_owned(),
        Ok(tl::enums::auth::LoginToken::Success(_)) => "success".to_owned(),
        Err(InvocationError::Rpc(error)) => format!("rpc {} {}", error.code, error.name),
        Err(error) => format!("err {error}"),
    };
    println!("[T4] expired_import={outcome}");
    assert!(
        !matches!(result, Ok(tl::enums::auth::LoginToken::Success(_))),
        "过期 token 不得授权"
    );
    if let Ok(tl::enums::auth::LoginToken::Token(fresh)) = &result {
        assert_ne!(fresh.token, token, "过期 token 不得原样复用");
    }
    let Step::Pending { token: next, .. } = classify(client.invoke(export).await) else {
        panic!("过期后重新导出应返回待扫码 token")
    };
    assert_ne!(next, token, "过期后重新导出必须是新 token");
    println!("[T4] refreshed=new_token");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "真机：需要人用 Telegram 手机端扫码"]
async fn live_user_qr_login() {
    let api_id: i32 = required("TGM_PLATFORM_API_ID")
        .parse()
        .expect("TGM_PLATFORM_API_ID 必须是整数");
    let api_hash = required("TGM_PLATFORM_API_HASH");
    let session_path = PathBuf::from(required("TGM_LIVE_USER_SESSION_FILE"));
    let link_path = PathBuf::from(required("TGM_LIVE_QR_LINK_FILE"));
    assert!(
        !session_path.exists(),
        "session 文件已存在，先移走旧文件再跑"
    );
    let minutes: u64 = std::env::var("TGM_LIVE_QR_MINUTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);

    if std::env::var("TGM_LIVE_QR_EXPIRY_PROBE").as_deref() == Ok("1") {
        let session = Arc::new(StoredSession::default());
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(Arc::clone(&session), api_id);
        drop(updates);
        let client = Client::new(handle.clone());
        let runner = tokio::spawn(async move { runner.run().await });
        let export = tl::functions::auth::ExportLoginToken {
            api_id,
            api_hash: api_hash.clone(),
            except_ids: Vec::new(),
        };
        expired_token_probe(&client, &export).await;
        handle.quit();
        let _ = runner.await;
    }

    // 生产驱动（tgm_tg::qr_login）：控制面端点与本工具走同一条真机路径。
    let mut driver = QrLoginDriver::start(api_id, &Secret::new(api_hash)).expect("建连失败");
    let start_dc = driver.home_dc_id().expect("读 home dc 失败");
    println!("[QR] start_dc={start_dc}");
    let mut shown = 0;
    let password_needed = loop {
        assert!(Instant::now() < deadline, "等待扫码超时");
        match driver.poll(now_unix()).await.expect("QR 登录推进失败") {
            QrPoll::Pending {
                link,
                generation,
                expires_at_unix,
            } => {
                if generation != shown {
                    write_private(&link_path, link.expose().as_bytes(), true);
                    println!(
                        "[QR] generation={generation} ttl_secs={} 链接已写入 TGM_LIVE_QR_LINK_FILE",
                        expires_at_unix - now_unix()
                    );
                    shown = generation;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            QrPoll::PasswordNeeded => break true,
            QrPoll::Authorized => break false,
        }
    };
    for (from, to) in driver.migrations() {
        println!("[QR] LoginTokenMigrateTo dc {from} -> {to}");
    }

    let mut rejected = 0;
    if password_needed {
        let password_path = PathBuf::from(
            std::env::var("TGM_LIVE_2FA_PASSWORD_FILE")
                .expect("账号开了 2FA，需要 TGM_LIVE_2FA_PASSWORD_FILE"),
        );
        println!("[QR] SESSION_PASSWORD_NEEDED，等待 2FA 密码文件");
        loop {
            let password = loop {
                assert!(Instant::now() < deadline, "等待 2FA 密码超时");
                if let Ok(bytes) = std::fs::read(&password_path) {
                    let _ = std::fs::remove_file(&password_path);
                    break bytes;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            };
            let password = String::from_utf8(password).expect("密码不是 UTF-8");
            let password = Secret::new(password.trim_end_matches(['\r', '\n']).to_owned());
            match driver
                .submit_password(&password)
                .await
                .expect("2FA 校验失败")
            {
                QrPassword::Accepted => {
                    println!("[T3] password=accepted rejected_before={rejected}");
                    break;
                }
                QrPassword::Rejected => {
                    rejected += 1;
                    assert!(
                        matches!(driver.state(), QrLoginState::AwaitingPassword { .. }),
                        "错误密码后状态必须停在等密码"
                    );
                    println!(
                        "[T3] password=rejected count={rejected} state=AwaitingPassword，等待下一个密码文件"
                    );
                }
            }
        }
    }
    assert_eq!(
        driver.state(),
        QrLoginState::Authorized,
        "状态机未到 Authorized"
    );
    let migrations = driver.migrations().to_vec();
    let generations = shown;
    let home_dc = driver.home_dc_id().expect("读 home dc 失败");
    let authorized = driver.finish().await.expect("取 session 失败");
    driver.close().await;
    write_private(&session_path, authorized.session.expose(), false);
    let _ = std::fs::remove_file(&link_path);
    println!(
        "[QR] authorized generations={generations} migrations={migrations:?} 2fa={password_needed} \
         password_rejected={rejected} start_dc={start_dc} home_dc={home_dc} session_bytes={}",
        authorized.session.expose().len()
    );
}

/// 用 `live_user_qr_login` 写出的 session 恢复，确认是已登录的用户账号，
/// 并能经用户名解析测试频道、读到最近消息（采集身份读源频道的前提）。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "真机：需要 live_user_qr_login 写出的 session 文件"]
async fn live_user_session_resume() {
    let api_id: i32 = required("TGM_PLATFORM_API_ID")
        .parse()
        .expect("TGM_PLATFORM_API_ID 必须是整数");
    let session_path = PathBuf::from(required("TGM_LIVE_USER_SESSION_FILE"));
    let bytes = std::fs::read(&session_path).expect("读 session 失败");
    let session = Arc::new(StoredSession::restore(&bytes).expect("session 快照无效"));
    let SenderPool {
        runner,
        updates,
        handle,
    } = SenderPool::new(Arc::clone(&session), api_id);
    drop(updates);
    let client = Client::new(handle.clone());
    let runner = tokio::spawn(async move { runner.run().await });
    let me = client.get_me().await.expect("恢复后 getMe 失败");
    // 探针：不带 access_hash（ambient）的频道引用，用户账号能不能读消息。
    let target: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数");
    let ambient = grammers_session::types::PeerId::from_bot_api_dialog_id(target)
        .expect("chat id 无效")
        .to_ambient_ref();
    match client.get_messages_by_id(ambient, &[1]).await {
        Ok(_) => println!("[resume] ambient_read=ok"),
        Err(error) => println!("[resume] ambient_read=err {error}"),
    }
    let username = required("TGM_LIVE_TARGET_USERNAME");
    let channel = client
        .resolve_username(username.trim_start_matches('@'))
        .await
        .expect("解析频道失败")
        .expect("频道不存在");
    let mut messages = client
        .iter_messages(
            channel
                .to_ref()
                .await
                .expect("取引用失败")
                .expect("引用为空"),
        )
        .limit(3);
    let mut seen = 0;
    while let Some(_message) = messages.next().await.expect("读消息失败") {
        seen += 1;
    }
    println!(
        "[resume] is_bot={} home_dc={} recent_messages={seen}",
        me.is_bot(),
        session.home_dc_id().expect("读 home dc 失败")
    );
    assert!(!me.is_bot());
    // 解析用户名时 grammers 把频道的 access_hash 缓存进 session；写回文件，
    // 媒体中转真机验收（crates/app/tests/media_relay_live.rs）靠它读源消息。
    let snapshot = session.snapshot().expect("取 session 失败");
    write_private(&session_path, snapshot.expose(), true);
    handle.quit();
    let _ = runner.await;
}
