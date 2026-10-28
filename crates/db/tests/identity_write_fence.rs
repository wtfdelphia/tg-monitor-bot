mod support;

use sqlx::{Connection, PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_core::ids::TenantId;
use tgm_db::lifecycle;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn tenant(admin: &PgPool, status: &str) -> Result<TenantId, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO tenants(name,owner_user_id,status) VALUES('identity-fence',98101,$1) RETURNING id")
        .bind(status)
        .fetch_one(admin)
        .await
}

async fn set_tenant(
    connection: &mut sqlx::PgConnection,
    tenant: TenantId,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(connection)
        .await?;
    Ok(())
}

async fn insert(connection: &mut sqlx::PgConnection, tenant: TenantId) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO public.identities(kind,tenant_id) VALUES('bot',$1) RETURNING id",
    )
    .bind(tenant.get())
    .fetch_one(connection)
    .await
}

async fn waiting(admin: &PgPool, pid: i32) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND locktype='advisory' AND NOT granted)",
        )
        .bind(pid)
        .fetch_one(admin)
        .await?;
        if blocked {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("expected advisory lock wait was not observed".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn denied(result: Result<i64, sqlx::Error>) -> TestResult {
    let error = result
        .err()
        .ok_or("identity write unexpectedly succeeded")?;
    let database = error
        .as_database_error()
        .ok_or("expected a database denial")?;
    assert_eq!(database.code().as_deref(), Some("42501"));
    Ok(())
}

async fn audit_fence(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let assertion = tgm_db::audit::parse(include_str!("../src/audit/queries.sql"))
        .into_iter()
        .find(|assertion| assertion.id == "C15")
        .ok_or_else(|| sqlx::Error::Protocol("C15 missing".to_owned()))?;
    sqlx::query_scalar(sqlx::AssertSqlSafe(assertion.sql))
        .fetch_all(pool)
        .await
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn audit_detects_and_restores_each_fence_mutation(injected: PgPool) -> TestResult {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    assert!(audit_fence(&app).await?.is_empty());
    for mutation in [
        "ALTER TABLE public.identities DISABLE TRIGGER identity_lifecycle_write_fence",
        "DROP TRIGGER identity_lifecycle_write_fence ON public.identities",
        "ALTER FUNCTION public.identity_lifecycle_write_fence() STABLE",
        "ALTER FUNCTION public.identity_lifecycle_write_fence() SET search_path=public",
        "ALTER FUNCTION public.identity_lifecycle_write_fence() SECURITY DEFINER",
        "REVOKE EXECUTE ON FUNCTION public.identity_lifecycle_write_fence() FROM app_user",
        "GRANT EXECUTE ON FUNCTION public.identity_lifecycle_write_fence() TO PUBLIC",
        "GRANT EXECUTE ON FUNCTION public.identity_lifecycle_write_fence() TO app_user WITH GRANT OPTION",
        "CREATE OR REPLACE FUNCTION public.identity_lifecycle_write_fence() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
        "CREATE FUNCTION public.identity_lifecycle_write_fence(bigint) RETURNS bigint LANGUAGE sql AS 'SELECT $1'",
        "GRANT INSERT(kind) ON public.identities TO platform_ops",
    ] {
        let mut transaction = admin.raw().begin().await?;
        sqlx::query("SET LOCAL ROLE tgm_owner")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(sqlx::AssertSqlSafe(mutation))
            .execute(&mut *transaction)
            .await?;
        let assertion = tgm_db::audit::parse(include_str!("../src/audit/queries.sql"))
            .into_iter()
            .find(|assertion| assertion.id == "C15")
            .ok_or("C15 missing")?;
        let violations: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(assertion.sql))
            .fetch_all(&mut *transaction)
            .await?;
        assert!(!violations.is_empty(), "undetected: {mutation}");
        transaction.rollback().await?;
        assert!(audit_fence(&app).await?.is_empty());
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn direct_writes_require_active_tenant_and_matching_context(injected: PgPool) -> TestResult {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    support::assert_not_bypassrls(&app).await;
    for status in ["active", "suspended", "deleted"] {
        let tenant = tenant(admin.raw(), status).await?;
        let mut transaction = app.begin().await?;
        set_tenant(&mut transaction, tenant).await?;
        for kind in ["bot", "mtproto"] {
            let mut attempt = transaction.begin().await?;
            let result = sqlx::query_scalar::<_, i64>(
                "INSERT INTO identities(kind,tenant_id) VALUES($1,$2) RETURNING id",
            )
            .bind(kind)
            .bind(tenant.get())
            .fetch_one(&mut *attempt)
            .await;
            if status == "active" {
                result?;
            } else {
                denied(result)?;
            }
            attempt.rollback().await?;
        }
        transaction.rollback().await?;
    }
    let first = tenant(admin.raw(), "active").await?;
    let second = tenant(admin.raw(), "active").await?;
    for context in [None, Some(second)] {
        let mut transaction = app.begin().await?;
        if let Some(context) = context {
            set_tenant(&mut transaction, context).await?;
        }
        denied(insert(&mut transaction, first).await)?;
        transaction.rollback().await?;
    }
    let mut empty_context = app.begin().await?;
    sqlx::query("SELECT set_config('app.tenant_id','',true)")
        .execute(&mut *empty_context)
        .await?;
    denied(insert(&mut empty_context, first).await)?;
    empty_context.rollback().await?;
    sqlx::query("DELETE FROM tenants WHERE id=$1")
        .bind(first.get())
        .execute(admin.raw())
        .await?;
    let mut transaction = app.begin().await?;
    set_tenant(&mut transaction, first).await?;
    denied(insert(&mut transaction, first).await)?;
    transaction.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn long_snapshots_and_privilege_bypasses_are_rejected(injected: PgPool) -> TestResult {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    let tenant = tenant(admin.raw(), "active").await?;
    for isolation in [
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    ] {
        let mut transaction = app.begin().await?;
        sqlx::query(sqlx::AssertSqlSafe(isolation))
            .execute(&mut *transaction)
            .await?;
        set_tenant(&mut transaction, tenant).await?;
        denied(insert(&mut transaction, tenant).await)?;
        transaction.rollback().await?;
    }
    for query in [
        "ALTER TABLE public.identities DISABLE TRIGGER identity_lifecycle_write_fence",
        "SET LOCAL session_replication_role='replica'",
        "CREATE OR REPLACE FUNCTION public.identity_lifecycle_write_fence() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
        "UPDATE public.identities SET tenant_id=tenant_id",
        "SET ROLE tgm_owner",
    ] {
        let mut transaction = app.begin().await?;
        let result = sqlx::query(sqlx::AssertSqlSafe(query))
            .execute(&mut *transaction)
            .await;
        assert!(result.is_err(), "unexpectedly allowed: {query}");
        transaction.rollback().await?;
    }
    let mut transaction = app.begin().await?;
    set_tenant(&mut transaction, tenant).await?;
    denied(
        sqlx::query_scalar("INSERT INTO public.identities(kind) VALUES('bot') RETURNING id")
            .fetch_one(&mut *transaction)
            .await,
    )?;
    transaction.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn temporary_objects_cannot_shadow_tenant_state(injected: PgPool) -> TestResult {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    let tenant = tenant(admin.raw(), "deleted").await?;
    let mut transaction = app.begin().await?;
    set_tenant(&mut transaction, tenant).await?;
    sqlx::query("CREATE TEMP TABLE tenants(id bigint,status text) ON COMMIT DROP")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("INSERT INTO pg_temp.tenants VALUES($1,'active')")
        .bind(tenant.get())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("SET LOCAL search_path=pg_temp,public,pg_catalog")
        .execute(&mut *transaction)
        .await?;
    denied(insert(&mut transaction, tenant).await)?;
    transaction.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn writer_first_blocks_deletion_until_identity_is_committed(injected: PgPool) -> TestResult {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    let tenant = tenant(admin.raw(), "active").await?;
    let operator = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            app.connect_options()
                .as_ref()
                .clone()
                .username("tenant_lifecycle")
                .password("tlpw"),
        )
        .await?;
    support::assert_role(&operator, "tenant_lifecycle").await;
    let mut writing = app.begin().await?;
    set_tenant(&mut writing, tenant).await?;
    let identity = insert(&mut writing, tenant).await?;
    let mut deleting = operator.begin().await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *deleting)
        .await?;
    let task = tokio::spawn(async move {
        lifecycle::lock(&mut deleting, tenant, true).await?;
        set_tenant(&mut deleting, tenant).await?;
        let visible: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM identities WHERE tenant_id=$1 AND id=$2)",
        )
        .bind(tenant.get())
        .bind(identity)
        .fetch_one(&mut *deleting)
        .await?;
        sqlx::query("UPDATE tenants SET status='suspended' WHERE id=$1")
            .bind(tenant.get())
            .execute(&mut *deleting)
            .await?;
        deleting.commit().await?;
        Ok::<_, sqlx::Error>(visible)
    });
    waiting(admin.raw(), pid).await?;
    writing.commit().await?;
    assert!(tokio::time::timeout(Duration::from_secs(10), task).await???);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn deletion_first_rechecks_state_after_waiting_not_statement_snapshot(
    injected: PgPool,
) -> TestResult {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    let tenant = tenant(admin.raw(), "active").await?;
    let operator = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            app.connect_options()
                .as_ref()
                .clone()
                .username("tenant_lifecycle")
                .password("tlpw"),
        )
        .await?;
    support::assert_role(&operator, "tenant_lifecycle").await;
    let mut deleting = operator.begin().await?;
    lifecycle::lock(&mut deleting, tenant, true).await?;
    set_tenant(&mut deleting, tenant).await?;
    let mut writing = app.begin().await?;
    set_tenant(&mut writing, tenant).await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *writing)
        .await?;
    let task = tokio::spawn(async move {
        let result = insert(&mut writing, tenant).await;
        writing.rollback().await?;
        Ok::<_, sqlx::Error>(result)
    });
    waiting(admin.raw(), pid).await?;
    sqlx::query("UPDATE tenants SET status='suspended' WHERE id=$1")
        .bind(tenant.get())
        .execute(&mut *deleting)
        .await?;
    sqlx::query("UPDATE tenants SET status='deleted' WHERE id=$1")
        .bind(tenant.get())
        .execute(&mut *deleting)
        .await?;
    deleting.commit().await?;
    denied(tokio::time::timeout(Duration::from_secs(10), task).await???)?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM identities WHERE tenant_id=$1")
        .bind(tenant.get())
        .fetch_one(admin.raw())
        .await?;
    assert_eq!(count, 0);
    let mut next_batch = app.begin().await?;
    set_tenant(&mut next_batch, tenant).await?;
    denied(insert(&mut next_batch, tenant).await)?;
    next_batch.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn removed_root_is_not_visible_through_the_prelock_snapshot(injected: PgPool) -> TestResult {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    support::assert_not_bypassrls(&app).await;
    let tenant = tenant(admin.raw(), "active").await?;
    let mut removing = admin.raw().begin().await?;
    lifecycle::lock(&mut removing, tenant, true).await?;
    let mut writing = app.begin().await?;
    set_tenant(&mut writing, tenant).await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *writing)
        .await?;
    let task = tokio::spawn(async move {
        let result = insert(&mut writing, tenant).await;
        writing.rollback().await?;
        Ok::<_, sqlx::Error>(result)
    });
    waiting(admin.raw(), pid).await?;
    sqlx::query("DELETE FROM tenants WHERE id=$1")
        .bind(tenant.get())
        .execute(&mut *removing)
        .await?;
    removing.commit().await?;
    denied(tokio::time::timeout(Duration::from_secs(10), task).await???)?;
    Ok(())
}
