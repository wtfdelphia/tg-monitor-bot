//! 投递状态机的 L2 测试。行为定义见 docs/design/spec/01-产品需求.md §七。

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{
    DeliveryTaskFailureInput, DeliveryTaskFailureStatus, claim_due_delivery_task,
    complete_delivery_task, load_delivery_task_payload, record_delivery_failure,
    recover_stale_delivery_tasks,
};

async fn seed_tenant(admin: &support::AdminOnlyPool, name: &str) -> TenantId {
    sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(if name == "delivery-tenant-a" {
        4001_i64
    } else {
        4002_i64
    })
    .fetch_one(admin.raw())
    .await
    .expect("创建投递测试租户失败")
}

async fn seed_identity(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    suffix: &str,
) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, display_name, status)
         VALUES ('bot', $1, $2, 'offline') RETURNING id",
    )
    .bind(tenant)
    .bind(format!("delivery-{suffix}"))
    .fetch_one(admin.raw())
    .await
    .expect("创建投递测试身份失败")
}

async fn seed_task(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    sender_identity_id: IdentityId,
    key: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, target_tg_chat_id, sender_identity_id, action, status,
             telegram_random_id, idem_key, payload_snapshot)
         VALUES ($1, $2, $3, 'COPY', 'PENDING', $5, $4, $6)
         RETURNING id",
    )
    .bind(tenant)
    .bind(-30_000 - tenant.get())
    .bind(sender_identity_id)
    .bind(key)
    .bind(tenant.get() * 1_000_000 + 1)
    .bind(serde_json::json!({
        "source": {"tg_chat_id": -1001, "tg_message_id": 42, "media_type": "TEXT"},
        "message": {"text": "frozen message", "parse_mode": "HTML"}
    }))
    .fetch_one(admin.raw())
    .await
    .expect("创建投递任务失败")
}

async fn seed_historical_task(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    sender_identity_id: IdentityId,
    key: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, target_tg_chat_id, sender_identity_id, action, status, idem_key)
         VALUES ($1, $2, $3, 'COPY', 'PENDING', $4)
         RETURNING id",
    )
    .bind(tenant)
    .bind(-40_000 - tenant.get())
    .bind(sender_identity_id)
    .bind(key)
    .fetch_one(admin.raw())
    .await
    .expect("创建历史投递任务失败")
}

async fn task_status(admin: &support::AdminOnlyPool, task_id: i64) -> (String, i32) {
    sqlx::query_as("SELECT status, attempt_count FROM delivery_tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(admin.raw())
        .await
        .expect("读取投递任务状态失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn claim_and_complete_delivery_task_writes_success_log(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "success").await;
    let task_id = seed_task(&admin, tenant, sender, "delivery-success").await;

    let claimed = claim_due_delivery_task(&app, tenant, sender)
        .await
        .expect("领取投递任务失败")
        .expect("应领取到待投递任务");
    assert_eq!(claimed.id, task_id);
    assert_eq!(claimed.status, "SENDING");
    assert_eq!(claimed.attempt_count, 0);

    assert!(
        complete_delivery_task(&app, tenant, task_id, 9876, Some(42))
            .await
            .expect("完成投递任务失败")
    );
    assert_eq!(task_status(&admin, task_id).await, ("DONE".to_owned(), 1));

    let (ok_count, attempt, message_id): (i64, i32, Option<i64>) = sqlx::query_as(
        "SELECT count(*), min(attempt), max(telegram_message_id)
           FROM delivery_logs AS logs
           JOIN delivery_tasks AS tasks ON tasks.id = logs.task_id
          WHERE logs.task_id = $1 AND tasks.tenant_id = $2 AND logs.ok",
    )
    .bind(task_id)
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("读取成功投递日志失败");
    assert_eq!(ok_count, 1);
    assert_eq!(attempt, 1);
    assert_eq!(message_id, Some(9876));

    assert!(
        !complete_delivery_task(&app, tenant, task_id, 9999, None)
            .await
            .expect("重复完成投递任务失败")
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn delivery_failures_retry_then_enter_dead_letter_and_log_each_attempt(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "retry").await;
    let task_id = seed_task(&admin, tenant, sender, "delivery-retry").await;

    let claimed = claim_due_delivery_task(&app, tenant, sender)
        .await
        .expect("首次领取投递任务失败")
        .expect("首次应领取到投递任务");
    let flood_wait_until: DateTime<Utc> =
        sqlx::query_scalar("SELECT clock_timestamp() + interval '90 seconds'")
            .fetch_one(admin.raw())
            .await
            .expect("构造 FloodWait 截止时间失败");
    let first_failure = record_delivery_failure(
        &app,
        tenant,
        task_id,
        DeliveryTaskFailureInput {
            status: DeliveryTaskFailureStatus::Pending,
            attempt_count: claimed.attempt_count + 1,
            next_retry_at: Some(flood_wait_until),
            error_code: "FLOOD_WAIT".to_owned(),
            error_message: Some("retry after server delay".to_owned()),
            latency_ms: Some(12),
        },
    )
    .await
    .expect("记录 FloodWait 失败");
    assert_eq!(
        first_failure.as_ref().map(|write| write.status.as_str()),
        Some("PENDING")
    );
    assert_eq!(task_status(&admin, task_id).await.0, "PENDING");

    for attempt in 2..=4 {
        sqlx::query("UPDATE delivery_tasks SET next_retry_at = clock_timestamp() WHERE id = $1")
            .bind(task_id)
            .execute(admin.raw())
            .await
            .expect("推进重试时间失败");
        let claimed = claim_due_delivery_task(&app, tenant, sender)
            .await
            .expect("领取重试任务失败")
            .expect("应领取到可重试任务");
        let final_attempt = attempt == 4;
        let result = record_delivery_failure(
            &app,
            tenant,
            task_id,
            DeliveryTaskFailureInput {
                status: if final_attempt {
                    DeliveryTaskFailureStatus::Dead
                } else {
                    DeliveryTaskFailureStatus::Pending
                },
                attempt_count: claimed.attempt_count + 1,
                next_retry_at: if final_attempt {
                    None
                } else {
                    Some(Utc::now())
                },
                error_code: if final_attempt {
                    "CHANNEL_NOT_FOUND".to_owned()
                } else {
                    "TIMEOUT".to_owned()
                },
                error_message: None,
                latency_ms: None,
            },
        )
        .await
        .expect("记录投递失败失败")
        .expect("SENDING 任务应可写入失败状态");
        assert_eq!(result.attempt_count, attempt);
        assert_eq!(
            result.status,
            if final_attempt { "DEAD" } else { "PENDING" }
        );
    }

    assert_eq!(task_status(&admin, task_id).await, ("DEAD".to_owned(), 4));
    let log_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_logs WHERE task_id = $1 AND ok = FALSE")
            .bind(task_id)
            .fetch_one(admin.raw())
            .await
            .expect("核对失败日志数量失败");
    assert_eq!(log_count, 4);
    assert!(
        claim_due_delivery_task(&app, tenant, sender)
            .await
            .expect("死信领取查询失败")
            .is_none()
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn delivery_task_operations_cannot_cross_tenant_boundary(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant_a = seed_tenant(&admin, "delivery-tenant-a").await;
    let tenant_b = seed_tenant(&admin, "delivery-tenant-b").await;
    let sender_a = seed_identity(&admin, tenant_a, "a").await;
    let sender_b = seed_identity(&admin, tenant_b, "b").await;
    let task_b = seed_task(&admin, tenant_b, sender_b, "delivery-cross-tenant").await;

    assert!(
        claim_due_delivery_task(&app, tenant_a, sender_b)
            .await
            .expect("跨租户领取查询失败")
            .is_none()
    );
    assert!(
        !complete_delivery_task(&app, tenant_a, task_b, 1234, None)
            .await
            .expect("跨租户完成查询失败")
    );
    assert!(
        record_delivery_failure(
            &app,
            tenant_a,
            task_b,
            DeliveryTaskFailureInput {
                status: DeliveryTaskFailureStatus::Dead,
                attempt_count: 1,
                next_retry_at: None,
                error_code: "FORBIDDEN".to_owned(),
                error_message: None,
                latency_ms: None,
            },
        )
        .await
        .expect("跨租户失败更新查询失败")
        .is_none()
    );
    assert_eq!(task_status(&admin, task_b).await, ("PENDING".to_owned(), 0));

    let task_a = seed_task(&admin, tenant_a, sender_a, "delivery-own-tenant").await;
    assert!(
        claim_due_delivery_task(&app, tenant_a, sender_a)
            .await
            .expect("本租户领取查询失败")
            .is_some()
    );
    assert_eq!(task_status(&admin, task_a).await.0, "SENDING");
}

/// ADR-0033 条款核对：Bot 开发者条款 §5.2(f) 禁止借他人凭据绕过封禁与限流。
/// 任务绑定创建时的发送身份，同租户的另一个 Bot 领不走，被封 Bot 的积压不会换号发出。
#[sqlx::test(migrations = "../../migrations-test")]
async fn delivery_tasks_are_not_claimed_by_another_bot_of_same_tenant(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let banned_bot = seed_identity(&admin, tenant, "banned").await;
    let other_bot = seed_identity(&admin, tenant, "other").await;
    let task = seed_task(&admin, tenant, banned_bot, "delivery-same-tenant").await;

    assert!(
        claim_due_delivery_task(&app, tenant, other_bot)
            .await
            .expect("同租户另一 Bot 领取查询失败")
            .is_none()
    );
    assert_eq!(task_status(&admin, task).await, ("PENDING".to_owned(), 0));

    let claimed = claim_due_delivery_task(&app, tenant, banned_bot)
        .await
        .expect("原发送身份领取查询失败")
        .expect("原发送身份应能领取");
    assert_eq!(claimed.id, task);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn stale_sending_tasks_are_recovered_after_worker_restart(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "restart").await;
    let task_id = seed_task(&admin, tenant, sender, "delivery-restart").await;

    let claimed = claim_due_delivery_task(&app, tenant, sender)
        .await
        .expect("重启前领取投递任务失败")
        .expect("重启前应领取到投递任务");
    assert_eq!(claimed.status, "SENDING");
    let telegram_random_id = claimed.telegram_random_id;
    let before = load_delivery_task_payload(&app, tenant, task_id)
        .await
        .expect("读取回收前快照失败")
        .expect("任务应存在")
        .payload_snapshot;

    sqlx::query(
        "UPDATE delivery_tasks
            SET updated_at = clock_timestamp() - interval '120 seconds'
          WHERE id = $1",
    )
    .bind(task_id)
    .execute(admin.raw())
    .await
    .expect("构造崩溃遗留任务失败");

    assert_eq!(
        recover_stale_delivery_tasks(&app, tenant, sender, 60)
            .await
            .expect("回收陈旧投递任务失败"),
        1
    );
    assert_eq!(task_status(&admin, task_id).await.0, "PENDING");

    let recovered = claim_due_delivery_task(&app, tenant, sender)
        .await
        .expect("重启后重新领取投递任务失败")
        .expect("重启后应继续领取遗留任务");
    assert_eq!(recovered.id, task_id);
    assert_eq!(recovered.status, "SENDING");
    assert_eq!(recovered.telegram_random_id, telegram_random_id);
    assert_eq!(
        load_delivery_task_payload(&app, tenant, task_id)
            .await
            .expect("读取回收后快照失败")
            .expect("任务应存在")
            .payload_snapshot,
        before
    );
    assert_eq!(
        recover_stale_delivery_tasks(&app, tenant, sender, 60)
            .await
            .expect("重复回收陈旧任务失败"),
        0
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn historical_tasks_without_random_id_are_not_claimed(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "historical").await;
    let task_id = seed_historical_task(&admin, tenant, sender, "delivery-historical").await;

    assert!(
        claim_due_delivery_task(&app, tenant, sender)
            .await
            .expect("历史任务领取查询失败")
            .is_none()
    );
    assert_eq!(
        task_status(&admin, task_id).await,
        ("PENDING".to_owned(), 0)
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn random_id_is_positive_and_immutable_for_app_user(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "random-id").await;
    let task_id = seed_task(&admin, tenant, sender, "delivery-random-id").await;
    let random_id: i64 =
        sqlx::query_scalar("SELECT telegram_random_id FROM delivery_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(admin.raw())
            .await
            .expect("读取 Telegram random_id 失败");
    assert!(random_id > 0);

    let duplicate_result = sqlx::query(
        "INSERT INTO delivery_tasks
            (tenant_id, target_tg_chat_id, sender_identity_id, action, telegram_random_id, idem_key)
         VALUES ($1, $2, $3, 'COPY', $4, 'delivery-random-id-duplicate')",
    )
    .bind(tenant)
    .bind(-30_000 - tenant.get())
    .bind(sender)
    .bind(random_id)
    .execute(admin.raw())
    .await
    .expect_err("同租户发送身份和 peer 下的 random_id 应唯一");
    assert!(
        duplicate_result
            .to_string()
            .contains("uq_task_telegram_random_id")
    );

    let mut app_tx = app.begin().await.expect("开启 app_user 事务失败");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.get().to_string())
        .execute(&mut *app_tx)
        .await
        .expect("设置 app_user 租户上下文失败");
    let status_update = sqlx::query("UPDATE delivery_tasks SET status = 'FAILED' WHERE id = $1")
        .bind(task_id)
        .execute(&mut *app_tx)
        .await
        .expect("app_user 正常更新任务状态失败");
    assert_eq!(status_update.rows_affected(), 1);
    let app_error = sqlx::query("UPDATE delivery_tasks SET telegram_random_id = $2 WHERE id = $1")
        .bind(task_id)
        .bind(random_id + 1)
        .execute(&mut *app_tx)
        .await
        .expect_err("app_user 不应获得 telegram_random_id 的 UPDATE 权限");
    app_tx.rollback().await.expect("回滚 app_user 事务失败");
    assert!(app_error.to_string().contains("permission denied"));

    let mut owner_tx = admin.raw().begin().await.expect("开启 owner 事务失败");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.get().to_string())
        .execute(&mut *owner_tx)
        .await
        .expect("设置管理员租户上下文失败");
    let owner_error =
        sqlx::query("UPDATE delivery_tasks SET telegram_random_id = $2 WHERE id = $1")
            .bind(task_id)
            .bind(random_id + 1)
            .execute(&mut *owner_tx)
            .await
            .expect_err("已分配 Telegram random_id 不应可变更");
    owner_tx.rollback().await.expect("回滚 owner 事务失败");
    assert!(
        owner_error
            .to_string()
            .contains("delivery payload snapshot is immutable")
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn delivery_snapshot_and_routing_cannot_be_rewritten(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "snapshot").await;
    let task_id = seed_task(&admin, tenant, sender, "snapshot").await;
    for statement in [
        "UPDATE delivery_tasks SET payload_snapshot = NULL WHERE id = $1",
        "UPDATE delivery_tasks SET payload_snapshot = '{}'::jsonb WHERE id = $1",
        "UPDATE delivery_tasks SET target_tg_chat_id = -9999 WHERE id = $1",
        "UPDATE delivery_tasks SET sender_identity_id = sender_identity_id + 1 WHERE id = $1",
        "UPDATE delivery_tasks SET action = 'FORWARD' WHERE id = $1",
        "UPDATE delivery_tasks SET tenant_event_id = NULL WHERE id = $1",
        "UPDATE delivery_tasks SET rule_id = 7 WHERE id = $1",
        "UPDATE delivery_tasks SET rule_version = 2 WHERE id = $1",
        "UPDATE delivery_tasks SET idem_key = 'changed' WHERE id = $1",
        "UPDATE delivery_tasks SET telegram_random_id = 99 WHERE id = $1",
        "UPDATE delivery_tasks SET tenant_id = tenant_id + 1 WHERE id = $1",
        "UPDATE delivery_tasks SET id = id + 1 WHERE id = $1",
    ] {
        let mut transaction = app.begin().await.expect("开启受限事务失败");
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant.get().to_string())
            .execute(&mut *transaction)
            .await
            .expect("设置租户失败");
        let error = sqlx::query(statement)
            .bind(task_id)
            .execute(&mut *transaction)
            .await
            .expect_err("不可变列应拒绝改写");
        let code = error.as_database_error().and_then(|error| error.code());
        assert!(matches!(code.as_deref(), Some("42501" | "428C9")));
        transaction.rollback().await.expect("回滚受限事务失败");
    }
    for statement in [
        "UPDATE delivery_tasks SET payload_snapshot = NULL WHERE id = $1",
        "UPDATE delivery_tasks SET target_tg_chat_id = -9999 WHERE id = $1",
        "UPDATE delivery_tasks SET action = 'FORWARD' WHERE id = $1",
    ] {
        let error = sqlx::query(statement)
            .bind(task_id)
            .execute(admin.raw())
            .await
            .expect_err("触发器应拒绝改写");
        assert!(
            error
                .to_string()
                .contains("delivery payload snapshot is immutable")
        );
    }
    let claimed = claim_due_delivery_task(&app, tenant, sender)
        .await
        .expect("正常领取失败")
        .expect("任务应可领取");
    assert_eq!(claimed.id, task_id);
    assert!(
        complete_delivery_task(&app, tenant, task_id, 1234, None)
            .await
            .expect("正常状态更新失败")
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn legacy_tasks_with_random_id_but_no_snapshot_remain_quarantined(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "legacy-snapshot").await;
    let task_id: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks
          (tenant_id, target_tg_chat_id, sender_identity_id, action, telegram_random_id,
           status, updated_at)
         VALUES ($1, -2001, $2, 'COPY', 12345, 'SENDING',
                 clock_timestamp() - interval '120 seconds') RETURNING id",
    )
    .bind(tenant)
    .bind(sender)
    .fetch_one(admin.raw())
    .await
    .expect("构造迁移前记录失败");
    assert_eq!(
        recover_stale_delivery_tasks(&app, tenant, sender, 60)
            .await
            .expect("查询回收失败"),
        0
    );
    sqlx::query("UPDATE delivery_tasks SET status = 'PENDING' WHERE id = $1")
        .bind(task_id)
        .execute(admin.raw())
        .await
        .expect("恢复待领取状态失败");
    assert!(
        claim_due_delivery_task(&app, tenant, sender)
            .await
            .expect("查询领取失败")
            .is_none()
    );
    let error = sqlx::query("UPDATE delivery_tasks SET payload_snapshot = $2 WHERE id = $1")
        .bind(task_id)
        .bind(serde_json::json!({
            "source": {"tg_chat_id": -1001, "tg_message_id": 42, "media_type": "TEXT"},
            "message": {"text": "unproven", "parse_mode": "HTML"}
        }))
        .execute(admin.raw())
        .await
        .expect_err("不得静默回填历史快照");
    assert!(
        error
            .to_string()
            .contains("delivery payload snapshot is immutable")
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn malformed_delivery_snapshots_are_rejected_at_insert(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "delivery-tenant-a").await;
    let sender = seed_identity(&admin, tenant, "invalid-snapshot").await;
    let valid = serde_json::json!({
        "source": {"tg_chat_id": -1001, "tg_message_id": 42, "media_type": "TEXT"},
        "message": {"text": "valid", "parse_mode": "HTML"}
    });
    let mut invalid = vec![
        serde_json::json!({}),
        serde_json::Value::Null,
        serde_json::json!([]),
    ];
    for (section, field) in [
        ("source", "tg_chat_id"),
        ("source", "tg_message_id"),
        ("source", "media_type"),
        ("message", "text"),
        ("message", "parse_mode"),
    ] {
        let mut missing = valid.clone();
        missing[section]
            .as_object_mut()
            .expect("快照应是对象")
            .remove(field);
        invalid.push(missing);
    }
    for snapshot in invalid {
        let result = tgm_db::tenant::tenant_tx(&app, tenant, |connection| Box::pin(async move {
            sqlx::query("INSERT INTO delivery_tasks
                (tenant_id, target_tg_chat_id, sender_identity_id, action, telegram_random_id, payload_snapshot)
                VALUES ($1, -2001, $2, 'COPY', 99, $3)")
                .bind(tenant).bind(sender).bind(snapshot).execute(connection).await
        })).await.expect_err("不完整快照应被 CHECK 拒绝");
        assert_eq!(
            result
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref(),
            Some("23514")
        );
    }
}
