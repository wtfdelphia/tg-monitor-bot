//! 跨身份媒体中转（docs/plan/proposals/WP-5-delivery-media.md 片 2）的 L2 判据。
//!
//! PG 以受限角色 app_user 连接；对象存储是 compose 的 MinIO。不连 Telegram：
//! 下载流与发送方都是假的，验的是入队、下载归档、delivery 等待与跨租户边界。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Mutex;

use sqlx::PgPool;
use sqlx::types::chrono::{TimeZone, Utc};
use tgm_app::delivery::{
    DeliveryPayload, DeliveryRunOutcome, DeliverySendError, DeliveryTransport, run_delivery_once,
};
use tgm_app::media_relay::{MediaOpener, MediaSource, MediaStore, RelayOutcome, run_relay_once};
use tgm_app::pipeline::process_tenant_event;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, LeaseGuard, acquire_lease,
    fanout_canonical_event, insert_canonical_event, load_media_relay_for_task,
};
use tgm_tg::TelegramClientError;

const CHAT: i64 = -1_001_234;

struct World {
    app: PgPool,
    admin: support::AdminOnlyPool,
    receiver: IdentityId,
}

struct TenantSetup {
    tenant: TenantId,
    sender: IdentityId,
}

async fn world(admin: PgPool) -> World {
    let (app, admin) = support::pools(admin).await;
    let user: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await
        .expect("读取测试角色失败");
    assert_eq!(user, "app_user", "中转测试必须以受限角色连接");
    // 多个租户共用同一采集身份订阅同一来源，才会出现「一个 job 多租户共用」。
    let owner = support::seed_tenant(&admin, "relay-owner", 7001).await;
    let receiver = sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'online') RETURNING id",
    )
    .bind(owner)
    .fetch_one(admin.raw())
    .await
    .expect("创建采集身份失败");
    World {
        app,
        admin,
        receiver,
    }
}

/// 一个租户：订阅 CHAT（采集身份共用）、一个 Bot 发送身份、一条 ANY 媒体规则。
async fn tenant(world: &World, name: &str, plan: &str, action: &str) -> TenantSetup {
    let admin = world.admin.raw();
    let tenant = support::seed_tenant(&world.admin, name, 7100).await;
    sqlx::query("UPDATE tenants SET plan = $2 WHERE id = $1")
        .bind(tenant)
        .bind(plan)
        .execute(admin)
        .await
        .expect("设置套餐失败");
    let sender = support::seed_identity(&world.admin, tenant).await;
    let source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, username, receiver_identity_id, enabled)
         VALUES ($1, $2, 'Relay Source', 'relay_src', $3, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(CHAT)
    .bind(world.receiver)
    .fetch_one(admin)
    .await
    .expect("创建来源失败");
    let target_ref: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -2001, $2, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(sender)
    .fetch_one(admin)
    .await
    .expect("创建目标失败");
    let keyword_id: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'urgent', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin)
    .await
    .expect("创建关键词失败");
    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rules
            (tenant_id, keyword_id, source_ref, media_type, target_ref, action, priority, version, enabled)
         VALUES ($1, $2, $3, 'ANY', $4, $5, 10, 1, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(keyword_id)
    .bind(source_ref)
    .bind(target_ref)
    .bind(action)
    .fetch_one(admin)
    .await
    .expect("创建规则失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, '{\"rule\": {}}'::jsonb)",
    )
    .bind(tenant)
    .bind(rule_id)
    .execute(admin)
    .await
    .expect("创建规则快照失败");
    TenantSetup { tenant, sender }
}

async fn media_event(world: &World, message_id: i64, caption: &str, file_size: Option<i64>) -> i64 {
    insert_canonical_event(
        &world.app,
        &CanonicalEventInput {
            adapter: CanonicalAdapter::MtProto,
            receiver_identity_id: world.receiver,
            tg_chat_id: CHAT,
            tg_message_id: message_id,
            event_type: CanonicalEventType::MessageCreated,
            edit_version: 0,
            update_id: None,
            media_group_id: None,
            sender_id: Some(7),
            message_date: Utc
                .timestamp_opt(1_760_000_000, 0)
                .single()
                .expect("构造时间失败"),
            text: None,
            caption: Some(caption.to_owned()),
            media_type: Some("DOCUMENT".to_owned()),
            content_hash: None,
            file_unique_id: None,
            file_name: Some("report.pdf".to_owned()),
            file_size,
            mime_type: Some("application/pdf".to_owned()),
            raw_file_id: None,
        },
    )
    .await
    .expect("写入事件失败")
    .id
}

/// 扇出并跑规则，返回该租户生成的投递任务 id。
async fn route(world: &World, setup: &TenantSetup, canonical_id: i64) -> i64 {
    let tenant_event = fanout_canonical_event(&world.app, setup.tenant, canonical_id)
        .await
        .expect("扇出失败")
        .expect("应生成租户事件");
    let result = process_tenant_event(&world.app, setup.tenant, tenant_event.id)
        .await
        .expect("规则处理失败");
    assert_eq!(result.tasks.len(), 1, "规则应命中一条");
    result.tasks[0].id
}

async fn job_count(world: &World) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM media_relay_jobs")
        .fetch_one(&world.app)
        .await
        .expect("统计 job 失败")
}

async fn task_state(
    world: &World,
    setup: &TenantSetup,
    task_id: i64,
) -> (String, i32, Option<String>) {
    tgm_db::tenant::tenant_tx(&world.app, setup.tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as(
                "SELECT status, attempt_count, last_error_code FROM delivery_tasks WHERE id = $1",
            )
            .bind(task_id)
            .fetch_one(conn)
            .await
        })
    })
    .await
    .expect("读取任务状态失败")
}

async fn lease(world: &World, worker: &str) -> LeaseGuard {
    let fencing_token = acquire_lease(&world.app, world.receiver, worker)
        .await
        .expect("抢租约失败")
        .expect("租约应可获取");
    LeaseGuard {
        identity_id: world.receiver,
        worker_id: worker.to_owned(),
        fencing_token,
    }
}

/// 假下载：按给定内容分片吐出，记打开次数。
struct FakeOpener {
    content: Vec<u8>,
    opens: Mutex<u32>,
    error: Option<fn() -> TelegramClientError>,
}

impl FakeOpener {
    fn new(content: Vec<u8>) -> Self {
        Self {
            content,
            opens: Mutex::new(0),
            error: None,
        }
    }
}

struct FakeSource {
    chunks: std::vec::IntoIter<Vec<u8>>,
}

#[async_trait::async_trait]
impl MediaSource for FakeSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TelegramClientError> {
        Ok(self.chunks.next())
    }
}

#[async_trait::async_trait]
impl MediaOpener for FakeOpener {
    async fn open(
        &self,
        _tg_chat_id: i64,
        _tg_message_id: i64,
    ) -> Result<Box<dyn MediaSource>, TelegramClientError> {
        *self.opens.lock().expect("计数锁") += 1;
        if let Some(error) = self.error {
            return Err(error());
        }
        let chunks: Vec<Vec<u8>> = self
            .content
            .chunks(512 * 1024)
            .map(<[u8]>::to_vec)
            .collect();
        Ok(Box::new(FakeSource {
            chunks: chunks.into_iter(),
        }))
    }
}

/// 假发送方：记下收到的载荷，返回固定 message id。
#[derive(Default)]
struct RecordingTransport {
    seen: Mutex<Vec<DeliveryPayload>>,
}

#[async_trait::async_trait]
impl DeliveryTransport for RecordingTransport {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
        self.seen.lock().expect("记录锁").push(payload);
        Ok(4242)
    }
}

async fn object_bytes(store: &MediaStore, key: &str) -> Vec<u8> {
    let (mut reader, size) = store.get_stream(key).await.expect("读对象失败");
    let mut bytes = Vec::with_capacity(size);
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes)
        .await
        .expect("读对象内容失败");
    bytes
}

struct ExpiringOpener {
    app: PgPool,
    cleanup: PgPool,
    event: i64,
}

#[async_trait::async_trait]
impl MediaOpener for ExpiringOpener {
    async fn open(
        &self,
        _tg_chat_id: i64,
        _tg_message_id: i64,
    ) -> Result<Box<dyn MediaSource>, TelegramClientError> {
        sqlx::query("UPDATE media_relay_jobs SET next_retry_at=clock_timestamp()-interval '1 hour' WHERE canonical_event_id=$1")
            .bind(self.event).execute(&self.app).await.expect("设置下载过期失败");
        assert_eq!(
            tgm_db::shared_retention::cleanup_batch(&self.cleanup)
                .await
                .expect("回收事件失败"),
            Some(1)
        );
        Ok(Box::new(FakeSource {
            chunks: vec![b"late download".to_vec()].into_iter(),
        }))
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn shared_cleanup_during_download_rolls_back_archive_and_deletes_uploaded_object(
    admin: PgPool,
) {
    let world = world(admin).await;
    let event = media_event(&world, 90001, "expired orphan", Some(13)).await;
    sqlx::query("UPDATE canonical_events SET fanned_out=true,created_at=clock_timestamp()-interval '32 days' WHERE id=$1")
        .bind(event).execute(&world.app).await.expect("设置原始事件到期失败");
    sqlx::query(
        "INSERT INTO media_relay_jobs (canonical_event_id,receiver_identity_id) VALUES ($1,$2)",
    )
    .bind(event)
    .bind(world.receiver)
    .execute(&world.app)
    .await
    .expect("创建孤儿 job 失败");
    let cleanup = sqlx::postgres::PgPoolOptions::new()
        .connect_with(
            world
                .app
                .connect_options()
                .as_ref()
                .clone()
                .username("shared_cleanup")
                .password("scpw"),
        )
        .await
        .expect("连接共享清理角色失败");
    tgm_db::shared_retention::check_connection(&cleanup)
        .await
        .expect("清理角色检查失败");
    let store = support::media_store("tgm-test-shared-expired-download").await;
    let opener = ExpiringOpener {
        app: world.app.clone(),
        cleanup,
        event,
    };
    let guard = lease(&world, "shared-expired-download").await;
    assert_eq!(
        run_relay_once(&world.app, &guard, &opener, &store)
            .await
            .expect("迟到下载处理失败"),
        RelayOutcome::LeaseLost
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_archive")
            .fetch_one(&world.app)
            .await
            .expect("读取档案失败"),
        0
    );
    let cutoff = aws_sdk_s3::primitives::DateTime::from(
        std::time::SystemTime::now() + std::time::Duration::from_secs(60),
    );
    assert!(
        store
            .list_older_than("staging/", cutoff)
            .await
            .expect("列举上传对象失败")
            .is_empty()
    );
}

/// pro 档建 job、free 档不建；同一事件两个 pro 租户共用一个 job。
#[sqlx::test(migrations = "../../migrations-test")]
async fn only_pro_media_creates_one_shared_job(admin: PgPool) {
    let world = world(admin).await;
    let pro_a = tenant(&world, "pro-a", "pro", "COPY").await;
    let pro_b = tenant(&world, "pro-b", "pro", "RESEND").await;
    let free = tenant(&world, "free", "free", "COPY").await;
    let canonical = media_event(&world, 10, "urgent report", Some(1024)).await;

    let free_task = route(&world, &free, canonical).await;
    assert_eq!(job_count(&world).await, 0, "free 档不得建中转 job");
    route(&world, &pro_a, canonical).await;
    route(&world, &pro_b, canonical).await;
    assert_eq!(
        job_count(&world).await,
        1,
        "多个 pro 租户命中同一事件只建一个 job"
    );

    let free_payload = tgm_db::repo::load_delivery_task_payload(&world.app, free.tenant, free_task)
        .await
        .expect("读载荷失败")
        .expect("载荷不存在");
    let free_payload =
        tgm_app::delivery::build_delivery_payload(&free_payload).expect("构造载荷失败");
    assert!(free_payload.text_only, "free 档媒体快照必须标记只发文字");
    assert!(!tgm_app::delivery::needs_media_relay(&free_payload));
}

/// free 档没有 caption 的媒体：正文是占位说明「[媒体：<media_type>] <来源链接>」。
/// 关键词用正则 `^$` 命中空正文。
#[sqlx::test(migrations = "../../migrations-test")]
async fn free_media_without_caption_uses_placeholder(admin: PgPool) {
    let world = world(admin).await;
    let free = tenant(&world, "free", "free", "COPY").await;
    sqlx::query("UPDATE keywords SET word = '^$', match_type = 'regex' WHERE tenant_id = $1")
        .bind(free.tenant)
        .execute(world.admin.raw())
        .await
        .expect("改关键词失败");
    let canonical = media_event(&world, 11, "", None).await;
    let task = route(&world, &free, canonical).await;
    let payload = tgm_db::repo::load_delivery_task_payload(&world.app, free.tenant, task)
        .await
        .expect("读载荷失败")
        .expect("载荷不存在");
    let payload = tgm_app::delivery::build_delivery_payload(&payload).expect("构造载荷失败");
    assert!(payload.text_only);
    assert_eq!(
        payload.message.text,
        "[媒体：DOCUMENT] https://t.me/relay_src/11"
    );
}

/// 超过 50 MB：入队即 DEAD MEDIA_TOO_LARGE，不建 job。
#[sqlx::test(migrations = "../../migrations-test")]
async fn oversized_media_is_dead_at_enqueue(admin: PgPool) {
    let world = world(admin).await;
    let pro = tenant(&world, "pro", "pro", "COPY").await;
    let canonical = media_event(&world, 12, "urgent", Some(50 * 1024 * 1024 + 1)).await;
    let task = route(&world, &pro, canonical).await;
    assert_eq!(job_count(&world).await, 0);
    let (status, _, code) = task_state(&world, &pro, task).await;
    assert_eq!(status, "DEAD");
    assert_eq!(code.as_deref(), Some("MEDIA_TOO_LARGE"));
}

/// 下载写入 MinIO 与 media_archive；同内容第二个事件复用旧对象。
/// delivery 在 job 未就绪时推后且不计尝试，就绪后带上对象发送。
#[sqlx::test(migrations = "../../migrations-test")]
async fn relay_downloads_archives_dedups_and_unblocks_delivery(admin: PgPool) {
    let world = world(admin).await;
    let pro = tenant(&world, "pro", "pro", "COPY").await;
    let store = support::media_store("tgm-test-relay").await;
    let content: Vec<u8> = (0..(9 * 1024 * 1024 + 123))
        .map(|index| (index % 251) as u8)
        .collect();
    let opener = FakeOpener::new(content.clone());
    let transport = RecordingTransport::default();

    let first = media_event(&world, 20, "urgent one", Some(content.len() as i64)).await;
    let first_task = route(&world, &pro, first).await;

    // 未就绪：推后，不计尝试、不写投递日志。
    let outcome = run_delivery_once(&world.app, pro.tenant, pro.sender, &transport)
        .await
        .expect("投递失败");
    assert_eq!(
        outcome,
        DeliveryRunOutcome::Deferred {
            task_id: first_task
        }
    );
    let (status, attempts, _) = task_state(&world, &pro, first_task).await;
    assert_eq!((status.as_str(), attempts), ("PENDING", 0));
    assert!(transport.seen.lock().expect("记录锁").is_empty());

    let guard = lease(&world, "relay-a").await;
    let outcome = run_relay_once(&world.app, &guard, &opener, &store)
        .await
        .expect("中转失败");
    assert_eq!(
        outcome,
        RelayOutcome::Ready {
            canonical_event_id: first,
            reused: false
        }
    );
    let (archive_count, key, hash, size): (i64, String, String, i64) = sqlx::query_as(
        "SELECT count(*) OVER (), object_key, content_hash, file_size FROM media_archive",
    )
    .fetch_one(&world.app)
    .await
    .expect("读档案失败");
    assert_eq!(archive_count, 1);
    assert_eq!(size, content.len() as i64);
    assert_eq!(hash.len(), 64);
    assert_eq!(
        object_bytes(&store, &key).await,
        content,
        "对象内容必须逐字节一致"
    );

    // 第二个事件同内容：下载后发现同 hash，job 指向旧档案。
    let second = media_event(&world, 21, "urgent two", Some(content.len() as i64)).await;
    route(&world, &pro, second).await;
    let outcome = run_relay_once(&world.app, &guard, &opener, &store)
        .await
        .expect("中转失败");
    assert_eq!(
        outcome,
        RelayOutcome::Ready {
            canonical_event_id: second,
            reused: true
        }
    );
    let archives: i64 = sqlx::query_scalar("SELECT count(*) FROM media_archive")
        .fetch_one(&world.app)
        .await
        .expect("统计档案失败");
    assert_eq!(archives, 1, "同 hash 不得新增档案");
    assert_eq!(
        run_relay_once(&world.app, &guard, &opener, &store)
            .await
            .expect("中转失败"),
        RelayOutcome::Idle
    );

    // 就绪：投递拿到对象与元数据。推后的 next_retry_at 先清掉，免得等 10 秒。
    sqlx::query("UPDATE delivery_tasks SET next_retry_at = NULL")
        .execute(world.admin.raw())
        .await
        .expect("清推后时间失败");
    let outcome = run_delivery_once(&world.app, pro.tenant, pro.sender, &transport)
        .await
        .expect("投递失败");
    assert_eq!(
        outcome,
        DeliveryRunOutcome::Done {
            task_id: first_task,
            telegram_message_id: 4242
        }
    );
    let seen = transport.seen.lock().expect("记录锁");
    let media = seen[0].media.as_ref().expect("就绪任务必须带媒体");
    assert_eq!(media.object_key, key);
    assert_eq!(media.mime_type.as_deref(), Some("application/pdf"));
    assert_eq!(media.file_name.as_deref(), Some("report.pdf"));
}

/// 下载失败按 30s/5min/30min 退避，第 4 次判死；源消息被删直接判死。delivery 跟着判死。
#[sqlx::test(migrations = "../../migrations-test")]
async fn relay_failures_back_off_then_die_and_delivery_follows(admin: PgPool) {
    let world = world(admin).await;
    let pro = tenant(&world, "pro", "pro", "COPY").await;
    let store = support::media_store("tgm-test-relay-fail").await;
    let guard = lease(&world, "relay-a").await;
    let canonical = media_event(&world, 30, "urgent", Some(10)).await;
    let task = route(&world, &pro, canonical).await;
    let mut flaky = FakeOpener::new(Vec::new());
    flaky.error = Some(|| TelegramClientError::MessageNotReturned);
    for attempt in 1..=4 {
        // tgm_test 控制角色对 0014 之后的新表没有授权；造数用 app_user（它本就有 UPDATE）。
        sqlx::query("UPDATE media_relay_jobs SET next_retry_at = NULL")
            .execute(&world.app)
            .await
            .expect("清退避失败");
        let outcome = run_relay_once(&world.app, &guard, &flaky, &store)
            .await
            .expect("中转失败");
        let RelayOutcome::Failed { dead, .. } = outcome else {
            panic!("第 {attempt} 次应失败：{outcome:?}");
        };
        assert_eq!(dead, attempt == 4, "第 {attempt} 次");
    }
    let outcome = run_delivery_once(
        &world.app,
        pro.tenant,
        pro.sender,
        &RecordingTransport::default(),
    )
    .await
    .expect("投递失败");
    assert!(matches!(outcome, DeliveryRunOutcome::Failed { .. }));
    let (status, _, code) = task_state(&world, &pro, task).await;
    assert_eq!(status, "DEAD");
    assert_eq!(code.as_deref(), Some("MESSAGE_NOT_RETURNED"));

    let deleted = media_event(&world, 31, "urgent", Some(10)).await;
    route(&world, &pro, deleted).await;
    let mut gone = FakeOpener::new(Vec::new());
    gone.error = Some(|| TelegramClientError::SourceMessageMissing);
    let outcome = run_relay_once(&world.app, &guard, &gone, &store)
        .await
        .expect("中转失败");
    assert!(
        matches!(outcome, RelayOutcome::Failed { dead: true, ref error_code, .. } if error_code == "SOURCE_MESSAGE_MISSING"),
        "源消息被删应一次判死：{outcome:?}"
    );
}

/// 等待超过 30 分钟判 MEDIA_RELAY_TIMEOUT；租约不在手时领不到 job。
#[sqlx::test(migrations = "../../migrations-test")]
async fn delivery_times_out_and_stale_lease_cannot_claim(admin: PgPool) {
    let world = world(admin).await;
    let pro = tenant(&world, "pro", "pro", "COPY").await;
    let store = support::media_store("tgm-test-relay-timeout").await;
    let canonical = media_event(&world, 40, "urgent", Some(10)).await;
    let task = route(&world, &pro, canonical).await;

    let stale = LeaseGuard {
        identity_id: world.receiver,
        worker_id: "nobody".to_owned(),
        fencing_token: 99,
    };
    assert_eq!(
        run_relay_once(&world.app, &stale, &FakeOpener::new(vec![1]), &store)
            .await
            .expect("中转失败"),
        RelayOutcome::Idle,
        "不持租约不得领 job"
    );

    sqlx::query("UPDATE delivery_tasks SET created_at = now() - interval '31 minutes'")
        .execute(world.admin.raw())
        .await
        .expect("改创建时间失败");
    run_delivery_once(
        &world.app,
        pro.tenant,
        pro.sender,
        &RecordingTransport::default(),
    )
    .await
    .expect("投递失败");
    let (status, _, code) = task_state(&world, &pro, task).await;
    assert_eq!(status, "DEAD");
    assert_eq!(code.as_deref(), Some("MEDIA_RELAY_TIMEOUT"));
}

/// 跨租户：别的租户的任务 id 读不到中转状态（丙类表的唯一入口经 tenant_tx 过滤）。
#[sqlx::test(migrations = "../../migrations-test")]
async fn relay_state_is_not_readable_across_tenants(admin: PgPool) {
    let world = world(admin).await;
    let victim = tenant(&world, "victim", "pro", "COPY").await;
    let attacker = tenant(&world, "attacker", "pro", "COPY").await;
    let canonical = media_event(&world, 50, "urgent", Some(10)).await;
    let victim_task = route(&world, &victim, canonical).await;
    assert!(
        load_media_relay_for_task(&world.app, victim.tenant, victim_task)
            .await
            .expect("读取失败")
            .is_some()
    );
    assert!(
        load_media_relay_for_task(&world.app, attacker.tenant, victim_task)
            .await
            .expect("读取失败")
            .is_none(),
        "别的租户的任务 id 不得读到中转状态"
    );
}

async fn album(world: &World, caption: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    for message_id in [103, 101, 102] {
        let id = media_event(
            world,
            message_id,
            if message_id == 102 { caption } else { "" },
            Some(10),
        )
        .await;
        sqlx::query("UPDATE canonical_events SET media_group_id = 'album', media_type = 'PHOTO', created_at = clock_timestamp() - interval '3 seconds' WHERE id = $1")
            .bind(id).execute(&world.app).await.expect("设置相册失败");
        ids.push(id);
    }
    ids
}

async fn group_task(world: &World, setup: &TenantSetup) -> i64 {
    let task: i64 = sqlx::query_scalar("SELECT id FROM delivery_tasks WHERE tenant_id = $1")
        .bind(setup.tenant)
        .fetch_one(world.admin.raw())
        .await
        .expect("读组任务失败");
    sqlx::query("UPDATE delivery_tasks SET next_retry_at = NULL WHERE id = $1")
        .bind(task)
        .execute(world.admin.raw())
        .await
        .expect("清退避失败");
    task
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_group_fanout_rule_and_relay_keep_member_order(admin: PgPool) {
    let world = world(admin).await;
    let pro = tenant(&world, "album-pro", "pro", "COPY").await;
    let free = tenant(&world, "album-free", "free", "COPY").await;
    let forward = tenant(&world, "album-forward", "pro", "FORWARD").await;
    let members = album(&world, "urgent album").await;
    let (left, right) = tokio::join!(
        tgm_app::pipeline::run_fanout_once(&world.app, 100),
        tgm_app::pipeline::run_fanout_once(&world.app, 100)
    );
    left.expect("并发扇出一失败");
    right.expect("并发扇出二失败");
    tgm_app::pipeline::run_rule_once(&world.app, 100)
        .await
        .expect("规则失败");
    tgm_app::pipeline::run_rule_once(&world.app, 100)
        .await
        .expect("规则重跑失败");
    let tasks: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_tasks")
        .fetch_one(world.admin.raw())
        .await
        .expect("数任务失败");
    assert_eq!(tasks, 3, "相册每个租户只生成一条任务");
    assert_eq!(job_count(&world).await, 3);
    let transport = RecordingTransport::default();
    let task = group_task(&world, &pro).await;
    assert_eq!(
        run_delivery_once(&world.app, pro.tenant, pro.sender, &transport)
            .await
            .expect("投递失败"),
        DeliveryRunOutcome::Deferred { task_id: task }
    );
    assert!(transport.seen.lock().expect("记录锁").is_empty());
    for (position, member) in members.iter().enumerate() {
        let archive: i64 = sqlx::query_scalar("INSERT INTO media_archive (content_hash, media_type, object_key, object_expires_at) VALUES ($1, 'PHOTO', $2, clock_timestamp() + interval '3 days') RETURNING id")
            .bind(format!("hash-{position}")).bind(format!("objects/{member}"))
            .fetch_one(&world.app).await.expect("插档案失败");
        sqlx::query("UPDATE media_relay_jobs SET status = 'READY', media_archive_id = $2 WHERE canonical_event_id = $1")
            .bind(member).bind(archive).execute(&world.app).await.expect("就绪失败");
    }
    group_task(&world, &pro).await;
    assert!(matches!(
        run_delivery_once(&world.app, pro.tenant, pro.sender, &transport)
            .await
            .expect("投递失败"),
        DeliveryRunOutcome::Done { .. }
    ));
    assert_eq!(
        run_delivery_once(&world.app, pro.tenant, pro.sender, &transport)
            .await
            .expect("重复投递失败"),
        DeliveryRunOutcome::Idle
    );
    let seen = transport.seen.lock().expect("记录锁").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].source.group_message_ids, vec![101, 102, 103]);
    assert_eq!(
        seen[0]
            .media_group
            .iter()
            .map(|media| media.object_key.clone())
            .collect::<Vec<_>>(),
        vec![
            format!("objects/{}", members[1]),
            format!("objects/{}", members[2]),
            format!("objects/{}", members[0])
        ]
    );
    assert_eq!(seen[0].message.text, "urgent album");
    drop(seen);
    let free_transport = RecordingTransport::default();
    run_delivery_once(&world.app, free.tenant, free.sender, &free_transport)
        .await
        .expect("free 投递失败");
    let seen = free_transport.seen.lock().expect("记录锁").clone();
    assert!(seen[0].text_only);
    assert!(seen[0].media_group.is_empty());
    drop(seen);
    let forward_transport = RecordingTransport::default();
    run_delivery_once(
        &world.app,
        forward.tenant,
        forward.sender,
        &forward_transport,
    )
    .await
    .expect("forward 投递失败");
    assert_eq!(
        forward_transport.seen.lock().expect("记录锁")[0]
            .source
            .group_message_ids,
        vec![101, 102, 103]
    );
    assert!(
        tgm_db::repo::load_media_group_relay_for_task(&world.app, free.tenant, task)
            .await
            .expect("跨租户查询失败")
            .is_empty()
    );
    let matched: i64 =
        sqlx::query_scalar("SELECT count(*) FROM canonical_events WHERE matched_any")
            .fetch_one(&world.app)
            .await
            .expect("数命中失败");
    assert_eq!(matched, 4, "成员与聚合事件一起标命中，避免提前清掉下载源");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_group_dead_member_overrides_pending_member(admin: PgPool) {
    let world = world(admin).await;
    let pro = tenant(&world, "album-dead", "pro", "COPY").await;
    album(&world, "urgent").await;
    tgm_app::pipeline::run_fanout_once(&world.app, 100)
        .await
        .expect("扇出失败");
    tgm_app::pipeline::run_rule_once(&world.app, 100)
        .await
        .expect("规则失败");
    sqlx::query("UPDATE media_relay_jobs SET status = 'DEAD', last_error_code = 'SOURCE_MESSAGE_MISSING' WHERE canonical_event_id = (SELECT id FROM canonical_events WHERE tg_message_id = 103 AND event_type = 'MESSAGE_CREATED')")
        .execute(&world.app).await.expect("判死失败");
    let task = group_task(&world, &pro).await;
    let transport = RecordingTransport::default();
    run_delivery_once(&world.app, pro.tenant, pro.sender, &transport)
        .await
        .expect("投递失败");
    let (status, _, error) = task_state(&world, &pro, task).await;
    assert_eq!(status, "DEAD");
    assert_eq!(error.as_deref(), Some("SOURCE_MESSAGE_MISSING"));
    assert!(transport.seen.lock().expect("记录锁").is_empty());
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_group_placeholder_and_oversized_member(admin: PgPool) {
    let world = world(admin).await;
    let free = tenant(&world, "album-placeholder", "free", "COPY").await;
    let pro = tenant(&world, "album-oversize", "pro", "RESEND").await;
    sqlx::query("UPDATE keywords SET word = '^$', match_type = 'regex'")
        .execute(world.admin.raw())
        .await
        .expect("改关键词失败");
    let members = album(&world, "").await;
    sqlx::query("UPDATE canonical_events SET file_size = 52428801 WHERE id = $1")
        .bind(members[2])
        .execute(&world.app)
        .await
        .expect("设置超限失败");
    tgm_app::pipeline::run_fanout_once(&world.app, 100)
        .await
        .expect("扇出失败");
    tgm_app::pipeline::run_rule_once(&world.app, 100)
        .await
        .expect("规则失败");
    let task = group_task(&world, &pro).await;
    let (status, _, code) = task_state(&world, &pro, task).await;
    assert_eq!(status, "DEAD");
    assert_eq!(code.as_deref(), Some("MEDIA_TOO_LARGE"));
    assert_eq!(job_count(&world).await, 0);
    let transport = RecordingTransport::default();
    run_delivery_once(&world.app, free.tenant, free.sender, &transport)
        .await
        .expect("free 投递失败");
    assert_eq!(
        transport.seen.lock().expect("记录锁")[0].message.text,
        "[媒体组：3 项] https://t.me/relay_src/101"
    );
}

/// 多个进程同时首启：同时发现桶不存在、同时建桶，后到的收到 409 也必须算成功。
/// 每轮用新桶名，保证起跑时桶确实不存在。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_ensure_bucket_all_succeed() {
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos();
    let store = MediaStore::new(&tgm_app::media_relay::S3Config {
        endpoint: "http://127.0.0.1:59000".to_owned(),
        bucket: format!("tgm-test-race-{run}"),
        region: "us-east-1".to_owned(),
        access_key_id: "minioadmin".to_owned(),
        secret_access_key: tgm_core::secret::Secret::new("minioadmin".to_owned()),
    });
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let store = store.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                store.ensure_bucket().await
            })
        })
        .collect();
    for task in tasks {
        task.await
            .expect("建桶任务异常退出")
            .expect("并发建桶必须全部成功");
    }
}
