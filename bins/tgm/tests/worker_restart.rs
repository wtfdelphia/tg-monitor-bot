use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;
use sqlx::types::chrono::Utc;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, CanonicalEventWrite,
    RuleCreateInput, create_rule, fanout_canonical_event, insert_canonical_event,
};
use tgm_db::tenant::tenant_tx;
use tokio::time::{Instant, sleep};

struct WorkerProcess {
    child: Child,
}

impl WorkerProcess {
    fn start(database_url: &str) -> Result<Self> {
        Self::start_role(database_url, "fanout")
    }

    fn start_role(database_url: &str, role: &str) -> Result<Self> {
        let child = Command::new(env!("CARGO_BIN_EXE_tgm"))
            .args(["serve", "--role", role])
            .env_clear()
            .env("TGM_PROFILE", "development")
            .env("TGM_DB_URL_APP", database_url)
            .env("TGM_REDIS_URL", "redis://127.0.0.1:56379")
            .env("RUST_LOG", "info,sqlx=warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("启动 {role} 子进程失败"))?;
        Ok(Self { child })
    }

    fn assert_running(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            let mut stderr = String::new();
            if let Some(mut output) = self.child.stderr.take() {
                output.read_to_string(&mut stderr)?;
            }
            anyhow::bail!("worker 子进程提前退出：{status}，{stderr}");
        }
        Ok(())
    }

    fn kill_and_wait(mut self) -> Result<()> {
        self.child.kill().context("强杀 worker 子进程失败")?;
        let status = self.child.wait().context("回收 worker 子进程失败")?;
        assert!(!status.success(), "必须实际强杀进程，不能用正常退出替代");
        Ok(())
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn event_progress(pool: &PgPool, tenant: TenantId, event_id: i64) -> Result<(bool, i64)> {
    Ok(tenant_tx(pool, tenant, |connection| {
        Box::pin(async move {
            sqlx::query_as::<_, (bool, i64)>(
                "SELECT ce.fanned_out,
                        (SELECT count(*) FROM tenant_events WHERE canonical_event_id = ce.id)
                   FROM canonical_events AS ce
                  WHERE ce.id = $1",
            )
            .bind(event_id)
            .fetch_one(connection)
            .await
        })
    })
    .await?)
}

async fn wait_for_progress(
    pool: &PgPool,
    tenant: TenantId,
    event_id: i64,
    worker: &mut WorkerProcess,
    finished: bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        worker.assert_running()?;
        let progress = event_progress(pool, tenant, event_id).await?;
        if progress == (finished, 1) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("fanout 未在期限内达到 ({finished}, 1)，实际为 {progress:?}");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_worker_disconnect(pool: &PgPool, application_name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let connections: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE datname = current_database()
                AND application_name = $1",
        )
        .bind(application_name)
        .fetch_one(pool)
        .await?;
        if connections == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("强杀后数据库仍有 {connections} 个 worker 连接");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn seed_event(
    pool: &PgPool,
    receiver_identity_id: IdentityId,
    message_id: i64,
) -> Result<CanonicalEventWrite> {
    Ok(insert_canonical_event(
        pool,
        &CanonicalEventInput {
            adapter: CanonicalAdapter::MtProto,
            receiver_identity_id,
            tg_chat_id: -1001,
            tg_message_id: message_id,
            event_type: CanonicalEventType::MessageCreated,
            edit_version: 0,
            update_id: None,
            media_group_id: None,
            sender_id: None,
            message_date: Utc::now(),
            text: Some("restart regression".to_owned()),
            caption: None,
            media_type: Some("TEXT".to_owned()),
            content_hash: None,
            file_unique_id: None,
            file_name: None,
            file_size: None,
            mime_type: None,
            raw_file_id: None,
        },
    )
    .await?)
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn fanout_process_recovers_partial_work_after_hard_kill(admin: PgPool) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let app_options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(55432)
        .username("app_user")
        .password("apw")
        .database(&database);
    let app = PgPool::connect_with(app_options).await?;
    let current_user: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await?;
    assert_eq!(current_user, "app_user");
    let app_url = format!(
        "postgres://app_user:apw@127.0.0.1:55432/{database}?application_name=tgm-restart-regression&options=-c%20client_connection_check_interval%3D100ms"
    );
    let tenant = sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id)
         VALUES ('restart-matching-tenant', 2001) RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    let other_tenant = sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id)
         VALUES ('restart-other-tenant', 2002) RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    let identity = sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status)
         VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await?;
    sqlx::query(
        "INSERT INTO source_subscriptions
             (tenant_id, tg_chat_id, receiver_identity_id, enabled)
         VALUES ($1, -1001, $2, TRUE)",
    )
    .bind(tenant)
    .bind(identity)
    .execute(&admin)
    .await?;

    let event = seed_event(&app, identity, 1).await?;
    let mut checkpoint_blocker = admin.begin().await?;
    sqlx::query("SELECT id FROM canonical_events WHERE id = $1 FOR NO KEY UPDATE")
        .bind(event.id)
        .fetch_one(&mut *checkpoint_blocker)
        .await?;

    let mut first_worker = WorkerProcess::start(&app_url)?;
    wait_for_progress(&app, tenant, event.id, &mut first_worker, false).await?;
    first_worker.kill_and_wait()?;
    wait_for_worker_disconnect(&app, "tgm-restart-regression").await?;
    assert_eq!(event_progress(&app, tenant, event.id).await?, (false, 1));
    checkpoint_blocker.rollback().await?;
    assert_eq!(event_progress(&app, tenant, event.id).await?, (false, 1));

    let mut restarted_worker = WorkerProcess::start(&app_url)?;
    wait_for_progress(&app, tenant, event.id, &mut restarted_worker, true).await?;
    assert_eq!(
        event_progress(&app, other_tenant, event.id).await?,
        (true, 0)
    );
    let replay = seed_event(&app, identity, 1).await?;
    assert_eq!(replay.id, event.id);
    assert!(!replay.inserted);

    let next_event = seed_event(&app, identity, 2).await?;
    wait_for_progress(&app, tenant, next_event.id, &mut restarted_worker, true).await?;
    assert_eq!(event_progress(&app, tenant, event.id).await?, (true, 1));
    assert_eq!(
        event_progress(&app, other_tenant, next_event.id).await?,
        (true, 0)
    );
    restarted_worker.kill_and_wait()?;
    app.close().await;
    Ok(())
}

async fn rule_progress(
    pool: &PgPool,
    tenant: TenantId,
    event_id: i64,
) -> Result<(bool, bool, bool, i64)> {
    Ok(tenant_tx(pool, tenant, |connection| {
        Box::pin(async move {
            sqlx::query_as(
                "SELECT te.processed_at IS NOT NULL, te.matched, ce.matched_any,
                        (SELECT count(*) FROM delivery_tasks WHERE tenant_event_id = te.id)
                   FROM tenant_events AS te
                   JOIN canonical_events AS ce ON ce.id = te.canonical_event_id
                  WHERE te.id = $1",
            )
            .bind(event_id)
            .fetch_one(connection)
            .await
        })
    })
    .await?)
}

async fn wait_for_rule_commit_block(
    pool: &PgPool,
    blocker_pid: i32,
    worker: &mut WorkerProcess,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        worker.assert_running()?;
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND application_name = 'tgm-rule-restart-regression'
                   AND usename = 'app_user'
                   AND state = 'active'
                   AND wait_event_type = 'Lock'
                   AND query LIKE '%UPDATE canonical_events AS ce%'
                   AND $1 = ANY(pg_blocking_pids(pid))
             )",
        )
        .bind(blocker_pid)
        .fetch_one(pool)
        .await?;
        if waiting {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("规则 worker 未在提交前的 canonical 更新处被测试锁阻塞");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_rule_progress(
    pool: &PgPool,
    tenant: TenantId,
    event_id: i64,
    worker: &mut WorkerProcess,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        worker.assert_running()?;
        let progress = rule_progress(pool, tenant, event_id).await?;
        if progress == (true, true, true, 2) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("规则 worker 未完成原子入队，实际为 {progress:?}");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn assert_rule_task_snapshot(
    pool: &PgPool,
    tenant: TenantId,
    event_id: i64,
    rule_ids: &[i64],
) -> Result<()> {
    let snapshots = tenant_tx(pool, tenant, |connection| {
        Box::pin(async move {
            sqlx::query_as::<_, (i64, i32, String, String)>(
                "SELECT rule_id, rule_version, action, status
                   FROM delivery_tasks WHERE tenant_event_id = $1 ORDER BY rule_id",
            )
            .bind(event_id)
            .fetch_all(connection)
            .await
        })
    })
    .await?;
    assert_eq!(
        snapshots,
        rule_ids
            .iter()
            .map(|rule_id| (*rule_id, 1, "COPY".to_owned(), "PENDING".to_owned()))
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn rule_process_rolls_back_uncommitted_tasks_and_recovers_after_hard_kill(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let app = PgPool::connect_with(
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(55432)
            .username("app_user")
            .password("apw")
            .database(&database),
    )
    .await?;
    let current_user: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await?;
    assert_eq!(current_user, "app_user");
    let bypasses_rls: bool = sqlx::query_scalar(
        "SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(&app)
    .await?;
    assert!(!bypasses_rls);
    let app_url = format!(
        "postgres://app_user:apw@127.0.0.1:55432/{database}?application_name=tgm-rule-restart-regression&options=-c%20client_connection_check_interval%3D100ms"
    );
    let tenant = sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id)
         VALUES ('rule-restart-matching-tenant', 3001) RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    let other_tenant = sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id)
         VALUES ('rule-restart-other-tenant', 3002) RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    let identity = sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status)
         VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await?;
    sqlx::query(
        "INSERT INTO source_subscriptions
             (tenant_id, tg_chat_id, receiver_identity_id, enabled)
         VALUES ($1, -1001, $2, TRUE)",
    )
    .bind(tenant)
    .bind(identity)
    .execute(&admin)
    .await?;
    let keyword_id: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'restart', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await?;
    let mut rule_ids = Vec::new();
    for target_chat_id in [-3001_i64, -3002] {
        let target_ref: i64 = sqlx::query_scalar(
            "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
             VALUES ($1, $2, $3, TRUE) RETURNING id",
        )
        .bind(tenant)
        .bind(target_chat_id)
        .bind(identity)
        .fetch_one(&admin)
        .await?;
        let rule = create_rule(
            &app,
            tenant,
            RuleCreateInput {
                keyword_id,
                source_ref: None,
                media_type: None,
                target_ref,
                action: "COPY".to_owned(),
                priority: 0,
                stop_processing: false,
                template: None,
                enabled: true,
                conditions: Vec::new(),
                replacements: Vec::new(),
            },
        )
        .await?;
        rule_ids.push(rule.id);
    }

    let event = seed_event(&app, identity, 1).await?;
    let tenant_event = fanout_canonical_event(&app, tenant, event.id)
        .await?
        .context("来源订阅应生成租户事件")?;
    let mut checkpoint_blocker = admin.begin().await?;
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *checkpoint_blocker)
        .await?;
    sqlx::query("SELECT id FROM canonical_events WHERE id = $1 FOR NO KEY UPDATE")
        .bind(event.id)
        .fetch_one(&mut *checkpoint_blocker)
        .await?;

    let mut first_worker = WorkerProcess::start_role(&app_url, "rule")?;
    wait_for_rule_commit_block(&app, blocker_pid, &mut first_worker).await?;
    assert_eq!(
        rule_progress(&app, tenant, tenant_event.id).await?,
        (false, false, false, 0),
        "提交前已执行的入队与处理标记不得被其他连接看见"
    );
    first_worker.kill_and_wait()?;
    wait_for_worker_disconnect(&app, "tgm-rule-restart-regression").await?;
    assert_eq!(
        rule_progress(&app, tenant, tenant_event.id).await?,
        (false, false, false, 0),
        "强杀必须回滚整个入队事务"
    );
    checkpoint_blocker.rollback().await?;
    assert_eq!(
        rule_progress(&app, tenant, tenant_event.id).await?,
        (false, false, false, 0)
    );

    let mut restarted_worker = WorkerProcess::start_role(&app_url, "rule")?;
    wait_for_rule_progress(&app, tenant, tenant_event.id, &mut restarted_worker).await?;
    assert_rule_task_snapshot(&app, tenant, tenant_event.id, &rule_ids).await?;
    restarted_worker.kill_and_wait()?;
    wait_for_worker_disconnect(&app, "tgm-rule-restart-regression").await?;
    let replay = seed_event(&app, identity, 1).await?;
    assert_eq!(replay.id, event.id);
    assert!(!replay.inserted);
    let replay_projection = fanout_canonical_event(&app, tenant, replay.id)
        .await?
        .context("重放应返回已有租户事件")?;
    assert_eq!(replay_projection.id, tenant_event.id);
    assert!(!replay_projection.inserted);

    let next_event = seed_event(&app, identity, 2).await?;
    let next_projection = fanout_canonical_event(&app, tenant, next_event.id)
        .await?
        .context("新消息应生成租户事件")?;
    let mut next_worker = WorkerProcess::start_role(&app_url, "rule")?;
    wait_for_rule_progress(&app, tenant, next_projection.id, &mut next_worker).await?;
    assert_eq!(
        rule_progress(&app, tenant, tenant_event.id).await?,
        (true, true, true, 2),
        "已提交事件重放并再次重启不得重复入队"
    );
    assert_rule_task_snapshot(&app, tenant, tenant_event.id, &rule_ids).await?;
    assert_rule_task_snapshot(&app, tenant, next_projection.id, &rule_ids).await?;
    let other_counts = tenant_tx(&app, other_tenant, |connection| {
        Box::pin(async move {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT (SELECT count(*) FROM tenant_events),
                        (SELECT count(*) FROM delivery_tasks)",
            )
            .fetch_one(connection)
            .await
        })
    })
    .await?;
    assert_eq!(other_counts, (0, 0));
    next_worker.kill_and_wait()?;
    wait_for_worker_disconnect(&app, "tgm-rule-restart-regression").await?;
    app.close().await;
    Ok(())
}
