use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_db::platform_retention::{LOCK_KEY, RetentionError, check_connection, cleanup_batch};

mod support;

async fn setup(
    injected: PgPool,
) -> Result<(PgPool, support::AdminOnlyPool), Box<dyn std::error::Error>> {
    let (_app, admin) = support::pools(injected).await;
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin.raw())
        .await?;
    let cleanup = PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!(
            "postgres://platform_cleanup:pcpw@127.0.0.1:55432/{database}"
        ))
        .await?;
    support::assert_role(&cleanup, "platform_cleanup").await;
    support::assert_not_bypassrls(&cleanup).await;
    check_connection(&cleanup).await?;
    Ok((cleanup, admin))
}

async fn seed(admin: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("INSERT INTO tenants (name, owner_user_id, status) VALUES
        ('platform-a',89001,'active'),('platform-b',89002,'suspended'),('platform-c',89003,'deleted');
        INSERT INTO system_logs (tenant_id,level,module,event,context,created_at)
        SELECT id,'ERROR','private','sentinel','{\"secret\":\"tenant-value\"}',now()-interval '100 days' FROM tenants;
        INSERT INTO system_logs (level,module,event,context,created_at) VALUES
        ('INFO','platform_retention','cleanup.done','{\"table\":\"system_logs\",\"rows\":2,\"elapsed_ms\":1}',now()-interval '91 days'),
        ('ERROR','private','old','{\"secret\":\"platform-value\"}',now()-interval '100 days'),
        ('INFO','private','recent','{}',now()-interval '89 days');").execute(admin).await?;
    Ok(())
}

async fn wait_for_lock(admin: &PgPool) -> Result<(), sqlx::Error> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid
            WHERE activity.datname=current_database() AND activity.usename='platform_cleanup' AND NOT locks.granted)")
            .fetch_one(admin).await?;
        if waiting {
            return Ok(());
        }
        assert!(Instant::now() < deadline, "未观察到真实行锁等待");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn eligible_platform_rows_only_and_fixed_transaction_logs(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(2));
    let logs: Vec<(Option<i64>, String, String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT tenant_id,level,module,event,context FROM system_logs WHERE module='platform_retention'")
        .fetch_all(admin.raw()).await?;
    assert_eq!(logs.len(), 1);
    assert_eq!(
        (
            &logs[0].0,
            logs[0].1.as_str(),
            logs[0].2.as_str(),
            logs[0].3.as_str()
        ),
        (&None, "INFO", "platform_retention", "cleanup.done")
    );
    let context = &logs[0].4;
    assert_eq!(context["table"], "system_logs");
    assert_eq!(context["rows"], 2);
    assert_eq!(context.as_object().ok_or("日志不是对象")?.len(), 3);
    assert!(context["elapsed_ms"].as_u64().is_some());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE event='sentinel'")
            .fetch_one(admin.raw())
            .await?,
        3
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE event='recent'")
            .fetch_one(admin.raw())
            .await?,
        1
    );
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn restricted_sql_cannot_read_context_or_delete_tenant_and_recent_rows(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    let tenant: i64 = sqlx::query_scalar("SELECT min(id) FROM tenants")
        .fetch_one(admin.raw())
        .await?;
    sqlx::query("SELECT set_config('app.tenant_id',$1,false)")
        .bind(tenant.to_string())
        .execute(&cleanup)
        .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(id) FROM system_logs")
            .fetch_one(&cleanup)
            .await?,
        2
    );
    for query in [
        "SELECT context FROM system_logs",
        "SELECT module FROM system_logs",
        "UPDATE system_logs SET context='{}'",
        "TRUNCATE system_logs",
        "SELECT * FROM web_sessions",
        "SELECT * FROM identity_secrets",
        "SELECT * FROM tenants",
        "SELECT * FROM canonical_events",
        "SELECT * FROM tenant_events",
        "SELECT * FROM delivery_tasks",
        "SELECT * FROM audit_logs",
    ] {
        assert!(
            sqlx::query(query).execute(&cleanup).await.is_err(),
            "应拒绝 {query}"
        );
    }
    assert_eq!(
        sqlx::query("DELETE FROM system_logs WHERE tenant_id IS NOT NULL")
            .execute(&cleanup)
            .await?
            .rows_affected(),
        0
    );
    assert_eq!(
        sqlx::query(
            "DELETE FROM system_logs WHERE created_at > statement_timestamp()-interval '90 days'"
        )
        .execute(&cleanup)
        .await?
        .rows_affected(),
        0
    );
    assert_eq!(
        sqlx::query("DELETE FROM system_logs")
            .execute(&cleanup)
            .await?
            .rows_affected(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs")
            .fetch_one(admin.raw())
            .await?,
        4
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn log_policy_refuses_forgery_and_unrelated_writers(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    for (tenant, level, module, event, context) in [
        (
            Some(1),
            "INFO",
            "platform_retention",
            "cleanup.done",
            serde_json::json!({"table":"system_logs","rows":0,"elapsed_ms":0}),
        ),
        (
            None,
            "ERROR",
            "platform_retention",
            "cleanup.done",
            serde_json::json!({"table":"system_logs","rows":0,"elapsed_ms":0}),
        ),
        (
            None,
            "INFO",
            "other",
            "cleanup.done",
            serde_json::json!({"table":"system_logs","rows":0,"elapsed_ms":0}),
        ),
        (
            None,
            "INFO",
            "platform_retention",
            "other",
            serde_json::json!({"table":"system_logs","rows":0,"elapsed_ms":0}),
        ),
    ] {
        assert!(sqlx::query("INSERT INTO system_logs (tenant_id,level,module,event,context) VALUES ($1,$2,$3,$4,$5)")
            .bind(tenant).bind(level).bind(module).bind(event).bind(context).execute(&cleanup).await.is_err());
    }
    for context in [
        serde_json::json!({}),
        serde_json::json!(null),
        serde_json::json!({"table":"other","rows":0,"elapsed_ms":0}),
        serde_json::json!({"table":"system_logs","rows":0,"elapsed_ms":0,"secret":"private"}),
        serde_json::json!({"table":"system_logs","rows":-1,"elapsed_ms":0}),
        serde_json::json!({"table":"system_logs","rows":5001,"elapsed_ms":0}),
        serde_json::json!({"table":"system_logs","rows":"1","elapsed_ms":0}),
        serde_json::json!({"table":"system_logs","rows":1.5,"elapsed_ms":0}),
        serde_json::json!({"table":"system_logs","rows":0,"elapsed_ms":-1}),
    ] {
        assert!(sqlx::query("INSERT INTO system_logs (level,module,event,context) VALUES ('INFO','platform_retention','cleanup.done',$1)")
            .bind(context).execute(&cleanup).await.is_err());
    }
    for query in [
        "INSERT INTO system_logs (level,module,event,context,created_at) VALUES ('INFO','platform_retention','cleanup.done','{\"table\":\"system_logs\",\"rows\":0,\"elapsed_ms\":0}',now()-interval '1 year')",
        "INSERT INTO system_logs (id,level,module,event) OVERRIDING SYSTEM VALUE VALUES (99999,'INFO','platform_retention','cleanup.done')",
    ] {
        assert!(sqlx::query(query).execute(&cleanup).await.is_err());
    }
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin.raw())
        .await?;
    for (role, password) in [
        ("app_user", "apw"),
        ("shared_cleanup", "scpw"),
        ("media_cleanup", "mcpw"),
        ("auth_lookup", "lpw"),
    ] {
        let pool = PgPool::connect(&format!(
            "postgres://{role}:{password}@127.0.0.1:55432/{database}"
        ))
        .await?;
        assert!(check_connection(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO system_logs (level,module,event,context) VALUES ('INFO','platform_retention','cleanup.done','{\"table\":\"system_logs\",\"rows\":0,\"elapsed_ms\":0}')").execute(&pool).await.is_err());
        pool.close().await;
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batches_overlap_and_logging_failure_are_atomic(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    sqlx::query("INSERT INTO system_logs (level,module,event,created_at) SELECT 'INFO','test','old',now()-interval '91 days' FROM generate_series(1,5001)").execute(admin.raw()).await?;
    let mut lock = admin.raw().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *lock)
        .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, None);
    lock.rollback().await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_platform CHECK (module <> 'platform_retention') NOT VALID; RESET ROLE").execute(admin.raw()).await?;
    assert!(cleanup_batch(&cleanup).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs")
            .fetch_one(admin.raw())
            .await?,
        5001
    );
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_platform; RESET ROLE",
    )
    .execute(admin.raw())
    .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(5000));
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn waiting_delete_rechecks_age_and_preserves_concurrent_insert(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("UPDATE system_logs SET created_at=now() WHERE event='old'")
        .execute(&mut *held)
        .await?;
    let pool = cleanup.clone();
    let pending = tokio::spawn(async move { cleanup_batch(&pool).await });
    wait_for_lock(admin.raw()).await?;
    sqlx::query("INSERT INTO system_logs (level,module,event,created_at) VALUES ('INFO','test','concurrent',now()-interval '91 days')").execute(admin.raw()).await?;
    held.commit().await?;
    assert_eq!(pending.await??, Some(1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM system_logs WHERE event IN ('old','concurrent')"
        )
        .fetch_one(admin.raw())
        .await?,
        2
    );
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn unsupported_isolation_and_changed_privileges_are_rejected(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (_cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin.raw())
        .await?;
    let isolated = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_isolation='repeatable read'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&format!(
            "postgres://platform_cleanup:pcpw@127.0.0.1:55432/{database}"
        ))
        .await?;
    assert!(matches!(
        cleanup_batch(&isolated).await,
        Err(RetentionError::Isolation)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs")
            .fetch_one(admin.raw())
            .await?,
        6
    );
    for (grant, undo) in [
        (
            "GRANT SELECT (context) ON system_logs TO platform_cleanup",
            "REVOKE SELECT (context) ON system_logs FROM platform_cleanup",
        ),
        (
            "REVOKE SELECT (id) ON system_logs FROM platform_cleanup",
            "GRANT SELECT (id) ON system_logs TO platform_cleanup",
        ),
        (
            "GRANT INSERT (context) ON system_logs TO platform_cleanup WITH GRANT OPTION",
            "REVOKE GRANT OPTION FOR INSERT (context) ON system_logs FROM platform_cleanup",
        ),
        (
            "GRANT USAGE ON SEQUENCE system_logs_id_seq TO platform_cleanup WITH GRANT OPTION",
            "REVOKE GRANT OPTION FOR USAGE ON SEQUENCE system_logs_id_seq FROM platform_cleanup",
        ),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "SET ROLE tgm_owner; {grant}; RESET ROLE"
        )))
        .execute(admin.raw())
        .await?;
        let reports = tgm_db::audit::run(admin.raw()).await?;
        assert!(
            reports
                .iter()
                .any(|report| report.id == "C13" && report.failed()),
            "C13 必须拒绝 {grant}"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "SET ROLE tgm_owner; {undo}; RESET ROLE"
        )))
        .execute(admin.raw())
        .await?;
    }
    let reports = tgm_db::audit::run(admin.raw()).await?;
    let report = reports
        .iter()
        .find(|report| report.id == "C13")
        .ok_or("缺少 C13")?;
    assert!(!report.failed(), "{report:#?}");
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn direct_delete_policy_sentinels(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    assert_eq!(
        sqlx::query(
            "DELETE FROM system_logs WHERE created_at > statement_timestamp()-interval '90 days'"
        )
        .execute(&cleanup)
        .await?
        .rows_affected(),
        0,
        "近期平台日志被误删"
    );
    assert_eq!(
        sqlx::query("DELETE FROM system_logs WHERE tenant_id IS NOT NULL")
            .execute(&cleanup)
            .await?
            .rows_affected(),
        0,
        "租户日志被误删"
    );
    Ok(())
}
