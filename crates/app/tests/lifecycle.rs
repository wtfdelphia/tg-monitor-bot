mod support;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tgm_app::control::{ControlPlaneState, router};
use tgm_core::auth::{AuthContext, AuthSource, TenantRole};
use tgm_db::lifecycle;
use tower::ServiceExt;

#[sqlx::test(migrations = "../../migrations-test")]
async fn existing_cookie_and_cached_bot_context_respect_tenant_state(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (app, admin) = support::pools(injected).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT current_user")
            .fetch_one(&app)
            .await?,
        "app_user"
    );
    let auth = support::auth_pool(&admin).await;
    let operator = PgPoolOptions::new()
        .connect_with(
            app.connect_options()
                .as_ref()
                .clone()
                .username("tenant_lifecycle")
                .password("tlpw"),
        )
        .await?;
    lifecycle::check_connection(&operator).await?;
    let tenant = support::seed_tenant(&admin, "http-lifecycle", 97201).await;
    sqlx::query(
        "INSERT INTO tenant_members(tenant_id,user_id,role) VALUES($1,97201,'TENANT_OWNER')",
    )
    .bind(tenant.get())
    .execute(admin.raw())
    .await?;
    let session = [31_u8; 32];
    let csrf = [32_u8; 32];
    sqlx::query("INSERT INTO web_sessions(tenant_id,id,user_id,csrf_token,expires_at) VALUES($1,$2,97201,$3,now()+interval '1 hour')")
        .bind(tenant.get()).bind(session.as_slice()).bind(csrf.as_slice()).execute(admin.raw()).await?;
    let application = router(ControlPlaneState::with_auth_pool(app.clone(), auth));
    let cookie = format!(
        "tgm_session={}; tgm_csrf={}",
        URL_SAFE_NO_PAD.encode(session),
        URL_SAFE_NO_PAD.encode(csrf)
    );
    let request = |method: &str, path: &str| -> Result<Request<Body>, axum::http::Error> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("cookie", &cookie)
            .header("x-csrf-token", URL_SAFE_NO_PAD.encode(csrf))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
    };
    assert_eq!(
        application
            .clone()
            .oneshot(request("GET", "/auth/me")?)
            .await?
            .status(),
        StatusCode::OK
    );
    lifecycle::transition(&operator, tenant.get(), true).await?;
    assert_eq!(
        application
            .clone()
            .oneshot(request("GET", "/api/v1/keywords")?)
            .await?
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        application
            .clone()
            .oneshot(request("POST", "/api/v1/keywords")?)
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut cached = Request::builder()
        .method("POST")
        .uri("/api/v1/keywords")
        .header("content-type", "application/json")
        .body(Body::from("{}"))?;
    cached.extensions_mut().insert(AuthContext::new(
        tenant,
        97201,
        TenantRole::Owner,
        AuthSource::Bot,
    ));
    assert_eq!(
        application.clone().oneshot(cached).await?.status(),
        StatusCode::FORBIDDEN
    );
    let contexts =
        tgm_app::bot_gateway::list_member_contexts(&support::auth_pool(&admin).await, 97201)
            .await?;
    assert_eq!(contexts.len(), 1);
    assert_eq!(
        application
            .clone()
            .oneshot(request("GET", "/auth/sessions")?)
            .await?
            .status(),
        StatusCode::OK
    );
    lifecycle::transition(&operator, tenant.get(), false).await?;
    assert_eq!(
        application
            .clone()
            .oneshot(request("GET", "/api/v1/keywords")?)
            .await?
            .status(),
        StatusCode::OK
    );
    lifecycle::transition(&operator, tenant.get(), true).await?;
    lifecycle::logical_delete(&operator, tenant.get()).await?;
    assert!(matches!(
        tgm_app::bot_gateway::list_member_contexts(&support::auth_pool(&admin).await, 97201).await,
        Err(tgm_core::error::AppError::NotInvited)
    ));
    assert_eq!(
        application
            .clone()
            .oneshot(request("GET", "/api/v1/keywords")?)
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        application
            .oneshot(request("POST", "/auth/logout")?)
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cancelled_command_keeps_committed_pause_and_can_resume(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (app, admin) = support::pools(injected).await;
    let tenant = support::seed_tenant(&admin, "cancel-lifecycle", 97202).await;
    let operator = PgPoolOptions::new()
        .connect_with(
            app.connect_options()
                .as_ref()
                .clone()
                .username("tenant_lifecycle")
                .password("tlpw"),
        )
        .await?;
    lifecycle::transition(&operator, tenant.get(), true).await?;
    assert!(matches!(
        tgm_app::lifecycle::run(&operator, tenant.get(), true, std::future::ready(())).await,
        Err(lifecycle::LifecycleError::Draining)
    ));
    assert!(!lifecycle::is_active(&app, tenant).await?);
    assert_eq!(
        tgm_app::lifecycle::run(&operator, tenant.get(), true, std::future::pending()).await?,
        0
    );
    tgm_app::lifecycle::run(&operator, tenant.get(), false, std::future::pending()).await?;
    assert!(lifecycle::is_active(&app, tenant).await?);
    Ok(())
}
