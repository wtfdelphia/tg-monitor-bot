mod support;

use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tgm_app::bot_console::{TelegramBotApiClient, TelegramBotApiError};
use tgm_app::target_probe::{ProbeChat, ProbeMember, evaluate};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::store_identity_secret;
use tgm_db::repo::{
    TargetChannelCreateInput, TargetChannelUpdateInput, create_target_channel,
    update_target_channel,
};
use tgm_db::target_probe::{
    ProbeOutcome, ProbeSuccess, claim_probe, finish_probe, read_probe, request_probe,
};

fn success() -> ProbeOutcome {
    ProbeOutcome::Success(ProbeSuccess {
        can_send: true,
        content_protected: false,
        bot_role: "administrator".to_owned(),
    })
}

#[test]
fn permissions_fail_closed_and_keep_target_protection_separate()
-> Result<(), Box<dyn std::error::Error>> {
    let cases = [
        (
            "channel",
            "administrator",
            Some(true),
            None,
            None,
            Some(true),
        ),
        (
            "channel",
            "administrator",
            Some(false),
            None,
            None,
            Some(false),
        ),
        ("channel", "administrator", None, None, None, None),
        ("channel", "member", None, None, None, Some(false)),
        ("channel", "left", None, None, None, Some(false)),
        ("channel", "kicked", None, None, None, Some(false)),
        ("channel", "creator", None, None, None, Some(true)),
        ("group", "administrator", None, None, None, Some(true)),
        ("group", "member", None, None, Some(true), Some(true)),
        ("group", "member", None, None, Some(false), Some(false)),
        (
            "supergroup",
            "restricted",
            None,
            Some(true),
            Some(true),
            Some(true),
        ),
        (
            "supergroup",
            "restricted",
            None,
            Some(false),
            Some(true),
            Some(false),
        ),
        ("supergroup", "restricted", None, None, Some(true), None),
        ("private", "administrator", Some(true), None, None, None),
        ("channel", "future_role", Some(true), None, None, None),
    ];
    for (kind, role, post, member, send, expected) in cases {
        let chat: ProbeChat = serde_json::from_value(
            json!({"id": -1001,"type":kind,"has_protected_content":true,"permissions":{"can_send_messages":send}}),
        )?;
        let member: ProbeMember = serde_json::from_value(
            json!({"user":{"id":123456,"is_bot":true},"status":role,"can_post_messages":post,"is_member":member,"can_send_messages":send}),
        )?;
        let result = evaluate(chat, member, -1001, 123456);
        assert_eq!(
            result.as_ref().map(|value| value.can_send),
            expected,
            "{kind}/{role}"
        );
        if let Some(result) = result {
            assert!(result.content_protected);
        }
    }
    for (chat_id, user_id, is_bot) in [
        (-1002, 123456, true),
        (-1001, 123457, true),
        (-1001, 123456, false),
    ] {
        let chat = serde_json::from_value(json!({"id":chat_id,"type":"channel"}))?;
        let member = serde_json::from_value(
            json!({"user":{"id":user_id,"is_bot":is_bot},"status":"administrator","can_post_messages":true}),
        )?;
        assert!(evaluate(chat, member, -1001, 123456).is_none());
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn target_probe_claim_route_rotation_and_cooldown_are_fenced(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (app, admin) = support::pools(admin).await;
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await?;
    assert_eq!(role, "app_user");
    let tenant = support::seed_tenant(&admin, "probe", 90101).await;
    let other = support::seed_tenant(&admin, "probe-other", 90102).await;
    let identity = support::seed_identity(&admin, tenant).await;
    let input = TargetChannelCreateInput {
        tg_chat_id: -10090101,
        title: None,
        sender_identity_id: identity,
    };
    let target = create_target_channel(&app, tenant, input.clone()).await?;
    let second = create_target_channel(
        &app,
        tenant,
        TargetChannelCreateInput {
            tg_chat_id: -10090102,
            ..input
        },
    )
    .await?;
    assert!(read_probe(&app, other, target.id).await?.is_none());
    assert!(request_probe(&app, other, target.id).await?.is_none());
    assert!(
        claim_probe(&app, other, "other".to_owned())
            .await?
            .is_none()
    );
    let initial = read_probe(&app, tenant, target.id)
        .await?
        .ok_or("probe missing")?;
    assert_eq!(initial.state, "PENDING");
    assert_eq!(
        request_probe(&app, tenant, target.id)
            .await?
            .ok_or("missing")?
            .generation,
        initial.generation
    );
    let (first, concurrent) = tokio::join!(
        claim_probe(&app, tenant, "first".to_owned()),
        claim_probe(&app, tenant, "concurrent".to_owned())
    );
    let first = first?;
    let concurrent = concurrent?;
    assert_eq!(
        usize::from(first.is_some()) + usize::from(concurrent.is_some()),
        1
    );
    let claim = first.or(concurrent).ok_or("claim missing")?;
    let claimed_id = claim.id;
    assert!(finish_probe(&app, tenant, claim, success()).await?);
    let ready = read_probe(&app, tenant, claimed_id)
        .await?
        .ok_or("missing")?;
    assert_eq!(ready.can_send, Some(true));
    let same = update_target_channel(
        &app,
        tenant,
        claimed_id,
        TargetChannelUpdateInput {
            tg_chat_id: Some(if claimed_id == target.id {
                -10090101
            } else {
                -10090102
            }),
            title: Some(Some("renamed".to_owned())),
            sender_identity_id: Some(identity.get()),
            enabled: Some(false),
        },
    )
    .await?;
    assert_eq!(same.can_send, Some(true));
    assert_eq!(same.probe_generation, ready.generation);
    request_probe(&app, tenant, claimed_id).await?;
    let stale = claim_probe(&app, tenant, "stale".to_owned())
        .await?
        .ok_or("missing")?;
    let stale_id = stale.id;
    let original_chat = stale.tg_chat_id;
    for chat in [original_chat - 100, original_chat] {
        update_target_channel(
            &app,
            tenant,
            stale_id,
            TargetChannelUpdateInput {
                tg_chat_id: Some(chat),
                ..Default::default()
            },
        )
        .await?;
    }
    assert!(!finish_probe(&app, tenant, stale, success()).await?);
    assert!(
        claim_probe(&app, tenant, "before-reservation-expiry".to_owned())
            .await?
            .is_none()
    );
    sqlx::query("UPDATE identities SET target_probe_not_before = '-infinity' WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await?;
    let claim = claim_probe(&app, tenant, "rate-limit".to_owned())
        .await?
        .ok_or("missing")?;
    let claim_id = claim.id;
    assert!(
        finish_probe(
            &app,
            tenant,
            claim,
            ProbeOutcome::Failure {
                code: "RATE_LIMITED",
                retry_seconds: Some(3600),
                rate_limited: true
            }
        )
        .await?
    );
    for id in [target.id, second.id, claim_id] {
        request_probe(&app, tenant, id).await?;
    }
    assert!(
        claim_probe(&app, tenant, "cooldown".to_owned())
            .await?
            .is_none()
    );
    let kek = EnvKek::new(
        Profile::Development,
        "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
    )?;
    store_identity_secret(
        &app,
        &kek,
        tenant,
        identity,
        &Secret::new(b"123456:fake-only".to_vec()),
    )
    .await?;
    assert!(
        claim_probe(&app, tenant, "rotation-cannot-bypass".to_owned())
            .await?
            .is_none()
    );
    sqlx::query("UPDATE identities SET target_probe_not_before = '-infinity' WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await?;
    let stale = claim_probe(&app, tenant, "old-token".to_owned())
        .await?
        .ok_or("missing")?;
    store_identity_secret(
        &app,
        &kek,
        tenant,
        identity,
        &Secret::new(b"123456:rotated-fake".to_vec()),
    )
    .await?;
    assert!(!finish_probe(&app, tenant, stale, success()).await?);
    sqlx::query("UPDATE identities SET target_probe_not_before = '-infinity' WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await?;
    let abandoned = claim_probe(&app, tenant, "abandoned".to_owned())
        .await?
        .ok_or("missing")?;
    sqlx::query("UPDATE target_channels SET probe_expires_at = clock_timestamp() - interval '1 second' WHERE tenant_id = $1 AND id = $2").bind(tenant).bind(abandoned.id).execute(admin.raw()).await?;
    sqlx::query("UPDATE identities SET target_probe_not_before = '-infinity' WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await?;
    let recovered = claim_probe(&app, tenant, "recovered".to_owned())
        .await?
        .ok_or("missing")?;
    assert!(!finish_probe(&app, tenant, abandoned, success()).await?);
    let recovered_id = recovered.id;
    assert!(finish_probe(&app, tenant, recovered, success()).await?);
    sqlx::query("UPDATE identities SET status = 'unauthorized' WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await?;
    let invalidated = read_probe(&app, tenant, recovered_id)
        .await?
        .ok_or("missing")?;
    assert_eq!(invalidated.state, "PENDING");
    assert_eq!(invalidated.can_send, None);
    Ok(())
}

#[tokio::test]
async fn bot_api_probe_only_reads_and_preserves_retry_after()
-> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route("/bot123456:fake/getChat", post(|| async { Json(json!({"ok":true,"result":{"id":-1001,"type":"channel","has_protected_content":true}})) }))
        .route("/bot123456:fake/getChatMember", post(|Json(payload): Json<Value>| async move {
            assert_eq!(payload, json!({"chat_id":-1001,"user_id":123456}));
            Json(json!({"ok":true,"result":{"user":{"id":123456,"is_bot":true},"status":"administrator","can_post_messages":true}}))
        }))
        .route("/bot123456:rate/getChat", post(|| async { Json(json!({"ok":false,"error_code":429,"description":"do not disclose this", "parameters":{"retry_after":123}})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = TelegramBotApiClient::new(
        Arc::new(Secret::new("123456:fake".to_owned())),
        format!("http://{address}"),
    )?;
    let result = client.probe_target(-1001, 123456).await?.ok_or("missing")?;
    assert!(result.can_send && result.content_protected);
    let client = TelegramBotApiClient::new(
        Arc::new(Secret::new("123456:rate".to_owned())),
        format!("http://{address}"),
    )?;
    assert!(matches!(
        client.probe_target(-1001, 123456).await,
        Err(TelegramBotApiError::Rejected {
            status: 429,
            retry_after: Some(123),
            ..
        })
    ));
    server.abort();
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn probe_http_roles_csrf_and_tenant_visibility(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tgm_app::control::{ControlPlaneState, router};
    use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
    use tower::ServiceExt;
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "probe-http", 90111).await;
    let other = support::seed_tenant(&admin, "probe-http-other", 90112).await;
    let identity = support::seed_identity(&admin, tenant).await;
    let target = create_target_channel(
        &app,
        tenant,
        TargetChannelCreateInput {
            tg_chat_id: -10090111,
            title: None,
            sender_identity_id: identity,
        },
    )
    .await?;
    let route = format!("/api/v1/targets/{}/probe", target.id);
    for (method, tenant_id, role, expected) in [
        ("GET", tenant, TenantRole::Viewer, StatusCode::OK),
        ("POST", tenant, TenantRole::Viewer, StatusCode::FORBIDDEN),
        ("POST", tenant, TenantRole::Member, StatusCode::FORBIDDEN),
        ("POST", tenant, TenantRole::Admin, StatusCode::ACCEPTED),
        ("POST", tenant, TenantRole::Owner, StatusCode::ACCEPTED),
        ("GET", other, TenantRole::Owner, StatusCode::NOT_FOUND),
        ("POST", other, TenantRole::Owner, StatusCode::NOT_FOUND),
    ] {
        let mut request = Request::builder()
            .method(method)
            .uri(&route)
            .body(Body::empty())?;
        request
            .extensions_mut()
            .insert(AuthContext::new(tenant_id, 90111, role, AuthSource::Web));
        let response = router(ControlPlaneState::new(app.clone()))
            .oneshot(request)
            .await?;
        assert_eq!(response.status(), expected, "{method}/{role:?}");
        if expected.is_success() {
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .ok_or("missing no-store")?,
                "no-store"
            );
            let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
            assert_eq!(body["state"], "PENDING");
            assert_eq!(body["can_send"], Value::Null);
            assert!(body.get("can_forward").is_none());
        }
    }
    let auth = support::auth_pool(&admin).await;
    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, 90111, 'TENANT_OWNER')",
    )
    .bind(tenant)
    .execute(admin.raw())
    .await?;
    let session = tgm_db::repo::issue_web_session_for_user(
        &auth,
        90111,
        "127.0.0.1",
        None,
        tgm_db::repo::WebSessionLoginMethod::Oidc,
    )
    .await?
    .ok_or("session missing")?;
    use base64::Engine as _;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&session.session_id);
    let csrf = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&session.csrf_token);
    let state = ControlPlaneState::with_auth_pool(app.clone(), auth);
    let request = Request::builder()
        .method("POST")
        .uri(&route)
        .header("cookie", format!("tgm_session={token}"))
        .body(Body::empty())?;
    assert_eq!(
        router(state.clone()).oneshot(request).await?.status(),
        StatusCode::FORBIDDEN
    );
    let request = Request::builder()
        .method("POST")
        .uri(&route)
        .header("cookie", format!("tgm_session={token}; tgm_csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .body(Body::empty())?;
    assert_eq!(
        router(state).oneshot(request).await?.status(),
        StatusCode::ACCEPTED
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn encrypted_credentials_drive_only_readonly_probe(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "probe-runtime", 90121).await;
    let identity = support::seed_identity(&admin, tenant).await;
    sqlx::query("UPDATE identities SET tg_user_id = 123456 WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await?;
    let kek = EnvKek::new(
        Profile::Development,
        "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
    )?;
    store_identity_secret(
        &app,
        &kek,
        tenant,
        identity,
        &Secret::new(b"123456:fixture".to_vec()),
    )
    .await?;
    let target = create_target_channel(
        &app,
        tenant,
        TargetChannelCreateInput {
            tg_chat_id: -10090121,
            title: None,
            sender_identity_id: identity,
        },
    )
    .await?;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let bot = Router::new().fallback(post(move |request: axum::extract::Request| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(request.uri().path().ends_with("/getChat"));
            Json(json!({"ok":false,"error_code":429,"description":"secret-description-sentinel","parameters":{"retry_after":300}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, bot).await;
    });
    let claim = claim_probe(&app, tenant, "runtime".to_owned())
        .await?
        .ok_or("missing")?;
    let outcome =
        tgm_app::target_probe::execute(&app, &kek, tenant, &claim, &format!("http://{address}"))
            .await;
    assert!(matches!(
        outcome,
        ProbeOutcome::Failure {
            code: "RATE_LIMITED",
            retry_seconds: Some(300),
            rate_limited: true
        }
    ));
    assert!(finish_probe(&app, tenant, claim, outcome).await?);
    let result = read_probe(&app, tenant, target.id)
        .await?
        .ok_or("missing")?;
    assert_eq!(result.error_code.as_deref(), Some("RATE_LIMITED"));
    assert_eq!(result.can_send, None);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn retry_budget_and_database_state_constraints_are_enforced(
    admin: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "probe-budget", 90131).await;
    let identity = support::seed_identity(&admin, tenant).await;
    let target = create_target_channel(
        &app,
        tenant,
        TargetChannelCreateInput {
            tg_chat_id: -10090131,
            title: None,
            sender_identity_id: identity,
        },
    )
    .await?;
    let permission: bool = sqlx::query_scalar("SELECT has_column_privilege(current_user, 'identities', 'target_probe_not_before', 'UPDATE') AND NOT has_column_privilege(current_user, 'identities', 'tenant_id', 'UPDATE') AND NOT has_table_privilege(current_user, 'identities', 'UPDATE')").fetch_one(&app).await?;
    assert!(permission);
    for attempt in 1..=3 {
        let claim = claim_probe(&app, tenant, format!("attempt-{attempt}"))
            .await?
            .ok_or("claim missing")?;
        assert_eq!(claim.attempts, attempt);
        assert!(
            finish_probe(
                &app,
                tenant,
                claim,
                ProbeOutcome::Failure {
                    code: "TELEGRAM_UNAVAILABLE",
                    retry_seconds: Some(30),
                    rate_limited: false
                }
            )
            .await?
        );
        let result = read_probe(&app, tenant, target.id)
            .await?
            .ok_or("missing")?;
        assert_eq!(result.state, if attempt < 3 { "PENDING" } else { "FAILED" });
        assert_eq!(result.can_send, None);
        if attempt < 3 {
            assert!(
                claim_probe(&app, tenant, "too-early".to_owned())
                    .await?
                    .is_none()
            );
            sqlx::query("UPDATE target_channels SET probe_due_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2").bind(tenant).bind(target.id).execute(admin.raw()).await?;
        }
    }
    let invalid = sqlx::query(
        "UPDATE target_channels SET probe_state = 'SUCCEEDED' WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(target.id)
    .execute(admin.raw())
    .await;
    assert!(invalid.is_err());
    let generation = read_probe(&app, tenant, target.id)
        .await?
        .ok_or("missing")?
        .generation;
    assert_eq!(
        request_probe(&app, tenant, target.id)
            .await?
            .ok_or("missing")?
            .generation,
        generation + 1
    );
    sqlx::query("UPDATE target_channels SET probe_due_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2").bind(tenant).bind(target.id).execute(admin.raw()).await?;
    let claim = claim_probe(&app, tenant, "missing-secret".to_owned())
        .await?
        .ok_or("missing")?;
    let kek = EnvKek::new(
        Profile::Development,
        "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
    )?;
    let outcome =
        tgm_app::target_probe::execute(&app, &kek, tenant, &claim, "http://127.0.0.1:1").await;
    assert!(matches!(
        outcome,
        ProbeOutcome::Failure {
            code: "CREDENTIAL_MISSING",
            retry_seconds: None,
            ..
        }
    ));
    assert!(finish_probe(&app, tenant, claim, outcome).await?);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：仅只读自检；需要权限受限的当前测试 Bot Token 文件与预期结果"]
async fn live_target_probe(admin: PgPool) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let token_path = std::env::var("TGM_TARGET_PROBE_TOKEN_FILE")?;
    let metadata = std::fs::symlink_metadata(&token_path)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("Token 必须使用仅本人可读写的常规文件".into());
    }
    let raw = Secret::new(std::fs::read_to_string(token_path)?);
    let token = Secret::new(raw.expose().trim().to_owned());
    let bot_id =
        tgm_app::delivery_runtime::bot_id_from_token(token.expose()).ok_or("Bot Token 格式无效")?;
    let chat_id: i64 = std::env::var("TGM_TARGET_PROBE_CHAT_ID")?.parse()?;
    let expected_send: bool = std::env::var("TGM_TARGET_PROBE_EXPECT_CAN_SEND")?.parse()?;
    let expected_protected: bool = std::env::var("TGM_TARGET_PROBE_EXPECT_PROTECTED")?.parse()?;
    let expected_role = std::env::var("TGM_TARGET_PROBE_EXPECT_ROLE")?;
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "live-target-probe", 90141).await;
    let identity = support::seed_identity(&admin, tenant).await;
    sqlx::query("UPDATE identities SET tg_user_id = $2 WHERE id = $1")
        .bind(identity)
        .bind(bot_id)
        .execute(admin.raw())
        .await?;
    let kek = EnvKek::new(
        Profile::Development,
        "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
    )?;
    store_identity_secret(
        &app,
        &kek,
        tenant,
        identity,
        &Secret::new(token.expose().as_bytes().to_vec()),
    )
    .await?;
    let target = create_target_channel(
        &app,
        tenant,
        TargetChannelCreateInput {
            tg_chat_id: chat_id,
            title: None,
            sender_identity_id: identity,
        },
    )
    .await?;
    let claim = claim_probe(&app, tenant, "live-readonly".to_owned())
        .await?
        .ok_or("claim missing")?;
    let outcome = tgm_app::target_probe::execute(
        &app,
        &kek,
        tenant,
        &claim,
        tgm_app::bot_console::TELEGRAM_BOT_API_DEFAULT_BASE_URL,
    )
    .await;
    assert!(finish_probe(&app, tenant, claim, outcome).await?);
    let result = read_probe(&app, tenant, target.id)
        .await?
        .ok_or("missing")?;
    assert_eq!(
        result.state, "SUCCEEDED",
        "只读自检未完成：{:?}",
        result.error_code
    );
    assert_eq!(result.can_send, Some(expected_send));
    assert_eq!(result.content_protected, Some(expected_protected));
    assert_eq!(result.bot_role.as_deref(), Some(expected_role.as_str()));
    println!(
        "真实只读目标自检完成：can_send={expected_send} content_protected={expected_protected} bot_role={expected_role}"
    );
    tgm_db::tenant::tenant_tx(&app, tenant, |conn| {
        Box::pin(async move {
            sqlx::query("DELETE FROM target_channels WHERE tenant_id = $1")
                .bind(tenant)
                .execute(&mut *conn)
                .await?;
            sqlx::query("DELETE FROM identities WHERE tenant_id = $1")
                .bind(tenant)
                .execute(conn)
                .await?;
            Ok(())
        })
    })
    .await?;
    Ok(())
}
