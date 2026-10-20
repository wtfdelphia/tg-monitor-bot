//! 媒体对象清理（docs/plan/proposals/WP-5-delivery-media.md 片 3 B 部分）的 L2 判据。
//!
//! PG 以受限角色 app_user 连接，对象存储是 compose 的 MinIO。每个用例用独立的桶，
//! 避免并行测试互相清掉对方的 `staging/` 对象。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use sqlx::PgPool;
use tgm_app::media_cleanup::{STAGING_PREFIX, run_cleanup_once};
use tgm_app::media_relay::{MediaSource, MediaStore};
use tgm_tg::TelegramClientError;

struct OneChunk(Option<Vec<u8>>);

#[async_trait::async_trait]
impl MediaSource for OneChunk {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TelegramClientError> {
        Ok(self.0.take())
    }
}

async fn put(store: &MediaStore, key: &str) {
    store
        .put_stream(key, &mut OneChunk(Some(key.as_bytes().to_vec())))
        .await
        .expect("写对象失败");
}

async fn exists(store: &MediaStore, key: &str) -> bool {
    store.get_stream(key).await.is_ok()
}

async fn app_pool(admin: PgPool) -> (PgPool, support::AdminOnlyPool) {
    let (app, admin) = support::pools(admin).await;
    let user: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await
        .expect("读取测试角色失败");
    assert_eq!(user, "app_user", "清理测试必须以受限角色连接");
    (app, admin)
}

/// 插一条档案。`expires` 是相对现在的秒数（负数 = 已过期），`age_days` 是 first_seen_at 的年龄。
async fn archive(
    app: &PgPool,
    hash: &str,
    key: Option<&str>,
    expires: Option<i64>,
    age_days: i32,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO media_archive
            (content_hash, media_type, object_key, object_expires_at, first_seen_at)
         VALUES ($1, 'DOCUMENT', $2,
                 clock_timestamp() + make_interval(secs => $3),
                 clock_timestamp() - make_interval(days => $4))
         RETURNING id",
    )
    .bind(hash)
    .bind(key)
    .bind(expires.map(|seconds| seconds as f64))
    .bind(age_days)
    .fetch_one(app)
    .await
    .expect("插档案失败")
}

async fn archive_key(app: &PgPool, id: i64) -> Option<Option<String>> {
    sqlx::query_scalar("SELECT object_key FROM media_archive WHERE id = $1")
        .bind(id)
        .fetch_optional(app)
        .await
        .expect("读档案失败")
}

/// 过期对象被删、key 置空；未过期的不动；没有对象的档案不碰对象存储。
#[sqlx::test(migrations = "../../migrations-test")]
async fn expired_objects_are_deleted_then_cleared(admin: PgPool) {
    let (app, _admin) = app_pool(admin).await;
    let store = support::media_store("tgm-test-cleanup-expire").await;
    put(&store, "objects/expired").await;
    put(&store, "objects/live").await;
    let expired = archive(&app, "h-expired", Some("objects/expired"), Some(-60), 1).await;
    let live = archive(&app, "h-live", Some("objects/live"), Some(3600), 1).await;

    let outcome = run_cleanup_once(&app, &store, Duration::from_secs(3600))
        .await
        .expect("清理失败");
    assert_eq!(outcome.objects_deleted, 1);
    assert_eq!(outcome.object_failures, 0);
    assert!(!exists(&store, "objects/expired").await, "过期对象必须删掉");
    assert_eq!(
        archive_key(&app, expired).await,
        Some(None),
        "删后 key 置空"
    );
    assert!(exists(&store, "objects/live").await, "未过期对象不得删");
    assert_eq!(
        archive_key(&app, live).await,
        Some(Some("objects/live".to_owned()))
    );

    let again = run_cleanup_once(&app, &store, Duration::from_secs(3600))
        .await
        .expect("重复清理失败");
    assert_eq!(again.objects_deleted, 0, "第二轮应当没有可清的");
}

/// 清理和中转并发：列出过期档案之后、置空之前，中转把同 hash 档案改指向了新对象。
/// 置空必须带上旧 key 作条件，不得把新 key 一起清掉。
#[sqlx::test(migrations = "../../migrations-test")]
async fn clearing_does_not_overwrite_a_replaced_key(admin: PgPool) {
    let (app, _admin) = app_pool(admin).await;
    let id = archive(&app, "h-race", Some("objects/old"), Some(-60), 1).await;
    sqlx::query(
        "UPDATE media_archive SET object_key = 'objects/new',
                object_expires_at = clock_timestamp() + interval '3 days' WHERE id = $1",
    )
    .bind(id)
    .execute(&app)
    .await
    .expect("模拟换 key 失败");
    assert!(
        !tgm_db::repo::clear_media_archive_object(&app, id, "objects/old")
            .await
            .expect("置空失败")
    );
    assert_eq!(
        archive_key(&app, id).await,
        Some(Some("objects/new".to_owned()))
    );
}

/// 180 天前、无对象、无 job 引用的档案删掉；仍被 job 引用的、仍挂着对象的、还在保留期内的不删。
#[sqlx::test(migrations = "../../migrations-test")]
async fn stale_archives_are_deleted_unless_referenced(admin: PgPool) {
    let (app, admin) = app_pool(admin).await;
    let store = support::media_store("tgm-test-cleanup-archive").await;
    let stale = archive(&app, "h-stale", None, None, 181).await;
    let recent = archive(&app, "h-recent", None, None, 179).await;
    let referenced = archive(&app, "h-referenced", None, None, 181).await;
    let keyed = archive(&app, "h-keyed", Some("objects/keyed"), Some(3600), 181).await;

    let tenant = support::seed_tenant(&admin, "cleanup", 7301).await;
    let identity: i64 = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'online') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建身份失败");
    let event: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events (adapter, receiver_identity_id, tg_chat_id, tg_message_id, message_date)
         VALUES ('mtproto', $1, -100, 1, now()) RETURNING id",
    )
    .bind(identity)
    .fetch_one(&app)
    .await
    .expect("创建事件失败");
    sqlx::query(
        "INSERT INTO media_relay_jobs (canonical_event_id, receiver_identity_id, status, media_archive_id)
         VALUES ($1, $2, 'READY', $3)",
    )
    .bind(event)
    .bind(identity)
    .bind(referenced)
    .execute(&app)
    .await
    .expect("创建 job 失败");

    let outcome = run_cleanup_once(&app, &store, Duration::from_secs(3600))
        .await
        .expect("清理失败");
    assert_eq!(outcome.archives_deleted, 1);
    assert_eq!(archive_key(&app, stale).await, None, "过期档案应删掉");
    assert!(archive_key(&app, recent).await.is_some(), "保留期内不得删");
    assert!(
        archive_key(&app, referenced).await.is_some(),
        "仍被 job 引用的档案不得删"
    );
    assert!(
        archive_key(&app, keyed).await.is_some(),
        "仍挂着对象的档案不得删"
    );
}

/// 超过年龄的 staging 孤儿删掉，新写的不动；正式对象前缀不在扫描范围内。
///
/// 桶跨轮复用，上一轮留下的对象也会被这一轮清掉，所以按本轮的唯一 key 断言而不是计数。
/// MinIO 的修改时间按秒取整，年龄判定留足余量：孤儿比阈值老 4 秒，新对象比阈值新 5 秒以上。
#[sqlx::test(migrations = "../../migrations-test")]
async fn staging_orphans_are_deleted_by_age(admin: PgPool) {
    let (app, _admin) = app_pool(admin).await;
    let store = support::media_store("tgm-test-cleanup-staging").await;
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos();
    let orphan = format!("{STAGING_PREFIX}orphan-{run}");
    let fresh = format!("{STAGING_PREFIX}fresh-{run}");
    let unlisted = format!("objects/unlisted-{run}");
    put(&store, &orphan).await;
    put(&store, &unlisted).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    put(&store, &fresh).await;

    let outcome = run_cleanup_once(&app, &store, Duration::from_secs(5))
        .await
        .expect("清理失败");
    assert!(outcome.staging_deleted >= 1);
    assert!(!exists(&store, &orphan).await, "孤儿必须删掉");
    assert!(exists(&store, &fresh).await, "新写的 staging 对象不得删");
    assert!(
        exists(&store, &unlisted).await,
        "staging 之外的对象不归这一步管"
    );
}

/// 对象存储不可用：删对象失败时档案保持原样，下一轮还能找到它。
#[sqlx::test(migrations = "../../migrations-test")]
async fn failed_object_delete_keeps_the_archive(admin: PgPool) {
    let (app, _admin) = app_pool(admin).await;
    let broken = MediaStore::new(&tgm_app::media_relay::S3Config {
        endpoint: "http://127.0.0.1:59000".to_owned(),
        bucket: "tgm-test-cleanup-missing-bucket".to_owned(),
        region: "us-east-1".to_owned(),
        access_key_id: "minioadmin".to_owned(),
        secret_access_key: tgm_core::secret::Secret::new("wrong-secret".to_owned()),
    });
    let id = archive(&app, "h-broken", Some("objects/x"), Some(-60), 1).await;
    let result = run_cleanup_once(&app, &broken, Duration::from_secs(3600)).await;
    assert_eq!(
        archive_key(&app, id).await,
        Some(Some("objects/x".to_owned())),
        "删对象失败时不得置空 key"
    );
    // 第三步列 staging 也会失败，整轮返回错误；第一步的失败按条计数，不中断。
    assert!(result.is_err(), "对象存储不可用时整轮应报错");
}
