mod support;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::{
    lifecycle::{self, LifecycleError, WorkerGuard},
    repo,
    tenant::tenant_write_tx,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct PausedKek {
    inner: tgm_core::kek::EnvKek,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl tgm_core::kek::Kek for PausedKek {
    async fn wrap(
        &self,
        dek: &tgm_core::secret::Secret<[u8; 32]>,
        context: &tgm_core::kek::KekContext,
    ) -> Result<Vec<u8>, tgm_core::kek::KekError> {
        self.started.notify_one();
        self.release.notified().await;
        self.inner.wrap(dek, context).await
    }
    async fn unwrap(
        &self,
        ciphertext: &[u8],
        context: &tgm_core::kek::KekContext,
    ) -> Result<tgm_core::secret::Secret<[u8; 32]>, tgm_core::kek::KekError> {
        self.inner.unwrap(ciphertext, context).await
    }
    fn kek_id(&self) -> &str {
        self.inner.kek_id()
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn kms_result_cannot_recreate_credentials_after_suspension(injected: PgPool) -> TestResult {
    let (app, operator, admin, tenant, identity) = setup(injected).await?;
    let kek = std::sync::Arc::new(PausedKek {
        inner: tgm_core::kek::EnvKek::new(
            tgm_core::profile::Profile::Development,
            "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
        )?,
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let pool = app.clone();
    let worker_kek = kek.clone();
    let writing = tokio::spawn(async move {
        tgm_db::credentials::store_identity_secret(
            &pool,
            worker_kek.as_ref(),
            tenant,
            identity,
            &tgm_core::secret::Secret::new(b"fake-session".to_vec()),
        )
        .await
    });
    kek.started.notified().await;
    lifecycle::transition(&operator, tenant.get(), true).await?;
    assert!(!lifecycle::drained(&operator, tenant).await?);
    kek.release.notify_one();
    assert!(writing.await?.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM identity_secrets WHERE identity_id=$1")
            .bind(identity.get())
            .fetch_one(admin.raw())
            .await?,
        0
    );
    assert!(lifecycle::drained(&operator, tenant).await?);
    lifecycle::transition(&operator, tenant.get(), false).await?;
    let pool = app.clone();
    let worker_kek = kek.clone();
    let writing = tokio::spawn(async move {
        tgm_db::credentials::store_identity_secret(
            &pool,
            worker_kek.as_ref(),
            tenant,
            identity,
            &tgm_core::secret::Secret::new(b"fake-session".to_vec()),
        )
        .await
    });
    kek.started.notified().await;
    kek.release.notify_one();
    writing.await??;
    lifecycle::transition(&operator, tenant.get(), true).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM identity_secrets WHERE identity_id=$1")
            .bind(identity.get())
            .fetch_one(admin.raw())
            .await?,
        1
    );
    Ok(())
}

async fn setup(
    injected: PgPool,
) -> Result<
    (PgPool, PgPool, support::AdminOnlyPool, TenantId, IdentityId),
    Box<dyn std::error::Error>,
> {
    let (app, admin) = support::pools(injected).await;
    support::assert_role(&app, "app_user").await;
    let lifecycle = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(
            app.connect_options()
                .as_ref()
                .clone()
                .username("tenant_lifecycle")
                .password("tlpw"),
        )
        .await?;
    support::assert_role(&lifecycle, "tenant_lifecycle").await;
    support::assert_not_bypassrls(&lifecycle).await;
    lifecycle::check_connection(&lifecycle).await?;
    let tenant: TenantId = sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id) VALUES('lifecycle',97101) RETURNING id",
    )
    .fetch_one(admin.raw())
    .await?;
    let identity: IdentityId =
        sqlx::query_scalar("INSERT INTO identities(kind,tenant_id) VALUES('bot',$1) RETURNING id")
            .bind(tenant.get())
            .fetch_one(admin.raw())
            .await?;
    Ok((app, lifecycle, admin, tenant, identity))
}

async fn task(
    admin: &PgPool,
    tenant: TenantId,
    identity: IdentityId,
    status: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("INSERT INTO delivery_tasks(tenant_id,sender_identity_id,target_tg_chat_id,action,status) VALUES($1,$2,-1001,'COPY',$3) RETURNING id")
        .bind(tenant.get()).bind(identity.get()).bind(status).fetch_one(admin).await
}

async fn waiting(admin: &PgPool, role: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid WHERE activity.datname=current_database() AND activity.usename=$1 AND NOT locks.granted)")
            .bind(role).fetch_one(admin).await? { return Ok(()); }
        assert!(Instant::now() < deadline, "未观察到真正的数据库锁等待");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn transitions_time_permissions_and_fencing(injected: PgPool) -> TestResult {
    let (app, operator, admin, tenant, identity) = setup(injected).await?;
    assert!(lifecycle::check_connection(&app).await.is_err());
    let missing: TenantId = sqlx::query_scalar("SELECT 0::bigint")
        .fetch_one(&app)
        .await?;
    assert!(WorkerGuard::acquire(&app, missing).await?.is_none());
    assert_eq!(
        repo::acquire_lease(&app, Some(tenant), identity, "old").await?,
        Some(1)
    );
    lifecycle::transition(&operator, tenant.get(), true).await?;
    let first: String = sqlx::query_scalar("SELECT suspended_at::text FROM tenants WHERE id=$1")
        .bind(tenant.get())
        .fetch_one(admin.raw())
        .await?;
    lifecycle::transition(&operator, tenant.get(), true).await?;
    assert_eq!(
        first,
        sqlx::query_scalar::<_, String>("SELECT suspended_at::text FROM tenants WHERE id=$1")
            .bind(tenant.get())
            .fetch_one(admin.raw())
            .await?
    );
    assert!(!repo::renew_lease(&app, Some(tenant), identity, "old", 1).await?);
    assert!(
        repo::acquire_lease(&app, Some(tenant), identity, "new")
            .await?
            .is_none()
    );
    assert!(
        tenant_write_tx(&app, tenant, |_| Box::pin(async { Ok(()) }))
            .await
            .is_err()
    );
    assert!(WorkerGuard::acquire(&app, tenant).await?.is_none());
    assert_eq!(lifecycle::cancel_batch(&operator, tenant).await?, Some(0));
    assert!(lifecycle::drained(&operator, tenant).await?);
    lifecycle::transition(&operator, tenant.get(), false).await?;
    lifecycle::transition(&operator, tenant.get(), false).await?;
    assert_eq!(
        repo::acquire_lease(&app, Some(tenant), identity, "new").await?,
        Some(2)
    );
    for query in [
        "UPDATE tenants SET status='suspended'",
        "UPDATE tenants SET suspended_at=now()",
        "UPDATE tenants SET plan='pro'",
    ] {
        assert!(sqlx::query(query).execute(&app).await.is_err());
    }
    for query in [
        "SELECT secret_ciphertext FROM identity_secrets",
        "SELECT text FROM canonical_events",
        "SELECT id FROM web_sessions",
        "DELETE FROM tenants",
        "UPDATE tenant_consents SET text_version='forged'",
    ] {
        assert!(sqlx::query(query).execute(&operator).await.is_err());
    }
    sqlx::query("UPDATE tenants SET status='deleted' WHERE id=$1")
        .bind(tenant.get())
        .execute(admin.raw())
        .await?;
    assert!(matches!(
        lifecycle::transition(&operator, tenant.get(), false).await,
        Err(LifecycleError::State)
    ));
    assert!(!lifecycle::readable(&app, tenant).await?);
    assert!(matches!(
        lifecycle::transition(&operator, i64::MAX, true).await,
        Err(LifecycleError::State)
    ));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn real_lock_order_preserves_admitted_write_and_blocks_late_write(
    injected: PgPool,
) -> TestResult {
    let (app, operator, admin, tenant, _identity) = setup(injected).await?;
    let mut writer = app.begin().await?;
    lifecycle::require_active(&mut writer, tenant).await?;
    sqlx::query("UPDATE tenants SET name='admitted' WHERE id=$1")
        .bind(tenant.get())
        .execute(&mut *writer)
        .await?;
    let pool = operator.clone();
    let suspending =
        tokio::spawn(async move { lifecycle::transition(&pool, tenant.get(), true).await });
    waiting(admin.raw(), "tenant_lifecycle").await?;
    writer.commit().await?;
    suspending.await??;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT name FROM tenants WHERE id=$1")
            .bind(tenant.get())
            .fetch_one(admin.raw())
            .await?,
        "admitted"
    );
    lifecycle::transition(&operator, tenant.get(), false).await?;
    let mut stopping = operator.begin().await?;
    lifecycle::lock(&mut stopping, tenant, true).await?;
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(&mut *stopping)
        .await?;
    sqlx::query("UPDATE tenants SET status='suspended' WHERE id=$1")
        .bind(tenant.get())
        .execute(&mut *stopping)
        .await?;
    let pool = app.clone();
    let writing = tokio::spawn(async move {
        tenant_write_tx(&pool, tenant, |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE tenants SET name='late' WHERE id=$1")
                    .bind(tenant.get())
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await
    });
    waiting(admin.raw(), "app_user").await?;
    stopping.commit().await?;
    assert!(writing.await?.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT name FROM tenants WHERE id=$1")
            .bind(tenant.get())
            .fetch_one(admin.raw())
            .await?,
        "admitted"
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn batches_inflight_success_failure_uncertainty_and_resume(injected: PgPool) -> TestResult {
    let (app, operator, admin, tenant, identity) = setup(injected).await?;
    sqlx::query("INSERT INTO delivery_tasks(tenant_id,sender_identity_id,target_tg_chat_id,action) SELECT $1,$2,-1001,'COPY' FROM generate_series(1,5001)")
        .bind(tenant.get()).bind(identity.get()).execute(admin.raw()).await?;
    let success = task(admin.raw(), tenant, identity, "SENDING").await?;
    let failure = task(admin.raw(), tenant, identity, "SENDING").await?;
    let uncertain = task(admin.raw(), tenant, identity, "SENDING").await?;
    let guard = WorkerGuard::acquire(&app, tenant)
        .await?
        .ok_or("执行准入失败")?;
    lifecycle::transition(&operator, tenant.get(), true).await?;
    assert_eq!(
        lifecycle::cancel_batch(&operator, tenant).await?,
        Some(5000)
    );
    assert_eq!(lifecycle::cancel_batch(&operator, tenant).await?, Some(1));
    assert!(!lifecycle::drained(&operator, tenant).await?);
    assert!(matches!(
        lifecycle::transition(&operator, tenant.get(), false).await,
        Err(LifecycleError::Draining)
    ));
    assert!(repo::complete_delivery_task(&app, tenant, success, 817, None).await?);
    for (id, code, expected) in [
        (failure, "CHAT_WRITE_FORBIDDEN", "CANCELLED"),
        (uncertain, "TELEGRAM_CLIENT_ERROR", "SENDING"),
    ] {
        let result = repo::record_delivery_failure(
            &app,
            tenant,
            id,
            repo::DeliveryTaskFailureInput {
                status: repo::DeliveryTaskFailureStatus::Pending,
                attempt_count: 1,
                next_retry_at: Some(sqlx::types::chrono::Utc::now()),
                error_code: code.to_owned(),
                error_message: None,
                latency_ms: None,
            },
        )
        .await?
        .ok_or("未写入结局")?;
        assert_eq!(result.status, expected);
        assert!(result.next_retry_at.is_none());
    }
    guard.close().await?;
    assert!(!lifecycle::drained(&operator, tenant).await?);
    assert!(repo::complete_delivery_task(&app, tenant, uncertain, 818, None).await?);
    assert!(lifecycle::drained(&operator, tenant).await?);
    lifecycle::transition(&operator, tenant.get(), false).await?;
    assert_eq!(lifecycle::cancel_batch(&operator, tenant).await?, None);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM delivery_tasks WHERE status='CANCELLED'"
        )
        .fetch_one(admin.raw())
        .await?,
        5002
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn live_execution_marker_blocks_resume_without_holding_transaction(
    injected: PgPool,
) -> TestResult {
    let (app, operator, _admin, tenant, _identity) = setup(injected).await?;
    let guard = WorkerGuard::acquire(&app, tenant)
        .await?
        .ok_or("准入失败")?;
    lifecycle::transition(&operator, tenant.get(), true).await?;
    assert!(!lifecycle::drained(&operator, tenant).await?);
    assert!(matches!(
        lifecycle::transition(&operator, tenant.get(), false).await,
        Err(LifecycleError::Draining)
    ));
    guard.close().await?;
    assert!(lifecycle::drained(&operator, tenant).await?);
    lifecycle::transition(&operator, tenant.get(), false).await?;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn execution_marker_is_visible_while_admission_waits_for_lifecycle_lock(
    injected: PgPool,
) -> TestResult {
    let (app, operator, admin, tenant, _identity) = setup(injected).await?;
    let mut stopping = operator.begin().await?;
    lifecycle::lock(&mut stopping, tenant, true).await?;
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(&mut *stopping)
        .await?;
    sqlx::query("UPDATE tenants SET status='suspended' WHERE id=$1")
        .bind(tenant.get())
        .execute(&mut *stopping)
        .await?;

    let pool = app.clone();
    let acquiring = tokio::spawn(async move { WorkerGuard::acquire(&pool, tenant).await });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let marker_visible: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
              WHERE datname=current_database() AND application_name=$1)",
        )
        .bind(format!("tgm-execution:{}", tenant.get()))
        .fetch_one(admin.raw())
        .await?;
        if marker_visible {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "执行准入等待生命周期锁期间没有登记执行标记"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    stopping.commit().await?;
    assert!(acquiring.await??.is_none());
    assert!(lifecycle::drained(&operator, tenant).await?);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cancellation_log_failure_rolls_back_and_target_isolation_holds(
    injected: PgPool,
) -> TestResult {
    let (_app, operator, admin, tenant, identity) = setup(injected).await?;
    let own = task(admin.raw(), tenant, identity, "PENDING").await?;
    let other: TenantId = sqlx::query_scalar(
        "INSERT INTO tenants(name,owner_user_id) VALUES('other',97102) RETURNING id",
    )
    .fetch_one(admin.raw())
    .await?;
    task(admin.raw(), other, identity, "PENDING").await?;
    lifecycle::transition(&operator, tenant.get(), true).await?;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT lifecycle_log_rejected CHECK(module<>'tenant_lifecycle'); RESET ROLE").execute(admin.raw()).await?;
    assert!(lifecycle::cancel_batch(&operator, tenant).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM delivery_tasks WHERE id=$1")
            .bind(own)
            .fetch_one(admin.raw())
            .await?,
        "PENDING"
    );
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT lifecycle_log_rejected; RESET ROLE").execute(admin.raw()).await?;
    assert_eq!(lifecycle::cancel_batch(&operator, tenant).await?, Some(1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM delivery_tasks WHERE tenant_id=$1 AND status='PENDING'"
        )
        .bind(other.get())
        .fetch_one(admin.raw())
        .await?,
        1
    );
    Ok(())
}
