use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use tgm_core::ids::TenantId;
use tgm_db::tenant_retention::{Category, check_connection, cleanup_batch};

mod support;

const CATEGORIES: [Category; 6] = [
    Category::LoginSessions,
    Category::NotifyDedup,
    Category::BotConsoleUpdates,
    Category::DeliveryLogs,
    Category::AuditLogs,
    Category::SystemLogs,
];

async fn setup(
    injected: PgPool,
) -> Result<(PgPool, support::AdminOnlyPool), Box<dyn std::error::Error>> {
    let (_app, admin) = support::pools(injected).await;
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
    support::assert_role(&cleanup, "tenant_cleanup").await;
    support::assert_not_bypassrls(&cleanup).await;
    check_connection(&cleanup).await?;
    Ok((cleanup, admin))
}

async fn seed(admin: &PgPool) -> Result<Vec<TenantId>, sqlx::Error> {
    sqlx::raw_sql("INSERT INTO tenants (name, owner_user_id, plan, status) VALUES
        ('retention-free', 91001, 'free', 'suspended'), ('retention-pro', 91002, 'pro', 'deleted');
        INSERT INTO identities (tenant_id, kind) SELECT id, 'bot' FROM tenants;
        INSERT INTO delivery_tasks (tenant_id, sender_identity_id, target_tg_chat_id, action)
            SELECT tenant_id, id, -1001, 'COPY' FROM identities;
        INSERT INTO login_sessions (tenant_id, kind, state, login_token, expires_at)
            SELECT id, 'qr', 'pending', decode('aabb','hex'), now() + offset_days * interval '1 day'
            FROM tenants CROSS JOIN (VALUES (-1), (1)) offsets(offset_days);
        INSERT INTO notify_dedup (tenant_id, fingerprint, target_ref, last_sent_at, expires_at)
            SELECT id, 'same-key', offset_days, now(), now() + offset_days * interval '1 day'
            FROM tenants CROSS JOIN (VALUES (-1), (1)) offsets(offset_days);
        INSERT INTO bot_console_updates (tenant_id, bot_id, update_id, processed_at)
            SELECT tenant_id, id, age_days, now() - age_days * interval '1 day'
            FROM identities CROSS JOIN (VALUES (8), (1)) ages(age_days);
        INSERT INTO delivery_logs (task_id, attempt, ok, created_at)
            SELECT id, 1, true, now() - age_days * interval '1 day'
            FROM delivery_tasks CROSS JOIN (VALUES (100), (1)) ages(age_days);
        INSERT INTO audit_logs (tenant_id, action, resource, created_at)
            SELECT id, 'test', 'retained', now() - age_days * interval '1 day'
            FROM tenants CROSS JOIN (VALUES (200), (1)) ages(age_days);
        INSERT INTO system_logs (tenant_id, level, module, event, created_at)
            SELECT id, 'INFO', 'seed', 'sentinel', now() - age_days * interval '1 day'
            FROM tenants CROSS JOIN (VALUES (100), (1)) ages(age_days);
        INSERT INTO audit_logs (action, resource, created_at) VALUES ('platform', 'sentinel', now() - interval '400 days');
        INSERT INTO system_logs (level, module, event, created_at) VALUES ('INFO', 'platform', 'sentinel', now() - interval '400 days');")
        .execute(admin).await?;
    sqlx::query_scalar("SELECT id FROM tenants ORDER BY id")
        .fetch_all(admin)
        .await
}

async fn wait_for_lock(admin: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid = locks.pid WHERE activity.datname = current_database() AND activity.usename = 'tenant_cleanup' AND NOT locks.granted)")
            .fetch_one(admin).await?;
        if waiting {
            return Ok(());
        }
        assert!(tokio::time::Instant::now() < deadline, "清理未等到真实行锁");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn all_categories_preserve_other_tenants_platform_rows_and_parent_tasks(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    let tenants = seed(admin.raw()).await?;
    sqlx::raw_sql("INSERT INTO source_subscriptions (tenant_id,tg_chat_id,receiver_identity_id) SELECT tenant_id,-1001,id FROM identities;
        INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,message_date,created_at) SELECT 'bot',id,-1001,1,now(),now()-interval '400 days' FROM identities;
        INSERT INTO tenant_events (tenant_id,canonical_event_id,source_ref,created_at) SELECT sources.tenant_id,events.id,sources.id,now()-interval '400 days' FROM source_subscriptions sources JOIN canonical_events events ON events.receiver_identity_id=sources.receiver_identity_id;
        INSERT INTO target_channels (tenant_id,tg_chat_id,sender_identity_id) SELECT tenant_id,-1002,id FROM identities;
        INSERT INTO keywords (tenant_id,word) SELECT id,'sentinel' FROM tenants;
        INSERT INTO rules (tenant_id,keyword_id,target_ref,action) SELECT words.tenant_id,words.id,targets.id,'COPY' FROM keywords words JOIN target_channels targets ON targets.tenant_id=words.tenant_id;
        INSERT INTO rule_versions (tenant_id,rule_id,version,snapshot,created_at) SELECT tenant_id,id,1,'{}',now()-interval '400 days' FROM rules;
        INSERT INTO tenant_members (tenant_id,user_id) SELECT id,owner_user_id FROM tenants;
        INSERT INTO tenant_consents (tenant_id,consent_type,text_version,user_id) SELECT id,'tos','sentinel',owner_user_id FROM tenants;
        INSERT INTO identity_secrets (identity_id,secret_ciphertext,nonce,dek_ciphertext,kek_id) SELECT id,decode('aabb','hex'),decode('aabb','hex'),decode('aabb','hex'),'sentinel' FROM identities;")
        .execute(admin.raw()).await?;
    for category in CATEGORIES {
        assert_eq!(
            cleanup_batch(&cleanup, tenants[0], category).await?,
            Some(1)
        );
        assert_eq!(
            cleanup_batch(&cleanup, tenants[0], category).await?,
            Some(0)
        );
    }
    let counts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM login_sessions WHERE tenant_id = $1), (SELECT count(*) FROM delivery_logs logs JOIN delivery_tasks task ON task.id = logs.task_id WHERE task.tenant_id = $1), (SELECT count(*) FROM audit_logs WHERE tenant_id IS NULL), (SELECT count(*) FROM delivery_tasks)")
        .bind(tenants[1].get()).fetch_one(admin.raw()).await?;
    assert_eq!(counts, (2, 2, 1, 2));
    for category in CATEGORIES {
        let expected = if category == Category::AuditLogs {
            0
        } else {
            1
        };
        assert_eq!(
            cleanup_batch(&cleanup, tenants[1], category).await?,
            Some(expected)
        );
    }
    let logs: Vec<(Option<i64>, serde_json::Value)> = sqlx::query_as(
        "SELECT tenant_id, context FROM system_logs WHERE module = 'tenant_retention'",
    )
    .fetch_all(admin.raw())
    .await?;
    assert_eq!(logs.len(), 18);
    for (tenant, context) in logs {
        assert!(tenant.is_some());
        assert_eq!(context.as_object().ok_or("日志不是对象")?.len(), 3);
        assert!(context["rows"].as_u64().is_some());
        assert!(context["elapsed_ms"].as_u64().is_some());
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs WHERE tenant_id IS NULL")
            .fetch_one(admin.raw())
            .await?,
        1
    );
    let sentinels: (i64,i64,i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM canonical_events), (SELECT count(*) FROM tenant_events), (SELECT count(*) FROM rule_versions), (SELECT count(*) FROM tenant_consents), (SELECT count(*) FROM identity_secrets)")
        .fetch_one(admin.raw()).await?;
    assert_eq!(sentinels, (2, 2, 2, 2, 2));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn every_category_caps_full_batches_and_preserves_new_rows(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    let tenants = seed(admin.raw()).await?;
    for query in [
        "INSERT INTO login_sessions (tenant_id,kind,state,expires_at) SELECT $1,'qr','expired',now()-interval '1 day' FROM generate_series(1,5000)",
        "INSERT INTO notify_dedup (tenant_id,fingerprint,target_ref,last_sent_at,expires_at) SELECT $1,'batch-'||number,1,now(),now()-interval '1 day' FROM generate_series(1,5000) number",
        "INSERT INTO bot_console_updates (tenant_id,bot_id,update_id,processed_at) SELECT $1,identities.id,100+number,now()-interval '8 days' FROM identities CROSS JOIN generate_series(1,5000) number WHERE tenant_id=$1",
        "INSERT INTO delivery_logs (task_id,attempt,ok,created_at) SELECT delivery_tasks.id,1,true,now()-interval '100 days' FROM delivery_tasks CROSS JOIN generate_series(1,5000) number WHERE tenant_id=$1",
        "INSERT INTO audit_logs (tenant_id,action,resource,created_at) SELECT $1,'batch','sentinel',now()-interval '200 days' FROM generate_series(1,5000)",
        "INSERT INTO system_logs (tenant_id,level,module,event,created_at) SELECT $1,'INFO','batch','sentinel',now()-interval '100 days' FROM generate_series(1,5000)",
    ] {
        sqlx::query(query)
            .bind(tenants[0].get())
            .execute(admin.raw())
            .await?;
    }
    for category in CATEGORIES {
        assert_eq!(
            cleanup_batch(&cleanup, tenants[0], category).await?,
            Some(5000),
            "{category:?}"
        );
        assert_eq!(
            cleanup_batch(&cleanup, tenants[0], category).await?,
            Some(1),
            "{category:?}"
        );
        assert_eq!(
            cleanup_batch(&cleanup, tenants[0], category).await?,
            Some(0),
            "{category:?}"
        );
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn role_denies_sensitive_columns_active_rows_mutation_and_unrelated_tables(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    let tenants = seed(admin.raw()).await?;
    for query in [
        "SELECT login_token FROM login_sessions",
        "SELECT snapshot FROM rule_versions",
        "SELECT context FROM system_logs",
        "SELECT secret_ciphertext FROM identity_secrets",
        "SELECT target_tg_chat_id FROM delivery_tasks",
        "SELECT name FROM tenants",
        "DELETE FROM tenant_events",
        "DELETE FROM delivery_tasks",
        "DELETE FROM tenant_consents",
        "DELETE FROM web_sessions",
        "UPDATE tenants SET plan = 'free'",
        "UPDATE login_sessions SET expires_at = now()",
        "TRUNCATE notify_dedup",
        "CREATE TABLE retention_forbidden (id int)",
        "SET ROLE app_user",
    ] {
        assert!(
            sqlx::raw_sql(query).execute(&cleanup).await.is_err(),
            "必须拒绝：{query}"
        );
    }
    assert!(
        sqlx::query("SELECT id FROM login_sessions")
            .fetch_all(&cleanup)
            .await
            .is_err()
    );
    for tenant in &tenants {
        let mut transaction = cleanup.begin().await?;
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant.get().to_string())
            .execute(&mut *transaction)
            .await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM login_sessions")
                .fetch_one(&mut *transaction)
                .await?,
            1
        );
        assert_eq!(
            sqlx::query("DELETE FROM login_sessions WHERE expires_at > now()")
                .execute(&mut *transaction)
                .await?
                .rows_affected(),
            0
        );
        transaction.rollback().await?;
    }
    for tenant in &tenants {
        let mut transaction = cleanup.begin().await?;
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant.get().to_string())
            .execute(&mut *transaction)
            .await?;
        for (category, query) in [
            (Category::LoginSessions, "DELETE FROM login_sessions"),
            (Category::NotifyDedup, "DELETE FROM notify_dedup"),
            (
                Category::BotConsoleUpdates,
                "DELETE FROM bot_console_updates",
            ),
            (Category::DeliveryLogs, "DELETE FROM delivery_logs"),
            (Category::AuditLogs, "DELETE FROM audit_logs"),
            (Category::SystemLogs, "DELETE FROM system_logs"),
        ] {
            let expected = if *tenant == tenants[1] && category == Category::AuditLogs {
                0
            } else {
                1
            };
            assert_eq!(
                sqlx::query(query)
                    .execute(&mut *transaction)
                    .await?
                    .rows_affected(),
                expected,
                "角色直接删除也必须只见当前租户到期行：{category:?}"
            );
        }
        transaction.rollback().await?;
    }
    for context in [
        serde_json::json!({}),
        serde_json::json!({"table":"tenants","rows":0,"elapsed_ms":0}),
        serde_json::json!({"table":"login_sessions","rows":5001,"elapsed_ms":0}),
        serde_json::json!({"table":"login_sessions","rows":0,"elapsed_ms":-1}),
        serde_json::json!({"table":"login_sessions","rows":null,"elapsed_ms":0}),
        serde_json::json!({"table":"login_sessions","rows":0,"elapsed_ms":0,"token":"secret"}),
    ] {
        let mut transaction = cleanup.begin().await?;
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenants[0].get().to_string())
            .execute(&mut *transaction)
            .await?;
        assert!(sqlx::query("INSERT INTO system_logs (tenant_id,level,module,event,context) VALUES ($1,'INFO','tenant_retention','cleanup.done',$2)")
            .bind(tenants[0].get()).bind(context).execute(&mut *transaction).await.is_err());
        transaction.rollback().await?;
    }
    for query in [
        "INSERT INTO system_logs (tenant_id,level,module,event,context) VALUES (NULL,'INFO','tenant_retention','cleanup.done','{\"table\":\"login_sessions\",\"rows\":0,\"elapsed_ms\":0}')",
        "INSERT INTO system_logs (tenant_id,level,module,event,context,created_at) VALUES (NULL,'INFO','tenant_retention','cleanup.done','{}',now()-interval '1 year')",
    ] {
        let mut transaction = cleanup.begin().await?;
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenants[0].get().to_string())
            .execute(&mut *transaction)
            .await?;
        assert!(
            sqlx::raw_sql(query)
                .execute(&mut *transaction)
                .await
                .is_err()
        );
        transaction.rollback().await?;
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batching_overlap_and_log_failure_are_atomic(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    let tenants = seed(admin.raw()).await?;
    sqlx::query("INSERT INTO login_sessions (tenant_id,kind,state,expires_at) SELECT $1,'qr','expired',now()-interval '1 day' FROM generate_series(1,5000)")
        .bind(tenants[0].get()).execute(admin.raw()).await?;
    let mut lock = cleanup.begin().await?;
    assert!(
        tgm_db::tenant_retention::try_lock(&mut lock, tenants[0], Category::LoginSessions).await?
    );
    assert_eq!(
        cleanup_batch(&cleanup, tenants[0], Category::LoginSessions).await?,
        None
    );
    assert_eq!(
        cleanup_batch(&cleanup, tenants[1], Category::LoginSessions).await?,
        Some(1)
    );
    lock.rollback().await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_retention CHECK (module <> 'tenant_retention') NOT VALID; RESET ROLE;").execute(admin.raw()).await?;
    assert!(
        cleanup_batch(&cleanup, tenants[0], Category::LoginSessions)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM login_sessions WHERE tenant_id=$1")
            .bind(tenants[0].get())
            .fetch_one(admin.raw())
            .await?,
        5002
    );
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_retention; RESET ROLE;",
    )
    .execute(admin.raw())
    .await?;
    assert_eq!(
        cleanup_batch(&cleanup, tenants[0], Category::LoginSessions).await?,
        Some(5000)
    );
    assert_eq!(
        cleanup_batch(&cleanup, tenants[0], Category::LoginSessions).await?,
        Some(1)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn refreshed_candidates_survive_real_row_lock_races(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    let tenants = seed(admin.raw()).await?;
    for (category, query) in [
        (
            Category::LoginSessions,
            "UPDATE login_sessions SET expires_at=now()+interval '1 day' WHERE tenant_id=$1 AND expires_at<now()",
        ),
        (
            Category::NotifyDedup,
            "UPDATE notify_dedup SET expires_at=now()+interval '1 day' WHERE tenant_id=$1 AND expires_at<now()",
        ),
        (
            Category::BotConsoleUpdates,
            "UPDATE bot_console_updates SET processed_at=now() WHERE tenant_id=$1 AND processed_at<now()-interval '7 days'",
        ),
    ] {
        let mut renewal = admin.raw().begin().await?;
        sqlx::query(query)
            .bind(tenants[0].get())
            .execute(&mut *renewal)
            .await?;
        let pool = cleanup.clone();
        let tenant = tenants[0];
        let pending = tokio::spawn(async move { cleanup_batch(&pool, tenant, category).await });
        wait_for_lock(admin.raw()).await?;
        renewal.commit().await?;
        assert_eq!(pending.await??, Some(0));
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn plan_is_reloaded_each_batch_and_statement_snapshot_defines_races(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    let tenants = seed(admin.raw()).await?;
    sqlx::query("UPDATE tenants SET plan='pro' WHERE id=$1")
        .bind(tenants[0].get())
        .execute(admin.raw())
        .await?;
    assert_eq!(
        cleanup_batch(&cleanup, tenants[0], Category::AuditLogs).await?,
        Some(0)
    );
    sqlx::query("UPDATE tenants SET plan='free' WHERE id=$1")
        .bind(tenants[0].get())
        .execute(admin.raw())
        .await?;
    let mut held = admin.raw().begin().await?;
    sqlx::query("UPDATE audit_logs SET action='locked' WHERE tenant_id=$1 AND created_at<now()-interval '90 days'").bind(tenants[0].get()).execute(&mut *held).await?;
    let pool = cleanup.clone();
    let tenant = tenants[0];
    let pending =
        tokio::spawn(async move { cleanup_batch(&pool, tenant, Category::AuditLogs).await });
    wait_for_lock(admin.raw()).await?;
    sqlx::query("UPDATE tenants SET plan='pro' WHERE id=$1")
        .bind(tenant.get())
        .execute(admin.raw())
        .await?;
    held.rollback().await?;
    assert_eq!(pending.await??, Some(1));
    sqlx::query("INSERT INTO audit_logs (tenant_id,action,resource,created_at) VALUES ($1,'after','upgrade',now()-interval '200 days')")
        .bind(tenant.get()).execute(admin.raw()).await?;
    assert_eq!(
        cleanup_batch(&cleanup, tenant, Category::AuditLogs).await?,
        Some(0)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn c9_rejects_extra_missing_and_grantable_permissions(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    let assertion = tgm_db::audit::parse(include_str!("../src/audit/queries.sql"))
        .into_iter()
        .find(|assertion| assertion.id == "C9")
        .ok_or("缺少 C9")?;
    let mut transaction = admin.raw().begin().await?;
    sqlx::raw_sql("SET LOCAL ROLE tgm_owner")
        .execute(&mut *transaction)
        .await?;
    for (broken, restored) in [
        (
            "GRANT SELECT (login_token) ON login_sessions TO tenant_cleanup",
            "REVOKE SELECT (login_token) ON login_sessions FROM tenant_cleanup",
        ),
        (
            "REVOKE SELECT (id) ON login_sessions FROM tenant_cleanup",
            "GRANT SELECT (id) ON login_sessions TO tenant_cleanup",
        ),
        (
            "GRANT SELECT (id) ON login_sessions TO tenant_cleanup WITH GRANT OPTION",
            "REVOKE GRANT OPTION FOR SELECT (id) ON login_sessions FROM tenant_cleanup",
        ),
        (
            "GRANT MAINTAIN ON login_sessions TO tenant_cleanup",
            "REVOKE MAINTAIN ON login_sessions FROM tenant_cleanup",
        ),
        (
            "GRANT USAGE ON SEQUENCE system_logs_id_seq TO tenant_cleanup WITH GRANT OPTION",
            "REVOKE GRANT OPTION FOR USAGE ON SEQUENCE system_logs_id_seq FROM tenant_cleanup",
        ),
    ] {
        sqlx::raw_sql(broken).execute(&mut *transaction).await?;
        let violations: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(assertion.sql.as_str()))
                .fetch_all(&mut *transaction)
                .await?;
        assert!(!violations.is_empty(), "C9 漏报：{broken}");
        sqlx::raw_sql(restored).execute(&mut *transaction).await?;
        let violations: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(assertion.sql.as_str()))
                .fetch_all(&mut *transaction)
                .await?;
        assert!(violations.is_empty(), "C9 恢复失败：{violations:?}");
    }
    transaction.rollback().await?;
    cleanup.close().await;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_policy_does_not_mask_missing_business_policy(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (_cleanup, admin) = setup(injected).await?;
    let assertion = tgm_db::audit::parse(include_str!("../src/audit/queries.sql"))
        .into_iter()
        .find(|assertion| assertion.id == "C1")
        .ok_or("缺少 C1")?;
    let mut transaction = admin.raw().begin().await?;
    sqlx::raw_sql("SET LOCAL ROLE tgm_owner; DROP POLICY tenant_isolation ON delivery_logs;")
        .execute(&mut *transaction)
        .await?;
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_policy WHERE polrelid='delivery_logs'::regclass",
    )
    .fetch_one(&mut *transaction)
    .await?;
    assert!(remaining > 0);
    let violations: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(assertion.sql.as_str()))
        .fetch_all(&mut *transaction)
        .await?;
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("delivery_logs"));
    transaction.rollback().await?;
    let violations: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(assertion.sql.as_str()))
        .fetch_all(admin.raw())
        .await?;
    assert!(violations.is_empty());
    Ok(())
}
