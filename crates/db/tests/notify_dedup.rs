//! G4 L3 通知指纹去重的并发与生命周期测试。

#![allow(clippy::expect_used, clippy::panic)]

use sqlx::PgPool;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::notification::notification_fingerprint;
use tgm_db::repo::{
    DeliveryTaskFailureInput, DeliveryTaskFailureStatus, DeliveryTaskInput,
    claim_due_delivery_task, complete_delivery_task, enqueue_delivery_tasks,
    record_delivery_failure,
};

mod support;

struct Fixture {
    tenant: TenantId,
    sender: IdentityId,
    source: i64,
    target: i64,
    event: i64,
    rule: i64,
}

type FingerprintSource = (
    String,
    i64,
    Option<i64>,
    Option<Vec<String>>,
    Option<String>,
    Option<String>,
    Option<String>,
);

type ScrubbedFingerprintSource = (
    i64,
    Option<i64>,
    Option<Vec<String>>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn fixture(admin: &support::AdminOnlyPool, suffix: &str, target_chat: i64) -> Fixture {
    let tenant: TenantId = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(format!("notify-{suffix}"))
    .bind(31_000_i64 + i64::try_from(suffix.len()).expect("后缀长度应可转换"))
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试租户失败");
    let sender: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (tenant_id, kind, status) VALUES ($1, 'bot', 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试发送身份失败");
    let source: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions (tenant_id, tg_chat_id, receiver_identity_id)
         VALUES ($1, -7_001, $2) RETURNING id",
    )
    .bind(tenant)
    .bind(sender)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试来源失败");
    let target: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(tenant)
    .bind(target_chat)
    .bind(sender)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试目标失败");
    let keyword: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type) VALUES ($1, 'notify', 'exact') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试关键词失败");
    let rule: i64 = sqlx::query_scalar(
        "INSERT INTO rules (tenant_id, keyword_id, target_ref, action, version)
         VALUES ($1, $2, $3, 'COPY', 1) RETURNING id",
    )
    .bind(tenant)
    .bind(keyword)
    .bind(target)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试规则失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(tenant)
    .bind(rule)
    .bind(serde_json::json!({"target_ref": target}))
    .execute(admin.raw())
    .await
    .expect("创建通知测试规则快照失败");
    let canonical: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events
            (adapter, receiver_identity_id, tg_chat_id, tg_message_id, message_date, sender_id, text)
         VALUES ('bot', $1, -7_001, $2, now(), 77, 'notify body') RETURNING id",
    )
    .bind(sender)
    .bind(1_i64 + i64::try_from(suffix.len()).expect("后缀长度应可转换"))
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试规范事件失败");
    let event: i64 = sqlx::query_scalar(
        "INSERT INTO tenant_events (tenant_id, canonical_event_id, source_ref)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(tenant)
    .bind(canonical)
    .bind(source)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试租户事件失败");
    Fixture {
        tenant,
        sender,
        source,
        target,
        event,
        rule,
    }
}

async fn additional_event(
    admin: &support::AdminOnlyPool,
    fixture: &Fixture,
    message_id: i64,
) -> i64 {
    let canonical: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events
            (adapter, receiver_identity_id, tg_chat_id, tg_message_id, message_date, sender_id, text)
         VALUES ('bot', $1, -7_001, $2, now(), 77, 'notify body') RETURNING id",
    )
    .bind(fixture.sender)
    .bind(message_id)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试额外事件失败");
    sqlx::query_scalar(
        "INSERT INTO tenant_events (tenant_id, canonical_event_id, source_ref)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(fixture.tenant)
    .bind(canonical)
    .bind(fixture.source)
    .fetch_one(admin.raw())
    .await
    .expect("创建通知测试租户额外事件失败")
}

fn task(fixture: &Fixture, fingerprint: &str, target_chat: i64) -> DeliveryTaskInput {
    DeliveryTaskInput {
        rule_id: fixture.rule,
        rule_version: 1,
        target_tg_chat_id: target_chat,
        target_ref: fixture.target,
        sender_identity_id: fixture.sender,
        notification_fingerprint: fingerprint.to_owned(),
        action: "COPY".to_owned(),
        payload_snapshot: serde_json::json!({
            "source": {"tg_chat_id": -7_001, "tg_message_id": 1, "media_type": "TEXT"},
            "message": {"text": "notify body", "parse_mode": "HTML"}
        }),
        media_relay: false,
        dead_error_code: None,
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn same_target_claims_once_and_success_confirms(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let first = fixture(&admin, "first", -8_001).await;
    let second_event = additional_event(&admin, &first, 2).await;
    let keywords = vec!["notify".to_owned()];
    let first_task = task(&first, "same-fingerprint", -8_001);
    let second_task = task(&first, "same-fingerprint", -8_001);

    let (first_write, second_write) = tokio::join!(
        enqueue_delivery_tasks(
            &app,
            first.tenant,
            first.event,
            &keywords,
            std::slice::from_ref(&first_task),
        ),
        enqueue_delivery_tasks(
            &app,
            first.tenant,
            second_event,
            &keywords,
            std::slice::from_ref(&second_task),
        )
    );
    let first_write = first_write.expect("并发首个 claim 失败");
    let second_write = second_write.expect("并发重复 claim 失败");
    assert_eq!(first_write.len() + second_write.len(), 1);
    assert!(
        first_write
            .iter()
            .chain(second_write.iter())
            .any(|write| write.inserted),
        "并发 claim 必须恰好保留一个新任务"
    );

    let claimed = claim_due_delivery_task(&app, first.tenant, first.sender)
        .await
        .expect("领取任务失败")
        .expect("任务应处于 CLAIMED 对应的 PENDING 状态");
    assert!(
        complete_delivery_task(&app, first.tenant, claimed.id, 9_001, None)
            .await
            .expect("确认发送失败")
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM notify_dedup WHERE tenant_id = $1 AND fingerprint = 'same-fingerprint'",
    )
    .bind(first.tenant)
    .fetch_one(admin.raw())
    .await
    .expect("读取 L3 状态失败");
    assert_eq!(status, "SENT");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn sent_fingerprint_recomputes_from_retained_event_and_becomes_unavailable_after_scrub(
    admin: PgPool,
) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let fixture = fixture(&admin, "recompute", -8_003).await;
    let keywords = vec!["notify".to_owned()];
    let fingerprint = notification_fingerprint(Some(77), &keywords, Some("notify body"), None);
    sqlx::query("UPDATE canonical_events SET content_hash = 'synthetic-content-hash' WHERE id = (SELECT canonical_event_id FROM tenant_events WHERE tenant_id = $1 AND id = $2)")
        .bind(fixture.tenant)
        .bind(fixture.event)
        .execute(admin.raw())
        .await
        .expect("设置合成内容哈希失败");
    let writes = enqueue_delivery_tasks(
        &app,
        fixture.tenant,
        fixture.event,
        &keywords,
        &[task(&fixture, &fingerprint, -8_003)],
    )
    .await
    .expect("创建 L3 claim 失败");
    assert_eq!(writes.len(), 1);
    assert!(writes[0].inserted);

    let claimed = claim_due_delivery_task(&app, fixture.tenant, fixture.sender)
        .await
        .expect("领取通知任务失败")
        .expect("通知任务缺失");
    assert!(
        complete_delivery_task(&app, fixture.tenant, claimed.id, 9_003, None)
            .await
            .expect("确认通知发送失败")
    );

    let canonical_event_id: i64 = sqlx::query_scalar(
        "SELECT canonical_event_id FROM tenant_events WHERE tenant_id = $1 AND id = $2",
    )
    .bind(fixture.tenant)
    .bind(fixture.event)
    .fetch_one(admin.raw())
    .await
    .expect("读取测试规范事件 id 失败");
    let source = sqlx::query_as::<_, FingerprintSource>(
        r#"
        SELECT dedup.fingerprint,
               event.id,
               event.sender_id,
               tenant_event.matched_keywords,
               event.text,
               event.caption,
               event.content_hash
          FROM notify_dedup AS dedup
          JOIN LATERAL (
              SELECT tenant_event_id
                FROM delivery_tasks
               WHERE tenant_id = dedup.tenant_id
                 AND notification_fingerprint = dedup.fingerprint
                 AND notification_target_ref = dedup.target_ref
                 AND status = 'DONE'
                 AND completed_at <= dedup.last_sent_at
               ORDER BY completed_at DESC, id DESC
               LIMIT 1
          ) AS task ON TRUE
          JOIN tenant_events AS tenant_event
            ON tenant_event.tenant_id = dedup.tenant_id
           AND tenant_event.id = task.tenant_event_id
          JOIN canonical_events AS event
            ON event.id = tenant_event.canonical_event_id
         WHERE dedup.tenant_id = $1
           AND dedup.fingerprint = $2
           AND dedup.target_ref = $3
           AND dedup.status = 'SENT'
        "#,
    )
    .bind(fixture.tenant)
    .bind(&fingerprint)
    .bind(fixture.target)
    .fetch_one(admin.raw())
    .await
    .expect("读取 SENT 指纹及其成功任务来源失败");
    let (stored, event_id, sender_id, matched_keywords, text, caption, content_hash) = source;
    assert_eq!(event_id, canonical_event_id);
    assert_eq!(content_hash.as_deref(), Some("synthetic-content-hash"));
    let matched_keywords = matched_keywords.expect("已处理事件应保留关键词命中结果");
    assert_eq!(
        notification_fingerprint(
            sender_id,
            &matched_keywords,
            text.as_deref(),
            caption.as_deref()
        ),
        stored,
        "沿任务关联从 tenant_events 与 canonical_events 重算的指纹应一致"
    );

    sqlx::query(
        "UPDATE canonical_events
            SET sender_id = NULL, text = NULL, caption = NULL, content_hash = NULL
          WHERE id = (
              SELECT canonical_event_id
                FROM tenant_events
               WHERE tenant_id = $1 AND id = $2
          )",
    )
    .bind(fixture.tenant)
    .bind(fixture.event)
    .execute(admin.raw())
    .await
    .expect("脱敏测试事件失败");

    let (event_id, sender_id, matched_keywords, text, caption, content_hash):
        ScrubbedFingerprintSource = sqlx::query_as(
        r#"
        SELECT event.id,
               event.sender_id,
               tenant_event.matched_keywords,
               event.text,
               event.caption,
               event.content_hash
          FROM notify_dedup AS dedup
          JOIN LATERAL (
              SELECT tenant_event_id
                FROM delivery_tasks
               WHERE tenant_id = dedup.tenant_id
                 AND notification_fingerprint = dedup.fingerprint
                 AND notification_target_ref = dedup.target_ref
                 AND status = 'DONE'
                 AND completed_at <= dedup.last_sent_at
               ORDER BY completed_at DESC, id DESC
               LIMIT 1
          ) AS task ON TRUE
          JOIN tenant_events AS tenant_event
            ON tenant_event.tenant_id = dedup.tenant_id
           AND tenant_event.id = task.tenant_event_id
          JOIN canonical_events AS event
            ON event.id = tenant_event.canonical_event_id
         WHERE dedup.tenant_id = $1
           AND dedup.fingerprint = $2
           AND dedup.target_ref = $3
           AND dedup.status = 'SENT'
        "#,
    )
    .bind(fixture.tenant)
    .bind(&fingerprint)
    .bind(fixture.target)
    .fetch_one(admin.raw())
    .await
    .expect("读取脱敏后的指纹来源失败");
    assert_eq!(event_id, canonical_event_id);
    assert!(sender_id.is_none());
    assert!(text.is_none());
    assert!(caption.is_none());
    assert!(content_hash.is_none());
    let matched_keywords = matched_keywords.expect("脱敏不应清除租户侧命中关键词");
    assert_ne!(
        notification_fingerprint(
            sender_id,
            &matched_keywords,
            text.as_deref(),
            caption.as_deref()
        ),
        stored,
        "脱敏后的输入不得误报为原指纹可重算"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn dead_task_releases_claim_and_other_target_does_not_block(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let first = fixture(&admin, "dead", -8_001).await;
    let same_target_event = additional_event(&admin, &first, 2).await;
    let other_target: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id)
         VALUES ($1, -8_002, $2) RETURNING id",
    )
    .bind(first.tenant)
    .bind(first.sender)
    .fetch_one(admin.raw())
    .await
    .expect("创建不同目标失败");

    let first_write = enqueue_delivery_tasks(
        &app,
        first.tenant,
        first.event,
        &["notify".to_owned()],
        &[task(&first, "release-fingerprint", -8_001)],
    )
    .await
    .expect("创建首个 claim 失败");
    let claimed = claim_due_delivery_task(&app, first.tenant, first.sender)
        .await
        .expect("领取首个任务失败")
        .expect("首个任务不存在");
    record_delivery_failure(
        &app,
        first.tenant,
        claimed.id,
        DeliveryTaskFailureInput {
            status: DeliveryTaskFailureStatus::Dead,
            attempt_count: 1,
            next_retry_at: None,
            error_code: "CHANNEL_NOT_FOUND".to_owned(),
            error_message: None,
            latency_ms: None,
        },
    )
    .await
    .expect("释放 claim 失败")
    .expect("死信任务没有更新");

    assert!(first_write[0].inserted);

    assert!(
        tgm_db::repo::retry_delivery_task(&app, first.tenant, claimed.id)
            .await
            .expect("人工重试重新 claim 失败")
    );
    let retry_blocked = enqueue_delivery_tasks(
        &app,
        first.tenant,
        same_target_event,
        &["notify".to_owned()],
        &[task(&first, "release-fingerprint", -8_001)],
    )
    .await
    .expect("人工重试后的去重检查失败");
    assert!(retry_blocked.is_empty(), "人工重试占用期间不得再建第二任务");

    let other_write = enqueue_delivery_tasks(
        &app,
        first.tenant,
        additional_event(&admin, &first, 4).await,
        &["notify".to_owned()],
        &[DeliveryTaskInput {
            target_ref: other_target,
            ..task(&first, "release-fingerprint", -8_002)
        }],
    )
    .await
    .expect("不同目标 claim 失败");
    assert_eq!(other_write.len(), 1);
}
