//! MTProto 用户账号 QR 登录真机工具（提案 `docs/plan/proposals/WP-5-delivery-media.md`
//! 片 2 批准记录第 6 条：真机验收用扫码登录的测试用户账号作为采集身份）。
//!
//! `#[ignore]`：需要人用 Telegram 手机端扫码，CI 与默认测试都不跑。不进生产代码。
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
//! ```
#![allow(clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use grammers_client::{Client, InvocationError, SenderPool, tl};
use grammers_session::Session;
use tgm_tg::StoredSession;

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
    Pending(Vec<u8>),
    Migrate { dc_id: i32, token: Vec<u8> },
    Success,
    PasswordNeeded,
}

fn classify(result: Result<tl::enums::auth::LoginToken, InvocationError>) -> Step {
    match result {
        Ok(tl::enums::auth::LoginToken::Token(token)) => Step::Pending(token.token),
        Ok(tl::enums::auth::LoginToken::MigrateTo(migrate)) => Step::Migrate {
            dc_id: migrate.dc_id,
            token: migrate.token,
        },
        Ok(tl::enums::auth::LoginToken::Success(_)) => Step::Success,
        Err(InvocationError::Rpc(error)) if error.is("SESSION_PASSWORD_NEEDED") => {
            Step::PasswordNeeded
        }
        Err(error) => panic!("QR 登录调用失败: {error}"),
    }
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

    let session = Arc::new(StoredSession::default());
    let SenderPool {
        runner,
        updates,
        handle,
    } = SenderPool::new(Arc::clone(&session), api_id);
    drop(updates);
    let client = Client::new(handle.clone());
    let runner = tokio::spawn(async move { runner.run().await });

    let export = || tl::functions::auth::ExportLoginToken {
        api_id,
        api_hash: api_hash.clone(),
        except_ids: Vec::new(),
    };
    let mut generation = 0_u32;
    let mut current: Option<Vec<u8>> = None;
    let mut migrations = Vec::new();
    let mut password_needed = false;
    loop {
        assert!(Instant::now() < deadline, "等待扫码超时");
        match classify(client.invoke(&export()).await) {
            Step::Pending(token) => {
                if current.as_ref() != Some(&token) {
                    generation += 1;
                    let link = format!(
                        "tg://login?token={}",
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&token)
                    );
                    write_private(&link_path, link.as_bytes(), true);
                    println!("[QR] generation={generation} 链接已写入 TGM_LIVE_QR_LINK_FILE");
                    current = Some(token);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Step::Migrate { dc_id, token } => {
                let old_dc = session.home_dc_id().expect("读 home dc 失败");
                println!("[QR] LoginTokenMigrateTo dc {old_dc} -> {dc_id}");
                migrations.push((old_dc, dc_id));
                handle.disconnect_from_dc(old_dc);
                session
                    .set_home_dc_id(dc_id)
                    .await
                    .expect("设置 home dc 失败");
                match classify(
                    client
                        .invoke(&tl::functions::auth::ImportLoginToken { token })
                        .await,
                ) {
                    Step::Success => break,
                    Step::PasswordNeeded => {
                        password_needed = true;
                        break;
                    }
                    Step::Pending(_) | Step::Migrate { .. } => {
                        panic!("importLoginToken 之后仍未授权")
                    }
                }
            }
            Step::Success => break,
            Step::PasswordNeeded => {
                password_needed = true;
                break;
            }
        }
    }

    if password_needed {
        let password_path = PathBuf::from(
            std::env::var("TGM_LIVE_2FA_PASSWORD_FILE")
                .expect("账号开了 2FA，需要 TGM_LIVE_2FA_PASSWORD_FILE"),
        );
        println!("[QR] SESSION_PASSWORD_NEEDED，等待 2FA 密码文件");
        let password = loop {
            assert!(Instant::now() < deadline, "等待 2FA 密码超时");
            if let Ok(bytes) = std::fs::read(&password_path) {
                let _ = std::fs::remove_file(&password_path);
                break bytes;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        let password = String::from_utf8(password).expect("密码不是 UTF-8");
        let token = match client
            .invoke(&tl::functions::account::GetPassword {})
            .await
            .expect("account.getPassword 失败")
        {
            tl::enums::account::Password::Password(password) => {
                grammers_client::client::PasswordToken::new(password)
            }
        };
        client
            .check_password(token, password.trim_end_matches(['\r', '\n']))
            .await
            .expect("2FA 校验失败");
    }

    let me = client.get_me().await.expect("登录后 getMe 失败");
    assert!(!me.is_bot(), "登录的是 Bot，不是用户账号");
    assert!(client.is_authorized().await.expect("is_authorized 失败"));
    // getMe 只回 User；grammers 靠缓存里 is_self 的 User 判断「已登录」，importLoginToken
    // 路径不像 sign_in 那样替我们写入，这里补上，让 worker 恢复 session 时走已登录分支。
    let tl::enums::User::User(raw) = &me.raw else {
        panic!("getMe 返回空用户")
    };
    session
        .cache_peer(&grammers_session::types::PeerInfo::User {
            id: raw.id,
            auth: raw
                .access_hash
                .map(grammers_session::types::PeerAuth::from_hash),
            bot: Some(false),
            is_self: Some(true),
        })
        .await
        .expect("缓存 self 失败");
    assert!(session.is_signed_in().expect("读 session 失败"));
    let snapshot = session.snapshot().expect("取 session 失败");
    write_private(&session_path, snapshot.expose(), false);
    let _ = std::fs::remove_file(&link_path);
    println!(
        "[QR] authorized generations={generation} migrations={migrations:?} 2fa={password_needed} \
         home_dc={} session_bytes={}",
        session.home_dc_id().expect("读 home dc 失败"),
        snapshot.expose().len()
    );
    handle.quit();
    let _ = runner.await;
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
