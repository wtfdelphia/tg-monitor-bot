use sqlx::{PgPool, postgres::PgPoolOptions};
use tgm_db::auth_retention::{Category, check_connection, cleanup_batch};

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
            "postgres://auth_cleanup:cpw@127.0.0.1:55432/{database}"
        ))
        .await?;
    support::assert_role(&cleanup, "auth_cleanup").await;
    support::assert_not_bypassrls(&cleanup).await;
    check_connection(&cleanup).await?;
    Ok((cleanup, admin))
}

async fn seed(admin: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("INSERT INTO tenants (name, owner_user_id) VALUES ('cleanup-a', 89001), ('cleanup-b', 89002);
        INSERT INTO tenant_members (tenant_id, user_id) SELECT id, owner_user_id FROM tenants;
        INSERT INTO console_login_challenges (nonce, confirm_code, origin_ip, expires_at) VALUES (decode('01','hex'), 'ABCD', '127.0.0.1', now() - interval '1 hour'), (decode('02','hex'), 'ABCD', '127.0.0.1', now() + interval '1 day');
        INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at) SELECT id, decode('01','hex'), owner_user_id, decode('02','hex'), now() - interval '1 hour' FROM tenants WHERE name = 'cleanup-a';
        INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at) SELECT id, decode('02','hex'), owner_user_id, decode('02','hex'), now() + interval '1 day' FROM tenants WHERE name = 'cleanup-b';
        INSERT INTO bot_console_sessions (tg_user_id, tenant_id, updated_at) SELECT owner_user_id, id, now() - CASE WHEN name = 'cleanup-a' THEN interval '91 days' ELSE interval '89 days' END FROM tenants;")
        .execute(admin).await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn expires_only_eligible_rows_and_commits_redacted_logs(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    for category in [
        Category::Challenges,
        Category::WebSessions,
        Category::BotSessions,
    ] {
        assert_eq!(cleanup_batch(&cleanup, category).await?, Some(1));
        assert_eq!(cleanup_batch(&cleanup, category).await?, Some(0));
    }
    let counts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM console_login_challenges), (SELECT count(*) FROM web_sessions), (SELECT count(*) FROM bot_console_sessions), (SELECT count(*) FROM system_logs WHERE tenant_id IS NULL AND module = 'auth_retention' AND event = 'cleanup.done')")
        .fetch_one(admin.raw()).await?;
    assert_eq!(counts, (1, 1, 1, 6));
    let contexts: Vec<serde_json::Value> = sqlx::query_scalar("SELECT context FROM system_logs")
        .fetch_all(admin.raw())
        .await?;
    for context in contexts {
        let object = context.as_object().ok_or("日志不是对象")?;
        assert_eq!(object.len(), 3);
        assert!(
            object.contains_key("table")
                && object.contains_key("rows")
                && object.contains_key("elapsed_ms")
        );
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn expired_row_rls_does_not_depend_on_tenant_or_user_context(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    let mut transaction = cleanup.begin().await?;
    sqlx::raw_sql("SET LOCAL app.tenant_id = '0'; SET LOCAL app.auth_user_id = '89002'; SET LOCAL app.session_id = '\\x02';")
        .execute(&mut *transaction).await?;
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions")
        .fetch_one(&mut *transaction)
        .await?;
    assert_eq!(visible, 1);
    let deleted = sqlx::query("DELETE FROM web_sessions")
        .execute(&mut *transaction)
        .await?
        .rows_affected();
    assert_eq!(deleted, 1);
    let deleted = sqlx::query("DELETE FROM bot_console_sessions")
        .execute(&mut *transaction)
        .await?
        .rows_affected();
    assert_eq!(deleted, 1);
    transaction.commit().await?;
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions")
        .fetch_one(admin.raw())
        .await?;
    assert_eq!(active, 1);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_role_cannot_access_secrets_or_mutate_sessions_and_logs(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    for query in [
        "SELECT csrf_token FROM web_sessions",
        "SELECT confirm_code FROM console_login_challenges",
        "SELECT state FROM bot_console_sessions",
        "SELECT * FROM identity_secrets",
        "UPDATE web_sessions SET expires_at = now()",
        "INSERT INTO console_login_challenges (nonce, confirm_code, origin_ip, expires_at) VALUES (decode('03','hex'), 'ABCD', '127.0.0.1', now())",
        "DELETE FROM tenant_members",
        "SELECT * FROM system_logs",
        "DELETE FROM system_logs",
        "TRUNCATE web_sessions",
        "CREATE TABLE public.cleanup_escape (id bigint)",
    ] {
        assert!(
            sqlx::query(query).execute(&cleanup).await.is_err(),
            "权限意外允许 {query}"
        );
    }
    for context in [
        serde_json::json!({"table":"web_sessions", "rows":1, "elapsed_ms":0, "nonce":"secret"}),
        serde_json::json!({"table":"identity_secrets", "rows":1, "elapsed_ms":0}),
        serde_json::json!({"table":"web_sessions", "rows":5001, "elapsed_ms":0}),
        serde_json::json!({"table":"web_sessions", "rows":-1, "elapsed_ms":0}),
        serde_json::json!({"table":"web_sessions", "rows":1.5, "elapsed_ms":0}),
        serde_json::json!({"table":"web_sessions", "rows":"1", "elapsed_ms":0}),
        serde_json::json!({"table":"web_sessions", "rows":null, "elapsed_ms":0}),
        serde_json::json!({"table":"web_sessions", "rows":1}),
        serde_json::json!({"table":"web_sessions", "rows":1, "elapsed_ms":-1}),
        serde_json::json!({"table":"web_sessions", "rows":1, "elapsed_ms":null}),
        serde_json::json!(null),
        serde_json::json!([]),
    ] {
        assert!(sqlx::query("INSERT INTO system_logs (tenant_id, level, module, event, context) VALUES (NULL, 'INFO', 'auth_retention', 'cleanup.done', $1)").bind(context).execute(&cleanup).await.is_err());
    }
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(admin.raw())
        .await?;
    let app = PgPoolOptions::new()
        .connect(&format!(
            "postgres://app_user:apw@127.0.0.1:55432/{database}"
        ))
        .await?;
    for table in [
        "web_sessions",
        "bot_console_sessions",
        "console_login_challenges",
    ] {
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT has_table_privilege(current_user, $1, 'SELECT,INSERT,UPDATE,DELETE')"
            )
            .bind(table)
            .fetch_one(&app)
            .await?
        );
    }
    assert!(check_connection(&app).await.is_err());
    for (tenant, level, module, event) in [
        (None, "ERROR", "auth_retention", "cleanup.done"),
        (None, "INFO", "other", "cleanup.done"),
        (None, "INFO", "auth_retention", "other"),
        (Some(1_i64), "INFO", "auth_retention", "cleanup.done"),
    ] {
        assert!(sqlx::query("INSERT INTO system_logs (tenant_id, level, module, event, context) VALUES ($1, $2, $3, $4, $5)")
            .bind(tenant).bind(level).bind(module).bind(event)
            .bind(serde_json::json!({"table":"web_sessions", "rows":1, "elapsed_ms":0}))
            .execute(&cleanup).await.is_err());
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_audit_rejects_extra_and_missing_column_grants(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (_cleanup, admin) = setup(injected).await?;
    let assertion = tgm_db::audit::parse(include_str!("../src/audit/queries.sql"))
        .into_iter()
        .find(|assertion| assertion.id == "C8")
        .ok_or("C8 缺失")?;
    let baseline = sqlx::query(sqlx::AssertSqlSafe(assertion.sql.as_str()))
        .fetch_all(admin.raw())
        .await?;
    assert!(baseline.is_empty(), "{baseline:?}");
    for mutation in [
        "GRANT SELECT (csrf_token) ON web_sessions TO auth_cleanup",
        "REVOKE SELECT (id) ON web_sessions FROM auth_cleanup",
        "GRANT UPDATE ON system_logs TO auth_cleanup",
        "GRANT SELECT ON SEQUENCE system_logs_id_seq TO auth_cleanup",
        "GRANT MAINTAIN ON web_sessions TO auth_cleanup",
        "GRANT SELECT (id) ON web_sessions TO auth_cleanup WITH GRANT OPTION",
        "GRANT USAGE ON SEQUENCE system_logs_id_seq TO auth_cleanup WITH GRANT OPTION",
    ] {
        let mut transaction = admin.raw().begin().await?;
        sqlx::query("SET LOCAL ROLE tgm_owner")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(mutation).execute(&mut *transaction).await?;
        assert!(
            !sqlx::query(sqlx::AssertSqlSafe(assertion.sql.as_str()))
                .fetch_all(&mut *transaction)
                .await?
                .is_empty(),
            "审计漏报 {mutation}"
        );
        transaction.rollback().await?;
    }
    assert!(
        sqlx::query(sqlx::AssertSqlSafe(assertion.sql.as_str()))
            .fetch_all(admin.raw())
            .await?
            .is_empty()
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batches_cap_at_limit_and_lock_overlap_leaves_no_log(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    sqlx::query("INSERT INTO console_login_challenges (nonce, confirm_code, origin_ip, expires_at) SELECT int8send(number), 'ABCD', '127.0.0.1', now() - interval '1 hour' FROM generate_series(1, 5001) number")
        .execute(admin.raw()).await?;
    sqlx::raw_sql("INSERT INTO tenants (name, owner_user_id) VALUES ('cleanup-batch', 89001);
        INSERT INTO tenant_members (tenant_id, user_id) SELECT id, 90000 + number FROM tenants CROSS JOIN generate_series(1, 5001) number;
        INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at) SELECT tenant_id, int8send(user_id), user_id, decode('01','hex'), now() - interval '1 hour' FROM tenant_members;
        INSERT INTO bot_console_sessions (tg_user_id, tenant_id, updated_at) SELECT user_id, tenant_id, now() - interval '91 days' FROM tenant_members;")
        .execute(admin.raw()).await?;
    for category in [
        Category::Challenges,
        Category::WebSessions,
        Category::BotSessions,
    ] {
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM system_logs")
            .fetch_one(admin.raw())
            .await?;
        let mut lock = cleanup.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(category.lock_key())
            .execute(&mut *lock)
            .await?;
        assert_eq!(cleanup_batch(&cleanup, category).await?, None);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM system_logs")
                .fetch_one(admin.raw())
                .await?,
            before
        );
        lock.rollback().await?;
        assert_eq!(cleanup_batch(&cleanup, category).await?, Some(5000));
        assert_eq!(cleanup_batch(&cleanup, category).await?, Some(1));
        assert_eq!(cleanup_batch(&cleanup, category).await?, Some(0));
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn logging_failure_rolls_back_deletes_and_allows_retry(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_cleanup CHECK (event <> 'cleanup.done') NOT VALID; RESET ROLE;")
        .execute(admin.raw()).await?;
    assert!(cleanup_batch(&cleanup, Category::Challenges).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM console_login_challenges")
            .fetch_one(admin.raw())
            .await?,
        2
    );
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_cleanup; RESET ROLE;",
    )
    .execute(admin.raw())
    .await?;
    assert_eq!(
        cleanup_batch(&cleanup, Category::Challenges).await?,
        Some(1)
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn concurrent_renewal_is_rechecked_after_waiting_for_row_lock(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, admin) = setup(injected).await?;
    seed(admin.raw()).await?;
    for category in [
        Category::WebSessions,
        Category::BotSessions,
        Category::Challenges,
    ] {
        let mut renewal = admin.raw().begin().await?;
        let query = match category {
            Category::WebSessions => {
                "UPDATE web_sessions SET expires_at = now() + interval '1 day' WHERE id = decode('01','hex')"
            }
            Category::BotSessions => {
                "UPDATE bot_console_sessions SET updated_at = now() WHERE tg_user_id = 89001"
            }
            Category::Challenges => {
                "UPDATE console_login_challenges SET expires_at = now() + interval '1 day' WHERE nonce = decode('01','hex')"
            }
        };
        sqlx::query(query).execute(&mut *renewal).await?;
        let pool = cleanup.clone();
        let pending = tokio::spawn(async move { cleanup_batch(&pool, category).await });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks waiting JOIN pg_stat_activity activity ON activity.pid = waiting.pid WHERE activity.datname = current_database() AND activity.usename = 'auth_cleanup' AND NOT waiting.granted)").fetch_one(admin.raw()).await?;
            if waiting {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "清理未等待真实行锁");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        renewal.commit().await?;
        assert_eq!(pending.await??, Some(0));
    }
    Ok(())
}
