#![allow(clippy::expect_used, clippy::panic)]

mod support;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use sqlx::PgPool;
use std::io::Write;
use std::process::{Command, Stdio};
use tgm_app::control::{ControlPlaneState, router};
use tgm_db::repo::{WebSessionLoginMethod, issue_web_session_for_user};
use tokio::net::TcpListener;

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
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(ControlPlaneState::with_auth_pool(app, auth)),
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
    let _ = server.await;
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
}
