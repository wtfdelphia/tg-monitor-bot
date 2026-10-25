use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_app::pipeline::{RuleWorker, RuleWorkerConfig, RuleWorkerExit};
use tgm_core::ids::TenantId;
use tgm_db::repo::{RuleUpdateInput, copy_rule_replacements, update_rule};
use tgm_db::tenant_retention::{Category, try_lock};

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn as_role(admin: &PgPool, role: &str, password: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(
            admin
                .connect_options()
                .as_ref()
                .clone()
                .username(role)
                .password(password),
        )
        .await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT current_user")
            .fetch_one(&pool)
            .await?,
        role
    );
    Ok(pool)
}

async fn seed(admin: &PgPool) -> Result<(TenantId, i64, i64, i64), sqlx::Error> {
    sqlx::raw_sql("INSERT INTO tenants(name,owner_user_id) VALUES ('worker-retention',99001);
        INSERT INTO identities(tenant_id,kind) SELECT id,'bot' FROM tenants;
        INSERT INTO source_subscriptions(tenant_id,tg_chat_id,receiver_identity_id) SELECT tenant_id,-1001,id FROM identities;
        INSERT INTO target_channels(tenant_id,tg_chat_id,sender_identity_id) SELECT tenant_id,-1002,id FROM identities;
        INSERT INTO keywords(tenant_id,word,match_type) SELECT id,'urgent','exact' FROM tenants;
        INSERT INTO rules(tenant_id,keyword_id,target_ref,action,version,enabled) SELECT words.tenant_id,words.id,targets.id,'COPY',1,true FROM keywords words JOIN target_channels targets ON targets.tenant_id=words.tenant_id;
        INSERT INTO rule_versions(tenant_id,rule_id,version,snapshot,created_at) SELECT tenant_id,id,1,'{}',now()-interval '400 days' FROM rules;
        INSERT INTO canonical_events(adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,text,media_type) SELECT 'bot',id,-1001,1,now(),'urgent','TEXT' FROM identities;
        INSERT INTO tenant_events(tenant_id,canonical_event_id,source_ref) SELECT sources.tenant_id,events.id,sources.id FROM source_subscriptions sources JOIN canonical_events events ON events.receiver_identity_id=sources.receiver_identity_id;")
        .execute(admin).await?;
    sqlx::query_as("SELECT tenant_id,id,keyword_id,target_ref FROM rules")
        .fetch_one(admin)
        .await
}

async fn wait_until(admin: &PgPool, query: &'static str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if sqlx::query_scalar::<_, bool>(query)
            .fetch_one(admin)
            .await?
        {
            return Ok(());
        }
        assert!(Instant::now() < deadline, "未观察到期望数据库状态");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn worker_replans_after_semantic_update_and_replacement_copy(admin: PgPool) -> TestResult {
    let app = as_role(&admin, "app_user", "apw").await?;
    let cleanup = as_role(&admin, "tenant_cleanup", "tcpw").await?;
    let (tenant, rule, keyword, target) = seed(&admin).await?;
    for copy_replacements in [false, true] {
        sqlx::query("DELETE FROM delivery_tasks")
            .execute(&admin)
            .await?;
        sqlx::query("UPDATE tenant_events SET processed_at=NULL")
            .execute(&admin)
            .await?;
        sqlx::query("UPDATE rule_versions SET created_at=now()-interval '400 days'")
            .execute(&admin)
            .await?;
        let mut held = cleanup.begin().await?;
        sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
            .bind(tenant.get().to_string())
            .execute(&mut *held)
            .await?;
        assert!(try_lock(&mut held, tenant, Category::RuleVersions).await?);
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let worker = RuleWorker::new(
            app.clone(),
            RuleWorkerConfig {
                poll_interval: Duration::from_millis(10),
                event_batch_size: 10,
            },
        );
        let running = tokio::spawn(worker.run(stopped));
        wait_until(&admin,"SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.usename='app_user' AND locks.locktype='advisory' AND NOT locks.granted)").await?;
        let next_version = if copy_replacements {
            sqlx::query("INSERT INTO rules(tenant_id,keyword_id,target_ref,action,version,enabled) VALUES ($1,$2,$3,'COPY',1,false)")
                .bind(tenant).bind(keyword).bind(target).execute(&admin).await?;
            let source: i64 = sqlx::query_scalar("SELECT max(id) FROM rules")
                .fetch_one(&admin)
                .await?;
            sqlx::query("INSERT INTO rule_replacements(tenant_id,rule_id,seq,pattern,replacement) VALUES ($1,$2,1,'urgent','copied')")
                .bind(tenant).bind(source).execute(&admin).await?;
            copy_rule_replacements(&app, tenant, source, vec![rule]).await?[0].version
        } else {
            update_rule(
                &app,
                tenant,
                rule,
                RuleUpdateInput {
                    keyword_id: keyword,
                    source_ref: None,
                    media_type: None,
                    target_ref: target,
                    action: "COPY".to_owned(),
                    priority: 0,
                    stop_processing: false,
                    template: Some("updated {text}".to_owned()),
                    enabled: true,
                    conditions: Vec::new(),
                    replacements: Vec::new(),
                },
            )
            .await?
            .version
        };
        sqlx::query("DELETE FROM rule_versions WHERE tenant_id=$1")
            .bind(tenant)
            .execute(&mut *held)
            .await?;
        held.commit().await?;
        wait_until(
            &admin,
            "SELECT bool_and(processed_at IS NOT NULL) FROM tenant_events",
        )
        .await?;
        stop.send(true)?;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), running).await???,
            RuleWorkerExit::Shutdown
        );
        let actual: i32 =
            sqlx::query_scalar("SELECT rule_version FROM delivery_tasks ORDER BY id DESC LIMIT 1")
                .fetch_one(&admin)
                .await?;
        assert_eq!(actual, next_version);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM delivery_tasks WHERE rule_version=1"
            )
            .fetch_one(&admin)
            .await?,
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM rule_versions WHERE rule_id=$1 AND version=$2"
            )
            .bind(rule)
            .bind(next_version)
            .fetch_one(&admin)
            .await?,
            1
        );
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn missing_current_snapshot_stops_without_marking_event_processed(
    admin: PgPool,
) -> TestResult {
    let app = as_role(&admin, "app_user", "apw").await?;
    seed(&admin).await?;
    sqlx::query("DELETE FROM rule_versions")
        .execute(&admin)
        .await?;
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let worker = RuleWorker::new(app, RuleWorkerConfig::default());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), worker.run(stopped))
            .await?
            .is_err()
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT bool_and(processed_at IS NULL) FROM tenant_events")
            .fetch_one(&admin)
            .await?
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_tasks")
            .fetch_one(&admin)
            .await?,
        0
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn worker_replans_when_snapshot_disappears_before_initial_load(admin: PgPool) -> TestResult {
    let app = as_role(&admin, "app_user", "apw").await?;
    let (tenant, rule, _, _) = seed(&admin).await?;
    let mut held = admin.begin().await?;
    sqlx::raw_sql(
        "SET LOCAL ROLE tgm_owner; LOCK TABLE rule_versions IN ACCESS EXCLUSIVE MODE; RESET ROLE;",
    )
    .execute(&mut *held)
    .await?;
    sqlx::query("UPDATE rules SET version=2 WHERE tenant_id=$1 AND id=$2")
        .bind(tenant)
        .bind(rule)
        .execute(&mut *held)
        .await?;
    sqlx::query(
        "INSERT INTO rule_versions(tenant_id,rule_id,version,snapshot) VALUES ($1,$2,2,'{}')",
    )
    .bind(tenant)
    .bind(rule)
    .execute(&mut *held)
    .await?;
    sqlx::query("DELETE FROM rule_versions WHERE tenant_id=$1 AND version=1")
        .bind(tenant)
        .execute(&mut *held)
        .await?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(
        RuleWorker::new(
            app,
            RuleWorkerConfig {
                poll_interval: Duration::from_millis(10),
                event_batch_size: 10,
            },
        )
        .run(stopped),
    );
    wait_until(&admin,"SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.usename='app_user' AND locks.relation='rule_versions'::regclass AND NOT locks.granted)").await?;
    held.commit().await?;
    wait_until(
        &admin,
        "SELECT bool_and(processed_at IS NOT NULL) FROM tenant_events",
    )
    .await?;
    stop.send(true)?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), running).await???,
        RuleWorkerExit::Shutdown
    );
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT rule_version FROM delivery_tasks")
            .fetch_one(&admin)
            .await?,
        2
    );
    Ok(())
}
