use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{DeliveryTaskInput, EnqueueError, enqueue_delivery_tasks};
use tgm_db::tenant_retention::{Category, cleanup_batch, lock_rule_versions, try_lock};

mod support;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Seed {
    tenant: TenantId,
    identity: IdentityId,
    rule: i64,
    event: i64,
}

impl Seed {
    fn task(&self, version: i32) -> DeliveryTaskInput {
        DeliveryTaskInput {
            rule_id: self.rule,
            rule_version: version,
            target_tg_chat_id: -1002,
            sender_identity_id: self.identity,
            action: "COPY".to_owned(),
            payload_snapshot: serde_json::json!({
                "source": {"tg_chat_id": -1001, "tg_message_id": 1, "media_type": "TEXT"},
                "message": {"text": "urgent", "parse_mode": "HTML"}
            }),
            media_relay: false,
            dead_error_code: None,
        }
    }
}

async fn setup(injected: PgPool) -> Result<(PgPool, PgPool, support::AdminOnlyPool), sqlx::Error> {
    let (app, admin) = support::pools(injected).await;
    let cleanup = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(
            admin
                .raw()
                .connect_options()
                .as_ref()
                .clone()
                .username("tenant_cleanup")
                .password("tcpw"),
        )
        .await?;
    support::assert_role(&app, "app_user").await;
    support::assert_role(&cleanup, "tenant_cleanup").await;
    support::assert_not_bypassrls(&cleanup).await;
    Ok((app, cleanup, admin))
}

async fn seed(admin: &PgPool, owner: i64, plan: &str, status: &str) -> Result<Seed, sqlx::Error> {
    let tenant: TenantId = sqlx::query_scalar("INSERT INTO tenants(name,owner_user_id,plan,status) VALUES ('versions',$1,$2,$3) RETURNING id")
        .bind(owner).bind(plan).bind(status).fetch_one(admin).await?;
    let identity: IdentityId =
        sqlx::query_scalar("INSERT INTO identities(tenant_id,kind) VALUES ($1,'bot') RETURNING id")
            .bind(tenant)
            .fetch_one(admin)
            .await?;
    let keyword: i64 = sqlx::query_scalar(
        "INSERT INTO keywords(tenant_id,word,match_type) VALUES ($1,'urgent','exact') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin)
    .await?;
    let target: i64 = sqlx::query_scalar("INSERT INTO target_channels(tenant_id,tg_chat_id,sender_identity_id) VALUES ($1,-1002,$2) RETURNING id")
        .bind(tenant).bind(identity).fetch_one(admin).await?;
    let rule: i64 = sqlx::query_scalar("INSERT INTO rules(tenant_id,keyword_id,target_ref,action,version,enabled) VALUES ($1,$2,$3,'COPY',20,false) RETURNING id")
        .bind(tenant).bind(keyword).bind(target).fetch_one(admin).await?;
    sqlx::query("INSERT INTO rule_versions(tenant_id,rule_id,version,snapshot,created_at) SELECT $1,$2,number,'{}',now()-interval '400 days' FROM generate_series(1,20) number")
        .bind(tenant).bind(rule).execute(admin).await?;
    let source: i64 = sqlx::query_scalar("INSERT INTO source_subscriptions(tenant_id,tg_chat_id,receiver_identity_id) VALUES ($1,-1001,$2) RETURNING id")
        .bind(tenant).bind(identity).fetch_one(admin).await?;
    let canonical: i64 = sqlx::query_scalar("INSERT INTO canonical_events(adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,text) VALUES ('bot',$1,-1001,1,now(),'urgent') RETURNING id")
        .bind(identity).fetch_one(admin).await?;
    let event: i64 = sqlx::query_scalar("INSERT INTO tenant_events(tenant_id,canonical_event_id,source_ref) VALUES ($1,$2,$3) RETURNING id")
        .bind(tenant).bind(canonical).bind(source).fetch_one(admin).await?;
    Ok(Seed {
        tenant,
        identity,
        rule,
        event,
    })
}

async fn context(connection: &mut sqlx::PgConnection, tenant: TenantId) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(connection)
        .await?;
    Ok(())
}

async fn wait_for_lock(admin: &PgPool, role: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.usename=$1 AND NOT locks.granted)")
            .bind(role).fetch_one(admin).await?;
        if waiting {
            return Ok(());
        }
        assert!(Instant::now() < deadline, "未观察到 {role} 锁等待");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn direct_role_delete_preserves_current_every_task_status_plan_and_other_tenant(
    injected: PgPool,
) -> TestResult {
    let (_app, cleanup, admin) = setup(injected).await?;
    let free = seed(admin.raw(), 98001, "free", "suspended").await?;
    let pro = seed(admin.raw(), 98002, "pro", "deleted").await?;
    for fixture in [&free, &pro] {
        sqlx::query("DELETE FROM rule_versions WHERE tenant_id=$1 AND version BETWEEN 11 AND 19")
            .bind(fixture.tenant)
            .execute(admin.raw())
            .await?;
        sqlx::query("UPDATE rule_versions SET created_at=now() WHERE tenant_id=$1 AND version=8")
            .bind(fixture.tenant)
            .execute(admin.raw())
            .await?;
        sqlx::query("UPDATE rule_versions SET created_at=now()-interval '200 days' WHERE tenant_id=$1 AND version=9")
            .bind(fixture.tenant).execute(admin.raw()).await?;
        for (offset, status) in ["PENDING", "SENDING", "DONE", "FAILED", "DEAD", "CANCELLED"]
            .iter()
            .enumerate()
        {
            sqlx::query("INSERT INTO delivery_tasks(tenant_id,rule_id,rule_version,target_tg_chat_id,sender_identity_id,action,status) VALUES ($1,$2,$3,-1002,$4,'COPY',$5)")
                .bind(fixture.tenant).bind(fixture.rule).bind(i32::try_from(offset)?+1).bind(fixture.identity).bind(status)
                .execute(admin.raw()).await?;
        }
    }
    sqlx::query("INSERT INTO delivery_tasks(tenant_id,rule_id,rule_version,target_tg_chat_id,sender_identity_id,action) VALUES ($1,$2,10,-1002,$3,'COPY')")
        .bind(pro.tenant).bind(free.rule).bind(pro.identity).execute(admin.raw()).await?;
    let mut transaction = cleanup.begin().await?;
    context(&mut transaction, free.tenant).await?;
    let mut deleted: Vec<i32> = sqlx::query_scalar("DELETE FROM rule_versions RETURNING version")
        .fetch_all(&mut *transaction)
        .await?;
    deleted.sort();
    assert_eq!(deleted, [7, 9, 10]);
    transaction.commit().await?;
    assert_eq!(
        cleanup_batch(&cleanup, pro.tenant, Category::RuleVersions).await?,
        Some(2)
    );
    for fixture in [&free, &pro] {
        let versions: Vec<i32> = sqlx::query_scalar(
            "SELECT version FROM rule_versions WHERE tenant_id=$1 ORDER BY version",
        )
        .bind(fixture.tenant)
        .fetch_all(admin.raw())
        .await?;
        assert_eq!(
            versions,
            if fixture.tenant == free.tenant {
                vec![1, 2, 3, 4, 5, 6, 8, 20]
            } else {
                vec![1, 2, 3, 4, 5, 6, 8, 9, 20]
            }
        );
    }
    assert_eq!(
        cleanup_batch(&cleanup, free.tenant, Category::RuleVersions).await?,
        Some(0)
    );
    for query in [
        "SELECT snapshot FROM rule_versions",
        "SELECT payload_snapshot FROM delivery_tasks",
        "UPDATE rules SET version=1",
        "UPDATE rule_versions SET version=1",
        "DELETE FROM rules",
        "TRUNCATE rule_versions",
    ] {
        assert!(
            sqlx::raw_sql(query).execute(&cleanup).await.is_err(),
            "必须拒绝 {query}"
        );
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_first_invalidates_entire_enqueue_and_allows_replan(
    injected: PgPool,
) -> TestResult {
    let (app, cleanup, admin) = setup(injected).await?;
    let fixture = seed(admin.raw(), 98003, "free", "active").await?;
    let mut held = cleanup.begin().await?;
    context(&mut held, fixture.tenant).await?;
    assert!(try_lock(&mut held, fixture.tenant, Category::RuleVersions).await?);
    let pool = app.clone();
    let tenant = fixture.tenant;
    let event = fixture.event;
    let tasks = [fixture.task(20), fixture.task(1)];
    let pending =
        tokio::spawn(
            async move { enqueue_delivery_tasks(&pool, tenant, event, &[], &tasks).await },
        );
    wait_for_lock(admin.raw(), "app_user").await?;
    assert_eq!(
        sqlx::query("DELETE FROM rule_versions WHERE tenant_id=$1 AND version=1")
            .bind(tenant)
            .execute(&mut *held)
            .await?
            .rows_affected(),
        1
    );
    held.commit().await?;
    assert!(matches!(
        pending.await?,
        Err(EnqueueError::VersionUnavailable { version: 1, .. })
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_tasks")
            .fetch_one(admin.raw())
            .await?,
        0
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT processed_at IS NULL FROM tenant_events WHERE id=$1")
            .bind(event)
            .fetch_one(admin.raw())
            .await?
    );
    let writes = enqueue_delivery_tasks(&app, tenant, event, &[], &[fixture.task(20)]).await?;
    let original_random: i64 =
        sqlx::query_scalar("SELECT telegram_random_id FROM delivery_tasks WHERE id=$1")
            .bind(writes[0].id)
            .fetch_one(admin.raw())
            .await?;
    sqlx::query("DELETE FROM rules WHERE tenant_id=$1")
        .bind(tenant)
        .execute(admin.raw())
        .await?;
    let duplicate = enqueue_delivery_tasks(&app, tenant, event, &[], &[fixture.task(20)]).await?;
    assert_eq!(duplicate[0].id, writes[0].id);
    assert!(!duplicate[0].inserted);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT telegram_random_id FROM delivery_tasks WHERE id=$1")
            .bind(writes[0].id)
            .fetch_one(admin.raw())
            .await?,
        original_random
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn enqueue_first_blocks_cleanup_until_task_is_visible(injected: PgPool) -> TestResult {
    let (app, cleanup, admin) = setup(injected).await?;
    let fixture = seed(admin.raw(), 98004, "free", "active").await?;
    sqlx::query("DELETE FROM rule_versions WHERE tenant_id=$1 AND version NOT IN (1,20)")
        .bind(fixture.tenant)
        .execute(admin.raw())
        .await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("SELECT id FROM tenant_events WHERE id=$1 FOR UPDATE")
        .bind(fixture.event)
        .execute(&mut *held)
        .await?;
    let pool = app.clone();
    let tenant = fixture.tenant;
    let event = fixture.event;
    let tasks = [fixture.task(1)];
    let pending =
        tokio::spawn(
            async move { enqueue_delivery_tasks(&pool, tenant, event, &[], &tasks).await },
        );
    wait_for_lock(admin.raw(), "app_user").await?;
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::RuleVersions).await?,
        None
    );
    held.rollback().await?;
    assert!(pending.await??[0].inserted);
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::RuleVersions).await?,
        Some(0)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM rule_versions")
            .fetch_one(admin.raw())
            .await?,
        2
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batches_reload_plan_and_failed_log_rolls_back_versions(injected: PgPool) -> TestResult {
    let (_app, cleanup, admin) = setup(injected).await?;
    let fixture = seed(admin.raw(), 98005, "free", "active").await?;
    sqlx::query("UPDATE rule_versions SET created_at=now()-interval '200 days' WHERE tenant_id=$1")
        .bind(fixture.tenant)
        .execute(admin.raw())
        .await?;
    sqlx::query("UPDATE tenants SET plan='pro' WHERE id=$1")
        .bind(fixture.tenant)
        .execute(admin.raw())
        .await?;
    assert_eq!(
        cleanup_batch(&cleanup, fixture.tenant, Category::RuleVersions).await?,
        Some(0)
    );
    sqlx::query("UPDATE tenants SET plan='free' WHERE id=$1")
        .bind(fixture.tenant)
        .execute(admin.raw())
        .await?;
    let mut broken = admin.raw().begin().await?;
    sqlx::raw_sql("SET LOCAL ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_rule_cleanup CHECK (module <> 'tenant_retention') NOT VALID;").execute(&mut *broken).await?;
    broken.commit().await?;
    assert!(
        cleanup_batch(&cleanup, fixture.tenant, Category::RuleVersions)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM rule_versions")
            .fetch_one(admin.raw())
            .await?,
        20
    );
    let mut restore = admin.raw().begin().await?;
    sqlx::raw_sql(
        "SET LOCAL ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_rule_cleanup;",
    )
    .execute(&mut *restore)
    .await?;
    restore.commit().await?;
    assert_eq!(
        cleanup_batch(&cleanup, fixture.tenant, Category::RuleVersions).await?,
        Some(19)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn unsupported_isolation_fails_without_deleting_or_enqueueing(
    injected: PgPool,
) -> TestResult {
    let (app, _cleanup, admin) = setup(injected).await?;
    let fixture = seed(admin.raw(), 98006, "free", "active").await?;
    let mut transaction = app.begin().await?;
    sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await?;
    assert!(
        lock_rule_versions(&mut transaction, fixture.tenant)
            .await
            .is_err()
    );
    transaction.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn bounded_batches_and_cancellation_keep_uncommitted_rows(injected: PgPool) -> TestResult {
    let (_app, cleanup, admin) = setup(injected).await?;
    let fixture = seed(admin.raw(), 98007, "free", "suspended").await?;
    sqlx::query("UPDATE rules SET version=6001 WHERE tenant_id=$1")
        .bind(fixture.tenant)
        .execute(admin.raw())
        .await?;
    sqlx::query("INSERT INTO rule_versions(tenant_id,rule_id,version,snapshot,created_at) SELECT $1,$2,number,'{}',now()-interval '400 days' FROM generate_series(21,6001) number")
        .bind(fixture.tenant).bind(fixture.rule).execute(admin.raw()).await?;
    assert_eq!(
        cleanup_batch(&cleanup, fixture.tenant, Category::RuleVersions).await?,
        Some(5000)
    );
    let mut held = admin.raw().begin().await?;
    sqlx::query("SELECT version FROM rule_versions WHERE tenant_id=$1 FOR UPDATE")
        .bind(fixture.tenant)
        .execute(&mut *held)
        .await?;
    let pool = cleanup.clone();
    let tenant = fixture.tenant;
    let pending =
        tokio::spawn(async move { cleanup_batch(&pool, tenant, Category::RuleVersions).await });
    wait_for_lock(admin.raw(), "tenant_cleanup").await?;
    pending.abort();
    assert!(pending.await.is_err());
    held.rollback().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM rule_versions")
            .fetch_one(admin.raw())
            .await?,
        1001
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM system_logs WHERE module='tenant_retention'"
        )
        .fetch_one(admin.raw())
        .await?,
        1
    );
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::RuleVersions).await?,
        Some(1000)
    );
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::RuleVersions).await?,
        Some(0)
    );
    Ok(())
}
