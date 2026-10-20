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

async fn group_member(app: &PgPool, identity: IdentityId, message_id: i64, caption: &str) -> i64 {
    let mut input = event(identity);
    input.tg_message_id = message_id;
    input.media_group_id = Some("album-1".to_owned());
    input.text = None;
    input.caption = Some(caption.to_owned());
    input.media_type = Some(if message_id == 10 { "PHOTO" } else { "VIDEO" }.to_owned());
    insert_canonical_event(app, &input)
        .await
        .expect("写组成员失败")
        .id
}

async fn expire_group_window(app: &PgPool) {
    sqlx::query(
        "UPDATE canonical_events SET created_at = clock_timestamp() - interval '3 seconds'",
    )
    .execute(app)
    .await
    .expect("推进聚合窗口失败");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_group_window_order_idempotency_and_late_arrival(admin: PgPool) {
    use tgm_db::repo::{aggregate_ready_media_groups, list_unfanned_canonical_event_ids};
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity = seed_identity(&admin).await;
    let last = group_member(&app, identity, 30, "urgent album").await;
    let first = group_member(&app, identity, 10, "").await;
    let middle = group_member(&app, identity, 20, "").await;
    aggregate_ready_media_groups(&app, 100)
        .await
        .expect("聚合失败");
    assert!(
        list_unfanned_canonical_event_ids(&app, 100)
            .await
            .expect("待扇出查询失败")
            .is_empty()
    );
    expire_group_window(&app).await;
    let (left, right) = tokio::join!(
        aggregate_ready_media_groups(&app, 100),
        aggregate_ready_media_groups(&app, 100)
    );
    left.expect("并发聚合一失败");
    right.expect("并发聚合二失败");
    let groups: Vec<(i64, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, tg_message_id, caption, media_type FROM canonical_events WHERE event_type = 'MEDIA_GROUP_COMPLETED'",
    ).fetch_all(&app).await.expect("读聚合事件失败");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].1, 10);
    assert_eq!(groups[0].2.as_deref(), Some("urgent album"));
    assert_eq!(groups[0].3.as_deref(), Some("PHOTO"));
    let members: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT member_event_id, position FROM media_group_members WHERE group_event_id = $1 ORDER BY position",
    ).bind(groups[0].0).fetch_all(&app).await.expect("读成员失败");
    assert_eq!(members, vec![(first, 0), (middle, 1), (last, 2)]);
    assert_eq!(
        list_unfanned_canonical_event_ids(&app, 100)
            .await
            .expect("查询失败"),
        vec![groups[0].0]
    );
    let late = group_member(&app, identity, 40, "late").await;
    aggregate_ready_media_groups(&app, 100)
        .await
        .expect("重扫失败");
    let ready = list_unfanned_canonical_event_ids(&app, 100)
        .await
        .expect("查询失败");
    assert_eq!(ready, vec![groups[0].0, late]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM media_group_members")
        .fetch_one(&app)
        .await
        .expect("数成员失败");
    assert_eq!(count, 3);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_group_members_privileges_and_cascade(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity = seed_identity(&admin).await;
    group_member(&app, identity, 10, "caption").await;
    expire_group_window(&app).await;
    tgm_db::repo::aggregate_ready_media_groups(&app, 10)
        .await
        .expect("聚合失败");
    let flags: (bool, bool) = sqlx::query_as("SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = 'media_group_members'::regclass")
        .fetch_one(&app).await.expect("读 RLS 标志失败");
    assert_eq!(flags, (false, false));
    let grants: (bool, bool, bool, bool) = sqlx::query_as("SELECT has_table_privilege(current_user, 'media_group_members', 'SELECT'), has_table_privilege(current_user, 'media_group_members', 'INSERT'), has_table_privilege(current_user, 'media_group_members', 'UPDATE'), has_table_privilege(current_user, 'media_group_members', 'DELETE')")
        .fetch_one(&app).await.expect("读权限失败");
    assert_eq!(grants, (true, true, false, false));
    assert!(
        sqlx::query("UPDATE media_group_members SET position = 9")
            .execute(&app)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM media_group_members")
            .execute(&app)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM canonical_events WHERE event_type = 'MEDIA_GROUP_COMPLETED'")
        .execute(admin.raw())
        .await
        .expect("删除组事件失败");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM media_group_members")
        .fetch_one(&app)
        .await
        .expect("数成员失败");
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_group_writes_wait_for_aggregation_lock(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let identity = seed_identity(&admin).await;
    let mut guard = app.begin().await.expect("开始聚合事务失败");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("media-group:{}:-1001:album-1", identity.get()))
        .execute(&mut *guard)
        .await
        .expect("锁定组失败");
    let writer_pool = app.clone();
    let writer =
        tokio::spawn(async move { group_member(&writer_pool, identity, 10, "caption").await });
    let waiting = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))")
                .fetch_one(admin.raw()).await.expect("探测锁等待失败");
            if blocked { break; }
            assert!(!writer.is_finished(), "写入未等待聚合锁");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await;
    guard.commit().await.expect("提交聚合事务失败");
    writer.await.expect("写入任务退出");
    waiting.expect("没有观测到 advisory 锁等待");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_group_keys_include_identity_and_chat(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let first = seed_identity(&admin).await;
    let second = seed_identity(&admin).await;
    group_member(&app, first, 10, "first").await;
    group_member(&app, second, 10, "second").await;
    let mut other_chat = event(first);
    other_chat.media_group_id = Some("album-1".to_owned());
    other_chat.tg_chat_id = -1002;
    other_chat.media_type = Some("PHOTO".to_owned());
    insert_canonical_event(&app, &other_chat)
        .await
        .expect("写另一频道失败");
    expire_group_window(&app).await;
    tgm_db::repo::aggregate_ready_media_groups(&app, 100)
        .await
        .expect("聚合失败");
    let sizes: Vec<i64> =
        sqlx::query_scalar("SELECT count(*) FROM media_group_members GROUP BY group_event_id")
            .fetch_all(&app)
            .await
            .expect("读组大小失败");
    assert_eq!(sizes, vec![1, 1, 1], "不同身份或频道不得被同 group id 混组");
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
