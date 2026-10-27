#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::panic)]

use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;

pub struct AdminOnlyPool(PgPool);

impl AdminOnlyPool {
    pub fn raw(&self) -> &PgPool {
        &self.0
    }
}

pub async fn pools(injected: PgPool) -> (PgPool, AdminOnlyPool) {
    let database: String = sqlx::query("SELECT current_database()")
        .fetch_one(&injected)
        .await
        .expect("读取测试数据库名失败")
        .get(0);
    let app = PgPoolOptions::new()
        .max_connections(3)
        .connect(&format!(
            "postgres://app_user:apw@127.0.0.1:55432/{database}"
        ))
        .await
        .expect("连接 app_user 测试池失败");
    (app, AdminOnlyPool(injected))
}

pub async fn auth_pool(admin: &AdminOnlyPool) -> PgPool {
    let database: String = sqlx::query("SELECT current_database()")
        .fetch_one(admin.raw())
        .await
        .expect("读取鉴权测试数据库名失败")
        .get(0);
    PgPoolOptions::new()
        .max_connections(3)
        .connect(&format!(
            "postgres://auth_lookup:lpw@127.0.0.1:55432/{database}"
        ))
        .await
        .expect("连接 auth_lookup 测试池失败")
}

pub async fn media_cleanup_pool(admin: &AdminOnlyPool) -> PgPool {
    let database: String = sqlx::query("SELECT current_database()")
        .fetch_one(admin.raw())
        .await
        .expect("读取媒体清理测试数据库名失败")
        .get(0);
    PgPoolOptions::new()
        .max_connections(3)
        .connect(&format!(
            "postgres://media_cleanup:mcpw@127.0.0.1:55432/{database}"
        ))
        .await
        .expect("连接 media_cleanup 测试池失败")
}

pub async fn seed_tenant(
    admin: &AdminOnlyPool,
    name: &str,
    owner_user_id: i64,
) -> tgm_core::ids::TenantId {
    sqlx::query_scalar::<_, tgm_core::ids::TenantId>(
        "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(owner_user_id)
    .fetch_one(admin.raw())
    .await
    .expect("创建测试租户失败")
}

pub async fn seed_identity(
    admin: &AdminOnlyPool,
    tenant: tgm_core::ids::TenantId,
) -> tgm_core::ids::IdentityId {
    sqlx::query_scalar::<_, tgm_core::ids::IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('bot', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建测试身份失败")
}

pub async fn seed_source(
    admin: &AdminOnlyPool,
    tenant: tgm_core::ids::TenantId,
    identity: tgm_core::ids::IdentityId,
    chat_id: i64,
    title: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, username, receiver_identity_id, enabled)
         VALUES ($1, $2, $3, $4, $5, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(chat_id)
    .bind(title)
    .bind(title.to_ascii_lowercase())
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("创建测试来源失败")
}

/// compose 里 MinIO 的测试 store（镜像默认 root 凭据，eng/02 §一 的 59000 端口）。
/// 每个测试用自己的桶名，互不干扰；桶不存在就建。
pub async fn media_store(bucket: &str) -> tgm_app::media_relay::MediaStore {
    let store = tgm_app::media_relay::MediaStore::new(&tgm_app::media_relay::S3Config {
        endpoint: "http://127.0.0.1:59000".to_owned(),
        bucket: bucket.to_owned(),
        region: "us-east-1".to_owned(),
        access_key_id: "minioadmin".to_owned(),
        secret_access_key: tgm_core::secret::Secret::new("minioadmin".to_owned()),
    });
    store.ensure_bucket().await.expect("建 MinIO 测试桶失败");
    store
}
