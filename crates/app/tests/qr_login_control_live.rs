//! WP-2 片 2 真机验收：经控制面端点完成一次 QR 登录（含 2FA），再让 worker 用落库的 session 上线
//! （`docs/plan/proposals/WP-2-qr-login-control.md`）。
//!
//! `#[ignore]`：需要人用 Telegram 手机端扫码，CI 与默认测试都不跑。
//! 请求直接打 `router()`，与生产同一套 handler、登记簿、限流与落库代码；租户上下文以
//! owner + Web 注入（会话中间件已有自己的测试）。不打印 `api_hash`、二维码内容、密码、session。
//!
//! ```text
//! TGM_PLATFORM_API_ID / TGM_PLATFORM_API_HASH   租户提交的 api_id / api_hash（测试里用平台应用）
//! TGM_LIVE_QR_HTML_FILE                         每代二维码覆盖写入的本地 HTML（0600，结束即删）
//! TGM_LIVE_2FA_PASSWORD_FILE                    需要 2FA 时轮询等待该文件出现，读完即删
//! ```
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tgm_app::control::{CUSTODY_CONSENT_TEXT_VERSION, ControlPlaneState, CredentialIntake, router};
use tgm_app::mtproto_worker::{IdentityExit, MtprotoTarget, MtprotoWorkerConfig, run_identity};
use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
use tgm_core::ids::IdentityId;
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tokio::sync::watch;
use tower::ServiceExt;

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";
const OWNER: i64 = 9401;

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("真机验收缺少环境变量 {name}"))
}

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

fn write_private(path: &PathBuf, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .expect("写本地文件失败");
    file.write_all(bytes).expect("写本地文件失败");
}

fn qr_page(svg: &str, generation: u32) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"3\">\
         <title>tgm QR</title></head><body style=\"background:#fff\">\
         <p>generation {generation}：Telegram 手机端 设置 → 设备 → 扫描二维码</p>\
         <div style=\"width:320px\">{svg}</div></body></html>"
    )
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
        .expect("执行请求失败");
    let status = response.status();
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .map(|value| value.as_bytes()),
        Some(&b"no-store"[..])
    );
    let bytes = to_bytes(response.into_body(), 4 * 1_048_576)
        .await
        .expect("读取响应失败");
    (
        status,
        serde_json::from_slice(&bytes).expect("解析响应失败"),
    )
}

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("user-agent", "tgm-live-acceptance")
        .body(Body::from(body.to_string()))
        .expect("构造请求失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要人用 Telegram 手机端扫码"]
async fn live_qr_login_via_control_plane_then_worker_online(admin: PgPool) {
    let api_id: i32 = required("TGM_PLATFORM_API_ID")
        .parse()
        .expect("TGM_PLATFORM_API_ID 必须是整数");
    let api_hash = required("TGM_PLATFORM_API_HASH");
    let html_path = PathBuf::from(required("TGM_LIVE_QR_HTML_FILE"));
    let password_path = PathBuf::from(required("TGM_LIVE_2FA_PASSWORD_FILE"));

    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "qr-control-live", OWNER).await;
    sqlx::query("UPDATE tenants SET plan = 'pro' WHERE id = $1")
        .bind(tenant)
        .execute(admin.raw())
        .await
        .expect("升 pro 失败");
    sqlx::query("INSERT INTO tenant_members (tenant_id, user_id) VALUES ($1, $2)")
        .bind(tenant)
        .bind(OWNER)
        .execute(admin.raw())
        .await
        .expect("加成员失败");
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
            "/api/v1/identities/login/qr",
            json!({
                "api_id": api_id,
                "api_hash": api_hash,
                "consent_text_version": CUSTODY_CONSENT_TEXT_VERSION,
            }),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "发起失败：{}",
        started["error"]
    );
    assert!(
        !started.to_string().contains(&api_hash),
        "响应回显了 api_hash"
    );
    let login_id = started["login_id"]
        .as_str()
        .expect("缺 login_id")
        .to_owned();
    let mut svg = started["qr_svg"].as_str().expect("缺 qr_svg").to_owned();
    let mut generations = 1_u32;
    write_private(&html_path, qr_page(&svg, generations).as_bytes());
    println!("[QRC] started login_id_bytes=32 html=written");

    let (status, early) = call(
        &state,
        owner,
        Request::builder()
            .uri(format!("/api/v1/identities/login/qr/{login_id}"))
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await;
    let (status_fast, _) = call(
        &state,
        owner,
        Request::builder()
            .uri(format!("/api/v1/identities/login/qr/{login_id}"))
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{early}");
    assert_eq!(
        status_fast,
        StatusCode::TOO_MANY_REQUESTS,
        "2 秒内第二次轮询应当 429"
    );
    let other_user = AuthContext::new(tenant, OWNER + 1, TenantRole::Owner, AuthSource::Web);
    let (status_other, _) = call(
        &state,
        other_user,
        Request::builder()
            .uri(format!("/api/v1/identities/login/qr/{login_id}"))
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await;
    assert_eq!(
        status_other,
        StatusCode::NOT_FOUND,
        "别的用户看不到这条登录"
    );
    println!("[QRC] rate_limit=429 other_user=404");

    let deadline = Instant::now() + Duration::from_secs(9 * 60);
    let mut password_rejected = 0_u32;
    let mut password_needed = false;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let (status, body) = call(
            &state,
            owner,
            Request::builder()
                .uri(format!("/api/v1/identities/login/qr/{login_id}"))
                .body(Body::empty())
                .expect("构造请求失败"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        match body["status"].as_str() {
            Some("pending") => {
                let next = body["qr_svg"].as_str().expect("pending 缺 qr_svg");
                if next != svg {
                    svg = next.to_owned();
                    generations += 1;
                    write_private(&html_path, qr_page(&svg, generations).as_bytes());
                    println!("[QRC] pending generation={generations}");
                }
            }
            Some("awaiting_password") => {
                if !password_needed {
                    println!("[QRC] awaiting_password，等待 TGM_LIVE_2FA_PASSWORD_FILE");
                    password_needed = true;
                }
                let _ = std::fs::remove_file(&html_path);
                let Ok(password) = std::fs::read_to_string(&password_path) else {
                    continue;
                };
                std::fs::remove_file(&password_path).expect("删除密码文件失败");
                let (status, checked) = call(
                    &state,
                    owner,
                    json_request(
                        "POST",
                        "/api/v1/identities/login/password",
                        json!({ "login_id": login_id, "password": password.trim_end_matches(['\r', '\n']) }),
                    ),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{checked}");
                match checked["status"].as_str() {
                    Some("awaiting_password") => {
                        password_rejected += 1;
                        println!("[QRC] password=rejected count={password_rejected}");
                    }
                    Some("confirmed") => {
                        last = checked;
                        break;
                    }
                    other => panic!("提交密码后状态异常：{other:?} {checked}"),
                }
            }
            Some("confirmed") => {
                last = body;
                break;
            }
            other => panic!("登录未成功：{other:?} {body}"),
        }
    }
    let _ = std::fs::remove_file(&html_path);
    assert_eq!(last["status"], "confirmed", "等待扫码超时");
    let identity_id = last["identity"]["id"].as_i64().expect("缺 identity.id");
    assert_eq!(last["identity"]["kind"], "mtproto");
    assert_eq!(last["identity"]["status"], "offline");

    let (status, gone) = call(
        &state,
        owner,
        Request::builder()
            .uri(format!("/api/v1/identities/login/qr/{login_id}"))
            .body(Body::empty())
            .expect("构造请求失败"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "终结后的 login_id 应当 404：{gone}"
    );

    let row: (i32, Option<i32>, Option<Vec<u8>>, String) = sqlx::query_as(
        "SELECT octet_length(s.secret_ciphertext), s.api_id, s.api_hash_ciphertext, s.kek_id
           FROM identity_secrets s WHERE s.identity_id = $1",
    )
    .bind(identity_id)
    .fetch_one(admin.raw())
    .await
    .expect("读凭据行失败");
    assert_eq!(row.1, Some(api_id));
    assert!(row.2.is_none(), "api_hash 不该落库");
    let consents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenant_consents
          WHERE tenant_id = $1 AND consent_type = 'credential_custody' AND text_version = $2
            AND user_id = $3 AND user_agent = 'tgm-live-acceptance'",
    )
    .bind(tenant)
    .bind(CUSTODY_CONSENT_TEXT_VERSION)
    .bind(OWNER)
    .fetch_one(admin.raw())
    .await
    .expect("读同意失败");
    assert_eq!(consents, 1);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.create' AND resource_id = $2
            AND source = 'web' AND NOT (after::text LIKE '%' || $3 || '%')",
    )
    .bind(tenant)
    .bind(identity_id)
    .bind(&api_hash)
    .fetch_one(admin.raw())
    .await
    .expect("读审计失败");
    assert_eq!(audits, 1);

    let identity: IdentityId = sqlx::query_scalar("SELECT id FROM identities WHERE id = $1")
        .bind(identity_id)
        .fetch_one(admin.raw())
        .await
        .expect("读身份失败");
    let (stop, shutdown) = watch::channel(false);
    let pool = app.clone();
    let worker = tokio::spawn(async move {
        run_identity(
            &pool,
            &kek(),
            MtprotoTarget {
                tenant,
                identity,
                api_id,
            },
            "qr-control-live",
            MtprotoWorkerConfig::default(),
            None,
            shutdown,
        )
        .await
    });
    let online_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status: String = sqlx::query_scalar("SELECT status FROM identities WHERE id = $1")
            .bind(identity_id)
            .fetch_one(admin.raw())
            .await
            .expect("读状态失败");
        if status == "online" {
            break;
        }
        assert!(
            Instant::now() < online_deadline,
            "worker 未能用落库的 session 上线（status={status}）"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    stop.send(true).expect("发停止信号失败");
    let exit = worker.await.expect("worker 任务失败").expect("worker 出错");
    assert_eq!(exit, IdentityExit::Shutdown);
    println!(
        "[QRC] confirmed generations={generations} 2fa={password_needed} password_rejected={password_rejected} \
         ciphertext_bytes={} api_id_set=true api_hash_stored=false kek_id={} consent=1 audit=1 \
         worker_online=true exit={exit:?}",
        row.0, row.3
    );
}
