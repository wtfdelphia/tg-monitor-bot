#![allow(clippy::expect_used, clippy::panic)]

mod support;

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, routing::post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use sqlx::PgPool;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tgm_app::control::{ControlPlaneState, CredentialIntake, router};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_db::repo::{WebSessionLoginMethod, issue_web_session_for_user};
use tokio::net::TcpListener;

const TEST_BOT_TOKEN: &str = "123456:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

async fn qr_http_stub(request: Request, next: Next, polls: &AtomicUsize) -> Response {
    let path = request.uri().path().to_owned();
    if !path.starts_with("/api/v1/identities/login/") {
        return next.run(request).await;
    }
    let pending = || {
        json!({
            "status": "pending",
            "qr_svg": "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"256\" height=\"256\"><path d=\"M0 0h256v256H0z\"/></svg>",
            "expires_at": (sqlx::types::chrono::Utc::now() + std::time::Duration::from_secs(30)).to_rfc3339(),
        })
    };
    let body = if path.ends_with("/password") {
        let bytes = to_bytes(request.into_body(), 4096)
            .await
            .expect("读取浏览器密码桩请求失败");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("解析密码桩请求失败");
        if body["password"] == "browser-wrong-password" {
            json!({"status":"awaiting_password","password_rejected":true})
        } else {
            json!({"status":"confirmed"})
        }
    } else if path.ends_with("/qr") {
        let bytes = to_bytes(request.into_body(), 4096)
            .await
            .expect("读取浏览器 QR 桩请求失败");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("解析 QR 桩请求失败");
        assert_eq!(body["consent_text_version"], "custody-v0");
        assert!(body["identity_id"].as_i64().is_some());
        polls.store(0, Ordering::SeqCst);
        let mut response = pending();
        response["login_id"] = json!("A".repeat(43));
        response
    } else if polls.fetch_add(1, Ordering::SeqCst) == 0 {
        let mut response = pending();
        response["qr_svg"] = json!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"257\" height=\"257\"><path d=\"M0 0h257v257H0z\"/></svg>"
        );
        response
    } else {
        json!({"status":"awaiting_password"})
    };
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        "cache-control",
        "no-store".parse().expect("构造测试响应头失败"),
    );
    response
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "需要 Node、Puppeteer 和真实 Chrome；显式运行，不以默认 CI 跳过作为通过"]
async fn console_real_browser_uses_restricted_http_and_temporary_database(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth = support::auth_pool(&admin).await;
    let app_role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await
        .expect("读取业务连接角色失败");
    let auth_role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&auth)
        .await
        .expect("读取鉴权连接角色失败");
    assert_eq!(app_role, "app_user");
    assert_eq!(auth_role, "auth_lookup");

    let tenant = support::seed_tenant(&admin, "browser-console", 88001).await;
    let other_tenant = support::seed_tenant(&admin, "browser-other", 88003).await;
    for (user_id, role) in [(88001_i64, "TENANT_ADMIN"), (88002, "VIEWER")] {
        sqlx::query("INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(tenant)
            .bind(user_id)
            .bind(role)
            .execute(admin.raw())
            .await
            .expect("创建浏览器测试成员失败");
    }
    let identity = support::seed_identity(&admin, tenant).await;
    sqlx::query("INSERT INTO identities (kind, tenant_id, tg_user_id, display_name, status) VALUES ('mtproto', $1, 88004, '扫码测试账号', 'unauthorized')")
        .bind(tenant)
        .execute(admin.raw())
        .await
        .expect("创建重新授权测试身份失败");
    sqlx::query("UPDATE identities SET display_name = $1 WHERE id = $2")
        .bind("浏览器测试身份-abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz")
        .bind(identity)
        .execute(admin.raw())
        .await
        .expect("设置测试身份名称失败");
    sqlx::query(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, title, sender_identity_id, enabled)
         VALUES ($1, -10088001, $2, $3, TRUE)",
    )
    .bind(tenant)
    .bind("浏览器目标-abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz")
    .bind(identity)
    .execute(admin.raw())
    .await
    .expect("创建测试目标失败");
    for (keyword_tenant, word) in [
        (
            tenant,
            "<img src=x onerror=\"window.browserInjected=true\">",
        ),
        (other_tenant, "OTHER_TENANT_BROWSER_SENTINEL"),
    ] {
        sqlx::query(
            "INSERT INTO keywords (tenant_id, word, match_type, enabled)
             VALUES ($1, $2, 'fuzzy', TRUE)",
        )
        .bind(keyword_tenant)
        .bind(word)
        .execute(admin.raw())
        .await
        .expect("创建测试关键词失败");
    }

    let mut sessions = Vec::new();
    for user_id in [88001_i64, 88002, 88001] {
        sessions.push(
            issue_web_session_for_user(
                &auth,
                user_id,
                "127.0.0.1",
                Some("console-browser-fixture"),
                WebSessionLoginMethod::Oidc,
            )
            .await
            .expect("签发本地测试会话失败")
            .expect("测试用户应只有一个成员关系"),
        );
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定浏览器测试监听失败");
    let origin = format!(
        "http://{}",
        listener.local_addr().expect("读取测试监听地址失败")
    );
    let config = json!({
        "origin": origin,
        "writer": {
            "session": URL_SAFE_NO_PAD.encode(&sessions[0].session_id),
            "csrf": URL_SAFE_NO_PAD.encode(&sessions[0].csrf_token),
        },
        "viewer": {
            "session": URL_SAFE_NO_PAD.encode(&sessions[1].session_id),
            "csrf": URL_SAFE_NO_PAD.encode(&sessions[1].csrf_token),
        },
    });
    let bot_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定浏览器 Bot API 桩失败");
    let bot_address = bot_listener.local_addr().expect("读取 Bot 桩地址失败");
    let bot_router = axum::Router::new().route(
        &format!("/bot{TEST_BOT_TOKEN}/getMe"),
        post(|| async { Json(json!({"ok":true,"result":{"id":123456,"is_bot":true,"first_name":"浏览器接入 Bot","username":"browser_local_bot"}})) }),
    )
    .route(&format!("/bot{TEST_BOT_TOKEN}/getChat"), post(|Json(payload): Json<serde_json::Value>| async move {
        let id = payload["chat_id"].as_i64().expect("目标 chat id");
        if id == -10088013 {
            Json(json!({"ok":false,"error_code":403,"description":"browser-secret-error-sentinel"}))
        } else {
            Json(json!({"ok":true,"result":{"id":id,"type":"channel","has_protected_content":true}}))
        }
    }))
    .route(&format!("/bot{TEST_BOT_TOKEN}/getChatMember"), post(|Json(payload): Json<serde_json::Value>| async move {
        assert_eq!(payload["user_id"], 123456);
        Json(json!({"ok":true,"result":{"user":{"id":123456,"is_bot":true},"status":"administrator","can_post_messages":payload["chat_id"] != -10088012}}))
    }));
    let bot_server = tokio::spawn(async move { axum::serve(bot_listener, bot_router).await });
    let kek = EnvKek::new(
        Profile::Development,
        "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
    )
    .expect("构造浏览器测试 KEK 失败");
    let kek = Arc::new(kek);
    let probe_pool = app.clone();
    let probe_kek = kek.clone();
    let probe_url = format!("http://{bot_address}");
    let (probe_stop, mut probe_shutdown) = tokio::sync::watch::channel(false);
    let probe_worker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = probe_shutdown.changed() => break,
                _ = interval.tick() => {}
            }
            let claim = tgm_db::target_probe::claim_probe(
                &probe_pool,
                tenant,
                "browser-fixture".to_owned(),
            )
            .await
            .expect("浏览器探测认领");
            if let Some(claim) = claim {
                let outcome = tgm_app::target_probe::execute(
                    &probe_pool,
                    probe_kek.as_ref(),
                    tenant,
                    &claim,
                    &probe_url,
                )
                .await;
                tgm_db::target_probe::finish_probe(&probe_pool, tenant, claim, outcome)
                    .await
                    .expect("浏览器探测回写");
            }
        }
    });
    let state = ControlPlaneState::with_auth_pool_and_bot(
        app.clone(),
        auth,
        tgm_app::bot_console::BotConsoleConfig {
            secret_token: "local-browser-secret".to_owned(),
            bot_identity_id: identity,
            bot_username: "browser_local_bot".to_owned(),
            bot_api: None,
        },
    )
    .with_credential_intake(CredentialIntake {
        kek: kek.clone(),
        bot_api_base_url: format!("http://{bot_address}"),
    });
    let polls = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).layer(middleware::from_fn(
                move |request: Request<Body>, next: Next| {
                    let polls = polls.clone();
                    async move { qr_http_stub(request, next, &polls).await }
                },
            )),
        )
        .await
    });
    let result = tokio::task::spawn_blocking(move || {
        let mut command = Command::new("node");
        command
            .arg(format!(
                "{}/tests/console_browser.js",
                env!("CARGO_MANIFEST_DIR")
            ))
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "PATH",
            "HOME",
            "TMPDIR",
            "NODE_PATH",
            "TGM_BROWSER_EXECUTABLE",
            "TGM_BROWSER_ALLOW_NO_SANDBOX",
            "TGM_BROWSER_ARTIFACT_DIR",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().expect("启动真实浏览器测试失败");
        child
            .stdin
            .take()
            .expect("浏览器测试 stdin 缺失")
            .write_all(config.to_string().as_bytes())
            .expect("传入本地测试配置失败");
        child.wait_with_output().expect("读取浏览器测试结果失败")
    })
    .await;
    server.abort();
    let _ = probe_stop.send(true);
    probe_worker.await.expect("结束浏览器自检 worker");
    let _ = server.await;
    bot_server.abort();
    let _ = bot_server.await;
    let output = result.expect("浏览器测试进程任务失败");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "真实浏览器验收失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM web_sessions WHERE user_id = 88001")
            .fetch_one(admin.raw())
            .await
            .expect("读取会话撤销结果失败");
    assert_eq!(remaining, 0, "远程撤销和退出登录应删除管理员的测试会话");
    let stored: (Vec<u8>, String) = sqlx::query_as("SELECT secret_ciphertext, kek_id FROM identity_secrets WHERE identity_id IN (SELECT id FROM identities WHERE tenant_id = $1 AND tg_user_id = 123456)")
        .bind(tenant)
        .fetch_one(admin.raw())
        .await
        .expect("浏览器接入 Bot 应已加密落库");
    assert!(
        !stored
            .0
            .windows(TEST_BOT_TOKEN.len())
            .any(|window| window == TEST_BOT_TOKEN.as_bytes())
    );
    assert_eq!(stored.1, "local:env:v1");
}
