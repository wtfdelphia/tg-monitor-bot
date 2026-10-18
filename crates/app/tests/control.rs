#![allow(clippy::expect_used, clippy::panic)]

mod support;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use josekit::jwk::alg::rsa::RsaKeyPair;
use josekit::jwk::{JwkSet, KeyPair};
use josekit::jws::{JwsHeader, RS256};
use josekit::jwt::{self, JwtPayload};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::types::chrono::{TimeZone, Utc};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tgm_app::bot_console::{BotConsoleConfig, process_console_login_action};
use tgm_app::console_login::ConsoleLoginAction;
use tgm_app::control::{ControlPlaneState, OIDC_ISSUER, OidcConfig, router};
use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
use tgm_core::oidc::OidcSigningAlgorithm;
use tgm_core::secret::Secret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body)
        .expect("构造测试请求失败")
}

fn request_with_content_type(
    method: &str,
    uri: &str,
    content_type: &str,
    body: Body,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", content_type)
        .body(body)
        .expect("构造带类型测试请求失败")
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1_048_576)
        .await
        .expect("读取测试响应失败");
    serde_json::from_slice(&bytes).expect("解析测试响应 JSON 失败")
}

fn session_cookie(value: &[u8]) -> String {
    format!("tgm_session={}", URL_SAFE_NO_PAD.encode(value),)
}

fn csrf_cookie(value: &[u8]) -> String {
    format!("tgm_csrf={}", URL_SAFE_NO_PAD.encode(value))
}

fn session_management_request(
    method: &str,
    uri: &str,
    session: &[u8],
    csrf: &[u8],
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "cookie",
            format!("{}; {}", session_cookie(session), csrf_cookie(csrf)),
        )
        .header("x-csrf-token", URL_SAFE_NO_PAD.encode(csrf))
        .body(Body::empty())
        .expect("构造会话管理请求失败")
}

fn with_auth(mut request: Request<Body>, auth: AuthContext) -> Request<Body> {
    request.extensions_mut().insert(auth);
    request
}

async fn read_http_headers(socket: &mut tokio::net::TcpStream) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = socket
            .read(&mut chunk)
            .await
            .expect("读取 OIDC 测试请求失败");
        assert!(read > 0, "OIDC 测试请求意外提前结束");
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return;
        }
    }
}

fn query_parameter(location: &str, name: &str) -> String {
    location
        .split('?')
        .nth(1)
        .and_then(|query| {
            query
                .split('&')
                .find_map(|part| part.strip_prefix(&format!("{name}=")))
        })
        .expect("OIDC authorization URL 缺少查询参数")
        .to_owned()
}

fn signed_test_id_token(nonce: &str) -> (String, String) {
    let mut key_pair = RsaKeyPair::generate(2048).expect("生成 OIDC 测试 RSA 密钥失败");
    key_pair.set_algorithm(Some("RS256"));
    key_pair.set_key_id(Some("test-kid"));
    let private_jwk = key_pair.to_jwk_key_pair();
    let mut public_jwk = key_pair.to_jwk_public_key();
    public_jwk.set_key_id("test-kid");
    public_jwk.set_algorithm("RS256");
    public_jwk.set_key_use("sig");
    public_jwk.set_key_operations(vec!["verify"]);

    let mut payload = JwtPayload::new();
    payload.set_issuer(OIDC_ISSUER);
    payload.set_subject("7001");
    payload.set_audience(vec!["12345"]);
    payload.set_expires_at(
        &(SystemTime::UNIX_EPOCH
            + Duration::from_secs(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("读取 OIDC 测试时间失败")
                    .as_secs()
                    + 600,
            )),
    );
    payload
        .set_claim("nonce", Some(json!(nonce)))
        .expect("写入 OIDC 测试 nonce 失败");

    let mut header = JwsHeader::new();
    header.set_algorithm("RS256");
    header.set_key_id("test-kid");
    let signer = RS256
        .signer_from_jwk(&private_jwk)
        .expect("创建 OIDC 测试 signer 失败");
    let token =
        jwt::encode_with_signer(&payload, &header, &signer).expect("签发 OIDC 测试 id_token 失败");

    let mut jwks = JwkSet::new();
    jwks.push_key(public_jwk);
    (token, jwks.to_string())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn source_router_fails_closed_without_auth(admin: PgPool) {
    let (app, _admin) = support::pools(admin).await;
    let response = router(ControlPlaneState::new(app))
        .oneshot(request("GET", "/api/v1/sources", Body::empty()))
        .await
        .expect("执行未认证请求失败");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "UNAUTHENTICATED");
    assert!(body.get("tenant_id").is_none());
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn auth_me_returns_current_web_identity(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "auth-me", 7002).await;
    const USER_ID: i64 = 7002;
    const SESSION_ID: &[u8] = b"auth-me-session-id-32-bytes-pad!";
    const CSRF_TOKEN: &[u8] = b"auth-me-session-csrf-32-bytes-pad";

    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role)
         VALUES ($1, $2, 'TENANT_ADMIN')",
    )
    .bind(tenant)
    .bind(USER_ID)
    .execute(admin.raw())
    .await
    .expect("创建 auth/me 测试成员失败");
    sqlx::query(
        "INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '30 days')",
    )
    .bind(tenant)
    .bind(SESSION_ID)
    .bind(USER_ID)
    .bind(CSRF_TOKEN)
    .execute(admin.raw())
    .await
    .expect("创建 auth/me 测试会话失败");

    let response = router(ControlPlaneState::with_auth_pool(
        app.clone(),
        auth_pool.clone(),
    ))
    .oneshot(
        Request::builder()
            .method("GET")
            .uri("/auth/me")
            .header("cookie", session_cookie(SESSION_ID))
            .body(Body::empty())
            .expect("构造 auth/me 请求失败"),
    )
    .await
    .expect("执行 auth/me 请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({
            "user_id": USER_ID,
            "tenant_id": tenant.get(),
            "role": "TENANT_ADMIN",
        })
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn auth_me_fails_closed_without_authentication(admin: PgPool) {
    let (app, _admin) = support::pools(admin).await;
    let response = router(ControlPlaneState::new(app))
        .oneshot(request("GET", "/auth/me", Body::empty()))
        .await
        .expect("执行未认证 auth/me 请求失败");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(
        response_json(response).await["error"]["code"],
        "UNAUTHENTICATED"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn auth_logout_revokes_current_session_and_clears_cookies(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "auth-logout", 7003).await;
    const USER_ID: i64 = 7003;
    const SESSION_ID: &[u8] = b"auth-logout-session-id-32-bytes!";
    const CSRF_TOKEN: &[u8] = b"auth-logout-session-csrf-32-byte";

    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role)
         VALUES ($1, $2, 'TENANT_ADMIN')",
    )
    .bind(tenant)
    .bind(USER_ID)
    .execute(admin.raw())
    .await
    .expect("创建 logout 测试成员失败");
    sqlx::query(
        "INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '30 days')",
    )
    .bind(tenant)
    .bind(SESSION_ID)
    .bind(USER_ID)
    .bind(CSRF_TOKEN)
    .execute(admin.raw())
    .await
    .expect("创建 logout 测试会话失败");

    let csrf_value = URL_SAFE_NO_PAD.encode(CSRF_TOKEN);
    let response = router(ControlPlaneState::with_auth_pool(
        app.clone(),
        auth_pool.clone(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/auth/logout")
            .header(
                "cookie",
                format!(
                    "{}; {}",
                    session_cookie(SESSION_ID),
                    csrf_cookie(CSRF_TOKEN)
                ),
            )
            .header("x-csrf-token", csrf_value)
            .body(Body::empty())
            .expect("构造 logout 请求失败"),
    )
    .await
    .expect("执行 logout 请求失败");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cookies = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();
    assert!(cookies.iter().any(|value| {
        value.starts_with("tgm_session=")
            && value.contains("Path=/")
            && value.contains("Max-Age=0")
            && value.contains("HttpOnly")
            && value.contains("Secure")
            && value.contains("SameSite=Lax")
    }));
    assert!(cookies.iter().any(|value| {
        value.starts_with("tgm_csrf=")
            && value.contains("Path=/")
            && value.contains("Max-Age=0")
            && value.contains("Secure")
            && value.contains("SameSite=Lax")
    }));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions WHERE id = $1")
        .bind(SESSION_ID)
        .fetch_one(admin.raw())
        .await
        .expect("查询 logout 后会话失败");
    assert_eq!(count, 0);

    let replay = router(ControlPlaneState::with_auth_pool(app, auth_pool))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/me")
                .header(
                    "cookie",
                    format!(
                        "{}; {}",
                        session_cookie(SESSION_ID),
                        csrf_cookie(CSRF_TOKEN)
                    ),
                )
                .body(Body::empty())
                .expect("构造 logout 后回放请求失败"),
        )
        .await
        .expect("执行 logout 后回放请求失败");
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn auth_logout_rejects_missing_or_invalid_csrf_without_deleting_session(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "auth-logout-csrf", 7004).await;
    const USER_ID: i64 = 7004;
    const SESSION_ID: &[u8] = b"auth-logout-csrf-session-id-32!!";
    const CSRF_TOKEN: &[u8] = b"auth-logout-csrf-session-token-3";

    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role)
         VALUES ($1, $2, 'TENANT_ADMIN')",
    )
    .bind(tenant)
    .bind(USER_ID)
    .execute(admin.raw())
    .await
    .expect("创建 CSRF 测试成员失败");
    sqlx::query(
        "INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '30 days')",
    )
    .bind(tenant)
    .bind(SESSION_ID)
    .bind(USER_ID)
    .bind(CSRF_TOKEN)
    .execute(admin.raw())
    .await
    .expect("创建 CSRF 测试会话失败");

    let missing = router(ControlPlaneState::with_auth_pool(
        app.clone(),
        auth_pool.clone(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/auth/logout")
            .header("cookie", session_cookie(SESSION_ID))
            .body(Body::empty())
            .expect("构造缺失 CSRF logout 请求失败"),
    )
    .await
    .expect("执行缺失 CSRF logout 请求失败");
    assert_eq!(missing.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(missing).await["error"]["code"],
        "CSRF_INVALID"
    );

    let invalid = router(ControlPlaneState::with_auth_pool(app, auth_pool))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/logout")
                .header(
                    "cookie",
                    format!(
                        "{}; {}",
                        session_cookie(SESSION_ID),
                        csrf_cookie(CSRF_TOKEN)
                    ),
                )
                .header("x-csrf-token", URL_SAFE_NO_PAD.encode([1_u8; 32]))
                .body(Body::empty())
                .expect("构造错误 CSRF logout 请求失败"),
        )
        .await
        .expect("执行错误 CSRF logout 请求失败");
    assert_eq!(invalid.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(invalid).await["error"]["code"],
        "CSRF_INVALID"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions WHERE id = $1")
        .bind(SESSION_ID)
        .fetch_one(admin.raw())
        .await
        .expect("查询 CSRF 失败后的会话失败");
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn auth_sessions_list_and_revoke_only_owned_sessions(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant_a = support::seed_tenant(&admin, "session-management-a", 7015).await;
    let tenant_b = support::seed_tenant(&admin, "session-management-b", 7016).await;
    const USER_ID: i64 = 7015;
    const OTHER_USER_ID: i64 = 7016;
    let current = [51_u8; 32];
    let remote = [52_u8; 32];
    let own_b = [53_u8; 32];
    let other_a = [54_u8; 32];
    let other_b = [55_u8; 32];
    let expired = [56_u8; 32];
    let csrf = [57_u8; 32];
    for tenant in [tenant_a, tenant_b] {
        sqlx::query("INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'VIEWER'), ($1, $3, 'TENANT_ADMIN')")
            .bind(tenant)
            .bind(USER_ID)
            .bind(OTHER_USER_ID)
            .execute(admin.raw())
            .await
            .expect("创建会话管理成员失败");
    }
    for (tenant, user_id, session, method, is_expired) in [
        (tenant_a, USER_ID, current, "unknown", false),
        (tenant_a, USER_ID, remote, "bot_qr", false),
        (tenant_b, USER_ID, own_b, "oidc", false),
        (tenant_a, OTHER_USER_ID, other_a, "oidc", false),
        (tenant_b, OTHER_USER_ID, other_b, "bot_qr", false),
        (tenant_a, USER_ID, expired, "unknown", true),
    ] {
        sqlx::query(
            "INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, ip, user_agent, login_method, expires_at)
             VALUES ($1, $2, $3, $4, '203.0.113.15', 'session-browser', $5,
                     CASE WHEN $6 THEN now() - interval '1 day' ELSE now() + interval '30 days' END)",
        )
        .bind(tenant)
        .bind(session.as_slice())
        .bind(user_id)
        .bind(csrf.as_slice())
        .bind(method)
        .bind(is_expired)
        .execute(admin.raw())
        .await
        .expect("创建会话管理数据失败");
    }
    let control = router(ControlPlaneState::with_auth_pool(app, auth_pool));
    let remote_reference = URL_SAFE_NO_PAD.encode(tgm_db::repo::web_session_reference(&remote));
    let current_reference = URL_SAFE_NO_PAD.encode(tgm_db::repo::web_session_reference(&current));
    let own_b_reference = URL_SAFE_NO_PAD.encode(tgm_db::repo::web_session_reference(&own_b));
    for (method, path) in [
        ("GET", "/auth/sessions".to_owned()),
        ("DELETE", format!("/auth/sessions/{remote_reference}")),
    ] {
        let response = control
            .clone()
            .oneshot(request(method, &path, Body::empty()))
            .await
            .expect("执行未认证会话请求失败");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = control
        .clone()
        .oneshot(session_management_request(
            "GET",
            "/auth/sessions?user_id=7016&tenant_id=999",
            &current,
            &csrf,
        ))
        .await
        .expect("执行会话列表请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let body = response_json(response).await;
    let items = body["items"].as_array().expect("会话列表没有 items");
    assert_eq!(items.len(), 3);
    for (reference, method, is_current) in [
        (&current_reference, "unknown", true),
        (&remote_reference, "bot_qr", false),
        (&own_b_reference, "oidc", false),
    ] {
        let item = items
            .iter()
            .find(|item| item["id"].as_str() == Some(reference.as_str()))
            .expect("本人活跃会话缺失");
        assert_eq!(item["login_method"], method);
        assert_eq!(item["is_current"], is_current);
        assert_eq!(item["ip"], "203.0.113.15");
        assert_eq!(item["user_agent"], "session-browser");
        assert!(item["created_at"].is_string());
        assert!(item["expires_at"].is_string());
        assert!(item.get("tenant_id").is_none());
        assert!(item.get("csrf_token").is_none());
    }
    for session in [current, remote, own_b, other_a, other_b, expired, csrf] {
        assert!(
            !body.to_string().contains(&URL_SAFE_NO_PAD.encode(session)),
            "会话列表泄露 bearer 或 CSRF 材料"
        );
    }
    let reference_cookie = URL_SAFE_NO_PAD
        .decode(&remote_reference)
        .expect("解码管理引用失败");
    let response = control
        .clone()
        .oneshot(session_management_request(
            "GET",
            "/auth/me",
            &reference_cookie,
            &csrf,
        ))
        .await
        .expect("回放会话引用失败");
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "管理引用竟能用作登录 Cookie"
    );

    let remote_path = format!("/auth/sessions/{remote_reference}");
    for mode in ["missing", "header", "cookie"] {
        let mut denied = session_management_request("DELETE", &remote_path, &current, &csrf);
        match mode {
            "missing" => {
                denied.headers_mut().remove("x-csrf-token");
            }
            "header" => {
                denied.headers_mut().insert(
                    "x-csrf-token",
                    URL_SAFE_NO_PAD
                        .encode([58_u8; 32])
                        .parse()
                        .expect("构造错误 CSRF header 失败"),
                );
            }
            _ => {
                denied.headers_mut().insert(
                    "cookie",
                    format!(
                        "{}; {}",
                        session_cookie(&current),
                        csrf_cookie(&[58_u8; 32])
                    )
                    .parse()
                    .expect("构造错误 CSRF Cookie 失败"),
                );
            }
        }
        let response = control
            .clone()
            .oneshot(denied)
            .await
            .expect("执行非法 CSRF 撤销失败");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "CSRF_INVALID"
        );
    }
    for reference in [
        "bad".to_owned(),
        URL_SAFE_NO_PAD.encode([59_u8; 31]),
        format!("{remote_reference}="),
    ] {
        let response = control
            .clone()
            .oneshot(session_management_request(
                "DELETE",
                &format!("/auth/sessions/{reference}"),
                &current,
                &csrf,
            ))
            .await
            .expect("执行非法引用撤销失败");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for reference in [
        URL_SAFE_NO_PAD.encode(tgm_db::repo::web_session_reference(&other_a)),
        URL_SAFE_NO_PAD.encode(tgm_db::repo::web_session_reference(&other_b)),
        URL_SAFE_NO_PAD.encode([60_u8; 32]),
        URL_SAFE_NO_PAD.encode(remote),
    ] {
        let response = control
            .clone()
            .oneshot(session_management_request(
                "DELETE",
                &format!("/auth/sessions/{reference}"),
                &current,
                &csrf,
            ))
            .await
            .expect("执行无权引用撤销失败");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 0);
        assert!(
            to_bytes(response.into_body(), 1024)
                .await
                .expect("读取统一撤销响应失败")
                .is_empty()
        );
    }
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions")
        .fetch_one(admin.raw())
        .await
        .expect("读取拒绝撤销后的会话失败");
    assert_eq!(remaining, 6);

    for reference in [&remote_reference, &remote_reference, &own_b_reference] {
        let response = control
            .clone()
            .oneshot(session_management_request(
                "DELETE",
                &format!("/auth/sessions/{reference}"),
                &current,
                &csrf,
            ))
            .await
            .expect("执行本人会话撤销失败");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 0);
    }
    for session in [remote, own_b] {
        let replay = control
            .clone()
            .oneshot(session_management_request(
                "GET", "/auth/me", &session, &csrf,
            ))
            .await
            .expect("回放已撤销 Cookie 失败");
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    }
    let response = control
        .clone()
        .oneshot(session_management_request(
            "DELETE",
            &format!("/auth/sessions/{current_reference}"),
            &current,
            &csrf,
        ))
        .await
        .expect("撤销当前会话失败");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
    let replay = control
        .oneshot(session_management_request(
            "GET", "/auth/me", &current, &csrf,
        ))
        .await
        .expect("回放当前已撤销 Cookie 失败");
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions WHERE user_id = $1")
        .bind(OTHER_USER_ID)
        .fetch_one(admin.raw())
        .await
        .expect("读取他人保留会话失败");
    assert_eq!(remaining, 2);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn console_pages_keep_login_public_and_protect_tenant_views(admin: PgPool) {
    let (app, _admin) = support::pools(admin).await;
    let login = router(ControlPlaneState::new(app.clone()))
        .oneshot(request("GET", "/login", Body::empty()))
        .await
        .expect("执行登录页请求失败");
    assert_eq!(login.status(), StatusCode::OK);
    assert_eq!(
        login
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.starts_with("text/html")),
        Some(true)
    );
    let login_body = to_bytes(login.into_body(), 1_048_576)
        .await
        .expect("读取登录页响应失败");
    let login_html = String::from_utf8(login_body.to_vec()).expect("登录页响应不是 UTF-8");
    assert!(login_html.contains("这是控制台登录，不是 Telegram 账号接入"));
    assert!(login_html.contains("仅在一致时确认登录"));
    assert!(login_html.contains("尚未被邀请加入任何工作区"));
    assert!(login_html.contains("role=\"status\" aria-live=\"polite\""));
    assert!(!login_html.contains("setInterval"));
    assert!(!login_html.contains("tenant_id"));

    let protected = router(ControlPlaneState::new(app))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/console/rules")
                .body(Body::empty())
                .expect("构造控制台页面请求失败"),
        )
        .await
        .expect("执行控制台页面请求失败");
    assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(protected).await["error"]["code"],
        "UNAUTHENTICATED"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn console_identity_page_exposes_safe_actions_without_tenant_parameters(admin: PgPool) {
    let (app, _admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&_admin, "console-page", 7010).await;
    let auth = AuthContext::new(tenant, 7010, TenantRole::Owner, AuthSource::Web);
    let response = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/console/identities", Body::empty()),
            auth,
        ))
        .await
        .expect("执行身份页请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1_048_576)
        .await
        .expect("读取身份页响应失败");
    let html = String::from_utf8(body.to_vec()).expect("身份页响应不是 UTF-8");
    assert!(html.contains("状态复位"));
    assert!(html.contains("目标频道"));
    assert!(!html.contains("tenant_id"));
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn console_pages_expose_complete_frontend_controls(admin: PgPool) {
    let (app, _admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&_admin, "console-controls", 7011).await;
    let auth = AuthContext::new(tenant, 7011, TenantRole::Owner, AuthSource::Web);

    let rules = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/console/rules", Body::empty()),
            auth,
        ))
        .await
        .expect("执行规则页请求失败");
    assert_eq!(rules.status(), StatusCode::OK);
    let rules_html = String::from_utf8(
        to_bytes(rules.into_body(), 1_048_576)
            .await
            .expect("读取规则页响应失败")
            .to_vec(),
    )
    .expect("规则页响应不是 UTF-8");
    assert!(
        rules_html.contains("async function rules(page=1,append=false,filters=ruleFilterState)")
    );
    assert!(rules_html.contains("rules-load-more"));
    assert!(rules_html.contains("api('/api/v1/rules?'+params.toString())"));
    assert!(rules_html.contains("ruleListLoading=true"));
    assert!(rules_html.contains("rules-filter-form"));
    assert!(rules_html.contains("rules-filter-clear"));
    assert!(rules_html.contains("['enabled','target_ref','source_ref']"));
    assert!(rules_html.contains("rules-bulk-form"));
    assert!(rules_html.contains("data-rule-select"));
    assert!(rules_html.contains("bindRuleBulk()"));
    assert!(rules_html.contains("currentRole='';"));
    assert!(
        rules_html.contains("canWrite=currentRole==='TENANT_OWNER'||currentRole==='TENANT_ADMIN'")
    );
    assert!(rules_html.contains("if(!canWrite)throw new Error('当前角色没有写入权限')"));
    assert!(rules_html.contains("async function runWrite(control,operation)"));
    assert!(rules_html.contains("if(control.dataset.pending)return"));
    assert!(rules_html.contains("delete control.dataset.pending"));
    assert!(rules_html.contains("bindRuleHistory(id,current.version)"));
    assert!(rules_html.contains("id=\"rule-delete\""));
    assert!(rules_html.contains("id=\"rule-delete-message\""));
    assert!(rules_html.contains("runWrite(ruleForm,async()=>"));
    assert!(rules_html.contains("data-rule-version"));
    assert!(rules_html.contains(
        "snapshot.textContent=JSON.stringify(ruleVersionSnapshot(detail.snapshot),null,2)"
    ));
    assert!(rules_html.contains("当前角色没有规则编辑权限"));
    assert!(rules_html.contains("enforceReadOnly()"));
    assert!(rules_html.contains("id=\"sessions-open\""));
    assert!(rules_html.contains("id=\"logout\""));
    assert!(rules_html.contains("id=\"sessions-panel\""));
    assert!(rules_html.contains("api('/auth/sessions')"));
    assert!(rules_html.contains("sessionWriteApi('/auth/logout','POST')"));
    assert!(rules_html.contains("sessionWriteApi('/auth/sessions/'+encodeURIComponent(button.dataset.sessionRevoke),'DELETE')"));
    assert!(rules_html.contains("if(!authenticated)return;bindSessionControls()"));
    assert!(!rules_html.contains("href=\"/login\">重新登录"));

    let identities = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/console/identities", Body::empty()),
            auth,
        ))
        .await
        .expect("执行身份页请求失败");
    assert_eq!(identities.status(), StatusCode::OK);
    let identities_html = String::from_utf8(
        to_bytes(identities.into_body(), 1_048_576)
            .await
            .expect("读取身份页响应失败")
            .to_vec(),
    )
    .expect("身份页响应不是 UTF-8");
    assert!(identities_html.contains("name=\"clear_proxy\""));
    assert!(identities_html.contains("body.proxy_url=null"));

    let keywords = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/console/keywords", Body::empty()),
            auth,
        ))
        .await
        .expect("执行关键词页请求失败");
    assert_eq!(keywords.status(), StatusCode::OK);
    let keywords_html = String::from_utf8(
        to_bytes(keywords.into_body(), 1_048_576)
            .await
            .expect("读取关键词页响应失败")
            .to_vec(),
    )
    .expect("关键词页响应不是 UTF-8");
    assert!(keywords_html.contains("<input name=\"category\" placeholder=\"分类\">"));
    assert!(keywords_html.contains("keyword-filter-form"));
    assert!(keywords_html.contains("keywordFilterState={q:String(form.get('q')||'').trim()"));
    assert!(keywords_html.contains("match_type:String(form.get('match_type')||'')"));
    assert!(keywords_html.contains("category:String(form.get('category')||'').trim()"));
    assert!(keywords_html.contains("category:form.get('category')||null"));
    assert!(keywords_html.contains("async function apiAllPages(url)"));
    assert!(keywords_html.contains("apiAllPages('/api/v1/keywords')"));
    assert!(keywords_html.contains("const [keywordsBody,targetsBody]=await Promise.all([apiAllPages('/api/v1/keywords'),apiAllPages('/api/v1/targets')])"));
    assert!(keywords_html.contains("MAX_LIST_PAGES=100"));
    assert!(keywords_html.contains("分页响应没有进展"));
    assert!(keywords_html.contains("控制台暂时不可用"));
    assert!(keywords_html.contains("if(!authenticated)return"));
    assert!(
        keywords_html
            .contains("#keyword-form','#keyword-test','#keyword-import-label','#target-form")
    );
    assert!(keywords_html.contains("#keyword-import-label"));
    assert!(keywords_html.contains("writeRawApi('/api/v1/keywords/import',contentType,text)"));
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn oidc_callback_issues_session_from_verified_member(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "oidc-callback", 7001).await;
    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role)
         VALUES ($1, 7001, 'TENANT_OWNER')",
    )
    .bind(tenant)
    .execute(admin.raw())
    .await
    .expect("创建 OIDC 测试成员失败");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("启动 OIDC 本地测试服务失败");
    let address = listener.local_addr().expect("读取 OIDC 本地测试地址失败");
    let state = ControlPlaneState::with_auth_pool_and_oidc(
        app,
        auth_pool,
        OidcConfig {
            bot_id: "12345".to_owned(),
            client_secret: Arc::new(Secret::new("test-client-secret".to_owned())),
            signing_algorithm: OidcSigningAlgorithm::Rs256,
            redirect_url: "https://console.example/auth/oidc/callback".to_owned(),
            authorization_endpoint: "http://127.0.0.1:9/auth".to_owned(),
            token_endpoint: format!("http://{address}/token"),
            issuer: OIDC_ISSUER.to_owned(),
            jwks_endpoint: format!("http://{address}/jwks"),
        },
    );
    let app = router(state.clone());
    let start = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/oidc/start")
                .body(Body::empty())
                .expect("构造 OIDC 成功起始请求失败"),
        )
        .await
        .expect("执行 OIDC 成功起始请求失败");
    assert_eq!(start.status(), StatusCode::FOUND);
    let location = start
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("OIDC 成功起始响应缺少 Location")
        .to_owned();
    let state_value = query_parameter(&location, "state");
    let state_cookie = start
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("OIDC 成功起始响应缺少 state cookie")
        .split(';')
        .next()
        .expect("OIDC state cookie 为空")
        .to_owned();
    let nonce = query_parameter(&location, "nonce");
    let (id_token, jwks) = signed_test_id_token(&nonce);
    let server = tokio::spawn(async move {
        let (mut token_socket, _) = listener
            .accept()
            .await
            .expect("接受 OIDC token 测试请求失败");
        read_http_headers(&mut token_socket).await;
        let token_body = format!(r#"{{"id_token":{}}}"#, json!(id_token));
        let token_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            token_body.len(),
            token_body
        );
        token_socket
            .write_all(token_response.as_bytes())
            .await
            .expect("写入 OIDC token 测试响应失败");

        let (mut jwks_socket, _) = listener
            .accept()
            .await
            .expect("接受 OIDC JWKS 测试请求失败");
        read_http_headers(&mut jwks_socket).await;
        let jwks_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            jwks.len(),
            jwks
        );
        jwks_socket
            .write_all(jwks_response.as_bytes())
            .await
            .expect("写入 OIDC JWKS 测试响应失败");
    });

    let callback = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/auth/oidc/callback?code=local-code&state={state_value}"
                ))
                .header("cookie", &state_cookie)
                .header("x-forwarded-for", "203.0.113.9")
                .header("user-agent", "oidc-test-browser")
                .body(Body::empty())
                .expect("构造 OIDC 成功回调请求失败"),
        )
        .await
        .expect("执行 OIDC 成功回调请求失败");
    server.await.expect("OIDC 本地测试服务任务失败");

    assert_eq!(callback.status(), StatusCode::FOUND);
    assert_eq!(
        callback
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/")
    );
    let cookies: Vec<String> = callback
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| {
            value
                .to_str()
                .expect("OIDC Set-Cookie 不是合法文本")
                .to_owned()
        })
        .collect();
    assert_eq!(cookies.len(), 3);
    assert!(
        cookies
            .iter()
            .any(|value| value.contains("tgm_oidc_state=") && value.contains("Max-Age=0"))
    );
    assert!(
        cookies
            .iter()
            .any(|value| value.starts_with("tgm_session=") && value.contains("HttpOnly"))
    );
    assert!(
        cookies
            .iter()
            .any(|value| value.starts_with("tgm_csrf=") && !value.contains("HttpOnly"))
    );

    let session_id = cookies
        .iter()
        .find_map(|value| value.strip_prefix("tgm_session="))
        .and_then(|value| value.split(';').next())
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .expect("OIDC session cookie 缺失");
    let row: Option<(i64, i64, String, Option<String>, String)> = sqlx::query_as(
        "SELECT tenant_id, user_id, ip::text, user_agent, login_method
           FROM web_sessions
          WHERE id = $1",
    )
    .bind(session_id)
    .fetch_one(admin.raw())
    .await
    .map(Some)
    .expect("读取 OIDC 签发会话失败");
    assert_eq!(
        row,
        Some((
            tenant.get(),
            7001,
            "203.0.113.9/32".to_owned(),
            Some("oidc-test-browser".to_owned()),
            "oidc".to_owned(),
        ))
    );

    let replay = router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/auth/oidc/callback?code=local-code&state={state_value}"
                ))
                .header("cookie", &state_cookie)
                .body(Body::empty())
                .expect("构造 OIDC 重放回调请求失败"),
        )
        .await
        .expect("执行 OIDC 重放回调请求失败");
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn qr_challenge_router_creates_and_polls_without_signing_a_session(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "qr-router", 9901).await;
    let bot_identity_id = support::seed_identity(&admin, tenant).await;
    let state = ControlPlaneState::with_auth_pool_and_bot(
        app,
        auth_pool,
        BotConsoleConfig {
            secret_token: "control-secret".to_owned(),
            bot_identity_id,
            bot_username: "control_bot".to_owned(),
            bot_api: None,
        },
    );

    let created = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/qr/challenge")
                .header("x-forwarded-for", "203.0.113.7")
                .header("user-agent", "test-browser")
                .body(Body::empty())
                .expect("构造扫码挑战请求失败"),
        )
        .await
        .expect("创建扫码挑战失败");
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        created
            .headers()
            .get("cache-control")
            .expect("创建扫码挑战响应缺少 cache-control")
            .to_str()
            .expect("创建扫码挑战响应的 cache-control 不是合法文本"),
        "no-store"
    );
    let created_body = response_json(created).await;
    let nonce = created_body["nonce"]
        .as_str()
        .expect("扫码挑战 nonce 缺失")
        .to_owned();
    assert!(
        created_body["deep_link"]
            .as_str()
            .unwrap_or_default()
            .starts_with("https://t.me/control_bot?start=login_")
    );
    assert_eq!(
        created_body["confirm_code"]
            .as_str()
            .unwrap_or_default()
            .len(),
        4
    );

    let polled = router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/auth/qr/challenge/{nonce}"))
                .body(Body::empty())
                .expect("构造扫码挑战轮询请求失败"),
        )
        .await
        .expect("轮询扫码挑战失败");
    assert_eq!(polled.status(), StatusCode::OK);
    assert_eq!(
        polled
            .headers()
            .get("cache-control")
            .expect("轮询扫码挑战响应缺少 cache-control")
            .to_str()
            .expect("轮询扫码挑战响应的 cache-control 不是合法文本"),
        "no-store"
    );
    let polled_body = response_json(polled).await;
    assert_eq!(polled_body["state"], "pending");
    assert!(polled_body.get("tg_user_id").is_none());
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn qr_confirmed_challenge_issues_one_session_for_unique_member(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "qr-session", 9911).await;
    const USER_ID: i64 = 9911;
    let nonce = vec![41_u8; 32];
    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'TENANT_OWNER')",
    )
    .bind(tenant)
    .bind(USER_ID)
    .execute(admin.raw())
    .await
    .expect("创建扫码成员失败");
    tgm_db::repo::create_console_login_challenge(
        &auth_pool,
        &nonce,
        "A7K3",
        "203.0.113.7",
        Some("test-browser"),
        Utc::now() + Duration::from_secs(120),
    )
    .await
    .expect("创建扫码挑战失败");
    assert!(
        tgm_db::repo::claim_console_login_challenge(&auth_pool, &nonce, USER_ID)
            .await
            .expect("扫码 claim 失败")
    );
    assert!(
        process_console_login_action(
            &auth_pool,
            USER_ID,
            ConsoleLoginAction::Confirm {
                nonce: nonce.clone(),
                confirm_code: "A7K3".to_owned(),
            },
        )
        .await
        .expect("确认挑战失败")
    );

    let state = ControlPlaneState::with_auth_pool(app, auth_pool.clone());
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/auth/qr/challenge/{}",
                    URL_SAFE_NO_PAD.encode(&nonce)
                ))
                .body(Body::empty())
                .expect("构造已确认轮询请求失败"),
        )
        .await
        .expect("轮询已确认挑战失败");
    assert_eq!(response.status(), StatusCode::OK);
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().expect("Set-Cookie 不是合法文本").to_owned())
        .collect();
    assert_eq!(cookies.len(), 2);
    assert!(cookies.iter().any(|value| {
        value.starts_with("tgm_session=")
            && value.contains("Path=/")
            && value.contains("HttpOnly")
            && value.contains("Secure")
            && value.contains("SameSite=Lax")
    }));
    assert!(cookies.iter().any(|value| {
        value.starts_with("tgm_csrf=")
            && value.contains("Path=/")
            && value.contains("Secure")
            && value.contains("SameSite=Lax")
            && !value.contains("HttpOnly")
    }));
    let session_id = cookies
        .iter()
        .find_map(|value| value.strip_prefix("tgm_session="))
        .and_then(|value| value.split(';').next())
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .expect("session cookie 缺失");
    let row: Option<(i64, i64, String)> =
        sqlx::query_as("SELECT tenant_id, user_id, login_method FROM web_sessions WHERE id = $1")
            .bind(&session_id)
            .fetch_optional(admin.raw())
            .await
            .expect("读取签发会话失败");
    assert_eq!(row, Some((tenant.get(), USER_ID, "bot_qr".to_owned())));

    let second = router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/auth/qr/challenge/{}",
                    URL_SAFE_NO_PAD.encode(&nonce)
                ))
                .body(Body::empty())
                .expect("构造重复轮询请求失败"),
        )
        .await
        .expect("重复轮询已确认挑战失败");
    assert_eq!(second.status(), StatusCode::NOT_FOUND);
    assert_eq!(second.headers().get_all("set-cookie").iter().count(), 0);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn qr_confirmed_challenge_does_not_guess_a_tenant_for_multi_member(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant_a = support::seed_tenant(&admin, "qr-multi-a", 9921).await;
    let tenant_b = support::seed_tenant(&admin, "qr-multi-b", 9922).await;
    const USER_ID: i64 = 9923;
    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role)
         VALUES ($1, $3, 'TENANT_OWNER'), ($2, $3, 'TENANT_OWNER')",
    )
    .bind(tenant_a)
    .bind(tenant_b)
    .bind(USER_ID)
    .execute(admin.raw())
    .await
    .expect("创建多租户扫码成员失败");

    let nonce = vec![42_u8; 32];
    tgm_db::repo::create_console_login_challenge(
        &auth_pool,
        &nonce,
        "B8M4",
        "203.0.113.8",
        Some("multi-tenant-browser"),
        Utc::now() + Duration::from_secs(120),
    )
    .await
    .expect("创建多租户扫码挑战失败");
    assert!(
        tgm_db::repo::claim_console_login_challenge(&auth_pool, &nonce, USER_ID)
            .await
            .expect("多租户扫码 claim 失败")
    );
    assert!(
        process_console_login_action(
            &auth_pool,
            USER_ID,
            ConsoleLoginAction::Confirm {
                nonce: nonce.clone(),
                confirm_code: "B8M4".to_owned(),
            },
        )
        .await
        .expect("多租户确认挑战失败")
    );

    let response = router(ControlPlaneState::with_auth_pool(app, auth_pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/auth/qr/challenge/{}",
                    URL_SAFE_NO_PAD.encode(&nonce)
                ))
                .body(Body::empty())
                .expect("构造多租户轮询请求失败"),
        )
        .await
        .expect("轮询多租户挑战失败");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get_all("set-cookie").iter().count(), 0);
    assert_eq!(response_json(response).await["state"], "rejected");
    let session_count: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions")
        .fetch_one(admin.raw())
        .await
        .expect("统计多租户签发会话失败");
    assert_eq!(session_count, 0);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn qr_confirmed_but_expired_challenge_does_not_issue_session(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "qr-expired", 9931).await;
    const USER_ID: i64 = 9931;
    let nonce = vec![43_u8; 32];
    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'TENANT_OWNER')",
    )
    .bind(tenant)
    .bind(USER_ID)
    .execute(admin.raw())
    .await
    .expect("创建过期扫码成员失败");
    tgm_db::repo::create_console_login_challenge(
        &auth_pool,
        &nonce,
        "C9P2",
        "203.0.113.9",
        Some("expired-browser"),
        Utc::now() + Duration::from_secs(120),
    )
    .await
    .expect("创建过期扫码挑战失败");
    assert!(
        tgm_db::repo::claim_console_login_challenge(&auth_pool, &nonce, USER_ID)
            .await
            .expect("过期扫码 claim 失败")
    );
    assert!(
        process_console_login_action(
            &auth_pool,
            USER_ID,
            ConsoleLoginAction::Confirm {
                nonce: nonce.clone(),
                confirm_code: "C9P2".to_owned(),
            },
        )
        .await
        .expect("确认过期扫码挑战失败")
    );
    sqlx::query(
        "UPDATE console_login_challenges
            SET expires_at = now() - interval '1 second'
          WHERE nonce = $1",
    )
    .bind(&nonce)
    .execute(admin.raw())
    .await
    .expect("将已确认挑战置为过期失败");

    let response = router(ControlPlaneState::with_auth_pool(app, auth_pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/auth/qr/challenge/{}",
                    URL_SAFE_NO_PAD.encode(&nonce)
                ))
                .body(Body::empty())
                .expect("构造过期挑战轮询请求失败"),
        )
        .await
        .expect("轮询过期挑战失败");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get_all("set-cookie").iter().count(), 0);
    assert_eq!(response_json(response).await["state"], "expired");

    let session_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM web_sessions WHERE tenant_id = $1 AND user_id = $2",
    )
    .bind(tenant)
    .bind(USER_ID)
    .fetch_one(admin.raw())
    .await
    .expect("统计过期挑战会话失败");
    assert_eq!(session_count, 0);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn web_session_auth_expires_immediately_after_member_removal(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "session-auth", 6101).await;
    const USER_ID: i64 = 6101;
    const SESSION_ID: &[u8] = b"session-auth-id-32-bytes-paddin!";
    const CSRF_TOKEN: &[u8] = b"session-auth-csrf-32-bytes-pad!!";

    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'TENANT_OWNER')",
    )
    .bind(tenant)
    .bind(USER_ID)
    .execute(admin.raw())
    .await
    .expect("创建会话成员失败");
    sqlx::query(
        "INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '30 days')",
    )
    .bind(tenant)
    .bind(SESSION_ID)
    .bind(USER_ID)
    .bind(CSRF_TOKEN)
    .execute(admin.raw())
    .await
    .expect("创建 web 会话失败");

    let valid = router(tgm_app::control::ControlPlaneState::with_auth_pool(
        app.clone(),
        auth_pool.clone(),
    ))
    .oneshot(
        Request::builder()
            .method("GET")
            .uri("/api/v1/sources")
            .header("cookie", session_cookie(SESSION_ID))
            .body(Body::empty())
            .expect("构造有效会话请求失败"),
    )
    .await
    .expect("执行有效会话请求失败");
    assert_eq!(valid.status(), StatusCode::OK);

    let csrf_missing = router(tgm_app::control::ControlPlaneState::with_auth_pool(
        app.clone(),
        auth_pool.clone(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/v1/keywords/test")
            .header("content-type", "application/json")
            .header("cookie", session_cookie(SESSION_ID))
            .body(Body::from(r#"{"text":"probe"}"#))
            .expect("构造缺少 CSRF 请求失败"),
    )
    .await
    .expect("执行缺少 CSRF 请求失败");
    assert_eq!(csrf_missing.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(csrf_missing).await["error"]["code"],
        "CSRF_INVALID"
    );

    let csrf_value = URL_SAFE_NO_PAD.encode(CSRF_TOKEN);
    let csrf_valid = router(tgm_app::control::ControlPlaneState::with_auth_pool(
        app.clone(),
        auth_pool.clone(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/v1/keywords/test")
            .header("content-type", "application/json")
            .header(
                "cookie",
                format!(
                    "{}; {}",
                    session_cookie(SESSION_ID),
                    csrf_cookie(CSRF_TOKEN)
                ),
            )
            .header("x-csrf-token", csrf_value)
            .body(Body::from(r#"{"text":"probe"}"#))
            .expect("构造有效 CSRF 请求失败"),
    )
    .await
    .expect("执行有效 CSRF 请求失败");
    assert_eq!(csrf_valid.status(), StatusCode::OK);

    sqlx::query("DELETE FROM tenant_members WHERE tenant_id = $1 AND user_id = $2")
        .bind(tenant)
        .bind(USER_ID)
        .execute(admin.raw())
        .await
        .expect("移除会话成员失败");

    let expired = router(tgm_app::control::ControlPlaneState::with_auth_pool(
        app, auth_pool,
    ))
    .oneshot(
        Request::builder()
            .method("GET")
            .uri("/api/v1/sources")
            .header("cookie", session_cookie(SESSION_ID))
            .body(Body::empty())
            .expect("构造失效会话请求失败"),
    )
    .await
    .expect("执行失效会话请求失败");
    assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response_json(expired).await["error"]["code"],
        "UNAUTHENTICATED"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn system_log_router_filters_by_tenant_level_and_module(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "system-log-a", 5221).await;
    let tenant_b = support::seed_tenant(&admin, "system-log-b", 5222).await;
    sqlx::query(
        "WITH base AS (SELECT clock_timestamp() AS ts)
         INSERT INTO system_logs (tenant_id, level, module, event, context, created_at)
         SELECT $1, 'WARN', 'pipeline', 'delivery.retry', $2, ts - interval '2 seconds' FROM base
         UNION ALL
         SELECT $1, 'INFO', 'control', 'identity.updated', $3, ts FROM base
         UNION ALL
         SELECT $4, 'WARN', 'pipeline', 'tenant-b-secret', $5, ts FROM base
         UNION ALL
         SELECT NULL, 'ERROR', 'platform', 'platform-only', $6, ts FROM base",
    )
    .bind(tenant_a)
    .bind(json!({ "task_id": 11 }))
    .bind(json!({ "identity_id": 12 }))
    .bind(tenant_b)
    .bind(json!({ "tenant": "b" }))
    .bind(json!({ "tenant": "platform" }))
    .execute(admin.raw())
    .await
    .expect("创建系统日志测试数据失败");
    sqlx::query(
        "WITH base AS (SELECT clock_timestamp() AS ts)
         INSERT INTO system_logs (tenant_id, level, module, event, context, created_at)
         SELECT $1, 'WARN', 'pipeline', 'delivery.retry.newer', $2,
                ts + interval '1 second' FROM base
         UNION ALL
         SELECT $1, 'WARN', 'pipeline', 'delivery.retry.older', $3,
                ts - interval '1 second' FROM base",
    )
    .bind(tenant_a)
    .bind(json!({ "task_id": 13 }))
    .bind(json!({ "task_id": 12 }))
    .execute(admin.raw())
    .await
    .expect("创建系统日志分页测试数据失败");

    let auth_a = AuthContext::new(tenant_a, 5221, TenantRole::Viewer, AuthSource::Web);
    let response = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                "/api/v1/logs?level=WARN&module=pipeline&per_page=1",
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("查询系统日志失败");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(
        body["items"].as_array().expect("系统日志 items 缺失").len(),
        1
    );
    assert_eq!(body["items"][0]["event"], "delivery.retry.newer");
    assert_eq!(body["items"][0]["context"]["task_id"], 13);
    assert_eq!(body["has_more"], true);

    let second_page = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                "/api/v1/logs?level=WARN&module=pipeline&per_page=1&page=2",
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("查询系统日志第二页失败");
    assert_eq!(second_page.status(), StatusCode::OK);
    let second_body = response_json(second_page).await;
    assert_eq!(second_body["items"][0]["event"], "delivery.retry.older");
    assert_eq!(second_body["items"][0]["context"]["task_id"], 12);
    assert_eq!(second_body["has_more"], true);

    let third_page = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                "/api/v1/logs?level=WARN&module=pipeline&per_page=1&page=3",
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("查询系统日志第三页失败");
    assert_eq!(third_page.status(), StatusCode::OK);
    let third_body = response_json(third_page).await;
    assert_eq!(third_body["items"][0]["event"], "delivery.retry");
    assert_eq!(third_body["items"][0]["context"]["task_id"], 11);
    assert_eq!(third_body["has_more"], false);

    let all = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!("/api/v1/logs?tenant_id={}", tenant_b.get()),
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("查询租户系统日志失败");
    let all_body = response_json(all).await;
    let events: Vec<&str> = all_body["items"]
        .as_array()
        .expect("系统日志列表缺失")
        .iter()
        .filter_map(|item| item["event"].as_str())
        .collect();
    assert!(events.contains(&"delivery.retry"));
    assert!(events.contains(&"identity.updated"));
    assert!(!events.contains(&"tenant-b-secret"));
    assert!(!events.contains(&"platform-only"));

    let invalid = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/api/v1/logs?page=not-a-number", Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行非法日志分页请求失败");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(invalid).await["error"]["code"],
        "CONDITION_INVALID"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn keyword_router_crud_filters_tenants_and_protects_references(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "keyword-tenant-a", 5801).await;
    let tenant_b = support::seed_tenant(&admin, "keyword-tenant-b", 5802).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let _identity_b = support::seed_identity(&admin, tenant_b).await;
    let target_a: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5809, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建关键词测试目标 A 失败");
    let keyword_b: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, category, priority, enabled)
         VALUES ($1, 'tenant-b-secret', 'exact', 'private', 1, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建关键词 B 失败");
    let keyword_a: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, category, priority, enabled)
         VALUES ($1, 'alpha-alert', 'exact', 'ops', 10, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建关键词 A 失败");
    let _keyword_a_two: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, category, priority, enabled)
         VALUES ($1, 'regex-alert', 'regex', 'ops', 5, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建关键词 A 第二条失败");
    let rule_a: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 1, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(keyword_a)
    .bind(target_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建关键词引用规则失败");

    let owner_a = AuthContext::new(tenant_a, 5801, TenantRole::Owner, AuthSource::Web);
    let viewer_a = AuthContext::new(tenant_a, 5803, TenantRole::Viewer, AuthSource::Web);
    let list = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!(
                    "/api/v1/keywords?q=alert&category=ops&per_page=1&tenant_id={}",
                    tenant_b.get()
                ),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行关键词筛选请求失败");
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = response_json(list).await;
    assert_eq!(
        list_body["items"].as_array().expect("关键词列表缺失").len(),
        1
    );
    assert_eq!(list_body["items"][0]["word"], "alpha-alert");
    assert_eq!(list_body["has_more"], true);

    let hidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/keywords/{keyword_b}"),
                Body::from(json!({"priority": 9}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户关键词更新请求失败");
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    let hidden_body = response_json(hidden).await;
    assert_eq!(hidden_body["error"]["code"], "NOT_FOUND");

    let created = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/keywords",
                Body::from(
                    json!({
                        "word": "new-alert",
                        "match_type": "exact",
                        "category": "ops",
                        "priority": 20,
                        "enabled": true
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行关键词创建请求失败");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = response_json(created).await;
    let created_id = created_body["id"].as_i64().expect("创建响应缺少关键词 id");
    assert_eq!(created_body["priority"], 20);

    let conflict = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/keywords",
                Body::from(
                    json!({
                        "word": "new-alert",
                        "match_type": "exact",
                        "priority": 0,
                        "enabled": true
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行关键词重复创建请求失败");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict_body = response_json(conflict).await;
    assert_eq!(conflict_body["error"]["code"], "KEYWORD_CONFLICT");
    assert_eq!(conflict_body["error"]["details"]["existing_id"], created_id);

    let updated = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/keywords/{created_id}"),
                Body::from(json!({"word": "new-alert-updated", "enabled": false}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行关键词更新请求失败");
    assert_eq!(updated.status(), StatusCode::OK);
    let updated_body = response_json(updated).await;
    assert_eq!(updated_body["word"], "new-alert-updated");
    assert_eq!(updated_body["enabled"], false);

    let forbidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/keywords/{created_id}"),
                Body::from(json!({"priority": 30}).to_string()),
            ),
            viewer_a,
        ))
        .await
        .expect("执行关键词只读角色更新请求失败");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let referenced = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "DELETE",
                &format!("/api/v1/keywords/{keyword_a}"),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行被引用关键词删除请求失败");
    assert_eq!(referenced.status(), StatusCode::CONFLICT);
    let referenced_body = response_json(referenced).await;
    assert_eq!(referenced_body["error"]["code"], "KEYWORD_REFERENCED");
    assert_eq!(
        referenced_body["error"]["details"]["affected_rule_ids"][0],
        rule_a
    );

    let deleted = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "DELETE",
                &format!("/api/v1/keywords/{created_id}"),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行关键词删除请求失败");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn keyword_router_patch_clears_category_and_preserves_omitted_values(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "keyword-nullable-patch", 5851).await;
    let other_tenant = support::seed_tenant(&admin, "keyword-nullable-patch-other", 5852).await;
    let owner = AuthContext::new(tenant, 5851, TenantRole::Owner, AuthSource::Web);
    let created = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/keywords",
                Body::from(
                    json!({
                        "word": "nullable-category", "match_type": "exact", "category": "ops",
                        "priority": 17, "enabled": true
                    })
                    .to_string(),
                ),
            ),
            owner,
        ))
        .await
        .expect("创建分类清空测试关键词失败");
    assert_eq!(created.status(), StatusCode::CREATED);
    let original = response_json(created).await;
    let keyword_id = original["id"].as_i64().expect("响应缺少关键词 id");
    let uri = format!("/api/v1/keywords/{keyword_id}");

    for (auth, status) in [
        (
            AuthContext::new(tenant, 5853, TenantRole::Viewer, AuthSource::Web),
            StatusCode::FORBIDDEN,
        ),
        (
            AuthContext::new(tenant, 5854, TenantRole::Member, AuthSource::Web),
            StatusCode::FORBIDDEN,
        ),
        (
            AuthContext::new(other_tenant, 5852, TenantRole::Owner, AuthSource::Web),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let denied = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request(
                    "PATCH",
                    &uri,
                    Body::from(json!({"category": null}).to_string()),
                ),
                auth,
            ))
            .await
            .expect("执行受限分类清空请求失败");
        assert_eq!(denied.status(), status);
    }

    let mut final_record = original.clone();
    for (patch, expected) in [
        (json!({"priority": 17}), json!("ops")),
        (json!({"category": null}), json!(null)),
        (json!({"category": null}), json!(null)),
        (json!({}), json!(null)),
        (json!({"category": "news"}), json!("news")),
        (json!({"enabled": true}), json!("news")),
        (json!({"category": ""}), json!("")),
        (json!({"category": null}), json!(null)),
    ] {
        let patched = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("PATCH", &uri, Body::from(patch.to_string())),
                owner,
            ))
            .await
            .expect("执行关键词分类 PATCH 失败");
        assert_eq!(patched.status(), StatusCode::OK, "patch: {patch}");
        let record = response_json(patched).await;
        assert_eq!(record["category"], expected, "patch: {patch}");
        for field in [
            "id",
            "word",
            "match_type",
            "priority",
            "enabled",
            "created_at",
        ] {
            assert_eq!(
                record[field], original[field],
                "patch: {patch}, field: {field}"
            );
        }
        let listed = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("GET", "/api/v1/keywords", Body::empty()),
                owner,
            ))
            .await
            .expect("回读分类 PATCH 后的关键词失败");
        assert_eq!(listed.status(), StatusCode::OK);
        let listed = response_json(listed).await;
        assert_eq!(
            listed["items"]
                .as_array()
                .expect("响应缺少关键词列表")
                .len(),
            1
        );
        assert_eq!(listed["items"][0], record);
        final_record = record;
    }

    for invalid in [json!(42), json!(false), json!([]), json!({})] {
        let rejected = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request(
                    "PATCH",
                    &uri,
                    Body::from(json!({"category": invalid, "priority": 999}).to_string()),
                ),
                owner,
            ))
            .await
            .expect("执行无效分类 PATCH 失败");
        assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    let invalid_word = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &uri,
                Body::from(json!({"word": " ", "category": "invalid-update"}).to_string()),
            ),
            owner,
        ))
        .await
        .expect("执行无效关键词与分类更新失败");
    assert_eq!(invalid_word.status(), StatusCode::BAD_REQUEST);

    let exported = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/keywords/export", Body::empty()),
            owner,
        ))
        .await
        .expect("导出清空分类后的关键词失败");
    assert_eq!(exported.status(), StatusCode::OK);
    assert_eq!(response_json(exported).await["keywords"][0], final_record);
    let old_category = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/keywords?category=ops", Body::empty()),
            owner,
        ))
        .await
        .expect("筛选已清空的旧分类失败");
    assert_eq!(old_category.status(), StatusCode::OK);
    assert_eq!(response_json(old_category).await["items"], json!([]));
    let hidden = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/api/v1/keywords", Body::empty()),
            AuthContext::new(other_tenant, 5852, TenantRole::Owner, AuthSource::Web),
        ))
        .await
        .expect("读取其他租户关键词列表失败");
    assert_eq!(hidden.status(), StatusCode::OK);
    assert_eq!(response_json(hidden).await["items"], json!([]));
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn keyword_test_matches_enabled_keywords_with_tenant_isolation(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "keyword-test-tenant-a", 5811).await;
    let tenant_b = support::seed_tenant(&admin, "keyword-test-tenant-b", 5812).await;
    sqlx::query(
        "INSERT INTO keywords (tenant_id, word, match_type, category, priority, enabled)
         VALUES ($1, 'alpha', 'exact', 'ops', 10, TRUE),
                ($1, 'urgent\\s+price', 'regex', 'ops', 20, TRUE),
                ($1, 'disabled-secret', 'exact', 'ops', 30, FALSE),
                ($2, 'tenant-b-secret', 'exact', 'private', 99, TRUE)",
    )
    .bind(tenant_a)
    .bind(tenant_b)
    .execute(admin.raw())
    .await
    .expect("创建关键词试跑测试数据失败");

    let viewer_a = AuthContext::new(tenant_a, 5813, TenantRole::Viewer, AuthSource::Web);
    let matched = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/keywords/test",
                Body::from(json!({"text": "urgent price alpha tenant-b-secret"}).to_string()),
            ),
            viewer_a,
        ))
        .await
        .expect("执行关键词试跑请求失败");
    assert_eq!(matched.status(), StatusCode::OK);
    let matched_body = response_json(matched).await;
    assert_eq!(matched_body["text"], "urgent price alpha tenant-b-secret");
    assert_eq!(
        matched_body["matches"]
            .as_array()
            .expect("缺少命中列表")
            .len(),
        2
    );
    assert_eq!(matched_body["matches"][0]["word"], "urgent\\s+price");
    assert_eq!(matched_body["matches"][1]["word"], "alpha");

    let invalid = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/keywords/test",
                Body::from(json!({"text": "   "}).to_string()),
            ),
            viewer_a,
        ))
        .await
        .expect("执行空文本关键词试跑请求失败");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let invalid_body = response_json(invalid).await;
    assert_eq!(invalid_body["error"]["code"], "CONDITION_INVALID");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn keyword_import_export_is_atomic_and_tenant_scoped(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "keyword-import-tenant-a", 5821).await;
    let tenant_b = support::seed_tenant(&admin, "keyword-import-tenant-b", 5822).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let target_a: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5829, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建关键词导入目标失败");
    sqlx::query(
        "INSERT INTO keywords (tenant_id, word, match_type, category, priority, enabled)
         VALUES ($1, 'already-there', 'exact', 'ops', 1, TRUE),
                ($2, 'tenant-b-only', 'exact', 'private', 2, TRUE)",
    )
    .bind(tenant_a)
    .bind(tenant_b)
    .execute(admin.raw())
    .await
    .expect("创建关键词导入隔离数据失败");

    let owner_a = AuthContext::new(tenant_a, 5821, TenantRole::Owner, AuthSource::Web);
    let imported = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/keywords/import",
                Body::from(
                    json!({
                        "keywords": [
                            {
                                "word": "alpha import",
                                "match_type": "exact",
                                "category": "ops",
                                "priority": 8,
                                "enabled": true
                            },
                            {
                                "word": "urgent\\s+price",
                                "match_type": "regex",
                                "category": "pricing",
                                "priority": 9,
                                "enabled": true
                            }
                        ],
                        "create_rules_with": {
                            "target_ref": target_a,
                            "action": "COPY",
                            "priority": 4,
                            "stop_processing": false,
                            "template": null,
                            "enabled": true,
                            "conditions": [{"kind": "file_name", "value": "\\.pdf$"}],
                            "replacements": []
                        }
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行 JSON 关键词导入请求失败");
    assert_eq!(imported.status(), StatusCode::CREATED);
    let imported_body = response_json(imported).await;
    assert_eq!(
        imported_body["items"]
            .as_array()
            .expect("导入 items 缺失")
            .len(),
        2
    );
    assert_eq!(
        imported_body["rule_ids"]
            .as_array()
            .expect("导入 rule_ids 缺失")
            .len(),
        2
    );

    let rule_count: i64 = sqlx::query_scalar("SELECT count(*) FROM rules WHERE tenant_id = $1")
        .bind(tenant_a)
        .fetch_one(admin.raw())
        .await
        .expect("读取批量规则数量失败");
    assert_eq!(rule_count, 2);
    let condition_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM rule_conditions AS rc
           JOIN rules AS r ON r.id = rc.rule_id
          WHERE r.tenant_id = $1 AND rc.kind = 'file_name' AND rc.value = '\\.pdf$'",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("读取批量规则条件数量失败");
    assert_eq!(condition_count, 2);

    let exported_json = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/keywords/export?format=json", Body::empty()),
            owner_a,
        ))
        .await
        .expect("执行 JSON 关键词导出请求失败");
    assert_eq!(exported_json.status(), StatusCode::OK);
    assert_eq!(
        exported_json
            .headers()
            .get("content-disposition")
            .and_then(|value| value.to_str().ok()),
        Some("attachment; filename=keywords.json")
    );
    let exported_body = response_json(exported_json).await;
    let exported_words: Vec<_> = exported_body["keywords"]
        .as_array()
        .expect("导出 keywords 缺失")
        .iter()
        .filter_map(|item| item["word"].as_str())
        .collect();
    assert!(exported_words.contains(&"alpha import"));
    assert!(exported_words.contains(&"urgent\\s+price"));
    assert!(!exported_words.contains(&"tenant-b-only"));

    let csv_import = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request_with_content_type(
                "POST",
                "/api/v1/keywords/import",
                "text/csv",
                Body::from(
                    "word,match_type,category,priority,enabled\n\"csv,word\",exact,ops,3,false\n",
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行 CSV 关键词导入请求失败");
    assert_eq!(csv_import.status(), StatusCode::CREATED);
    let csv_import_body = response_json(csv_import).await;
    assert_eq!(csv_import_body["items"][0]["word"], "csv,word");
    assert_eq!(csv_import_body["items"][0]["enabled"], false);

    let exported_csv = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/keywords/export?format=csv", Body::empty()),
            owner_a,
        ))
        .await
        .expect("执行 CSV 关键词导出请求失败");
    assert_eq!(exported_csv.status(), StatusCode::OK);
    assert_eq!(
        exported_csv
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/csv; charset=utf-8")
    );
    let csv_body = String::from_utf8(
        to_bytes(exported_csv.into_body(), 1_048_576)
            .await
            .expect("读取 CSV 导出响应失败")
            .to_vec(),
    )
    .expect("解析 CSV 导出响应失败");
    assert!(csv_body.contains("\"csv,word\""));
    assert!(!csv_body.contains("tenant-b-only"));

    let failed_import = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/keywords/import",
                Body::from(
                    json!({
                        "keywords": [
                            {
                                "word": "rolled-back",
                                "match_type": "exact",
                                "category": null,
                                "priority": 1,
                                "enabled": true
                            },
                            {
                                "word": "already-there",
                                "match_type": "exact",
                                "category": null,
                                "priority": 1,
                                "enabled": true
                            }
                        ]
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行失败关键词导入请求失败");
    assert_eq!(failed_import.status(), StatusCode::CONFLICT);
    let rolled_back: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM keywords WHERE tenant_id = $1 AND word = 'rolled-back'",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("读取回滚关键词数量失败");
    assert_eq!(rolled_back, 0);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn target_router_crud_is_tenant_scoped_and_logically_deletes(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "target-tenant-a", 5901).await;
    let tenant_b = support::seed_tenant(&admin, "target-tenant-b", 5902).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;
    let owner_a = AuthContext::new(tenant_a, 5901, TenantRole::Owner, AuthSource::Web);
    let viewer_a = AuthContext::new(tenant_a, 5903, TenantRole::Viewer, AuthSource::Web);

    let created = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/targets",
                Body::from(
                    json!({
                        "chat": "-1005901",
                        "title": "Target A",
                        "sender_identity_id": identity_a.get()
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行目标频道创建请求失败");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = response_json(created).await;
    let target_id = created_body["id"].as_i64().expect("创建响应缺少目标 id");
    assert_eq!(created_body["tg_chat_id"], -1005901);
    assert_eq!(created_body["can_send"], Value::Null);
    assert_eq!(created_body["enabled"], true);

    let conflict = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/targets",
                Body::from(
                    json!({
                        "chat": "-1005901",
                        "title": "Duplicate",
                        "sender_identity_id": identity_a.get()
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行目标频道重复创建请求失败");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict_body = response_json(conflict).await;
    assert_eq!(conflict_body["error"]["code"], "TARGET_CONFLICT");
    assert_eq!(conflict_body["error"]["details"]["existing_id"], target_id);

    let hidden_identity = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/targets",
                Body::from(
                    json!({
                        "chat": "-1005902",
                        "title": "Cross tenant identity",
                        "sender_identity_id": identity_b.get()
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户发送身份请求失败");
    assert_eq!(hidden_identity.status(), StatusCode::NOT_FOUND);

    let listed = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/targets?per_page=1", Body::empty()),
            owner_a,
        ))
        .await
        .expect("执行目标频道列表请求失败");
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = response_json(listed).await;
    assert_eq!(
        listed_body["items"].as_array().expect("目标列表缺失").len(),
        1
    );
    assert_eq!(listed_body["items"][0]["id"], target_id);
    assert_eq!(listed_body["has_more"], false);

    let updated = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/targets/{target_id}"),
                Body::from(json!({"title": "Target A renamed", "enabled": false}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行目标频道更新请求失败");
    assert_eq!(updated.status(), StatusCode::OK);
    let updated_body = response_json(updated).await;
    assert_eq!(updated_body["title"], "Target A renamed");
    assert_eq!(updated_body["enabled"], false);

    let forbidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/targets/{target_id}"),
                Body::from(json!({"title": "viewer cannot write"}).to_string()),
            ),
            viewer_a,
        ))
        .await
        .expect("执行目标频道只读角色更新请求失败");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let deleted = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "DELETE",
                &format!("/api/v1/targets/{target_id}"),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行目标频道删除请求失败");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let enabled: bool =
        sqlx::query_scalar("SELECT enabled FROM target_channels WHERE tenant_id = $1 AND id = $2")
            .bind(tenant_a)
            .bind(target_id)
            .fetch_one(admin.raw())
            .await
            .expect("读取目标频道逻辑删除状态失败");
    assert!(!enabled);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn target_router_patch_clears_title_and_preserves_omitted_values(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "target-nullable-patch", 5951).await;
    let other_tenant = support::seed_tenant(&admin, "target-nullable-patch-other", 5952).await;
    let identity = support::seed_identity(&admin, tenant).await;
    let target_id: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels
            (tenant_id, tg_chat_id, title, sender_identity_id, can_send, checked_at, enabled)
         VALUES ($1, -1005951, 'Target A', $2, TRUE, clock_timestamp(), TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("创建名称清空测试目标失败");
    let owner = AuthContext::new(tenant, 5951, TenantRole::Owner, AuthSource::Web);
    let uri = format!("/api/v1/targets/{target_id}");
    let listed = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/targets", Body::empty()),
            owner,
        ))
        .await
        .expect("读取初始目标失败");
    assert_eq!(listed.status(), StatusCode::OK);
    let original = response_json(listed).await["items"][0].clone();
    assert_eq!(original["can_send"], true);
    assert!(original["checked_at"].is_string());

    for (auth, status) in [
        (
            AuthContext::new(tenant, 5953, TenantRole::Viewer, AuthSource::Web),
            StatusCode::FORBIDDEN,
        ),
        (
            AuthContext::new(tenant, 5954, TenantRole::Member, AuthSource::Web),
            StatusCode::FORBIDDEN,
        ),
        (
            AuthContext::new(other_tenant, 5952, TenantRole::Owner, AuthSource::Web),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let denied = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request(
                    "PATCH",
                    &uri,
                    Body::from(json!({"title": null}).to_string()),
                ),
                auth,
            ))
            .await
            .expect("执行受限目标名称清空请求失败");
        assert_eq!(denied.status(), status);
    }

    let mut final_record = original.clone();
    for (patch, expected) in [
        (json!({"enabled": true}), json!("Target A")),
        (json!({"title": null}), json!(null)),
        (json!({"title": null}), json!(null)),
        (json!({"title": "  Renamed  "}), json!("Renamed")),
        (json!({"enabled": true}), json!("Renamed")),
        (json!({"title": ""}), json!("")),
        (json!({"title": null}), json!(null)),
    ] {
        let patched = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("PATCH", &uri, Body::from(patch.to_string())),
                owner,
            ))
            .await
            .expect("执行目标名称 PATCH 失败");
        assert_eq!(patched.status(), StatusCode::OK, "patch: {patch}");
        let record = response_json(patched).await;
        assert_eq!(record["title"], expected, "patch: {patch}");
        for field in [
            "id",
            "tg_chat_id",
            "sender_identity_id",
            "can_send",
            "checked_at",
            "enabled",
        ] {
            assert_eq!(
                record[field], original[field],
                "patch: {patch}, field: {field}"
            );
        }
        let stored = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("GET", "/api/v1/targets", Body::empty()),
                owner,
            ))
            .await
            .expect("回读名称 PATCH 后的目标失败");
        assert_eq!(stored.status(), StatusCode::OK);
        let stored = response_json(stored).await;
        assert_eq!(
            stored["items"].as_array().expect("响应缺少目标列表").len(),
            1
        );
        assert_eq!(stored["items"][0], record);
        final_record = record;
    }

    for (patch, status) in [
        (json!({}), StatusCode::BAD_REQUEST),
        (
            json!({"title": 42, "enabled": false}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"title": false, "enabled": false}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"title": [], "enabled": false}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"title": {}, "enabled": false}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"title": "invalid-update", "chat": "invalid"}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let rejected = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("PATCH", &uri, Body::from(patch.to_string())),
                owner,
            ))
            .await
            .expect("执行无效名称 PATCH 失败");
        assert_eq!(rejected.status(), status, "patch: {patch}");
    }
    let unchanged = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/targets", Body::empty()),
            owner,
        ))
        .await
        .expect("读取拒绝无效 PATCH 后的目标失败");
    assert_eq!(unchanged.status(), StatusCode::OK);
    assert_eq!(response_json(unchanged).await["items"][0], final_record);

    let routing_change = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "PATCH",
                &uri,
                Body::from(json!({"chat": "-1005953", "title": null}).to_string()),
            ),
            owner,
        ))
        .await
        .expect("执行目标路由与名称更新失败");
    assert_eq!(routing_change.status(), StatusCode::OK);
    let routing_change = response_json(routing_change).await;
    assert_eq!(routing_change["tg_chat_id"], -1005953);
    assert_eq!(routing_change["title"], Value::Null);
    assert_eq!(routing_change["can_send"], Value::Null);
    assert_eq!(routing_change["checked_at"], Value::Null);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn source_router_hides_cross_tenant_rows_and_allows_owner_writes(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "control-tenant-a", 5101).await;
    let tenant_b = support::seed_tenant(&admin, "control-tenant-b", 5102).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;
    let source_b = support::seed_source(&admin, tenant_b, identity_b, -5102, "Tenant B").await;
    let shared_identity: i64 = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, display_name, status)
         VALUES ('bot', NULL, 'shared-demo', 'online')
         RETURNING id",
    )
    .fetch_one(admin.raw())
    .await
    .expect("创建共享演示身份失败");

    let auth_a = AuthContext::new(tenant_a, 5101, TenantRole::Owner, AuthSource::Web);
    let hidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", &format!("/api/v1/sources/{source_b}"), Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行跨租户来源请求失败");
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    let hidden_body = response_json(hidden).await;
    assert_eq!(hidden_body["error"]["code"], "NOT_FOUND");

    let private_cross_tenant = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/sources",
                Body::from(
                    serde_json::json!({
                        "chat": "-5103",
                        "receiver_identity_id": identity_b.get()
                    })
                    .to_string(),
                ),
            ),
            auth_a,
        ))
        .await
        .expect("执行他租户私有身份建源请求失败");
    assert_eq!(private_cross_tenant.status(), StatusCode::NOT_FOUND);
    let private_cross_tenant_body = response_json(private_cross_tenant).await;
    assert_eq!(private_cross_tenant_body["error"]["code"], "NOT_FOUND");
    let private_cross_tenant_source_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM source_subscriptions
         WHERE tenant_id = $1 AND receiver_identity_id = $2",
    )
    .bind(tenant_a)
    .bind(identity_b)
    .fetch_one(admin.raw())
    .await
    .expect("检查跨租户私有身份来源写入失败");
    assert_eq!(private_cross_tenant_source_count, 0);

    let missing_identity = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/sources",
                Body::from(
                    json!({ "chat": "-5103", "receiver_identity_id": i64::MAX }).to_string(),
                ),
            ),
            auth_a,
        ))
        .await
        .expect("执行不存在身份建源请求失败");
    assert_eq!(missing_identity.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response_json(missing_identity).await["error"]["code"],
        private_cross_tenant_body["error"]["code"]
    );

    let shared = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/sources",
                Body::from(
                    serde_json::json!({
                        "chat": "-5104",
                        "receiver_identity_id": shared_identity
                    })
                    .to_string(),
                ),
            ),
            auth_a,
        ))
        .await
        .expect("执行共享演示身份建源请求失败");
    assert_eq!(shared.status(), StatusCode::FORBIDDEN);
    assert_eq!(response_json(shared).await["error"]["code"], "FORBIDDEN");
    let shared_source_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM source_subscriptions
         WHERE tenant_id = $1 AND receiver_identity_id = $2",
    )
    .bind(tenant_a)
    .bind(shared_identity)
    .fetch_one(admin.raw())
    .await
    .expect("检查共享身份来源写入失败");
    assert_eq!(shared_source_count, 0);

    let create = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/sources",
                Body::from(
                    serde_json::json!({
                        "chat": "-5101",
                        "receiver_identity_id": identity_a.get()
                    })
                    .to_string(),
                ),
            ),
            auth_a,
        ))
        .await
        .expect("执行来源创建请求失败");
    assert_eq!(create.status(), StatusCode::CREATED);
    let created = response_json(create).await;
    assert_eq!(created["tg_chat_id"], -5101);
    assert_eq!(created["enabled"], false);

    let source_a = created["id"].as_i64().expect("创建来源 ID 缺失");
    let identities = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/identities", Body::empty()),
            auth_a,
        ))
        .await
        .expect("查询可见身份列表失败");
    assert_eq!(identities.status(), StatusCode::OK);
    let identities_body = response_json(identities).await;
    let visible = identities_body["items"].as_array().expect("身份列表缺失");
    assert!(
        visible
            .iter()
            .any(|identity| identity["id"] == shared_identity)
    );
    assert!(
        !visible
            .iter()
            .any(|identity| identity["id"] == identity_b.get())
    );

    for (identity_id, expected_status, expected_code) in [
        (shared_identity, StatusCode::FORBIDDEN, "FORBIDDEN"),
        (identity_b.get(), StatusCode::NOT_FOUND, "NOT_FOUND"),
        (i64::MAX, StatusCode::NOT_FOUND, "NOT_FOUND"),
    ] {
        let update = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request(
                    "PATCH",
                    &format!("/api/v1/sources/{source_a}"),
                    Body::from(
                        json!({ "enabled": false, "receiver_identity_id": identity_id })
                            .to_string(),
                    ),
                ),
                auth_a,
            ))
            .await
            .expect("执行非法接收身份替换请求失败");
        assert_eq!(update.status(), expected_status);
        assert_eq!(response_json(update).await["error"]["code"], expected_code);
        let persisted: (i64, bool) = sqlx::query_as(
            "SELECT receiver_identity_id, enabled FROM source_subscriptions
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_a)
        .bind(source_a)
        .fetch_one(admin.raw())
        .await
        .expect("检查拒绝请求后的来源配置失败");
        assert_eq!(persisted, (identity_a.get(), false));
    }

    let hidden_update = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/sources/{source_b}"),
                Body::from(json!({ "receiver_identity_id": shared_identity }).to_string()),
            ),
            auth_a,
        ))
        .await
        .expect("执行跨租户来源身份替换请求失败");
    assert_eq!(hidden_update.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response_json(hidden_update).await["error"]["code"],
        "NOT_FOUND"
    );

    let second_identity_a = support::seed_identity(&admin, tenant_a).await;
    let updated = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/sources/{source_a}"),
                Body::from(json!({ "receiver_identity_id": second_identity_a.get() }).to_string()),
            ),
            auth_a,
        ))
        .await
        .expect("执行同租户接收身份替换请求失败");
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(
        response_json(updated).await["receiver_identity_id"],
        second_identity_a.get()
    );

    let viewer = AuthContext::new(tenant_a, 5103, TenantRole::Viewer, AuthSource::Web);
    let forbidden = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/sources/{}", created["id"]),
                Body::from(serde_json::json!({"enabled": false}).to_string()),
            ),
            viewer,
        ))
        .await
        .expect("执行只读角色写请求失败");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let body = response_json(forbidden).await;
    assert_eq!(body["error"]["code"], "FORBIDDEN");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn identity_router_lists_owned_and_shared_identities(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "identity-tenant-a", 5201).await;
    let tenant_b = support::seed_tenant(&admin, "identity-tenant-b", 5202).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let _identity_b = support::seed_identity(&admin, tenant_b).await;
    sqlx::query(
        "INSERT INTO identities (kind, tenant_id, display_name, status)
         VALUES ('bot', NULL, 'shared-demo', 'online')",
    )
    .execute(admin.raw())
    .await
    .expect("创建共享演示身份失败");

    let auth_a = AuthContext::new(tenant_a, 5201, TenantRole::Viewer, AuthSource::Web);
    let response = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/api/v1/identities", Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行身份列表请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["items"].as_array().expect("items 缺失").len(), 2);
    assert_eq!(body["items"][0]["id"], identity_a.get());
    assert_eq!(body["items"][0]["kind"], "bot");
    assert_eq!(body["items"][0]["status"], "offline");
    assert_eq!(body["items"][1]["display_name"], "shared-demo");
    assert_eq!(body["items"][1]["status"], "online");
    assert!(body.get("tenant_id").is_none());
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn identity_write_routes_enforce_scope_state_and_delete_guards(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "identity-write-a", 5211).await;
    let tenant_b = support::seed_tenant(&admin, "identity-write-b", 5212).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;
    sqlx::query(
        "INSERT INTO identity_secrets (identity_id, secret_ciphertext, nonce, dek_ciphertext, kek_id)
         VALUES ($1, $2, $3, $4, 'local:env:v1')",
    )
    .bind(identity_a)
    .bind(vec![1_u8, 2, 3])
    .bind(vec![4_u8, 5, 6])
    .bind(vec![7_u8, 8, 9])
    .execute(admin.raw())
    .await
    .expect("创建身份凭据失败");
    let owner_a = AuthContext::new(tenant_a, 5211, TenantRole::Owner, AuthSource::Web);
    let viewer_a = AuthContext::new(tenant_a, 5213, TenantRole::Viewer, AuthSource::Web);

    let updated = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/identities/{}", identity_a.get()),
                Body::from(
                    json!({
                        "proxy_url": "socks5://user:secret@proxy.example:1080",
                        "display_name": "采集账号"
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("更新身份失败");
    assert_eq!(updated.status(), StatusCode::OK);
    let updated_body = response_json(updated).await;
    assert_eq!(updated_body["display_name"], "采集账号");
    assert_eq!(updated_body["status"], "offline");
    let proxy_url: Option<String> =
        sqlx::query_scalar("SELECT proxy_url FROM identities WHERE id = $1")
            .bind(identity_a)
            .fetch_one(admin.raw())
            .await
            .expect("读取代理配置失败");
    assert_eq!(
        proxy_url.as_deref(),
        Some("socks5://user:secret@proxy.example:1080")
    );

    let cleared = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/identities/{}", identity_a.get()),
                Body::from(json!({ "proxy_url": null }).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("清空代理配置失败");
    assert_eq!(cleared.status(), StatusCode::OK);
    let cleared_proxy: Option<String> =
        sqlx::query_scalar("SELECT proxy_url FROM identities WHERE id = $1")
            .bind(identity_a)
            .fetch_one(admin.raw())
            .await
            .expect("读取清空后的代理配置失败");
    assert!(cleared_proxy.is_none());

    let invalid = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/identities/{}", identity_a.get()),
                Body::from(json!({ "proxy_url": "file:///etc/passwd" }).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行非法代理配置请求失败");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(invalid).await["error"]["code"],
        "CONDITION_INVALID"
    );

    let forbidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/identities/{}", identity_a.get()),
                Body::from(json!({ "display_name": "blocked" }).to_string()),
            ),
            viewer_a,
        ))
        .await
        .expect("执行无权限身份更新请求失败");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let cross_tenant = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/identities/{}", identity_b.get()),
                Body::from(json!({ "display_name": "cross-tenant" }).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户身份更新请求失败");
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response_json(cross_tenant).await["error"]["code"],
        "NOT_FOUND"
    );

    sqlx::query("UPDATE identities SET status = 'flood_wait' WHERE id = $1")
        .bind(identity_b)
        .execute(admin.raw())
        .await
        .expect("设置跨租户身份 flood_wait 状态失败");
    let cross_tenant_reset = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/identities/{}/reset-status", identity_b.get()),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户身份复位请求失败");
    assert_eq!(cross_tenant_reset.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response_json(cross_tenant_reset).await["error"]["code"],
        "NOT_FOUND"
    );
    let identity_b_status: String =
        sqlx::query_scalar("SELECT status FROM identities WHERE id = $1")
            .bind(identity_b)
            .fetch_one(admin.raw())
            .await
            .expect("读取跨租户身份状态失败");
    assert_eq!(identity_b_status, "flood_wait");

    let cross_tenant_delete = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "DELETE",
                &format!("/api/v1/identities/{}", identity_b.get()),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户身份删除请求失败");
    assert_eq!(cross_tenant_delete.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response_json(cross_tenant_delete).await["error"]["code"],
        "NOT_FOUND"
    );
    let identity_b_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identities WHERE id = $1)")
            .bind(identity_b)
            .fetch_one(admin.raw())
            .await
            .expect("检查跨租户身份是否仍存在失败");
    assert!(identity_b_exists);

    sqlx::query("UPDATE identities SET status = 'flood_wait' WHERE id = $1")
        .bind(identity_a)
        .execute(admin.raw())
        .await
        .expect("设置 flood_wait 状态失败");
    let reset = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/identities/{}/reset-status", identity_a.get()),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("复位身份状态失败");
    assert_eq!(reset.status(), StatusCode::OK);
    assert_eq!(response_json(reset).await["status"], "offline");

    let reset_offline = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/identities/{}/reset-status", identity_a.get()),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行 offline 身份复位请求失败");
    assert_eq!(reset_offline.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(reset_offline).await["error"]["code"],
        "CONDITION_INVALID"
    );

    support::seed_source(&admin, tenant_a, identity_a, -5219, "依赖来源").await;

    let delete_blocked = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "DELETE",
                &format!("/api/v1/identities/{}", identity_a.get()),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行有依赖身份删除请求失败");
    assert_eq!(delete_blocked.status(), StatusCode::CONFLICT);
    let blocked_body = response_json(delete_blocked).await;
    assert_eq!(blocked_body["error"]["code"], "IDENTITY_REFERENCED");
    assert!(!blocked_body.to_string().contains("identity_secrets"));

    sqlx::query(
        "DELETE FROM source_subscriptions WHERE tenant_id = $1 AND receiver_identity_id = $2",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .execute(admin.raw())
    .await
    .expect("清理身份依赖失败");
    let deleted = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "DELETE",
                &format!("/api/v1/identities/{}", identity_a.get()),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("删除无依赖身份失败");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let secret_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identity_secrets WHERE identity_id = $1")
            .bind(identity_a)
            .fetch_one(admin.raw())
            .await
            .expect("检查身份凭据销毁失败");
    assert_eq!(secret_count, 0);
    let delete_audit: String = sqlx::query_scalar(
        "SELECT action FROM audit_logs WHERE tenant_id = $1 AND resource_id = $2
         ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant_a)
    .bind(identity_a.get())
    .fetch_one(admin.raw())
    .await
    .expect("读取删除审计失败");
    assert_eq!(delete_audit, "identity.delete");

    let audit: (String, Value, Option<Value>) = sqlx::query_as(
        "SELECT action, before, after FROM audit_logs
         WHERE tenant_id = $1 AND resource_id = $2
           AND action = 'identity.reset_status'
         ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant_a)
    .bind(identity_a.get())
    .fetch_one(admin.raw())
    .await
    .expect("读取身份审计失败");
    assert_eq!(audit.0, "identity.reset_status");
    assert_eq!(audit.1["status"], "flood_wait");
    assert_eq!(
        audit.2.as_ref().expect("复位审计 after 缺失")["status"],
        "offline"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn dashboard_returns_tenant_scoped_status_and_today_summary(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "dashboard-tenant-a", 5301).await;
    let tenant_b = support::seed_tenant(&admin, "dashboard-tenant-b", 5302).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;
    sqlx::query("UPDATE identities SET status = 'online' WHERE id = $1")
        .bind(identity_a)
        .execute(admin.raw())
        .await
        .expect("更新租户 A 身份状态失败");
    sqlx::query("UPDATE identities SET status = 'banned' WHERE id = $1")
        .bind(identity_b)
        .execute(admin.raw())
        .await
        .expect("更新租户 B 身份状态失败");

    let source_a = support::seed_source(&admin, tenant_a, identity_a, -5301, "Dashboard A").await;
    let canonical_id: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events
            (adapter, receiver_identity_id, tg_chat_id, tg_message_id, message_date)
         VALUES ('bot', $1, -5301, 1, $2)
         RETURNING id",
    )
    .bind(identity_a)
    .bind(
        Utc.timestamp_opt(1_760_000_000, 0)
            .single()
            .expect("构造事件时间失败"),
    )
    .fetch_one(admin.raw())
    .await
    .expect("创建规范化事件失败");
    let tenant_event_id: i64 = sqlx::query_scalar(
        "INSERT INTO tenant_events
            (tenant_id, canonical_event_id, source_ref, matched, processed_at)
         VALUES ($1, $2, $3, TRUE, clock_timestamp())
         RETURNING id",
    )
    .bind(tenant_a)
    .bind(canonical_id)
    .bind(source_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户事件失败");
    let task_id: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, tenant_event_id, target_tg_chat_id, sender_identity_id, action,
             status, idem_key)
         VALUES ($1, $2, -5309, $3, 'COPY', 'PENDING', 'dashboard-task-a')
         RETURNING id",
    )
    .bind(tenant_a)
    .bind(tenant_event_id)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建投递任务失败");
    sqlx::query(
        "INSERT INTO delivery_logs (task_id, attempt, ok, latency_ms)
         VALUES ($1, 1, TRUE, 10), ($1, 2, FALSE, 20)",
    )
    .bind(task_id)
    .execute(admin.raw())
    .await
    .expect("创建投递日志失败");

    let auth_a = AuthContext::new(tenant_a, 5301, TenantRole::Viewer, AuthSource::Web);
    let response = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/api/v1/dashboard/summary", Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行 Dashboard 请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["identities"]["online"], 1);
    assert_eq!(body["identities"]["banned"], 0);
    assert_eq!(body["queue_depth"], 1);
    assert_eq!(body["today"]["received"], 1);
    assert_eq!(body["today"]["matched"], 1);
    assert_eq!(body["today"]["delivered"], 1);
    assert_eq!(body["today"]["failed"], 1);
    assert!(body["flood_wait_until"].is_null());
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn task_router_lists_details_filters_tenants_and_retries_failed_tasks(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "task-tenant-a", 5401).await;
    let tenant_b = support::seed_tenant(&admin, "task-tenant-b", 5402).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;

    let target_a: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5409, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建任务测试目标失败");
    let keyword_a: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'task-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建任务测试关键词失败");
    let rule_a: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 1, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(keyword_a)
    .bind(target_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建任务测试规则失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(tenant_a)
    .bind(rule_a)
    .bind(json!({"marker": "v1", "target_ref": target_a, "action": "COPY"}))
    .execute(admin.raw())
    .await
    .expect("创建任务测试规则快照失败");
    sqlx::query("UPDATE rules SET action = 'RESEND', version = 2 WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_a)
        .bind(rule_a)
        .execute(admin.raw())
        .await
        .expect("更新任务测试当前规则失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 2, $3)",
    )
    .bind(tenant_a)
    .bind(rule_a)
    .bind(json!({"marker": "v2", "target_ref": target_a, "action": "RESEND"}))
    .execute(admin.raw())
    .await
    .expect("创建任务测试新规则快照失败");

    let task_a1: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, rule_id, rule_version, target_tg_chat_id, sender_identity_id, action,
             status, attempt_count, last_error_code, created_at, updated_at, idem_key)
         VALUES ($1, $2, 1, -5410, $3, 'COPY', 'DEAD', 2, 'TIMEOUT',
                 clock_timestamp() - interval '2 minutes',
                 clock_timestamp() - interval '2 minutes', 'task-a1')
         RETURNING id",
    )
    .bind(tenant_a)
    .bind(rule_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 A 死信任务失败");
    let _task_a2: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, rule_id, rule_version, target_tg_chat_id, sender_identity_id, action,
             status, attempt_count, created_at, updated_at, idem_key)
         VALUES ($1, $2, 1, -5411, $3, 'COPY', 'FAILED', 1,
                 clock_timestamp() - interval '1 minute',
                 clock_timestamp() - interval '1 minute', 'task-a2')
         RETURNING id",
    )
    .bind(tenant_a)
    .bind(rule_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 A 失败任务失败");
    let task_b: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, target_tg_chat_id, sender_identity_id, action, status, idem_key)
         VALUES ($1, -5420, $2, 'COPY', 'DEAD', 'task-b1')
         RETURNING id",
    )
    .bind(tenant_b)
    .bind(identity_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 B 死信任务失败");
    sqlx::query(
        "INSERT INTO delivery_logs (task_id, attempt, ok, error_code, error_msg, latency_ms)
         VALUES ($1, 1, TRUE, NULL, NULL, 12),
                ($1, 2, FALSE, 'TIMEOUT', 'telegram timeout', 31)",
    )
    .bind(task_a1)
    .execute(admin.raw())
    .await
    .expect("创建任务投递日志失败");

    let auth_a = AuthContext::new(tenant_a, 5401, TenantRole::Owner, AuthSource::Web);
    let list = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!(
                    "/api/v1/tasks?status=DEAD&target_ref={target_a}&tenant_id={}",
                    tenant_b.get()
                ),
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("执行任务列表请求失败");
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = response_json(list).await;
    assert_eq!(
        list_body["items"].as_array().expect("任务列表缺失").len(),
        1
    );
    assert_eq!(list_body["items"][0]["id"], task_a1);
    assert_eq!(list_body["items"][0]["target_ref"], target_a);
    assert_eq!(list_body["items"][0]["status"], "DEAD");

    let paged = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/tasks?per_page=1", Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行任务分页请求失败");
    assert_eq!(paged.status(), StatusCode::OK);
    let paged_body = response_json(paged).await;
    assert_eq!(paged_body["per_page"], 1);
    assert_eq!(paged_body["has_more"], true);
    assert_eq!(paged_body["items"][0]["status"], "FAILED");

    let detail = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", &format!("/api/v1/tasks/{task_a1}"), Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行任务详情请求失败");
    assert_eq!(detail.status(), StatusCode::OK);
    let detail_body = response_json(detail).await;
    assert_eq!(detail_body["task"]["id"], task_a1);
    assert_eq!(detail_body["task"]["action"], "COPY");
    assert_eq!(detail_body["rule_snapshot"]["marker"], "v1");
    assert_eq!(
        detail_body["delivery_logs"]
            .as_array()
            .expect("投递日志缺失")
            .len(),
        2
    );
    assert_eq!(detail_body["delivery_logs"][1]["error_code"], "TIMEOUT");

    let hidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", &format!("/api/v1/tasks/{task_b}"), Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行跨租户任务详情请求失败");
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

    let retry = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/tasks/{task_a1}/retry"),
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("执行任务重投请求失败");
    assert_eq!(retry.status(), StatusCode::OK);
    let retry_body = response_json(retry).await;
    assert_eq!(retry_body["task"]["status"], "PENDING");
    assert!(retry_body["task"]["last_error_code"].is_null());
    assert_eq!(retry_body["rule_snapshot"]["marker"], "v1");

    let second_retry = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/tasks/{task_a1}/retry"),
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("执行不可重投任务请求失败");
    assert_eq!(second_retry.status(), StatusCode::BAD_REQUEST);
    let second_retry_body = response_json(second_retry).await;
    assert_eq!(second_retry_body["error"]["code"], "CONDITION_INVALID");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn audit_log_router_filters_by_source_and_actor_without_cross_tenant_rows(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "audit-tenant-a", 5501).await;
    let tenant_b = support::seed_tenant(&admin, "audit-tenant-b", 5502).await;

    let audit_a: i64 = sqlx::query_scalar(
        "INSERT INTO audit_logs
            (tenant_id, actor_user_id, action, resource, resource_id, before, after, source)
         VALUES ($1, 5501, 'rule.create', 'rule', 11, $2, $3, 'web')
         RETURNING id",
    )
    .bind(tenant_a)
    .bind(json!({"enabled": false}))
    .bind(json!({"enabled": true}))
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 A 审计日志失败");
    sqlx::query(
        "INSERT INTO audit_logs
            (tenant_id, actor_user_id, action, resource, resource_id, source, created_at)
         VALUES ($1, 5501, 'source.update', 'source', 12, 'bot',
                 clock_timestamp() - interval '1 minute')",
    )
    .bind(tenant_a)
    .execute(admin.raw())
    .await
    .expect("创建租户 A 第二条审计日志失败");
    let audit_b: i64 = sqlx::query_scalar(
        "INSERT INTO audit_logs
            (tenant_id, actor_user_id, action, resource, resource_id, source)
         VALUES ($1, 5502, 'rule.create', 'rule', 21, 'web')
         RETURNING id",
    )
    .bind(tenant_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 B 审计日志失败");

    let auth_a = AuthContext::new(tenant_a, 5501, TenantRole::Viewer, AuthSource::Web);
    let filtered = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                "/api/v1/audit-logs?source=web&actor=5501&per_page=1",
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("执行审计日志筛选请求失败");
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered_body = response_json(filtered).await;
    assert_eq!(
        filtered_body["items"]
            .as_array()
            .expect("审计日志列表缺失")
            .len(),
        1
    );
    assert_eq!(filtered_body["items"][0]["id"], audit_a);
    assert_eq!(filtered_body["items"][0]["source"], "web");
    assert_eq!(filtered_body["items"][0]["actor_user_id"], 5501);
    assert_eq!(filtered_body["items"][0]["before"]["enabled"], false);
    assert_eq!(filtered_body["items"][0]["after"]["enabled"], true);
    assert_eq!(filtered_body["has_more"], false);

    let all = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", "/api/v1/audit-logs", Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行租户审计日志列表请求失败");
    assert_eq!(all.status(), StatusCode::OK);
    let all_body = response_json(all).await;
    assert_eq!(
        all_body["items"]
            .as_array()
            .expect("审计日志列表缺失")
            .len(),
        2
    );
    assert!(
        all_body["items"]
            .as_array()
            .expect("审计日志列表缺失")
            .iter()
            .all(|item| item["id"] != audit_b)
    );
    assert!(all_body.get("tenant_id").is_none());

    let invalid = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/api/v1/audit-logs?source=unknown", Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行非法审计日志筛选请求失败");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let invalid_body = response_json(invalid).await;
    assert_eq!(invalid_body["error"]["code"], "CONDITION_INVALID");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_router_lists_detail_versions_and_hides_cross_tenant_rules(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "rule-tenant-a", 5601).await;
    let tenant_b = support::seed_tenant(&admin, "rule-tenant-b", 5602).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;

    let target_a: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5609, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 A 规则目标失败");
    let keyword_a: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'rule-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 A 规则关键词失败");
    let rule_a: i64 = sqlx::query_scalar(
        "INSERT INTO rules
            (tenant_id, keyword_id, target_ref, action, priority, stop_processing, template, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 9, TRUE, 'header', 2, TRUE)
         RETURNING id",
    )
    .bind(tenant_a)
    .bind(keyword_a)
    .bind(target_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 A 规则失败");
    sqlx::query(
        "INSERT INTO rule_conditions (rule_id, kind, value)
         VALUES ($1, 'exclude_word', '广告'), ($1, 'size_min', '100')",
    )
    .bind(rule_a)
    .execute(admin.raw())
    .await
    .expect("创建规则条件失败");
    sqlx::query(
        "INSERT INTO rule_replacements (tenant_id, rule_id, seq, pattern, replacement, enabled)
         VALUES ($1, $2, 1, 'foo', 'bar', TRUE)",
    )
    .bind(tenant_a)
    .bind(rule_a)
    .execute(admin.raw())
    .await
    .expect("创建规则替换失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3), ($1, $2, 2, $4)",
    )
    .bind(tenant_a)
    .bind(rule_a)
    .bind(json!({"marker": "v1", "target_ref": target_a}))
    .bind(json!({"marker": "v2", "target_ref": target_a}))
    .execute(admin.raw())
    .await
    .expect("创建规则版本失败");

    let target_b: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5619, $2, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .bind(identity_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 B 规则目标失败");
    let keyword_b: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'other-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 B 规则关键词失败");
    let rule_b: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 1, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .bind(keyword_b)
    .bind(target_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 B 规则失败");

    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(tenant_b)
    .bind(rule_b)
    .bind(json!({"marker": "other-tenant"}))
    .execute(admin.raw())
    .await
    .expect("创建租户 B 历史版本失败");

    let auth_a = AuthContext::new(tenant_a, 5601, TenantRole::Viewer, AuthSource::Web);
    let list = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!("/api/v1/rules?enabled=true&target_ref={target_a}&per_page=1"),
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("执行规则列表请求失败");
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = response_json(list).await;
    assert_eq!(
        list_body["items"].as_array().expect("规则列表缺失").len(),
        1
    );
    assert_eq!(list_body["items"][0]["id"], rule_a);
    assert_eq!(list_body["items"][0]["keyword"], "rule-keyword");
    assert_eq!(list_body["items"][0]["template"], "header");

    let detail = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", &format!("/api/v1/rules/{rule_a}"), Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行规则详情请求失败");
    assert_eq!(detail.status(), StatusCode::OK);
    let detail_body = response_json(detail).await;
    assert_eq!(detail_body["rule"]["id"], rule_a);
    assert_eq!(
        detail_body["conditions"]
            .as_array()
            .expect("条件缺失")
            .len(),
        2
    );
    assert_eq!(detail_body["replacements"][0]["seq"], 1);
    assert_eq!(detail_body["replacements"][0]["replacement"], "bar");

    let versions = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!("/api/v1/rules/{rule_a}/versions"),
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("执行规则版本列表请求失败");
    assert_eq!(versions.status(), StatusCode::OK);
    let versions_body = response_json(versions).await;
    assert_eq!(
        versions_body["items"]
            .as_array()
            .expect("版本列表缺失")
            .len(),
        2
    );
    assert_eq!(versions_body["items"][0]["version"], 2);

    let version = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!("/api/v1/rules/{rule_a}/versions/1"),
                Body::empty(),
            ),
            auth_a,
        ))
        .await
        .expect("执行规则版本详情请求失败");
    assert_eq!(version.status(), StatusCode::OK);
    let version_body = response_json(version).await;
    assert_eq!(version_body["version"], 1);
    assert_eq!(version_body["snapshot"]["marker"], "v1");

    for uri in [
        format!("/api/v1/rules/{rule_b}/versions"),
        format!("/api/v1/rules/{rule_b}/versions/1"),
        format!("/api/v1/rules/{rule_a}/versions/999"),
    ] {
        let unavailable = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(request("GET", &uri, Body::empty()), auth_a))
            .await
            .expect("执行不可见版本请求失败");
        assert_eq!(unavailable.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_json(unavailable).await["error"]["code"],
            "NOT_FOUND"
        );
    }

    let hidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", &format!("/api/v1/rules/{rule_b}"), Body::empty()),
            auth_a,
        ))
        .await
        .expect("执行跨租户规则详情请求失败");
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

    let veto = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{rule_a}/test"),
                Body::from(
                    json!({
                        "text": "rule-keyword 广告",
                        "media_type": "DOCUMENT",
                        "file_size": 200
                    })
                    .to_string(),
                ),
            ),
            auth_a,
        ))
        .await
        .expect("执行规则否决试跑请求失败");
    assert_eq!(veto.status(), StatusCode::OK);
    let veto_body = response_json(veto).await;
    assert_eq!(veto_body["matched"], false);
    assert_eq!(veto_body["trace"][2]["stage"], "trigger");
    assert_eq!(veto_body["trace"][2]["pass"], true);
    assert_eq!(veto_body["trace"][3]["stage"], "veto");
    assert_eq!(veto_body["trace"][3]["pass"], false);

    let matched = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{rule_a}/test"),
                Body::from(
                    json!({
                        "text": "rule-keyword",
                        "media_type": "DOCUMENT",
                        "file_size": 200
                    })
                    .to_string(),
                ),
            ),
            auth_a,
        ))
        .await
        .expect("执行规则命中试跑请求失败");
    assert_eq!(matched.status(), StatusCode::OK);
    let matched_body = response_json(matched).await;
    assert_eq!(matched_body["matched"], true);
    assert_eq!(matched_body["route"]["target_ref"], target_a);
    assert_eq!(matched_body["route"]["action"], "COPY");

    let cross_tenant = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{rule_b}/test"),
                Body::from(json!({"text": "other-keyword"}).to_string()),
            ),
            auth_a,
        ))
        .await
        .expect("执行跨租户规则试跑请求失败");
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_router_filters_intersect_paginate_and_remain_tenant_scoped(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "rule-filters", 5651).await;
    let other_tenant = support::seed_tenant(&admin, "rule-filters-other", 5652).await;
    let identity = support::seed_identity(&admin, tenant).await;
    let other_identity = support::seed_identity(&admin, other_tenant).await;
    let source_a = support::seed_source(&admin, tenant, identity, -5651, "Filter A").await;
    let source_b = support::seed_source(&admin, tenant, identity, -5652, "Filter B").await;
    let other_source =
        support::seed_source(&admin, other_tenant, other_identity, -5653, "Other").await;
    let mut targets = Vec::new();
    for (target_tenant, sender, chat_id) in [
        (tenant, identity, -5654_i64),
        (tenant, identity, -5655),
        (other_tenant, other_identity, -5656),
    ] {
        let target: i64 = sqlx::query_scalar(
            "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
             VALUES ($1, $2, $3, TRUE) RETURNING id",
        )
        .bind(target_tenant)
        .bind(chat_id)
        .bind(sender)
        .fetch_one(admin.raw())
        .await
        .expect("创建规则筛选目标失败");
        targets.push(target);
    }
    let mut rule_ids = Vec::new();
    for (index, (rule_tenant, target, source, enabled, priority)) in [
        (tenant, targets[0], Some(source_a), true, 30),
        (tenant, targets[0], Some(source_b), false, 20),
        (tenant, targets[1], Some(source_a), false, 10),
        (tenant, targets[0], Some(source_a), true, 5),
        (tenant, targets[1], None, true, 0),
        (other_tenant, targets[2], Some(other_source), false, 99),
    ]
    .into_iter()
    .enumerate()
    {
        let keyword: i64 = sqlx::query_scalar(
            "INSERT INTO keywords (tenant_id, word, match_type, enabled)
             VALUES ($1, $2, 'exact', TRUE) RETURNING id",
        )
        .bind(rule_tenant)
        .bind(format!("filter-{index}"))
        .fetch_one(admin.raw())
        .await
        .expect("创建规则筛选关键词失败");
        let rule_id: i64 = sqlx::query_scalar(
            "INSERT INTO rules (tenant_id, keyword_id, source_ref, target_ref, action, priority, version, enabled)
             VALUES ($1, $2, $3, $4, 'COPY', $5, 1, $6) RETURNING id",
        )
        .bind(rule_tenant)
        .bind(keyword)
        .bind(source)
        .bind(target)
        .bind(priority)
        .bind(enabled)
        .fetch_one(admin.raw())
        .await
        .expect("创建规则筛选样本失败");
        rule_ids.push(rule_id);
    }
    let viewer = AuthContext::new(tenant, 5653, TenantRole::Viewer, AuthSource::Web);
    let intersection = format!(
        "enabled=true&target_ref={}&source_ref={source_a}",
        targets[0]
    );
    for (query, expected, page, has_more) in [
        (String::new(), rule_ids[..5].to_vec(), 1, false),
        (
            "enabled=true".to_owned(),
            vec![rule_ids[0], rule_ids[3], rule_ids[4]],
            1,
            false,
        ),
        (
            "enabled=false".to_owned(),
            vec![rule_ids[1], rule_ids[2]],
            1,
            false,
        ),
        (
            format!("target_ref={}", targets[0]),
            vec![rule_ids[0], rule_ids[1], rule_ids[3]],
            1,
            false,
        ),
        (
            format!("source_ref={source_a}"),
            vec![rule_ids[0], rule_ids[2], rule_ids[3]],
            1,
            false,
        ),
        (
            format!("source_ref={source_b}"),
            vec![rule_ids[1]],
            1,
            false,
        ),
        (
            intersection.clone(),
            vec![rule_ids[0], rule_ids[3]],
            1,
            false,
        ),
        (
            format!("{intersection}&per_page=1&page=1"),
            vec![rule_ids[0]],
            1,
            true,
        ),
        (
            format!("{intersection}&per_page=1&page=2"),
            vec![rule_ids[3]],
            2,
            false,
        ),
        (
            format!("{intersection}&per_page=1&page=3"),
            vec![],
            3,
            false,
        ),
        (
            format!(
                "enabled=false&target_ref={}&source_ref={source_a}",
                targets[0]
            ),
            vec![],
            1,
            false,
        ),
        (format!("target_ref={}", targets[2]), vec![], 1, false),
        (format!("source_ref={other_source}"), vec![], 1, false),
        ("source_ref=9007199254740993".to_owned(), vec![], 1, false),
        (
            format!("{intersection}&tenant_id={}", other_tenant.get()),
            vec![rule_ids[0], rule_ids[3]],
            1,
            false,
        ),
    ] {
        let response = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("GET", &format!("/api/v1/rules?{query}"), Body::empty()),
                viewer,
            ))
            .await
            .expect("执行规则组合筛选请求失败");
        assert_eq!(response.status(), StatusCode::OK, "query: {query}");
        let body = response_json(response).await;
        let actual: Vec<_> = body["items"]
            .as_array()
            .expect("响应缺少规则列表")
            .iter()
            .map(|rule| rule["id"].as_i64().expect("规则缺少 id"))
            .collect();
        assert_eq!(actual, expected, "query: {query}");
        assert_eq!(body["page"], page, "query: {query}");
        assert_eq!(body["has_more"], has_more, "query: {query}");
    }
    for query in [
        "enabled=invalid",
        "target_ref=abc",
        "source_ref=1.5",
        "source_ref=9223372036854775808",
    ] {
        let rejected = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("GET", &format!("/api/v1/rules?{query}"), Body::empty()),
                viewer,
            ))
            .await
            .expect("执行无效规则筛选请求失败");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST, "query: {query}");
    }
    let other = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", "/api/v1/rules?enabled=false", Body::empty()),
            AuthContext::new(other_tenant, 5652, TenantRole::Owner, AuthSource::Web),
        ))
        .await
        .expect("读取另一租户规则筛选结果失败");
    assert_eq!(other.status(), StatusCode::OK);
    let other = response_json(other).await;
    assert_eq!(
        other["items"]
            .as_array()
            .expect("响应缺少其他租户规则列表")
            .len(),
        1
    );
    assert_eq!(other["items"][0]["id"], rule_ids[5]);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_router_writes_rules_with_conflict_version_and_role_guards(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "rule-write-tenant-a", 5701).await;
    let tenant_b = support::seed_tenant(&admin, "rule-write-tenant-b", 5702).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;
    let target_a: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5709, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则写入目标 A 失败");
    let target_b: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5719, $2, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .bind(identity_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则写入目标 B 失败");
    let keyword_a: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'write-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则写入关键词 A 失败");
    let keyword_b: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'write-keyword-b', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则写入关键词 B 失败");

    let owner_a = AuthContext::new(tenant_a, 5701, TenantRole::Owner, AuthSource::Web);
    let viewer_a = AuthContext::new(tenant_a, 5703, TenantRole::Viewer, AuthSource::Web);
    let create_payload = json!({
        "keyword_id": keyword_a,
        "target_ref": target_a,
        "action": "COPY",
        "priority": 10,
        "stop_processing": false,
        "template": "header",
        "enabled": true,
        "conditions": [{"kind": "sender", "value": "42"}],
        "replacements": [{"seq": 1, "pattern": "foo", "replacement": "bar", "enabled": true}]
    });
    let created = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/rules",
                Body::from(create_payload.to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行规则创建请求失败");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = response_json(created).await;
    let rule_id = created_body["rule"]["id"]
        .as_i64()
        .expect("创建响应缺少规则 id");
    assert_eq!(created_body["rule"]["version"], 1);
    assert_eq!(created_body["conditions"][0]["kind"], "sender");
    assert_eq!(created_body["replacements"][0]["replacement"], "bar");

    let conflict = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/rules",
                Body::from(create_payload.to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行规则冲突请求失败");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict_body = response_json(conflict).await;
    assert_eq!(conflict_body["error"]["code"], "RULE_CONFLICT");
    assert_eq!(conflict_body["error"]["details"]["existing_id"], rule_id);

    let priority_patch = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/rules/{rule_id}"),
                Body::from(json!({"priority": 99, "enabled": false}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行规则非语义更新请求失败");
    assert_eq!(priority_patch.status(), StatusCode::OK);
    let priority_body = response_json(priority_patch).await;
    assert_eq!(priority_body["rule"]["version"], 1);
    assert_eq!(priority_body["rule"]["priority"], 99);
    assert_eq!(priority_body["rule"]["enabled"], false);

    for enabled in [true, false] {
        let toggled = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request(
                    "PATCH",
                    &format!("/api/v1/rules/{rule_id}"),
                    Body::from(json!({"enabled": enabled}).to_string()),
                ),
                owner_a,
            ))
            .await
            .expect("执行规则启停请求失败");
        assert_eq!(toggled.status(), StatusCode::OK);
        let toggled_body = response_json(toggled).await;
        assert_eq!(toggled_body["rule"]["enabled"], enabled);
        assert_eq!(toggled_body["rule"]["version"], 1);
        assert_eq!(toggled_body["rule"]["action"], "COPY");
        assert_eq!(toggled_body["rule"]["priority"], 99);
        assert_eq!(toggled_body["rule"]["template"], "header");
        assert_eq!(toggled_body["conditions"], created_body["conditions"]);
        assert_eq!(toggled_body["replacements"], created_body["replacements"]);
    }

    let semantic_patch = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/rules/{rule_id}"),
                Body::from(
                    json!({
                        "action": "FORWARD",
                        "conditions": [{"kind": "sender", "value": "7"}]
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行规则语义更新请求失败");
    assert_eq!(semantic_patch.status(), StatusCode::OK);
    let semantic_body = response_json(semantic_patch).await;
    assert_eq!(semantic_body["rule"]["version"], 2);
    assert_eq!(semantic_body["rule"]["action"], "FORWARD");
    assert_eq!(semantic_body["conditions"][0]["value"], "7");

    let forbidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/rules/{rule_id}"),
                Body::from(json!({"enabled": true}).to_string()),
            ),
            viewer_a,
        ))
        .await
        .expect("执行 Viewer 规则更新请求失败");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let rule_b: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 1, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .bind(keyword_b)
    .bind(target_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户 B 规则失败");
    let cross_tenant_patch = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PATCH",
                &format!("/api/v1/rules/{rule_b}"),
                Body::from(json!({"enabled": false}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户规则启停请求失败");
    assert_eq!(cross_tenant_patch.status(), StatusCode::NOT_FOUND);
    let owner_b = AuthContext::new(tenant_b, 5702, TenantRole::Owner, AuthSource::Web);
    let unchanged = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", &format!("/api/v1/rules/{rule_b}"), Body::empty()),
            owner_b,
        ))
        .await
        .expect("查询未被跨租户启停的规则失败");
    assert_eq!(unchanged.status(), StatusCode::OK);
    assert_eq!(response_json(unchanged).await["rule"]["enabled"], true);
    let cross_tenant = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("DELETE", &format!("/api/v1/rules/{rule_b}"), Body::empty()),
            owner_a,
        ))
        .await
        .expect("执行跨租户规则删除请求失败");
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);

    let deleted = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("DELETE", &format!("/api/v1/rules/{rule_id}"), Body::empty()),
            owner_a,
        ))
        .await
        .expect("执行规则删除请求失败");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let missing = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", &format!("/api/v1/rules/{rule_id}"), Body::empty()),
            owner_a,
        ))
        .await
        .expect("执行删除后规则查询失败");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_router_delete_uses_session_csrf_and_keeps_tenant_and_task_boundaries(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let auth_pool = support::auth_pool(&admin).await;
    let tenant = support::seed_tenant(&admin, "rule-delete-session", 5751).await;
    let other_tenant = support::seed_tenant(&admin, "rule-delete-other", 5752).await;
    let identity = support::seed_identity(&admin, tenant).await;
    let other_identity = support::seed_identity(&admin, other_tenant).await;
    let csrf = [71_u8; 32];
    let owner_session = [72_u8; 32];
    let admin_session = [73_u8; 32];
    let viewer_session = [74_u8; 32];
    let member_session = [75_u8; 32];
    let other_session = [76_u8; 32];
    for (member_tenant, user_id, role, session) in [
        (tenant, 5751_i64, "TENANT_OWNER", owner_session),
        (tenant, 5753, "TENANT_ADMIN", admin_session),
        (tenant, 5754, "VIEWER", viewer_session),
        (tenant, 5755, "MEMBER", member_session),
        (other_tenant, 5752, "TENANT_OWNER", other_session),
    ] {
        sqlx::query("INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(member_tenant)
            .bind(user_id)
            .bind(role)
            .execute(admin.raw())
            .await
            .expect("创建规则删除测试成员失败");
        sqlx::query(
            "INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at)
             VALUES ($1, $2, $3, $4, now() + interval '30 days')",
        )
        .bind(member_tenant)
        .bind(session.as_slice())
        .bind(user_id)
        .bind(csrf.as_slice())
        .execute(admin.raw())
        .await
        .expect("创建规则删除测试会话失败");
    }
    let mut rule_ids = Vec::new();
    let mut task_ids = Vec::new();
    for (rule_tenant, sender, chat_id) in [
        (tenant, identity, -5751_i64),
        (other_tenant, other_identity, -5752),
    ] {
        let target: i64 = sqlx::query_scalar(
            "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
             VALUES ($1, $2, $3, TRUE) RETURNING id",
        )
        .bind(rule_tenant)
        .bind(chat_id)
        .bind(sender)
        .fetch_one(admin.raw())
        .await
        .expect("创建规则删除目标失败");
        let keyword: i64 = sqlx::query_scalar(
            "INSERT INTO keywords (tenant_id, word, match_type, enabled)
             VALUES ($1, 'delete-rule', 'exact', TRUE) RETURNING id",
        )
        .bind(rule_tenant)
        .fetch_one(admin.raw())
        .await
        .expect("创建规则删除关键词失败");
        let rule = tgm_db::repo::create_rule(
            &app,
            rule_tenant,
            tgm_db::repo::RuleCreateInput {
                keyword_id: keyword,
                source_ref: None,
                media_type: Some("TEXT".to_owned()),
                target_ref: target,
                action: "COPY".to_owned(),
                priority: 1,
                stop_processing: false,
                template: Some("header".to_owned()),
                enabled: true,
                conditions: vec![tgm_db::repo::RuleConditionInput {
                    kind: "sender".to_owned(),
                    value: "42".to_owned(),
                }],
                replacements: vec![tgm_db::repo::RuleReplacementInput {
                    seq: 1,
                    pattern: "foo".to_owned(),
                    replacement: "bar".to_owned(),
                    enabled: true,
                }],
            },
        )
        .await
        .expect("创建规则删除样本失败");
        rule_ids.push(rule.id);
        let task_id: i64 = sqlx::query_scalar(
            "INSERT INTO delivery_tasks
                (tenant_id, rule_id, rule_version, target_tg_chat_id, sender_identity_id, action, status)
             VALUES ($1, $2, 1, $3, $4, 'COPY', 'PENDING') RETURNING id",
        )
        .bind(rule_tenant)
        .bind(rule.id)
        .bind(chat_id)
        .bind(sender)
        .fetch_one(admin.raw())
        .await
        .expect("创建规则删除前的投递任务失败");
        task_ids.push(task_id);
    }
    let uri = format!("/api/v1/rules/{}", rule_ids[0]);
    for (request, status, code) in [
        (
            request("DELETE", &uri, Body::empty()),
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        ),
        (
            Request::builder()
                .method("DELETE")
                .uri(&uri)
                .header("cookie", session_cookie(&owner_session))
                .body(Body::empty())
                .expect("构造缺少 CSRF 的规则删除请求失败"),
            StatusCode::FORBIDDEN,
            "CSRF_INVALID",
        ),
        (
            session_management_request("DELETE", &uri, &owner_session, &[0_u8; 32]),
            StatusCode::FORBIDDEN,
            "CSRF_INVALID",
        ),
        (
            session_management_request("DELETE", &uri, &viewer_session, &csrf),
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        ),
        (
            session_management_request("DELETE", &uri, &member_session, &csrf),
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        ),
        (
            session_management_request("DELETE", &uri, &other_session, &csrf),
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        ),
    ] {
        let denied = router(ControlPlaneState::with_auth_pool(
            app.clone(),
            auth_pool.clone(),
        ))
        .oneshot(request)
        .await
        .expect("执行被拒绝的规则删除请求失败");
        assert_eq!(denied.status(), status);
        assert_eq!(response_json(denied).await["error"]["code"], code);
        for (rule_tenant, rule_id) in [(tenant, rule_ids[0]), (other_tenant, rule_ids[1])] {
            let detail = tgm_db::repo::load_rule_detail(&app, rule_tenant, rule_id)
                .await
                .expect("读取被拒绝删除后的规则失败")
                .expect("拒绝删除后规则不应消失");
            assert_eq!(detail.1.len(), 1);
            assert_eq!(detail.2.len(), 1);
            assert!(
                tgm_db::repo::load_rule_version(&app, rule_tenant, rule_id, 1)
                    .await
                    .expect("读取拒绝删除后的快照失败")
                    .is_some()
            );
        }
    }
    for (rule_tenant, rule_id, task_id, session) in [
        (tenant, rule_ids[0], task_ids[0], admin_session),
        (other_tenant, rule_ids[1], task_ids[1], other_session),
    ] {
        let deleted = router(ControlPlaneState::with_auth_pool(
            app.clone(),
            auth_pool.clone(),
        ))
        .oneshot(session_management_request(
            "DELETE",
            &format!("/api/v1/rules/{rule_id}"),
            &session,
            &csrf,
        ))
        .await
        .expect("执行有效 Cookie 规则删除请求失败");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert!(
            to_bytes(deleted.into_body(), 1024)
                .await
                .expect("读取删除响应失败")
                .is_empty()
        );
        let counts: (i64, i64, i64, i64) = tgm_db::tenant::tenant_tx(&app, rule_tenant, |connection| {
            Box::pin(async move {
                sqlx::query_as(
                    "SELECT
                        (SELECT count(*) FROM rules WHERE tenant_id = $1 AND id = $2),
                        (SELECT count(*) FROM rule_conditions WHERE rule_id = $2),
                        (SELECT count(*) FROM rule_replacements WHERE tenant_id = $1 AND rule_id = $2),
                        (SELECT count(*) FROM rule_versions WHERE tenant_id = $1 AND rule_id = $2)",
                )
                .bind(rule_tenant.get()).bind(rule_id).fetch_one(&mut *connection).await
            })
        }).await.expect("受限角色核对规则级联清理失败");
        assert_eq!(counts, (0, 0, 0, 0));
        let (task, _logs) = tgm_db::repo::load_delivery_task_detail(&app, rule_tenant, task_id)
            .await
            .expect("读取删除规则后的投递任务失败")
            .expect("规则删除不应删除投递任务");
        assert_eq!(task.rule_id, Some(rule_id));
        assert_eq!(task.rule_version, Some(1));
        assert_eq!(task.status, "PENDING");
        assert_eq!(task.attempt_count, 0);
        let repeated = router(ControlPlaneState::with_auth_pool(
            app.clone(),
            auth_pool.clone(),
        ))
        .oneshot(session_management_request(
            "DELETE",
            &format!("/api/v1/rules/{rule_id}"),
            &session,
            &csrf,
        ))
        .await
        .expect("执行重复规则删除请求失败");
        assert_eq!(repeated.status(), StatusCode::NOT_FOUND);
        if rule_tenant == tenant {
            assert!(
                tgm_db::repo::load_rule_detail(&app, other_tenant, rule_ids[1])
                    .await
                    .expect("核对其他租户规则失败")
                    .is_some()
            );
            let (other_task, _logs) =
                tgm_db::repo::load_delivery_task_detail(&app, other_tenant, task_ids[1])
                    .await
                    .expect("核对其他租户投递任务失败")
                    .expect("其他租户任务不应消失");
            assert_eq!(other_task.status, "PENDING");
        }
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_router_patch_clears_nullable_fields_and_preserves_omitted_values(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "rule-nullable-patch", 5801).await;
    let identity = support::seed_identity(&admin, tenant).await;
    let source = support::seed_source(&admin, tenant, identity, -5808, "Patch Source").await;
    let target: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5809, $2, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("创建可空字段测试目标失败");
    let keyword: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'nullable-patch', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建可空字段测试关键词失败");
    let owner = AuthContext::new(tenant, 5801, TenantRole::Owner, AuthSource::Web);
    let created = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/rules",
                Body::from(
                    json!({
                        "keyword_id": keyword,
                        "source_ref": source,
                        "media_type": "TEXT",
                        "target_ref": target,
                        "action": "COPY",
                        "priority": 17,
                        "stop_processing": true,
                        "template": "header",
                        "enabled": true,
                        "conditions": [{"kind": "sender", "value": "42"}],
                        "replacements": [{"seq": 1, "pattern": "foo", "replacement": "bar", "enabled": true}]
                    })
                    .to_string(),
                ),
            ),
            owner,
        ))
        .await
        .expect("创建可空字段测试规则失败");
    assert_eq!(created.status(), StatusCode::CREATED);
    let original = response_json(created).await;
    let rule_id = original["rule"]["id"].as_i64().expect("响应缺少规则 id");
    let uri = format!("/api/v1/rules/{rule_id}");

    let other_tenant = support::seed_tenant(&admin, "rule-nullable-patch-other", 5802).await;
    for (auth, status) in [
        (
            AuthContext::new(tenant, 5803, TenantRole::Viewer, AuthSource::Web),
            StatusCode::FORBIDDEN,
        ),
        (
            AuthContext::new(tenant, 5804, TenantRole::Member, AuthSource::Web),
            StatusCode::FORBIDDEN,
        ),
        (
            AuthContext::new(other_tenant, 5802, TenantRole::Owner, AuthSource::Web),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let denied = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request(
                    "PATCH",
                    &uri,
                    Body::from(
                        json!({"source_ref": null, "media_type": null, "template": null})
                            .to_string(),
                    ),
                ),
                auth,
            ))
            .await
            .expect("执行受限规则清空请求失败");
        assert_eq!(denied.status(), status);
        let unchanged = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(request("GET", &uri, Body::empty()), owner))
            .await
            .expect("读取受限清空请求后的规则失败");
        assert_eq!(unchanged.status(), StatusCode::OK);
        assert_eq!(response_json(unchanged).await, original);
    }

    for (patch, source_ref, media_type, template, version) in [
        (json!({}), json!(source), json!("TEXT"), json!("header"), 1),
        (
            json!({"media_type": null}),
            json!(source),
            json!(null),
            json!("header"),
            2,
        ),
        (
            json!({"template": null}),
            json!(source),
            json!(null),
            json!(null),
            3,
        ),
        (
            json!({"source_ref": null}),
            json!(null),
            json!(null),
            json!(null),
            4,
        ),
        (
            json!({"source_ref": null, "media_type": null, "template": null}),
            json!(null),
            json!(null),
            json!(null),
            4,
        ),
        (
            json!({"source_ref": source, "media_type": "PHOTO", "template": ""}),
            json!(source),
            json!("PHOTO"),
            json!(""),
            5,
        ),
        (
            json!({"source_ref": null, "media_type": null, "template": null}),
            json!(null),
            json!(null),
            json!(null),
            6,
        ),
        (json!({}), json!(null), json!(null), json!(null), 6),
    ] {
        let patched = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("PATCH", &uri, Body::from(patch.to_string())),
                owner,
            ))
            .await
            .expect("执行可空字段规则 PATCH 失败");
        assert_eq!(patched.status(), StatusCode::OK, "patch: {patch}");
        let detail = response_json(patched).await;
        assert_eq!(detail["rule"]["source_ref"], source_ref, "patch: {patch}");
        assert_eq!(detail["rule"]["media_type"], media_type, "patch: {patch}");
        assert_eq!(detail["rule"]["template"], template, "patch: {patch}");
        assert_eq!(detail["rule"]["version"], version, "patch: {patch}");
        for field in [
            "keyword_id",
            "target_ref",
            "action",
            "priority",
            "stop_processing",
            "enabled",
        ] {
            assert_eq!(
                detail["rule"][field], original["rule"][field],
                "patch: {patch}, field: {field}"
            );
        }
        assert_eq!(detail["conditions"], original["conditions"]);
        assert_eq!(detail["replacements"], original["replacements"]);

        let stored = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(request("GET", &uri, Body::empty()), owner))
            .await
            .expect("读取 PATCH 后的规则失败");
        assert_eq!(stored.status(), StatusCode::OK);
        assert_eq!(response_json(stored).await, detail);

        let snapshot = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("GET", &format!("{uri}/versions/{version}"), Body::empty()),
                owner,
            ))
            .await
            .expect("读取可空字段规则快照失败");
        assert_eq!(snapshot.status(), StatusCode::OK);
        let snapshot = response_json(snapshot).await;
        assert_eq!(snapshot["snapshot"]["rule"]["source_ref"], source_ref);
        assert_eq!(snapshot["snapshot"]["rule"]["media_type"], media_type);
        assert_eq!(snapshot["snapshot"]["rule"]["template"], template);
        assert_eq!(snapshot["snapshot"]["rule"]["version"], version);
        assert_eq!(snapshot["snapshot"]["conditions"], original["conditions"]);
        assert_eq!(
            snapshot["snapshot"]["replacements"],
            original["replacements"]
        );
    }

    for (patch, status) in [
        (
            json!({"source_ref": "42", "priority": 999}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"media_type": false, "priority": 999}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"template": [], "priority": 999}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"media_type": "INVALID", "priority": 999}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let rejected = router(ControlPlaneState::new(app.clone()))
            .oneshot(with_auth(
                request("PATCH", &uri, Body::from(patch.to_string())),
                owner,
            ))
            .await
            .expect("执行无效可空字段规则 PATCH 失败");
        assert_eq!(rejected.status(), status, "patch: {patch}");
    }
    let unchanged = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(request("GET", &uri, Body::empty()), owner))
        .await
        .expect("读取拒绝无效 PATCH 后的规则失败");
    assert_eq!(unchanged.status(), StatusCode::OK);
    let unchanged = response_json(unchanged).await;
    assert_eq!(unchanged["rule"]["priority"], 17);
    assert_eq!(unchanged["rule"]["version"], 6);
    for field in ["source_ref", "media_type", "template"] {
        assert_eq!(unchanged["rule"][field], json!(null));
    }
    assert_eq!(unchanged["conditions"], original["conditions"]);
    assert_eq!(unchanged["replacements"], original["replacements"]);

    let versions = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request("GET", &format!("{uri}/versions"), Body::empty()),
            owner,
        ))
        .await
        .expect("读取可空字段规则版本列表失败");
    assert_eq!(versions.status(), StatusCode::OK);
    let versions = response_json(versions).await;
    assert_eq!(
        versions["items"]
            .as_array()
            .expect("响应缺少版本列表")
            .len(),
        6
    );
    let first_version = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request("GET", &format!("{uri}/versions/1"), Body::empty()),
            owner,
        ))
        .await
        .expect("读取未被改写的初始快照失败");
    assert_eq!(first_version.status(), StatusCode::OK);
    let first_version = response_json(first_version).await;
    assert_eq!(first_version["snapshot"]["rule"]["source_ref"], source);
    assert_eq!(first_version["snapshot"]["rule"]["media_type"], "TEXT");
    assert_eq!(first_version["snapshot"]["rule"]["template"], "header");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_router_copies_rules_with_children_and_tenant_guards(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "rule-copy-tenant-a", 5901).await;
    let tenant_b = support::seed_tenant(&admin, "rule-copy-tenant-b", 5902).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;
    let target_a: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5909, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则复制源目标失败");
    let target_a_copy: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5919, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则复制目标失败");
    let target_b: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5929, $2, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .bind(identity_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建跨租户规则复制目标失败");
    let keyword_a: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'copy-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则复制关键词失败");
    let source_rule: i64 = sqlx::query_scalar(
        "INSERT INTO rules
            (tenant_id, keyword_id, source_ref, media_type, target_ref, action,
             priority, stop_processing, template, version, enabled)
         VALUES ($1, $2, NULL, 'TEXT', $3, 'COPY', 17, TRUE, 'prefix', 3, TRUE)
         RETURNING id",
    )
    .bind(tenant_a)
    .bind(keyword_a)
    .bind(target_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则复制源规则失败");
    sqlx::query(
        "INSERT INTO rule_conditions (rule_id, kind, value)
         VALUES ($1, 'sender', '7'), ($1, 'exclude_word', '广告')",
    )
    .bind(source_rule)
    .execute(admin.raw())
    .await
    .expect("创建规则复制条件失败");
    sqlx::query(
        "INSERT INTO rule_replacements (tenant_id, rule_id, seq, pattern, replacement, enabled)
         VALUES ($1, $2, 2, 'bar', 'BAR', FALSE), ($1, $2, 1, 'foo', 'FOO', TRUE)",
    )
    .bind(tenant_a)
    .bind(source_rule)
    .execute(admin.raw())
    .await
    .expect("创建规则复制替换失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3), ($1, $2, 2, $4), ($1, $2, 3, $5)",
    )
    .bind(tenant_a)
    .bind(source_rule)
    .bind(json!({"marker": "v1"}))
    .bind(json!({"marker": "v2"}))
    .bind(json!({"marker": "v3"}))
    .execute(admin.raw())
    .await
    .expect("创建规则复制历史失败");

    let owner_a = AuthContext::new(tenant_a, 5901, TenantRole::Owner, AuthSource::Web);
    let viewer_a = AuthContext::new(tenant_a, 5903, TenantRole::Viewer, AuthSource::Web);
    let copied = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{source_rule}/copy"),
                Body::from(json!({"target_ref": target_a_copy}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行规则复制请求失败");
    assert_eq!(copied.status(), StatusCode::CREATED);
    let copied_body = response_json(copied).await;
    let copied_rule = copied_body["rule"]["id"]
        .as_i64()
        .expect("复制响应缺少新规则 id");
    assert_ne!(copied_rule, source_rule);
    assert_eq!(copied_body["rule"]["target_ref"], target_a_copy);
    assert_eq!(copied_body["rule"]["version"], 1);
    assert_eq!(copied_body["rule"]["priority"], 17);
    assert_eq!(copied_body["conditions"][0]["value"], "7");
    assert_eq!(copied_body["conditions"][1]["value"], "广告");
    assert_eq!(copied_body["replacements"][0]["seq"], 1);
    assert_eq!(copied_body["replacements"][1]["seq"], 2);
    assert_eq!(copied_body["replacements"][1]["enabled"], false);

    let copied_versions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM rule_versions WHERE tenant_id = $1 AND rule_id = $2",
    )
    .bind(tenant_a)
    .bind(copied_rule)
    .fetch_one(admin.raw())
    .await
    .expect("读取复制规则版本失败");
    assert_eq!(copied_versions, 1);

    let conflict = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{source_rule}/copy"),
                Body::from(json!({"target_ref": target_a_copy}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行重复规则复制请求失败");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict_body = response_json(conflict).await;
    assert_eq!(conflict_body["error"]["code"], "RULE_CONFLICT");
    assert_eq!(
        conflict_body["error"]["details"]["existing_id"],
        copied_rule
    );

    let forbidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{source_rule}/copy"),
                Body::from(json!({"target_ref": target_a_copy}).to_string()),
            ),
            viewer_a,
        ))
        .await
        .expect("执行 Viewer 规则复制请求失败");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let cross_target = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{source_rule}/copy"),
                Body::from(json!({"target_ref": target_b}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户目标复制请求失败");
    assert_eq!(cross_target.status(), StatusCode::NOT_FOUND);

    let cross_source = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/rules/999999999/copy",
                Body::from(json!({"target_ref": target_a_copy}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行不存在规则复制请求失败");
    assert_eq!(cross_source.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_replacement_router_replaces_in_order_and_hides_cross_tenant_rules(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "replacement-tenant-a", 5801).await;
    let tenant_b = support::seed_tenant(&admin, "replacement-tenant-b", 5802).await;
    let identity_a = support::seed_identity(&admin, tenant_a).await;
    let identity_b = support::seed_identity(&admin, tenant_b).await;
    let target_a: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5809, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换测试目标 A 失败");
    let target_b: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5819, $2, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .bind(identity_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换测试目标 B 失败");
    let keyword_a: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'replacement-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换测试关键词 A 失败");
    let keyword_b: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'replacement-keyword-b', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换测试关键词 B 失败");
    let rule_a: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 1, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(keyword_a)
    .bind(target_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换测试规则 A 失败");
    let rule_b: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 1, TRUE) RETURNING id",
    )
    .bind(tenant_b)
    .bind(keyword_b)
    .bind(target_b)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换测试规则 B 失败");
    sqlx::query(
        "INSERT INTO rule_replacements (tenant_id, rule_id, seq, pattern, replacement, enabled)
         VALUES ($1, $2, 1, 'foo', 'bar', TRUE)",
    )
    .bind(tenant_a)
    .bind(rule_a)
    .execute(admin.raw())
    .await
    .expect("创建初始替换失败");

    let owner_a = AuthContext::new(tenant_a, 5801, TenantRole::Owner, AuthSource::Web);
    let initial = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!("/api/v1/rules/{rule_a}/replacements"),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行初始替换查询失败");
    assert_eq!(initial.status(), StatusCode::OK);
    let initial_body = response_json(initial).await;
    assert_eq!(initial_body["items"][0]["pattern"], "foo");

    let replaced = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "PUT",
                &format!("/api/v1/rules/{rule_a}/replacements"),
                Body::from(
                    json!({
                        "replacements": [
                            {"seq": 2, "pattern": "bar", "replacement": "baz", "enabled": true},
                            {"seq": 1, "pattern": "foo", "replacement": "BAR", "enabled": true}
                        ]
                    })
                    .to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行整体替换请求失败");
    assert_eq!(replaced.status(), StatusCode::OK);
    let replaced_body = response_json(replaced).await;
    assert_eq!(replaced_body["items"][0]["seq"], 1);
    assert_eq!(replaced_body["items"][0]["replacement"], "BAR");
    assert_eq!(replaced_body["items"][1]["seq"], 2);

    let preview = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                &format!("/api/v1/rules/{rule_a}/replacements/test"),
                Body::from(json!({"text": "foo bar foo"}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行替换试跑请求失败");
    assert_eq!(preview.status(), StatusCode::OK);
    let preview_body = response_json(preview).await;
    assert_eq!(preview_body["original"], "foo bar foo");
    assert_eq!(preview_body["steps"][0]["text"], "BAR bar BAR");
    assert_eq!(preview_body["steps"][1]["text"], "BAR baz BAR");
    assert_eq!(preview_body["final_text"], "BAR baz BAR");

    let version: i32 =
        sqlx::query_scalar("SELECT version FROM rules WHERE tenant_id = $1 AND id = $2")
            .bind(tenant_a)
            .bind(rule_a)
            .fetch_one(admin.raw())
            .await
            .expect("读取替换更新后的规则版本失败");
    assert_eq!(version, 2);

    let target_a_copy: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -5829, $2, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(identity_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换复制目标失败");
    let keyword_a_copy: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'replacement-copy-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换复制关键词失败");
    let rule_copy: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version, enabled)
         VALUES ($1, $2, $3, 'COPY', 1, TRUE) RETURNING id",
    )
    .bind(tenant_a)
    .bind(keyword_a_copy)
    .bind(target_a_copy)
    .fetch_one(admin.raw())
    .await
    .expect("创建替换复制目标规则失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(tenant_a)
    .bind(rule_copy)
    .bind(json!({"marker": "initial"}))
    .execute(admin.raw())
    .await
    .expect("创建替换复制目标初始快照失败");
    sqlx::query(
        "INSERT INTO rule_replacements (tenant_id, rule_id, seq, pattern, replacement, enabled)
         VALUES ($1, $2, 1, 'old', 'new', TRUE)",
    )
    .bind(tenant_a)
    .bind(rule_copy)
    .execute(admin.raw())
    .await
    .expect("创建替换复制目标初始内容失败");

    let copied = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/replacements/copy",
                Body::from(json!({"from_rule": rule_a, "to_rules": [rule_copy]}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行替换规则复制请求失败");
    assert_eq!(copied.status(), StatusCode::OK);
    let copied_body = response_json(copied).await;
    assert_eq!(copied_body["items"][0]["rule_id"], rule_copy);
    assert_eq!(copied_body["items"][0]["version"], 2);
    assert_eq!(copied_body["items"][0]["semantic_changed"], true);

    let copied_replacements = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "GET",
                &format!("/api/v1/rules/{rule_copy}/replacements"),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("读取复制后的替换规则失败");
    assert_eq!(copied_replacements.status(), StatusCode::OK);
    let copied_replacements_body = response_json(copied_replacements).await;
    assert_eq!(copied_replacements_body["items"][0]["pattern"], "foo");
    assert_eq!(copied_replacements_body["items"][0]["replacement"], "BAR");
    assert_eq!(copied_replacements_body["items"][1]["seq"], 2);

    let copied_versions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM rule_versions WHERE tenant_id = $1 AND rule_id = $2",
    )
    .bind(tenant_a)
    .bind(rule_copy)
    .fetch_one(admin.raw())
    .await
    .expect("读取替换复制版本数失败");
    assert_eq!(copied_versions, 2);

    let idempotent = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/replacements/copy",
                Body::from(json!({"from_rule": rule_a, "to_rules": [rule_copy]}).to_string()),
            ),
            owner_a,
        ))
        .await
        .expect("执行幂等替换规则复制请求失败");
    assert_eq!(idempotent.status(), StatusCode::OK);
    let idempotent_body = response_json(idempotent).await;
    assert_eq!(idempotent_body["items"][0]["version"], 2);
    assert_eq!(idempotent_body["items"][0]["semantic_changed"], false);

    let cross_tenant_batch = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/replacements/copy",
                Body::from(
                    json!({"from_rule": rule_a, "to_rules": [rule_copy, rule_b]}).to_string(),
                ),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户替换规则复制请求失败");
    assert_eq!(cross_tenant_batch.status(), StatusCode::NOT_FOUND);
    let version_after_failed_batch: i32 =
        sqlx::query_scalar("SELECT version FROM rules WHERE tenant_id = $1 AND id = $2")
            .bind(tenant_a)
            .bind(rule_copy)
            .fetch_one(admin.raw())
            .await
            .expect("读取失败批量复制后的规则版本失败");
    assert_eq!(version_after_failed_batch, 2);

    let viewer = AuthContext::new(tenant_a, 5803, TenantRole::Viewer, AuthSource::Web);
    let forbidden = router(ControlPlaneState::new(app.clone()))
        .oneshot(with_auth(
            request(
                "POST",
                "/api/v1/replacements/copy",
                Body::from(json!({"from_rule": rule_a, "to_rules": [rule_copy]}).to_string()),
            ),
            viewer,
        ))
        .await
        .expect("执行 Viewer 替换规则复制请求失败");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let hidden = router(ControlPlaneState::new(app))
        .oneshot(with_auth(
            request(
                "GET",
                &format!("/api/v1/rules/{rule_b}/replacements"),
                Body::empty(),
            ),
            owner_a,
        ))
        .await
        .expect("执行跨租户替换查询失败");
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
}

/// 起一个只回 getMe 的 Bot API mock，返回 (base_url, 收到的请求路径)。
async fn mock_get_me(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("启动 getMe mock 失败");
    let address = listener.local_addr().expect("读取 getMe mock 地址失败");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("接受 getMe 请求失败");
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.expect("读取 getMe 请求失败");
            assert!(read > 0, "getMe 请求意外结束");
            buffer.extend_from_slice(&chunk[..read]);
        }
        let request_line = String::from_utf8_lossy(&buffer)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("写入 getMe 响应失败");
        request_line
    });
    (format!("http://{address}"), task)
}

fn bot_intake_state(app: PgPool, base_url: String) -> ControlPlaneState {
    let kek = tgm_core::kek::EnvKek::new(
        tgm_core::profile::Profile::Development,
        "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
    )
    .expect("构造 EnvKek 失败");
    ControlPlaneState::new(app).with_credential_intake(tgm_app::control::CredentialIntake {
        kek: Arc::new(kek),
        bot_api_base_url: base_url,
    })
}

const BOT_TOKEN: &str = "123456:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn bot_token_request() -> Request<Body> {
    request(
        "POST",
        "/api/v1/identities/bot",
        Body::from(json!({ "token": BOT_TOKEN }).to_string()),
    )
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_identity_intake_validates_with_get_me_and_never_echoes_token(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "bot-intake", 5301).await;
    let owner = AuthContext::new(tenant, 5301, TenantRole::Owner, AuthSource::Web);
    let (base_url, server) = mock_get_me(
        r#"{"ok":true,"result":{"id":123456,"is_bot":true,"first_name":"Data","username":"data_bot"}}"#,
    )
    .await;

    let response = router(bot_intake_state(app.clone(), base_url))
        .oneshot(with_auth(bot_token_request(), owner))
        .await
        .expect("执行 Bot Token 提交失败");
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 1_048_576)
        .await
        .expect("读取响应失败");
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("AAAAAAAAAAAA"), "响应回显了 token：{text}");
    let body: Value = serde_json::from_slice(&bytes).expect("解析响应失败");
    assert_eq!(body["kind"], "bot");
    assert_eq!(body["tg_user_id"], 123456);
    assert_eq!(body["display_name"], "Data");
    let request_line = server.await.expect("等待 getMe mock 失败");
    assert!(
        request_line.starts_with(&format!("POST /bot{BOT_TOKEN}/getMe ")),
        "getMe 请求路径不对：{request_line}"
    );

    let identity_id = body["id"].as_i64().expect("id 缺失");
    let stored: (Vec<u8>, String) = sqlx::query_as(
        "SELECT secret_ciphertext, kek_id FROM identity_secrets WHERE identity_id = $1",
    )
    .bind(identity_id)
    .fetch_one(admin.raw())
    .await
    .expect("凭据应当已落库");
    assert!(
        !stored
            .0
            .windows(b"AAAAAAAAAAAA".len())
            .any(|window| window == b"AAAAAAAAAAAA"),
        "库里出现了 token 明文"
    );
    assert_eq!(stored.1, "local:env:v1");
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.create' AND resource_id = $2
            AND NOT (after::text LIKE '%AAAAAAAAAAAA%')",
    )
    .bind(tenant)
    .bind(identity_id)
    .fetch_one(admin.raw())
    .await
    .expect("读审计失败");
    assert_eq!(audits, 1);
}

/// spec/02 §6.3 与 spec/07 §2.1：Bot 入口提交凭据永久拒绝，而且拒在任何外呼之前。
/// 角色不足同样拒在 getMe 之前。base_url 指向不可达端口，一旦外呼就会变成 500。
#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_identity_intake_rejects_bot_source_role_and_bad_token_before_any_call(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "bot-intake-deny", 5311).await;
    let unreachable = "http://127.0.0.1:9".to_owned();

    for (auth, status, code) in [
        (
            AuthContext::new(tenant, 5311, TenantRole::Owner, AuthSource::Bot),
            StatusCode::FORBIDDEN,
            "CREDENTIAL_VIA_WEB_ONLY",
        ),
        (
            AuthContext::new(tenant, 5312, TenantRole::Member, AuthSource::Web),
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        ),
    ] {
        let response = router(bot_intake_state(app.clone(), unreachable.clone()))
            .oneshot(with_auth(bot_token_request(), auth))
            .await
            .expect("执行请求失败");
        assert_eq!(response.status(), status);
        assert_eq!(response_json(response).await["error"]["code"], code);
    }

    let owner = AuthContext::new(tenant, 5311, TenantRole::Owner, AuthSource::Web);
    for bad in [
        "no-colon",
        "abc:AAAAAAAAAAAAAAAAAAAAAAAA",
        "1:short",
        "1:AAAAAAAAAAAAAAAAAAAA/../x",
    ] {
        let response = router(bot_intake_state(app.clone(), unreachable.clone()))
            .oneshot(with_auth(
                request(
                    "POST",
                    "/api/v1/identities/bot",
                    Body::from(json!({ "token": bad }).to_string()),
                ),
                owner,
            ))
            .await
            .expect("执行请求失败");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(
            response_json(response).await["error"]["code"],
            "CONDITION_INVALID"
        );
    }

    let identities: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identities WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await
            .expect("计数失败");
    assert_eq!(identities, 0, "被拒的提交不该建出身份");
}

/// getMe 说这不是 Bot，或 id 与 token 前缀不符：拒绝且不落库。
#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_identity_intake_rejects_mismatched_get_me(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "bot-intake-mismatch", 5321).await;
    let owner = AuthContext::new(tenant, 5321, TenantRole::Owner, AuthSource::Web);
    for body in [
        r#"{"ok":true,"result":{"id":123456,"is_bot":false,"first_name":"Human"}}"#,
        r#"{"ok":true,"result":{"id":999,"is_bot":true,"first_name":"Other"}}"#,
        r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
    ] {
        let (base_url, server) = mock_get_me(body).await;
        let response = router(bot_intake_state(app.clone(), base_url))
            .oneshot(with_auth(bot_token_request(), owner))
            .await
            .expect("执行请求失败");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        let _ = server.await;
    }
    let identities: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identities WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await
            .expect("计数失败");
    assert_eq!(identities, 0);
}

/// 未配置 KEK 时端点 fail closed，不落库。
#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_identity_intake_fails_closed_without_kek(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "bot-intake-nokek", 5331).await;
    let owner = AuthContext::new(tenant, 5331, TenantRole::Owner, AuthSource::Web);
    let response = router(ControlPlaneState::new(app))
        .oneshot(with_auth(bot_token_request(), owner))
        .await
        .expect("执行请求失败");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let identities: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identities WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(admin.raw())
            .await
            .expect("计数失败");
    assert_eq!(identities, 0);
}
