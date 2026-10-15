#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{list_source_subscriptions, load_source_subscription};

async fn seed_tenant(admin: &support::AdminOnlyPool, name: &str) -> TenantId {
    sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(if name == "sources-tenant-a" {
        4101_i64
    } else {
        4102_i64
    })
    .fetch_one(admin.raw())
    .await
    .expect("创建来源测试租户失败")
}

async fn seed_identity(admin: &support::AdminOnlyPool, tenant: TenantId) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建来源测试身份失败")
}

async fn seed_source(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    identity: IdentityId,
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
    .expect("创建来源订阅失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn source_repo_is_tenant_scoped_and_filters_without_url_tenant(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant_a = seed_tenant(&admin, "sources-tenant-a").await;
    let tenant_b = seed_tenant(&admin, "sources-tenant-b").await;
    let identity_a = seed_identity(&admin, tenant_a).await;
    let identity_b = seed_identity(&admin, tenant_b).await;
    let source_a = seed_source(&admin, tenant_a, identity_a, -4101, "Alpha News").await;
    let source_b = seed_source(&admin, tenant_b, identity_b, -4102, "Beta News").await;

    let visible =
        list_source_subscriptions(&app, tenant_a, Some("enabled"), None, Some("alpha"), 20, 0)
            .await
            .expect("按租户查询来源失败");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, source_a);
    assert_eq!(visible[0].title.as_deref(), Some("Alpha News"));

    let hidden = load_source_subscription(&app, tenant_a, source_b)
        .await
        .expect("跨租户读取来源失败");
    assert!(hidden.is_none(), "跨租户来源必须表现为不存在");
}
