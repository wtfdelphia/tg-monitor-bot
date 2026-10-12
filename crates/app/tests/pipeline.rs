#![allow(clippy::expect_used, clippy::panic)]

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::chrono::{TimeZone, Utc};
use std::sync::Arc;
use std::time::Duration;
use tgm_app::delivery::{
    DeliveryPayload, DeliveryRunOutcome, DeliverySendError, DeliveryTransport, DeliveryWorker,
    DeliveryWorkerConfig, DeliveryWorkerExit, run_delivery_once,
};
use tgm_app::pipeline::{
    FanoutWorker, FanoutWorkerConfig, FanoutWorkerExit, RuleWorker, RuleWorkerConfig,
    RuleWorkerExit, run_fanout_once, run_rule_once,
};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, fanout_canonical_event,
    insert_canonical_event, load_delivery_task_payload,
};

struct ExpectingTransport {
    expected_text: String,
}

#[async_trait::async_trait]
impl DeliveryTransport for ExpectingTransport {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
        if payload.message.text == self.expected_text {
            Ok(9_877)
        } else {
            Err(DeliverySendError {
                code: "PAYLOAD_MISMATCH".to_owned(),
                message: Some(payload.message.text),
                flood_wait_seconds: None,
            })
        }
    }
}

async fn app_pool(admin: &PgPool) -> PgPool {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin)
        .await
        .expect("读取测试数据库名失败");
    PgPoolOptions::new()
        .max_connections(3)
        .connect(&format!(
            "postgres://app_user:apw@127.0.0.1:55432/{database}"
        ))
        .await
        .expect("连接 app_user 测试池失败")
}

async fn seed_tenant(admin: &PgPool) -> TenantId {
    sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('pipeline-tenant', 2001) RETURNING id",
    )
    .fetch_one(admin)
    .await
    .expect("创建测试租户失败")
}

async fn seed_identity(admin: &PgPool, tenant: TenantId) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin)
    .await
    .expect("创建测试身份失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn pipeline_fanout_rule_match_and_delivery_outbox_are_idempotent(admin: PgPool) {
    let app = app_pool(&admin).await;
    let current_user: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await
        .expect("读取测试角色失败");
    assert_eq!(current_user, "app_user", "规则链路测试必须使用受限角色");
    let tenant = seed_tenant(&admin).await;
    let receiver_identity = seed_identity(&admin, tenant).await;
    let sender_identity = seed_identity(&admin, tenant).await;

    let source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, username, receiver_identity_id, enabled)
         VALUES ($1, -1001, 'Source <A>', 'source_channel', $2, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(receiver_identity)
    .fetch_one(&admin)
    .await
    .expect("创建测试来源订阅失败");

    let target_ref: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels
            (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -2001, $2, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(sender_identity)
    .fetch_one(&admin)
    .await
    .expect("创建测试目标频道失败");

    let keyword_id: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'urgent', 'exact', TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await
    .expect("创建测试关键词失败");

    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rules
            (tenant_id, keyword_id, source_ref, media_type, target_ref, action, priority, version, enabled)
         VALUES ($1, $2, $3, 'TEXT', $4, 'COPY', 10, 1, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(keyword_id)
    .bind(source_ref)
    .bind(target_ref)
    .fetch_one(&admin)
    .await
    .expect("创建测试规则失败");

    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(tenant)
    .bind(rule_id)
    .bind(serde_json::json!({
        "rule": {
            "template": "📢 {chat_title} (@{username})\n🔑 {matched_keywords}"
        },
        "replacements": [{
            "seq": 1,
            "pattern": "urgent",
            "replacement": "URGENT",
            "enabled": true
        }]
    }))
    .execute(&admin)
    .await
    .expect("创建测试规则快照失败");

    let canonical = CanonicalEventInput {
        adapter: CanonicalAdapter::MtProto,
        receiver_identity_id: receiver_identity,
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
            .expect("构造测试事件时间失败"),
        text: Some("urgent update".to_owned()),
        caption: None,
        media_type: Some("TEXT".to_owned()),
        content_hash: Some("pipeline-hash".to_owned()),
        file_unique_id: None,
        file_name: None,
        file_size: None,
        mime_type: None,
        raw_file_id: None,
    };
    let canonical = insert_canonical_event(&app, &canonical)
        .await
        .expect("写入规范化事件失败");
    let tenant_event = fanout_canonical_event(&app, tenant, canonical.id)
        .await
        .expect("扇出测试事件失败")
        .expect("匹配来源订阅后应生成租户事件");

    let first = tgm_app::pipeline::process_tenant_event(&app, tenant, tenant_event.id)
        .await
        .expect("首次规则处理失败");
    assert!(first.processed);
    assert_eq!(first.matched_rule_ids, vec![rule_id]);
    assert_eq!(first.tasks.len(), 1);
    assert!(first.tasks[0].inserted);

    let second = tgm_app::pipeline::process_tenant_event(&app, tenant, tenant_event.id)
        .await
        .expect("重复规则处理失败");
    assert!(!second.processed);
    assert!(second.tasks.is_empty());

    let task_count: i64 = tgm_db::tenant::tenant_tx(&app, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar("SELECT count(*) FROM delivery_tasks")
                .fetch_one(conn)
                .await
        })
    })
    .await
    .expect("查询投递任务数量失败");
    assert_eq!(task_count, 1);

    let (status, target): (String, i64) = tgm_db::tenant::tenant_tx(&app, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as("SELECT status, target_tg_chat_id FROM delivery_tasks")
                .fetch_one(conn)
                .await
        })
    })
    .await
    .expect("查询投递任务内容失败");
    assert_eq!(status, "PENDING");
    assert_eq!(target, -2001);

    let payload = load_delivery_task_payload(&app, tenant, first.tasks[0].id)
        .await
        .expect("读取投递任务载荷失败")
        .expect("投递任务载荷不存在");
    let rendered = tgm_app::delivery::build_delivery_payload(&payload).expect("构造投递载荷失败");
    assert_eq!(rendered.target_tg_chat_id, -2001);
    assert_eq!(rendered.source.receiver_identity_id, receiver_identity);
    assert_eq!(rendered.source.tg_chat_id, -1001);
    assert_eq!(rendered.source.tg_message_id, 42);
    assert_eq!(rendered.source.media_type.as_deref(), Some("TEXT"));
    assert_eq!(
        rendered.message.text,
        "📢 Source &lt;A&gt; (@source_channel)\n🔑 urgent\nURGENT update"
    );

    let transport = ExpectingTransport {
        expected_text: rendered.message.text.clone(),
    };
    let outcome = run_delivery_once(&app, tenant, sender_identity, &transport)
        .await
        .expect("执行投递 worker 一次失败");
    assert_eq!(
        outcome,
        DeliveryRunOutcome::Done {
            task_id: first.tasks[0].id,
            telegram_message_id: 9_877,
        }
    );

    let (status, attempt_count, message_id): (String, i32, Option<i64>) = sqlx::query_as(
        "SELECT status, attempt_count, telegram_message_id
           FROM delivery_tasks
          WHERE id = $1",
    )
    .bind(first.tasks[0].id)
    .fetch_one(&admin)
    .await
    .expect("读取 worker 完成状态失败");
    assert_eq!(status, "DONE");
    assert_eq!(attempt_count, 1);
    assert_eq!(message_id, Some(9_877));

    let missing_snapshot_task: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, tenant_event_id, rule_id, rule_version, target_tg_chat_id,
             sender_identity_id, action, status, idem_key)
         VALUES ($1, $2, $3, 99, -2001, $4, 'COPY', 'PENDING', 'missing-snapshot-task')
         RETURNING id",
    )
    .bind(tenant)
    .bind(tenant_event.id)
    .bind(rule_id)
    .bind(sender_identity)
    .fetch_one(&admin)
    .await
    .expect("创建缺失快照任务失败");
    let missing_snapshot_outcome = run_delivery_once(&app, tenant, sender_identity, &transport)
        .await
        .expect("缺失快照任务处理失败");
    assert_eq!(
        missing_snapshot_outcome,
        DeliveryRunOutcome::Failed {
            task_id: missing_snapshot_task,
            status: tgm_app::delivery::DeliveryDecisionStatus::Dead,
            attempt_count: 1,
        }
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn delivery_worker_stops_cleanly_when_shutdown_is_requested(admin: PgPool) {
    let app = app_pool(&admin).await;
    let tenant = seed_tenant(&admin).await;
    let sender_identity = seed_identity(&admin, tenant).await;
    let transport = Arc::new(ExpectingTransport {
        expected_text: String::new(),
    });
    let worker = DeliveryWorker::new(
        app,
        tenant,
        sender_identity,
        transport,
        DeliveryWorkerConfig {
            poll_interval: Duration::from_secs(30),
        },
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(worker.run(shutdown_rx));

    tokio::time::sleep(Duration::from_millis(25)).await;
    shutdown_tx
        .send(true)
        .expect("发送 worker shutdown 信号失败");

    let outcome = handle
        .await
        .expect("等待 delivery worker 退出失败")
        .expect("delivery worker 返回错误");
    assert_eq!(outcome, DeliveryWorkerExit::Shutdown);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_worker_processes_pending_events_and_stops_cleanly(admin: PgPool) {
    let app = app_pool(&admin).await;
    let tenant = seed_tenant(&admin).await;
    let receiver_identity = seed_identity(&admin, tenant).await;

    let _source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, receiver_identity_id, enabled)
         VALUES ($1, -1009, $2, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(receiver_identity)
    .fetch_one(&admin)
    .await
    .expect("创建规则 worker 来源失败");

    let canonical = tgm_db::repo::insert_canonical_event(
        &app,
        &CanonicalEventInput {
            adapter: CanonicalAdapter::MtProto,
            receiver_identity_id: receiver_identity,
            tg_chat_id: -1009,
            tg_message_id: 99,
            event_type: CanonicalEventType::MessageCreated,
            edit_version: 0,
            update_id: None,
            media_group_id: None,
            sender_id: Some(42),
            message_date: Utc
                .timestamp_opt(1_760_000_010, 0)
                .single()
                .expect("构造 worker 事件时间失败"),
            text: Some("pending event".to_owned()),
            caption: None,
            media_type: Some("TEXT".to_owned()),
            content_hash: Some("worker-hash".to_owned()),
            file_unique_id: None,
            file_name: None,
            file_size: None,
            mime_type: None,
            raw_file_id: None,
        },
    )
    .await
    .expect("写入规则 worker 事件失败");
    let tenant_event = fanout_canonical_event(&app, tenant, canonical.id)
        .await
        .expect("规则 worker 扇出失败")
        .expect("规则 worker 应看到租户事件");

    let run = run_rule_once(&app, 10)
        .await
        .expect("执行规则 worker 一次失败");
    assert_eq!(run.tenant_count, 1);
    assert_eq!(run.event_count, 1);

    let processed: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL FROM tenant_events WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(tenant_event.id)
    .fetch_one(&admin)
    .await
    .expect("读取规则 worker 状态失败");
    assert!(processed);

    let second = run_rule_once(&app, 10)
        .await
        .expect("重复执行规则 worker 失败");
    assert_eq!(second.event_count, 0);

    let worker = RuleWorker::new(
        app,
        RuleWorkerConfig {
            poll_interval: Duration::from_secs(30),
            event_batch_size: 10,
        },
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(worker.run(shutdown_rx));
    tokio::time::sleep(Duration::from_millis(25)).await;
    shutdown_tx
        .send(true)
        .expect("发送规则 worker shutdown 信号失败");
    let outcome = handle
        .await
        .expect("等待规则 worker 退出失败")
        .expect("规则 worker 返回错误");
    assert_eq!(outcome, RuleWorkerExit::Shutdown);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn fanout_worker_marks_events_and_stops_cleanly(admin: PgPool) {
    let app = app_pool(&admin).await;
    let tenant = seed_tenant(&admin).await;
    let receiver_identity = seed_identity(&admin, tenant).await;
    let _source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, receiver_identity_id, enabled)
         VALUES ($1, -1010, $2, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(receiver_identity)
    .fetch_one(&admin)
    .await
    .expect("创建扇出 worker 来源失败");

    let canonical = tgm_db::repo::insert_canonical_event(
        &app,
        &CanonicalEventInput {
            adapter: CanonicalAdapter::MtProto,
            receiver_identity_id: receiver_identity,
            tg_chat_id: -1010,
            tg_message_id: 100,
            event_type: CanonicalEventType::MessageCreated,
            edit_version: 0,
            update_id: None,
            media_group_id: None,
            sender_id: Some(43),
            message_date: Utc
                .timestamp_opt(1_760_000_011, 0)
                .single()
                .expect("构造扇出 worker 事件时间失败"),
            text: Some("fanout event".to_owned()),
            caption: None,
            media_type: Some("TEXT".to_owned()),
            content_hash: Some("fanout-hash".to_owned()),
            file_unique_id: None,
            file_name: None,
            file_size: None,
            mime_type: None,
            raw_file_id: None,
        },
    )
    .await
    .expect("写入扇出 worker 事件失败");

    let run = run_fanout_once(&app, 10)
        .await
        .expect("执行扇出 worker 一次失败");
    assert_eq!(run.event_count, 1);
    assert_eq!(run.tenant_count, 1);

    let (fanned_out, tenant_event_count): (bool, i64) = sqlx::query_as(
        "SELECT ce.fanned_out,
                (SELECT count(*) FROM tenant_events WHERE tenant_id = $2 AND canonical_event_id = ce.id)
           FROM canonical_events AS ce
          WHERE ce.id = $1",
    )
    .bind(canonical.id)
    .bind(tenant)
    .fetch_one(&admin)
    .await
    .expect("读取扇出 worker 状态失败");
    assert!(fanned_out);
    assert_eq!(tenant_event_count, 1);

    let second = run_fanout_once(&app, 10)
        .await
        .expect("重复执行扇出 worker 失败");
    assert_eq!(second.event_count, 0);

    let worker = FanoutWorker::new(
        app,
        FanoutWorkerConfig {
            poll_interval: Duration::from_secs(30),
            event_batch_size: 10,
        },
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(worker.run(shutdown_rx));
    tokio::time::sleep(Duration::from_millis(25)).await;
    shutdown_tx
        .send(true)
        .expect("发送扇出 worker shutdown 信号失败");
    let outcome = handle
        .await
        .expect("等待扇出 worker 退出失败")
        .expect("扇出 worker 返回错误");
    assert_eq!(outcome, FanoutWorkerExit::Shutdown);
}
