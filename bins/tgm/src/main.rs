//! 单二进制多 subcommand（eng/00 §一）。7 个组件共用一个 bin，
//! 靠 `--role` 区分进程角色 —— 首发部署形态就是单机跑多进程（ops/00）。
//!
//! 迁移、RLS 审计、OpenAPI 导出以及已接入的 HTTP/worker 角色在这里汇合；
//! 尚未接入的角色会明确失败，不伪装成成功启动。

use clap::{Parser, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::sync::Arc;
use tgm_core::config::Role as CoreRole;
use tokio::sync::watch;

mod kms_kek;

#[derive(Parser)]
#[command(name = "tgm", version, about = "Telegram 多租户内容监控与分发平台")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动一个进程角色
    Serve {
        #[arg(long)]
        role: Role,
    },
    /// 跑数据库迁移（与 `sqlx migrate run --source migrations` 同一份文件）
    Migrate,
    /// RLS 静态审计：主查询 + eng/04 §二 + §三（C3 除外，它比对文件不查库）
    AuditRls,
    CleanupAuth {
        #[arg(long, value_enum)]
        category: CleanupCategory,
    },
    CleanupTenant {
        #[arg(long, value_enum)]
        category: TenantCleanupCategory,
    },
    CleanupShared,
    CleanupPlatform,
    TenantLifecycle {
        #[command(subcommand)]
        action: LifecycleAction,
    },
    /// 导出 OpenAPI 契约；`--check` 则与仓库内的 openapi.json 比对
    Openapi {
        #[arg(long)]
        check: bool,
    },
}

#[derive(clap::Subcommand)]
enum LifecycleAction {
    Suspend {
        #[arg(long)]
        tenant_id: i64,
    },
    Resume {
        #[arg(long)]
        tenant_id: i64,
    },
    Delete {
        #[arg(long)]
        tenant_id: i64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CleanupCategory {
    Challenges,
    WebSessions,
    BotSessions,
}

#[derive(Clone, Copy, ValueEnum)]
enum TenantCleanupCategory {
    TenantNotifications,
    UnmatchedEvents,
    TenantEvents,
    DeliveryTasks,
    LoginSessions,
    NotifyDedup,
    BotConsoleUpdates,
    DeliveryLogs,
    AuditLogs,
    SystemLogs,
    RuleVersions,
}

impl From<TenantCleanupCategory> for tgm_db::tenant_retention::Category {
    fn from(category: TenantCleanupCategory) -> Self {
        match category {
            TenantCleanupCategory::TenantNotifications => Self::TenantNotifications,
            TenantCleanupCategory::UnmatchedEvents => Self::UnmatchedEvents,
            TenantCleanupCategory::TenantEvents => Self::TenantEvents,
            TenantCleanupCategory::DeliveryTasks => Self::DeliveryTasks,
            TenantCleanupCategory::LoginSessions => Self::LoginSessions,
            TenantCleanupCategory::NotifyDedup => Self::NotifyDedup,
            TenantCleanupCategory::BotConsoleUpdates => Self::BotConsoleUpdates,
            TenantCleanupCategory::DeliveryLogs => Self::DeliveryLogs,
            TenantCleanupCategory::AuditLogs => Self::AuditLogs,
            TenantCleanupCategory::SystemLogs => Self::SystemLogs,
            TenantCleanupCategory::RuleVersions => Self::RuleVersions,
        }
    }
}

impl From<CleanupCategory> for tgm_db::auth_retention::Category {
    fn from(category: CleanupCategory) -> Self {
        match category {
            CleanupCategory::Challenges => Self::Challenges,
            CleanupCategory::WebSessions => Self::WebSessions,
            CleanupCategory::BotSessions => Self::BotSessions,
        }
    }
}

/// 角色取值与 eng/00 §一 的 `tgm serve --role` 列表一一对应。
#[derive(Clone, ValueEnum)]
enum Role {
    ControlPlane,
    BotGateway,
    MtprotoWorker,
    Fanout,
    Rule,
    Delivery,
}

impl Role {
    fn as_str(&self) -> &'static str {
        match self {
            Role::ControlPlane => "control-plane",
            Role::BotGateway => "bot-gateway",
            Role::MtprotoWorker => "mtproto-worker",
            Role::Fanout => "fanout",
            Role::Rule => "rule",
            Role::Delivery => "delivery",
        }
    }
}

/// clap 的角色 → 领域层的角色。**全仓库唯一一处映射**。
///
/// 两份枚举的存在理由在 `tgm_core::config::Role` 的注释里（core 不依赖 clap，
/// 而必填集是领域知识）。代价就是这里：加一个角色时若漏了这一处，编译器会报
/// non-exhaustive match —— 这是为什么不写 `_ => ` 兜底。
impl From<&Role> for CoreRole {
    fn from(r: &Role) -> Self {
        match r {
            Role::ControlPlane => CoreRole::ControlPlane,
            Role::BotGateway => CoreRole::BotGateway,
            Role::MtprotoWorker => CoreRole::MtprotoWorker,
            Role::Fanout => CoreRole::Fanout,
            Role::Rule => CoreRole::Rule,
            Role::Delivery => CoreRole::Delivery,
        }
    }
}

/// 按 `TGM_KEK_BACKEND` 构造 KEK。预检已保证变量组合合法，这里的报错是兜底。
async fn build_kek(
    profile: tgm_core::profile::Profile,
) -> anyhow::Result<Arc<dyn tgm_core::kek::Kek>> {
    match std::env::var("TGM_KEK_BACKEND").as_deref() {
        Ok("env") => {
            let raw = std::env::var("TGM_LOCAL_KEK")
                .map_err(|_| anyhow::anyhow!("TGM_KEK_BACKEND=env 但缺少 TGM_LOCAL_KEK"))?;
            Ok(Arc::new(tgm_core::kek::EnvKek::new(profile, &raw)?))
        }
        Ok("kms") => {
            let key_arn = std::env::var("TGM_KMS_KEY_ARN")
                .map_err(|_| anyhow::anyhow!("TGM_KMS_KEY_ARN 缺失 —— 预检本该拦住"))?;
            let kek = kms_kek::KmsKek::from_arn(&key_arn)
                .await
                .map_err(|_| anyhow::anyhow!("KMS KEK 初始化失败"))?;
            Ok(Arc::new(kek))
        }
        _ => anyhow::bail!("TGM_KEK_BACKEND 只接受 kms 或 env"),
    }
}

async fn serve_control_plane(
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    // preflight 已解析并校验过同一个值，这里重读只为不把 profile 穿过 main 的分支。
    let profile = tgm_core::profile::Profile::parse(
        &std::env::var("TGM_PROFILE")
            .map_err(|_| anyhow::anyhow!("缺少 TGM_PROFILE —— 启动期校验本应先拦住"))?,
    )?;
    let app_url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 启动期校验本应先拦住"))?;
    let auth_url = std::env::var("TGM_DB_URL_AUTH")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_AUTH —— 控制面鉴权需要独立连接池"))?;
    let oidc_bot_id = std::env::var("TGM_OIDC_BOT_ID")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_OIDC_BOT_ID —— 启动期校验本应先拦住"))?;
    if oidc_bot_id.parse::<u64>().is_err() {
        return Err(anyhow::anyhow!("TGM_OIDC_BOT_ID 必须是正整数"));
    }
    let oidc_alg = std::env::var("TGM_OIDC_ALG")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_OIDC_ALG —— 启动期校验本应先拦住"))?;
    let oidc_alg = tgm_core::config::validate_oidc_alg(&oidc_alg)
        .map_err(|error| anyhow::anyhow!("OIDC 配置无效：{error}"))?;
    let oidc_redirect_url = std::env::var("TGM_OIDC_REDIRECT_URL")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_OIDC_REDIRECT_URL —— 启动期校验本应先拦住"))?;
    let oidc_client_secret = std::env::var("TGM_OIDC_CLIENT_SECRET")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_OIDC_CLIENT_SECRET —— 启动期校验本应先拦住"))?;
    if oidc_client_secret.is_empty() {
        return Err(anyhow::anyhow!("TGM_OIDC_CLIENT_SECRET 不能为空"));
    }
    let bot_secret = std::env::var("TGM_CONTROL_BOT_SECRET_TOKEN").map_err(|_| {
        anyhow::anyhow!(
            "缺少 TGM_CONTROL_BOT_SECRET_TOKEN —— 控制 Bot Webhook 必须启用 secret_token"
        )
    })?;
    let bot_token = std::env::var("TGM_CONTROL_BOT_TOKEN")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_CONTROL_BOT_TOKEN —— 控制 Bot 需要出站调用"))?;
    let bot_api = tgm_app::bot_console::TelegramBotApiClient::from_token(bot_token)
        .map_err(|error| anyhow::anyhow!("控制 Bot API 配置无效：{error}"))?;
    tgm_app::bot_gateway::verify_webhook_secret(&bot_secret, Some(&bot_secret))
        .map_err(|_| anyhow::anyhow!("TGM_CONTROL_BOT_SECRET_TOKEN 格式无效"))?;
    let bot_identity_id = std::env::var("TGM_CONTROL_BOT_IDENTITY_ID")
        .map_err(|_| {
            anyhow::anyhow!("缺少 TGM_CONTROL_BOT_IDENTITY_ID —— 控制 Bot Webhook 必须绑定身份")
        })?
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("TGM_CONTROL_BOT_IDENTITY_ID 必须是正整数"))?;
    if bot_identity_id <= 0 {
        return Err(anyhow::anyhow!("TGM_CONTROL_BOT_IDENTITY_ID 必须是正整数"));
    }
    let bot_username = std::env::var("TGM_CONTROL_BOT_USERNAME")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_CONTROL_BOT_USERNAME —— 扫码深链需要绑定 Bot"))?;
    if bot_username.is_empty()
        || bot_username.len() > 64
        || !bot_username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(anyhow::anyhow!(
            "TGM_CONTROL_BOT_USERNAME 必须是 1-64 位 ASCII 字母、数字或下划线"
        ));
    }
    let pool = sqlx::PgPool::connect(&app_url).await?;
    let auth_pool = sqlx::PgPool::connect(&auth_url).await?;
    let mut identity_tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id', '0', true)")
        .execute(&mut *identity_tx)
        .await?;
    let bot_identity_id = sqlx::query_scalar::<_, tgm_core::ids::IdentityId>(
        "SELECT id
           FROM identities
          WHERE id = $1
            AND kind = 'bot'
            AND tenant_id IS NULL",
    )
    .bind(bot_identity_id)
    .fetch_optional(&mut *identity_tx)
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!("TGM_CONTROL_BOT_IDENTITY_ID 必须指向 tenant_id IS NULL 的 bot 身份")
    })?;
    identity_tx.commit().await?;
    let raw_addr =
        std::env::var("TGM_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = raw_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("TGM_LISTEN_ADDR 无效 {raw_addr:?}: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(listen_addr = %addr, "control-plane HTTP 服务已启动");
    let bot_api = Arc::new(bot_api);
    // spec/08 §五 第 2 条：身份失去授权时经控制 Bot 通知租户 owner。
    // HTTP 服务优雅退出后再停它。
    let (notify_stop, notify_shutdown) = watch::channel(false);
    let notify_task = tokio::spawn(tgm_app::tenant_notify::run(
        pool.clone(),
        Arc::clone(&bot_api),
        tgm_app::tenant_notify::TenantNotifyConfig::default(),
        notify_shutdown,
    ));
    let served = tgm_app::control::serve_with_auth_and_bot_and_oidc(
        pool,
        auth_pool,
        tgm_app::bot_console::BotConsoleConfig {
            secret_token: bot_secret,
            bot_identity_id,
            bot_username,
            bot_api: Some(bot_api),
        },
        tgm_app::control::OidcConfig {
            bot_id: oidc_bot_id,
            client_secret: Arc::new(tgm_core::secret::Secret::new(oidc_client_secret)),
            signing_algorithm: oidc_alg,
            redirect_url: oidc_redirect_url,
            authorization_endpoint: tgm_app::control::OIDC_AUTHORIZATION_ENDPOINT.to_owned(),
            token_endpoint: tgm_app::control::OIDC_TOKEN_ENDPOINT.to_owned(),
            issuer: tgm_app::control::OIDC_ISSUER.to_owned(),
            jwks_endpoint: tgm_app::control::OIDC_JWKS_ENDPOINT.to_owned(),
        },
        tgm_app::control::CredentialIntake {
            kek: build_kek(profile).await?,
            bot_api_base_url: tgm_app::bot_console::TELEGRAM_BOT_API_DEFAULT_BASE_URL.to_owned(),
        },
        listener,
        signal,
    )
    .await;
    let _ = notify_stop.send(true);
    let _ = notify_task.await;
    served?;
    Ok(())
}

async fn serve_bot_gateway(
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app_url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 启动期校验本应先拦住"))?;
    let auth_url = std::env::var("TGM_DB_URL_AUTH")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_AUTH —— Bot 鉴权需要独立连接池"))?;
    let secret_token = std::env::var("TGM_DATA_BOT_SECRET_TOKEN").map_err(|_| {
        anyhow::anyhow!("缺少 TGM_DATA_BOT_SECRET_TOKEN —— Webhook 必须启用 secret_token")
    })?;
    let receiver_identity_id = std::env::var("TGM_DATA_BOT_IDENTITY_ID")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DATA_BOT_IDENTITY_ID —— Webhook 必须绑定接收身份"))?
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("TGM_DATA_BOT_IDENTITY_ID 必须是正整数"))?;
    if receiver_identity_id <= 0 {
        return Err(anyhow::anyhow!("TGM_DATA_BOT_IDENTITY_ID 必须是正整数"));
    }
    tgm_app::bot_gateway::verify_webhook_secret(&secret_token, Some(&secret_token))
        .map_err(|_| anyhow::anyhow!("TGM_DATA_BOT_SECRET_TOKEN 格式无效"))?;
    let pool = sqlx::PgPool::connect(&app_url).await?;
    let auth_pool = sqlx::PgPool::connect(&auth_url).await?;
    let raw_addr = std::env::var("TGM_BOT_GATEWAY_LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8081".to_owned());
    let addr: SocketAddr = raw_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("TGM_BOT_GATEWAY_LISTEN_ADDR 无效 {raw_addr:?}: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(listen_addr = %addr, "bot-gateway HTTP 服务已启动");
    let receiver_identity_id =
        sqlx::query_scalar::<_, tgm_core::ids::IdentityId>("SELECT $1::bigint")
            .bind(receiver_identity_id)
            .fetch_one(&pool)
            .await?;
    tgm_app::bot_gateway::serve(
        pool,
        auth_pool,
        secret_token,
        receiver_identity_id,
        listener,
        signal,
    )
    .await?;
    Ok(())
}

async fn serve_rule(
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app_url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 启动期校验本应先拦住"))?;
    let pool = sqlx::PgPool::connect(&app_url).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal_task = tokio::spawn(async move {
        signal.await;
        let _ = shutdown_tx.send(true);
    });
    let worker =
        tgm_app::pipeline::RuleWorker::new(pool, tgm_app::pipeline::RuleWorkerConfig::default());
    let result = worker.run(shutdown_rx).await;
    signal_task.abort();
    result?;
    Ok(())
}

/// mtproto-worker：持租约收消息落 `canonical_events`（spec/08 §四）。
///
/// worker_id 取主机名加进程号：同机重启换 id，旧进程的租约只能等过期或被接管，
/// 不会被新进程当成「自己的」续上。
/// 媒体中转的对象存储（`TGM_S3_*`，eng/01 §二）。development 下桶不存在就建，
/// 方便 compose 的 MinIO；其余 profile 要求部署方先建好桶。
async fn media_store() -> anyhow::Result<tgm_app::media_relay::MediaStore> {
    let config =
        tgm_app::media_relay::S3Config::from_env().map_err(|error| anyhow::anyhow!(error))?;
    let store = tgm_app::media_relay::MediaStore::new(&config);
    if std::env::var("TGM_PROFILE").as_deref() == Ok("development") {
        store.ensure_bucket().await?;
    }
    Ok(store)
}

async fn serve_mtproto_worker(
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let profile = tgm_core::profile::Profile::parse(
        &std::env::var("TGM_PROFILE")
            .map_err(|_| anyhow::anyhow!("缺少 TGM_PROFILE —— 启动期校验本应先拦住"))?,
    )?;
    let app_url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 启动期校验本应先拦住"))?;
    let pool = sqlx::PgPool::connect(&app_url).await?;
    let kek = build_kek(profile).await?;
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "unknown-host".to_owned());
    let worker_id = format!("{host}:{}", std::process::id());
    tracing::info!(worker_id = %worker_id, "mtproto-worker 已启动");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal_task = tokio::spawn(async move {
        signal.await;
        let _ = shutdown_tx.send(true);
    });
    let media_store = media_store().await?;
    let worker = tgm_app::mtproto_worker::MtprotoWorker::new(
        pool,
        kek,
        worker_id,
        tgm_app::mtproto_worker::MtprotoWorkerConfig::default(),
        Some(media_store),
    );
    let result = worker.run(shutdown_rx).await;
    signal_task.abort();
    result?;
    Ok(())
}

async fn serve_delivery(
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let profile = tgm_core::profile::Profile::parse(
        &std::env::var("TGM_PROFILE")
            .map_err(|_| anyhow::anyhow!("缺少 TGM_PROFILE —— 启动期校验本应先拦住"))?,
    )?;
    let app_url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 启动期校验本应先拦住"))?;
    let api_id = std::env::var("TGM_PLATFORM_API_ID")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_PLATFORM_API_ID —— 启动期校验本应先拦住"))?
        .parse::<i32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow::anyhow!("TGM_PLATFORM_API_ID 必须是正整数"))?;
    let api_hash = std::env::var("TGM_PLATFORM_API_HASH")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_PLATFORM_API_HASH —— 启动期校验本应先拦住"))?;
    if api_hash.len() != 32 || !api_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow::anyhow!(
            "TGM_PLATFORM_API_HASH 必须是 32 位十六进制"
        ));
    }
    let pool = sqlx::PgPool::connect(&app_url).await?;
    let kek = build_kek(profile).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal_task = tokio::spawn(async move {
        signal.await;
        let _ = shutdown_tx.send(true);
    });
    tracing::info!("delivery 已启动");
    let runtime = tgm_app::delivery_runtime::DeliveryRuntime::new(
        pool,
        kek,
        tgm_app::delivery_runtime::PlatformApp {
            api_id,
            api_hash: Arc::new(tgm_core::secret::Secret::new(api_hash)),
            media_store: media_store().await?,
        },
        tgm_app::delivery_runtime::DeliveryRuntimeConfig::default(),
    );
    let result = runtime.run(shutdown_rx).await;
    signal_task.abort();
    result?;
    Ok(())
}

async fn serve_fanout(
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app_url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 启动期校验本应先拦住"))?;
    let cleanup_url = std::env::var("TGM_DB_URL_MEDIA_CLEANUP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_MEDIA_CLEANUP —— 启动期校验本应先拦住"))?;
    let pool = sqlx::PgPool::connect(&app_url).await?;
    let cleanup_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&cleanup_url)
        .await?;
    tgm_db::media_cleanup::check_connection(&cleanup_pool)
        .await
        .map_err(|error| anyhow::anyhow!("媒体清理数据库连接校验失败：{error}"))?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal_task = tokio::spawn(async move {
        signal.await;
        let _ = shutdown_tx.send(true);
    });
    let worker = tgm_app::pipeline::FanoutWorker::new(
        pool,
        tgm_app::pipeline::FanoutWorkerConfig::default(),
    )
    .with_media_cleanup(media_store().await?, cleanup_pool);
    let result = worker.run(shutdown_rx).await;
    signal_task.abort();
    result?;
    Ok(())
}

fn openapi(check: bool) -> anyhow::Result<()> {
    const PUBLIC_PATH: &str = "openapi.json";
    const ADMIN_PATH: &str = "openapi-admin.json";
    let public = format!("{}\n", tgm_app::control::public_openapi_json()?);
    let admin = format!("{}\n", tgm_app::control::admin_openapi_json()?);
    if check {
        let existing_public = std::fs::read_to_string(PUBLIC_PATH)
            .map_err(|e| anyhow::anyhow!("读取 {PUBLIC_PATH} 失败: {e}"))?;
        let existing_admin = std::fs::read_to_string(ADMIN_PATH)
            .map_err(|e| anyhow::anyhow!("读取 {ADMIN_PATH} 失败: {e}"))?;
        if existing_public != public {
            anyhow::bail!("{PUBLIC_PATH} 与代码生成结果不一致");
        }
        if existing_admin != admin {
            anyhow::bail!("{ADMIN_PATH} 与代码生成结果不一致");
        }
        println!("tgm: OpenAPI 契约一致：{PUBLIC_PATH}, {ADMIN_PATH}");
        return Ok(());
    }
    std::fs::write(PUBLIC_PATH, public)?;
    std::fs::write(ADMIN_PATH, admin)?;
    println!("tgm: 已导出 OpenAPI 契约：{PUBLIC_PATH}, {ADMIN_PATH}");
    Ok(())
}

/// 跑迁移。ops/00 §2.1 的第 1 步：用 `TGM_DB_URL_OWNER`，跑完即结束。
///
/// **迁移文件在编译期嵌进二进制**（`sqlx::migrate!`），不在运行时读磁盘。
/// 理由是部署形态：那台机器上只有一个二进制，没有仓库，`--source migrations`
/// 无从指向。副作用刚好是 eng/00 §一 想要的那条保证 —— `tgm migrate` 与
/// `sqlx migrate run --source migrations` 指向同一份文件，由编译期路径保证，
/// 不靠人记得同步。
///
/// 这里**不做** eng/01 §4.2 的 PG 版本下限检查。那条是 `serve` 的启动期校验，
/// 而这条命令跑在业务进程之前：版本不够时该拦住的是流量，不是 DDL。
/// 真正的结构性把关是 §2.1 的第 2 步 `audit-rls`，它在这条之后立刻跑。
///
/// 已应用过的文件被改动时 sqlx 在这里就会报
/// `migration N was previously applied but has been modified` 并且**一条都不执行**
/// —— 校验和存的是整个文件的哈希，不区分注释与 DDL（eng/02 §3.2）。
async fn migrate() -> anyhow::Result<()> {
    let url = std::env::var(MIGRATE_URL_ENV).map_err(|_| {
        anyhow::anyhow!("{MIGRATE_URL_ENV} 未设置 —— 迁移要 owner 身份（ops/00 §2.1）")
    })?;
    let pool = sqlx::PgPool::connect(&url).await?;
    // 每个 DDL 文件自己 `SET ROLE tgm_owner`（见 0002 开头的注释）——
    // 表的 owner 决定 FORCE 那一层是真的还是装饰，所以不能靠连接身份碰巧对。
    let outcome = sqlx::migrate!("../../migrations").run(&pool).await;
    let applied =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE success")
            .fetch_one(&pool)
            .await;
    pool.close().await;
    outcome?;
    // 打出条数而不是只说「成功」：一次什么都没做的 run 和一次跑了 12 个文件的 run
    // 在退出码上同形，而前者在「忘了加新迁移」时正是会发生的事。
    eprintln!("tgm: 迁移完成：{} 个版本已应用", applied?);
    Ok(())
}

/// 迁移用的连接串。ops/00 §2.1 与 eng/01 §二 都指定 owner 身份 ——
/// 建表、开 RLS、发 GRANT 都不是 `app_user` 能做的事。
/// 与 `AUDIT_URL_ENV` 是同一个变量名，但两条命令的用途不同，各自写出来
/// 比共享一个常量清楚：将来审计若改用更低权限的角色，这里不该跟着变。
const MIGRATE_URL_ENV: &str = "TGM_DB_URL_OWNER";

/// 审计用的连接串。**不复用 `TGM_DB_URL_APP`**：
/// 断言里的 `has_table_privilege('app_user', ...)` 查的是别人的权限，
/// 用被查的那个角色自己去跑，语义上是绕的。
/// 也不接 PgBouncer —— 审计读目录，不需要连接池，且 `transaction` 模式下
/// 会话语义会让排查变复杂（eng/02 §一）。
const AUDIT_URL_ENV: &str = "TGM_DB_URL_OWNER";

/// 跑 RLS 静态审计，返回是否有断言报红。
///
/// 报告格式刻意做成「红的打全部违规行，绿的只打一行」：
/// eng/04 §五 说得很直接 —— 全绿这个状态本身不携带信息，
/// 所以绿的输出越短越好，红的输出要能直接拿去修。
async fn audit_rls() -> anyhow::Result<bool> {
    let url = std::env::var(AUDIT_URL_ENV)
        .map_err(|_| anyhow::anyhow!("{AUDIT_URL_ENV} 未设置 —— 审计需要一个跑完全部迁移的库"))?;
    let pool = sqlx::PgPool::connect(&url).await?;
    let outcomes = tgm_db::audit::run(&pool).await?;
    pool.close().await;

    let mut failed = 0usize;
    let mut errored = 0usize;
    for o in &outcomes {
        // 报错与违规分开打。两者都让退出码非 0，但修法完全不同：
        // 违规要改库，报错是断言自己写坏了 —— 混成一句会把人引向错的方向
        if let Some(e) = &o.error {
            failed += 1;
            errored += 1;
            println!("ERR  {} {} —— 断言自身执行失败", o.id, o.title);
            println!("       {e}");
        } else if o.failed() {
            failed += 1;
            println!("FAIL {} {} —— {} 项违规", o.id, o.title, o.violations.len());
            for v in &o.violations {
                println!("       {v}");
            }
        } else {
            println!("ok   {} {}", o.id, o.title);
        }
    }
    println!("\n{} 条断言，{failed} 条报红", outcomes.len());
    if errored > 0 {
        println!("其中 {errored} 条是断言自身报错 —— 这些断言什么都没检查，先修它们");
    }
    if failed == 0 {
        // 这句不是客套。ADR-0022 的代价一栏第一条就是「断言全绿不等于隔离正确」，
        // 而读 CI 日志的人看到的就是这最后一行 —— 把限度写在它旁边。
        println!("注意：全绿只说明配置存在，不说明策略写对（eng/04 §五）。");
        println!("      USING (true) 能通过全部断言。隔离由 eng/03 §三 的反向测试证明。");
    }
    Ok(failed > 0)
}

/// **全仓库唯一一处读 `std::env` 的地方**（`tgm_core::config` 的注释承诺了这一点，
/// 上面的 `audit_rls` 是例外：它是运维命令，不是常驻进程的启动路径）。
///
/// 收的是「存在且非空」的变量名。空串算不存在，因为部署模板里
/// `TGM_LOCAL_KEK=` 这种写法和没写是一个意思，而按「存在」判会让它躲过必填检查。
fn present_env_names() -> Vec<String> {
    std::env::vars()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, _)| k)
        .collect()
}

/// eng/01 §四 的启动期校验。全部六项按文档顺序，**但缺失项收齐再一次报**。
///
/// 顺序不是随意的：不连库的两条（§4.1/§4.5）先跑，因为连库那两条要用
/// `TGM_DB_URL_APP`，而那个变量缺失时 §4.2 的报错会是「连不上」——
/// 指向了错的方向。
///
/// 返回校验过的 `Profile` 给调用方装 subscriber 用。**不在这里装** ——
/// 本函数自己的输出必须在 subscriber 存在之前就可见（配置错误是它要报的第一件事），
/// 所以这一路全是 `eprintln!`，而 `telemetry::init` 只能在它返回之后。
async fn preflight(role: &Role) -> anyhow::Result<tgm_core::profile::Profile> {
    let core_role = CoreRole::from(role);

    // TGM_PROFILE 不在这里解析：连原始值一起交给 check()，让它失败时也只是
    // 一条 problem。早先这里是 `Profile::parse(...)?`，于是 profile 缺失或拼错
    // 时只报它一条，同时缺的别的变量下一轮才看见 —— §四 的元规则被自己的
    // 实现顺序破掉了。空串按缺失算，与 present_env_names 同一口径。
    let raw_profile = std::env::var("TGM_PROFILE").ok().filter(|s| !s.is_empty());

    let names = present_env_names();
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let rust_log = std::env::var("RUST_LOG").ok();
    let mut problems = tgm_core::config::check(
        core_role,
        raw_profile.as_deref(),
        &borrowed,
        rust_log.as_deref(),
    );
    let kek_backend = std::env::var("TGM_KEK_BACKEND")
        .ok()
        .filter(|value| !value.is_empty());
    let kms_key_arn = std::env::var("TGM_KMS_KEY_ARN")
        .ok()
        .filter(|value| !value.is_empty());
    let local_kek = std::env::var("TGM_LOCAL_KEK")
        .ok()
        .filter(|value| !value.is_empty());
    let imds_disabled = std::env::var("AWS_EC2_METADATA_DISABLED").ok();
    problems.extend(tgm_core::config::check_kek_backend(
        core_role,
        kek_backend.as_deref(),
        kms_key_arn.as_deref(),
        local_kek.as_deref(),
        imds_disabled.as_deref(),
    ));
    if !problems.is_empty() {
        // 一次性打印，不 `?` 在第一条上 —— §四 的元规则。
        for p in &problems {
            eprintln!("tgm: 配置错误：{p}");
        }
        anyhow::bail!("{} 项配置错误（见上），拒绝启动", problems.len());
    }

    // 到这里 profile 一定是合法的（上面那两条 problem 会先让我们退出），
    // 但类型上还是 Option<&str> —— 再解析一次而不是让 check 把它返回出来：
    // check 的返回值是「问题清单」，往里塞一个成功值会让它同时承担两种语义。
    let raw_profile =
        raw_profile.ok_or_else(|| anyhow::anyhow!("TGM_PROFILE 缺失 —— 上一步本该拦住"))?;
    let profile = tgm_core::profile::Profile::parse(&raw_profile)?;

    if matches!(core_role, CoreRole::ControlPlane) {
        let oidc_alg = std::env::var("TGM_OIDC_ALG")
            .map_err(|_| anyhow::anyhow!("缺少 TGM_OIDC_ALG —— 上一步本该拦住，这里是兜底"))?;
        tgm_core::config::validate_oidc_alg(&oidc_alg)
            .map_err(|error| anyhow::anyhow!("OIDC 配置无效：{error}"))?;
    }

    // §4.2 与 §4.3 都走业务连接池的那条连接串：检查的对象就是**这条连接**
    // 的属性，换一条连接去查等于没查（实测 server_version_num 经 PgBouncer
    // 的 transaction 模式可以透传，所以不需要旁路直连）。
    let url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 上一步本该拦住，这里是兜底"))?;
    let pool = sqlx::PgPool::connect(&url).await?;
    let version = tgm_db::preflight::check_version(&pool).await;
    let db_role = tgm_db::preflight::check_runtime_role(&pool).await;
    if matches!(
        core_role,
        CoreRole::ControlPlane | CoreRole::MtprotoWorker | CoreRole::Delivery
    ) && std::env::var("TGM_DB_URL_EXECUTION").is_ok()
    {
        let tenant: tgm_core::ids::TenantId = sqlx::query_scalar("SELECT 0::bigint")
            .fetch_one(&pool)
            .await?;
        tgm_db::lifecycle::WorkerGuard::acquire(&pool, tenant).await?;
    }
    pool.close().await;
    let version = version?;
    // 这一行是事后唯一能回答「那次运行到底用的哪个角色」的证据。
    eprintln!(
        "tgm: 启动期校验通过：profile={raw_profile} pg={version} db_role={}",
        db_role?
    );

    // §4.4。`backend=env` 时顺带校验 TGM_LOCAL_KEK 确实是 32 字节 ——
    // 那是 EnvKek::new 做的事，这里只负责调它。
    // control-plane 只 wrap（ADR-0032）：kms 下做 encrypt-only 自测，实测 Decrypt 被拒；
    // env 下 EnvKek 对称、必然解得开，只验 wrap（ADR-0012 已接受的本地差距）。
    let control_plane = matches!(core_role, CoreRole::ControlPlane);
    match std::env::var("TGM_KEK_BACKEND").as_deref() {
        Ok(backend @ ("env" | "kms")) => {
            let kek = build_kek(profile).await?;
            if control_plane && backend == "kms" {
                tgm_core::kek::encrypt_only_self_test(kek.as_ref())
                    .await
                    .map_err(|error| anyhow::anyhow!("KMS KEK encrypt-only 自测失败：{error}"))?;
                eprintln!("tgm: KEK encrypt-only 自测通过：backend=kms（Decrypt 已被拒）");
            } else if control_plane {
                kek.wrap(
                    &tgm_core::secret::Secret::new([0x5a; tgm_core::kek::DEK_LEN]),
                    &tgm_core::kek::KekContext::SELF_TEST,
                )
                .await?;
                eprintln!(
                    "tgm: KEK wrap 自测通过：backend=env kek_id={}（本地对称，不验无解密权）",
                    kek.kek_id()
                );
            } else {
                tgm_core::kek::self_test(kek.as_ref())
                    .await
                    .map_err(|_| anyhow::anyhow!("KEK 往返自测失败"))?;
                // kek_id 打进日志是安全的（它是标识，不是密钥材料），而且必要：
                // 轮换之后要能从日志里回答「那条密文是哪个 KEK 包的」。
                eprintln!("tgm: KEK 往返自测通过：kek_id={}", kek.kek_id());
            }
        }
        Ok(other) => anyhow::bail!("TGM_KEK_BACKEND 只接受 kms 或 env，收到 {other:?}"),
        // 不配这个变量的角色（fanout / rule）就不该做 KEK 自测。
        // 该配却没配的情况由上面的 §4.1 检查拦。
        Err(_) => {}
    }

    Ok(profile)
}

fn main() -> anyhow::Result<()> {
    let cmd = Cli::parse().command;
    if let Command::CleanupTenant { category } = cmd {
        let present = present_env_names();
        let names: Vec<&str> = present.iter().map(String::as_str).collect();
        let profile = std::env::var("TGM_PROFILE").ok();
        let rust_log = std::env::var("RUST_LOG").ok();
        let problems =
            tgm_core::config::check_tenant_cleanup(profile.as_deref(), &names, rust_log.as_deref());
        if !problems.is_empty() {
            for problem in problems {
                eprintln!("tgm: 配置错误：{problem}");
            }
            anyhow::bail!("租户清理配置错误，拒绝执行");
        }
        return tokio::runtime::Runtime::new()?.block_on(async {
            let shutdown = tgm_app::shutdown_signal();
            let url = std::env::var("TGM_DB_URL_TENANT_CLEANUP")
                .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_TENANT_CLEANUP"))?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .map_err(|_| anyhow::anyhow!("租户清理数据库连接失败"))?;
            let result = tgm_app::tenant_retention::run(&pool, category.into(), shutdown).await;
            pool.close().await;
            let result = result.map_err(|error| anyhow::anyhow!("{error}"))?;
            println!(
                "tgm: 租户清理结束：deleted={} retained={} batches={} tenants={} overlaps={} stopped={}",
                result.deleted, result.retained, result.batches, result.tenants, result.overlaps, result.stopped
            );
            if matches!(category, TenantCleanupCategory::UnmatchedEvents) {
                if result.retained > 0 {
                    eprintln!("tgm: 仍有受保护的到龄未命中投影，短期留存尚未全部回收");
                }
                if result.overlaps > 0 || result.stopped {
                    anyhow::bail!("未命中投影清理未完整执行，拒绝将本轮标记为成功");
                }
            }
            Ok(())
        });
    }
    if let Command::CleanupAuth { category } = cmd {
        let present = present_env_names();
        let names: Vec<&str> = present.iter().map(String::as_str).collect();
        let profile = std::env::var("TGM_PROFILE").ok();
        let rust_log = std::env::var("RUST_LOG").ok();
        let problems =
            tgm_core::config::check_auth_cleanup(profile.as_deref(), &names, rust_log.as_deref());
        if !problems.is_empty() {
            for problem in problems {
                eprintln!("tgm: 配置错误：{problem}");
            }
            anyhow::bail!("鉴权清理配置错误，拒绝执行");
        }
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(async {
            let shutdown = tgm_app::shutdown_signal();
            let url = std::env::var("TGM_DB_URL_AUTH_CLEANUP")
                .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_AUTH_CLEANUP"))?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .map_err(|_| anyhow::anyhow!("鉴权清理数据库连接失败"))?;
            let result = tgm_app::auth_retention::run(&pool, category.into(), shutdown).await;
            pool.close().await;
            let result = result.map_err(|error| anyhow::anyhow!("{error}"))?;
            println!(
                "tgm: 鉴权清理结束：deleted={} batches={} overlapped={} stopped={}",
                result.deleted, result.batches, result.overlapped, result.stopped
            );
            Ok(())
        });
    }
    if let Command::CleanupShared = cmd {
        let present = present_env_names();
        let names: Vec<&str> = present.iter().map(String::as_str).collect();
        let profile = std::env::var("TGM_PROFILE").ok();
        let rust_log = std::env::var("RUST_LOG").ok();
        let problems =
            tgm_core::config::check_shared_cleanup(profile.as_deref(), &names, rust_log.as_deref());
        if !problems.is_empty() {
            for problem in problems {
                eprintln!("tgm: 配置错误：{problem}");
            }
            anyhow::bail!("共享事件清理配置错误，拒绝执行");
        }
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(async {
            let shutdown = tgm_app::shutdown_signal();
            let url = std::env::var("TGM_DB_URL_SHARED_CLEANUP")
                .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_SHARED_CLEANUP"))?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .map_err(|_| anyhow::anyhow!("共享事件清理数据库连接失败"))?;
            let result = tgm_app::shared_retention::run(&pool, shutdown).await;
            pool.close().await;
            let result = result.map_err(|error| anyhow::anyhow!("{error}"))?;
            println!(
                "tgm: 共享事件清理结束：deleted={} batches={} overlapped={} stopped={}",
                result.deleted, result.batches, result.overlapped, result.stopped
            );
            Ok(())
        });
    }
    if let Command::TenantLifecycle { action } = cmd {
        let present = present_env_names();
        let names: Vec<&str> = present.iter().map(String::as_str).collect();
        if !tgm_core::config::check_tenant_lifecycle(
            std::env::var("TGM_PROFILE").ok().as_deref(),
            &names,
            std::env::var("RUST_LOG").ok().as_deref(),
        )
        .is_empty()
        {
            anyhow::bail!("租户生命周期配置错误，拒绝执行");
        }
        let (id, operation) = match action {
            LifecycleAction::Suspend { tenant_id } => (tenant_id, "suspend"),
            LifecycleAction::Resume { tenant_id } => (tenant_id, "resume"),
            LifecycleAction::Delete { tenant_id } => (tenant_id, "delete"),
        };
        if id <= 0 {
            anyhow::bail!("租户编号必须为正整数");
        }
        return tokio::runtime::Runtime::new()?.block_on(async {
            let url = std::env::var("TGM_DB_URL_TENANT_LIFECYCLE")
                .map_err(|_| anyhow::anyhow!("缺少租户生命周期连接配置"))?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .map_err(|_| anyhow::anyhow!("租户生命周期数据库连接失败"))?;
            let result = match operation {
                "delete" => tgm_app::lifecycle::delete(&pool, id, tgm_app::shutdown_signal())
                    .await
                    .map(|()| 0),
                _ => {
                    tgm_app::lifecycle::run(
                        &pool,
                        id,
                        operation == "suspend",
                        tgm_app::shutdown_signal(),
                    )
                    .await
                }
            };
            pool.close().await;
            match result {
                Ok(_) if operation == "delete" => {
                    println!("tgm: 租户逻辑注销完成")
                }
                Ok(rows) => println!("tgm: 租户生命周期完成：cancelled={rows}"),
                Err(tgm_db::lifecycle::LifecycleError::Draining) => {
                    anyhow::bail!("租户仍可能暂停但排空未完成，需核对在途结果后重跑")
                }
                Err(_) => anyhow::bail!("租户生命周期失败，未提交批次已回滚；需核对租户状态"),
            }
            Ok(())
        });
    }
    if let Command::CleanupPlatform = cmd {
        let present = present_env_names();
        let names: Vec<&str> = present.iter().map(String::as_str).collect();
        let profile = std::env::var("TGM_PROFILE").ok();
        let rust_log = std::env::var("RUST_LOG").ok();
        let problems = tgm_core::config::check_platform_cleanup(
            profile.as_deref(),
            &names,
            rust_log.as_deref(),
        );
        if !problems.is_empty() {
            for problem in problems {
                eprintln!("tgm: 配置错误：{problem}");
            }
            anyhow::bail!("平台日志清理配置错误，拒绝执行");
        }
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(async {
            let shutdown = tgm_app::shutdown_signal();
            let url = std::env::var("TGM_DB_URL_PLATFORM_CLEANUP")
                .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_PLATFORM_CLEANUP"))?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .map_err(|_| anyhow::anyhow!("平台日志清理数据库连接失败"))?;
            let result = tgm_app::platform_retention::run(&pool, shutdown).await;
            pool.close().await;
            let result = result
                .map_err(|_| anyhow::anyhow!("平台日志清理失败，未提交的删除和日志已回滚"))?;
            println!(
                "tgm: 平台日志清理结束：deleted={} batches={} overlapped={} stopped={}",
                result.deleted, result.batches, result.overlapped, result.stopped
            );
            if result.overlapped || result.stopped {
                anyhow::bail!("平台日志清理未完成，需重跑");
            }
            Ok(())
        });
    }
    if let Command::Migrate = cmd {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(migrate())?;
        return Ok(());
    }
    if let Command::AuditRls = cmd {
        // 只有这一条子命令需要异步运行时，故在此处而不是 main 上建 ——
        // `serve` 落地时会各自需要不同的运行时配置（eng/00 §二 的 tokio features）。
        let rt = tokio::runtime::Runtime::new()?;
        if rt.block_on(audit_rls())? {
            // 退出码 1 是这条卡口的全部意义所在。
            std::process::exit(1);
        }
        return Ok(());
    }
    // SIGTERM 监听必须在 preflight 连库之前注册：preflight 一连上库，外部就可能把进程视为已启动，
    // 这之后、监听注册之前到达的 SIGTERM 会走默认处置直接杀进程（strace 实测）。
    let mut serve_runtime = None;
    if let Command::Serve { role } = &cmd {
        let rt = tokio::runtime::Runtime::new()?;
        // signal::unix::signal 要求在运行时上下文里调用，enter 只为注册，不轮询。
        let signal = {
            let _guard = rt.enter();
            tgm_app::shutdown_signal()
        };
        // 启动期校验要跑，**哪怕 serve 本体还没实现**。
        // 理由：这六项校验管的是「这个部署的配置对不对」，与业务代码是否落地无关；
        // 而且 `?` 往外抛让退出码非 0 —— 这是 serve 这条路径上第一个
        // 不是空壳的断言（对比 not_implemented 那个恒 0 的退出码）。
        let profile = rt.block_on(preflight(role))?;
        // subscriber 在校验之后装：装它要 profile，而 profile 的合法性是校验的产出。
        // 代价是 preflight 的输出走 stderr 而非结构化日志 —— 接受，因为
        // 「配置错误导致拒绝启动」这条消息的读者是人，不是日志管道。
        tgm_core::telemetry::init(profile);
        tracing::info!(
            role = role.as_str(),
            "subscriber 已装（eng/00 §5.1）—— 本行之前的输出都在 stderr"
        );
        serve_runtime = Some((rt, signal));
    }
    match cmd {
        Command::Serve { role } => {
            let (rt, signal) = serve_runtime
                .take()
                .ok_or_else(|| anyhow::anyhow!("serve 运行时未初始化"))?;
            // 穷举 match：新增角色忘了接线时编译器报 non-exhaustive，而不是运行期才发现。
            match role {
                Role::ControlPlane => rt.block_on(serve_control_plane(signal))?,
                Role::BotGateway => rt.block_on(serve_bot_gateway(signal))?,
                Role::Rule => rt.block_on(serve_rule(signal))?,
                Role::Fanout => rt.block_on(serve_fanout(signal))?,
                Role::MtprotoWorker => rt.block_on(serve_mtproto_worker(signal))?,
                Role::Delivery => rt.block_on(serve_delivery(signal))?,
            }
        }
        Command::Migrate => unreachable!("已在上面处理"),
        Command::AuditRls => unreachable!("已在上面处理"),
        Command::CleanupAuth { .. } => unreachable!("已在上面处理"),
        Command::CleanupTenant { .. } => unreachable!("已在上面处理"),
        Command::CleanupShared => unreachable!("已在上面处理"),
        Command::CleanupPlatform => unreachable!("已在上面处理"),
        Command::TenantLifecycle { .. } => unreachable!("已在上面处理"),
        Command::Openapi { check } => {
            openapi(check)?;
        }
    }
    Ok(())
}
