#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use sqlx::types::chrono::{TimeZone, Utc};
use tgm_core::ids::IdentityId;
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, insert_canonical_event,
};

async fn seed_identity(admin: &support::AdminOnlyPool) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, status) VALUES ('mtproto', 'offline') RETURNING id",
    )
    .fetch_one(admin.raw())
    .await
    .expect("创建接收身份失败")
}

fn event(identity_id: IdentityId) -> CanonicalEventInput {
    CanonicalEventInput {
        adapter: CanonicalAdapter::MtProto,
        receiver_identity_id: identity_id,
        tg_chat_id: -1001,
        tg_message_id: 42,
        event_type: CanonicalEventType::MessageCreated,
        edit_version: 0,
        update_id: None,
        media_group_id: None,
        sender_id: Some(7),
        message_date: Utc
            .timestamp_opt(1_760_000_000, 0)
            .single()
            .expect("构造事件时间失败"),
        text: Some("hello".to_owned()),
        caption: None,
        media_type: None,
        content_hash: Some("hash-1".to_owned()),
        file_unique_id: None,
        file_name: None,
        file_size: None,
        mime_type: None,
        raw_file_id: None,
    }
}

fn bot_event(identity_id: IdentityId, message_id: i64, update_id: i64) -> CanonicalEventInput {
    CanonicalEventInput {
        adapter: CanonicalAdapter::Bot,
        receiver_identity_id: identity_id,
        tg_chat_id: -1002,
        tg_message_id: message_id,
        event_type: CanonicalEventType::MessageCreated,
        edit_version: 0,
        update_id: Some(update_id),
        media_group_id: None,
        sender_id: Some(8),
        message_date: Utc
            .timestamp_opt(1_760_000_001, 0)
            .single()
            .expect("构造事件时间失败"),
        text: Some("bot event".to_owned()),
        caption: None,
        media_type: None,
        content_hash: None,
        file_unique_id: None,
        file_name: None,
        file_size: None,
        mime_type: None,
        raw_file_id: None,
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn canonical_event_insert_is_idempotent_and_edit_versions_are_distinct(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity_id = seed_identity(&admin).await;

    let first = insert_canonical_event(&app, &event(identity_id))
        .await
        .expect("首次写入事件失败");
    assert!(first.inserted);

    let duplicate = insert_canonical_event(&app, &event(identity_id))
        .await
        .expect("重复写入事件失败");
    assert!(!duplicate.inserted);
    assert_eq!(duplicate.id, first.id);

    let mut edited = event(identity_id);
    edited.event_type = CanonicalEventType::MessageEdited;
    edited.edit_version = 1;
    edited.text = Some("hello edited".to_owned());

    let edit = insert_canonical_event(&app, &edited)
        .await
        .expect("写入编辑事件失败");
    assert!(edit.inserted);
    assert_ne!(edit.id, first.id);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM canonical_events WHERE receiver_identity_id = $1")
            .bind(identity_id)
            .fetch_one(admin.raw())
            .await
            .expect("核对事件数量失败");
    assert_eq!(count, 2);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_update_id_conflict_is_not_treated_as_same_message(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity_id = seed_identity(&admin).await;

    let first = insert_canonical_event(&app, &bot_event(identity_id, 100, 9001))
        .await
        .expect("首次写入 Bot 事件失败");
    assert!(first.inserted);

    let conflict = insert_canonical_event(&app, &bot_event(identity_id, 101, 9001))
        .await
        .expect_err("不同消息复用 update_id 应被唯一约束拒绝");
    assert!(conflict.to_string().contains("uq_canonical_update"));
}

async fn seed_tenant(admin: &support::AdminOnlyPool, name: &str) -> tgm_core::ids::TenantId {
    sqlx::query_scalar::<_, tgm_core::ids::TenantId>(
        "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(if name == "tenant-a" {
        1001_i64
    } else {
        1002_i64
    })
    .fetch_one(admin.raw())
    .await
    .expect("创建租户失败")
}

async fn seed_subscription(
    admin: &support::AdminOnlyPool,
    tenant: tgm_core::ids::TenantId,
    identity_id: IdentityId,
    chat_id: i64,
    enabled: bool,
) {
    sqlx::query(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, receiver_identity_id, enabled)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant)
    .bind(chat_id)
    .bind(identity_id)
    .bind(enabled)
    .execute(admin.raw())
    .await
    .expect("创建来源订阅失败");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn fanout_is_idempotent_and_visible_only_to_matching_tenant(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity_id = seed_identity(&admin).await;
    let tenant_a = seed_tenant(&admin, "tenant-a").await;
    let tenant_b = seed_tenant(&admin, "tenant-b").await;

    seed_subscription(&admin, tenant_a, identity_id, -1001, true).await;
    seed_subscription(&admin, tenant_b, identity_id, -1001, true).await;

    let canonical = insert_canonical_event(&app, &event(identity_id))
        .await
        .expect("写入规范化事件失败");

    let first = tgm_db::repo::fanout_canonical_event(&app, tenant_a, canonical.id)
        .await
        .expect("首次扇出失败")
        .expect("匹配订阅后应生成租户事件");
    assert!(first.inserted);

    let duplicate = tgm_db::repo::fanout_canonical_event(&app, tenant_a, canonical.id)
        .await
        .expect("重复扇出失败")
        .expect("已有租户事件应返回其 id");
    assert!(!duplicate.inserted);
    assert_eq!(duplicate.id, first.id);

    let tenant_a_count: i64 = tgm_db::tenant::tenant_tx(&app, tenant_a, |conn| {
        Box::pin(async move {
            sqlx::query_scalar("SELECT count(*) FROM tenant_events")
                .fetch_one(conn)
                .await
        })
    })
    .await
    .expect("查询租户 A 事件失败");
    assert_eq!(tenant_a_count, 1);

    let tenant_b_count_before: i64 = tgm_db::tenant::tenant_tx(&app, tenant_b, |conn| {
        Box::pin(async move {
            sqlx::query_scalar("SELECT count(*) FROM tenant_events")
                .fetch_one(conn)
                .await
        })
    })
    .await
    .expect("查询租户 B 事件失败");
    assert_eq!(tenant_b_count_before, 0);

    let second_tenant = tgm_db::repo::fanout_canonical_event(&app, tenant_b, canonical.id)
        .await
        .expect("租户 B 扇出失败")
        .expect("租户 B 的匹配订阅应生成租户事件");
    assert!(second_tenant.inserted);
    assert_ne!(second_tenant.id, first.id);

    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_events WHERE canonical_event_id = $1")
            .bind(canonical.id)
            .fetch_one(admin.raw())
            .await
            .expect("核对扇出总数失败");
    assert_eq!(total, 2);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn fanout_skips_disabled_or_non_matching_subscriptions(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity_id = seed_identity(&admin).await;
    let tenant = seed_tenant(&admin, "tenant-a").await;
    seed_subscription(&admin, tenant, identity_id, -1001, false).await;

    let canonical = insert_canonical_event(&app, &event(identity_id))
        .await
        .expect("写入规范化事件失败");

    assert!(
        tgm_db::repo::fanout_canonical_event(&app, tenant, canonical.id)
            .await
            .expect("禁用订阅扇出失败")
            .is_none()
    );

    let unmatched = CanonicalEventInput {
        tg_chat_id: -2002,
        ..event(identity_id)
    };
    let unmatched = insert_canonical_event(&app, &unmatched)
        .await
        .expect("写入未匹配规范化事件失败");
    assert!(
        tgm_db::repo::fanout_canonical_event(&app, tenant, unmatched.id)
            .await
            .expect("未匹配订阅扇出失败")
            .is_none()
    );
}
