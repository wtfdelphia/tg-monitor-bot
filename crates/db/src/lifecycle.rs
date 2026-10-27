use sqlx::{Connection, PgConnection, PgPool, postgres::PgConnectOptions};
use tgm_core::ids::{IdentityId, TenantId};

pub const INACTIVE: &str = "tgm: tenant is not active";
pub const BATCH_LIMIT: i64 = 5000;

pub async fn lock(
    connection: &mut PgConnection,
    tenant: TenantId,
    exclusive: bool,
) -> Result<(), sqlx::Error> {
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut *connection)
        .await?;
    if isolation != "read committed" {
        return Err(sqlx::Error::Protocol(
            "tenant lifecycle requires READ COMMITTED".to_owned(),
        ));
    }
    let query = if exclusive {
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 419871))"
    } else {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 419871))"
    };
    sqlx::query(query)
        .bind(format!("tenant_lifecycle:{}", tenant.get()))
        .execute(connection)
        .await?;
    Ok(())
}

pub async fn active(connection: &mut PgConnection, tenant: TenantId) -> Result<bool, sqlx::Error> {
    lock(connection, tenant, false).await?;
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tenants WHERE id=$1 AND status='active')")
        .bind(tenant.get())
        .fetch_one(connection)
        .await
}

pub async fn is_active(pool: &PgPool, tenant: TenantId) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tenants WHERE id=$1 AND status='active')")
        .bind(tenant.get())
        .fetch_one(pool)
        .await
}

pub async fn readable(pool: &PgPool, tenant: TenantId) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tenants WHERE id=$1 AND status IN ('active','suspended'))",
    )
    .bind(tenant.get())
    .fetch_one(pool)
    .await
}

pub async fn require_active(
    connection: &mut PgConnection,
    tenant: TenantId,
) -> Result<(), sqlx::Error> {
    if !active(connection, tenant).await? {
        return Err(sqlx::Error::Protocol(INACTIVE.to_owned()));
    }
    Ok(())
}

pub async fn identity_active(
    connection: &mut PgConnection,
    identity: IdentityId,
    tenant: Option<TenantId>,
) -> Result<bool, sqlx::Error> {
    if let Some(tenant) = tenant
        && !active(connection, tenant).await?
    {
        return Ok(false);
    }
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.map_or_else(|| "0".to_owned(), |tenant| tenant.get().to_string()))
        .execute(&mut *connection)
        .await?;
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM identities WHERE id=$1 AND tenant_id IS NOT DISTINCT FROM $2)",
    )
    .bind(identity.get())
    .bind(tenant.map(TenantId::get))
    .fetch_one(connection)
    .await
}

pub struct WorkerGuard(PgConnection);

impl WorkerGuard {
    pub async fn acquire(pool: &PgPool, tenant: TenantId) -> Result<Option<Self>, sqlx::Error> {
        let options = match std::env::var("TGM_DB_URL_EXECUTION") {
            Ok(url) => url.parse::<PgConnectOptions>()?,
            Err(_) if std::env::var("TGM_PROFILE").as_deref() == Ok("production") => {
                return Err(sqlx::Error::Protocol(
                    "production execution marker requires a direct connection".to_owned(),
                ));
            }
            Err(_) => pool.connect_options().as_ref().clone(),
        }
        .application_name(&format!("tgm-execution:{}", tenant.get()));
        let mut connection = PgConnection::connect_with(&options).await?;
        let expected: (String, Option<String>, i32) = sqlx::query_as(
            "SELECT current_database()::text,inet_server_addr()::text,inet_server_port()",
        )
        .fetch_one(pool)
        .await?;
        let actual: (String, Option<String>, i32) = sqlx::query_as(
            "SELECT current_database()::text,inet_server_addr()::text,inet_server_port()",
        )
        .fetch_one(&mut connection)
        .await?;
        if actual != expected {
            return Err(sqlx::Error::Protocol(
                "execution marker database mismatch".to_owned(),
            ));
        }
        let mut transaction = connection.begin().await?;
        let valid: bool = sqlx::query_scalar("SELECT current_user='app_user' AND NOT rolsuper AND NOT rolbypassrls FROM pg_roles WHERE rolname=current_user").fetch_one(&mut *transaction).await?;
        if !valid {
            return Err(sqlx::Error::Protocol(
                "execution marker requires app_user".to_owned(),
            ));
        }
        let is_active = active(&mut transaction, tenant).await?;
        let marker_name = format!("tgm-execution:{}", tenant.get());
        let (marker_pid, registered_name): (i32, String) =
            sqlx::query_as("SELECT pg_backend_pid(), current_setting('application_name')")
                .fetch_one(&mut *transaction)
                .await?;
        if registered_name != marker_name {
            return Err(sqlx::Error::Protocol(
                "execution marker requires a session-pooling direct connection".to_owned(),
            ));
        }
        transaction.commit().await?;
        let (current_pid, current_name): (i32, String) =
            sqlx::query_as("SELECT pg_backend_pid(), current_setting('application_name')")
                .fetch_one(&mut connection)
                .await?;
        if current_pid != marker_pid || current_name != marker_name {
            let _ = connection.close().await;
            return Err(sqlx::Error::Protocol(
                "execution marker requires a session-pooling direct connection".to_owned(),
            ));
        }
        if is_active {
            Ok(Some(Self(connection)))
        } else {
            connection.close().await?;
            Ok(None)
        }
    }

    pub async fn close(self) -> Result<(), sqlx::Error> {
        self.0.close().await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("租户生命周期数据库预检失败")]
    Preflight(#[from] crate::preflight::PreflightError),
    #[error("租户生命周期必须直接使用独立受限角色")]
    WrongRole,
    #[error("租户不存在或状态不允许该操作")]
    State,
    #[error("租户仍有在途执行，未完成排空")]
    Draining,
    #[error("租户生命周期事务失败")]
    Database(#[from] sqlx::Error),
}

pub async fn check_connection(pool: &PgPool) -> Result<(), LifecycleError> {
    crate::preflight::check_version(pool).await?;
    let role = crate::preflight::check_runtime_role(pool).await?;
    let safe: bool = sqlx::query_scalar("SELECT session_user='tenant_lifecycle' AND rolcanlogin
       AND NOT(rolsuper OR rolbypassrls OR rolcreatedb OR rolcreaterole OR rolreplication)
       AND NOT EXISTS(SELECT 1 FROM pg_auth_members WHERE member=pg_roles.oid OR roleid=pg_roles.oid)
       FROM pg_roles WHERE rolname=current_user").fetch_one(pool).await?;
    if role != "tenant_lifecycle" || !safe {
        return Err(LifecycleError::WrongRole);
    }
    Ok(())
}

async fn tenant(pool: &PgPool, id: i64) -> Result<TenantId, LifecycleError> {
    sqlx::query_scalar("SELECT id FROM tenants WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(LifecycleError::State)
}

async fn workers_stopped(
    connection: &mut PgConnection,
    tenant: TenantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND usename='app_user' AND application_name=$1)")
        .bind(format!("tgm-execution:{}", tenant.get())).fetch_one(connection).await
}

pub async fn transition(pool: &PgPool, id: i64, suspend: bool) -> Result<TenantId, LifecycleError> {
    let tenant = tenant(pool, id).await?;
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let mut transaction = connection.begin().await?;
    lock(&mut transaction, tenant, true).await?;
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(id.to_string())
        .execute(&mut *transaction)
        .await?;
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    if !matches!(status.as_str(), "active" | "suspended") {
        return Err(LifecycleError::State);
    }
    if !suspend {
        let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM delivery_tasks WHERE tenant_id=$1 AND status IN ('PENDING','FAILED','SENDING'))")
            .bind(id).fetch_one(&mut *transaction).await?;
        let leases: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account_leases WHERE expires_at>statement_timestamp() AND identity_id IN(SELECT id FROM identities WHERE tenant_id=$1))")
            .bind(id).fetch_one(&mut *transaction).await?;
        if status == "suspended"
            && (pending || leases || !workers_stopped(&mut transaction, tenant).await?)
        {
            return Err(LifecycleError::Draining);
        }
    }
    let next = if suspend { "suspended" } else { "active" };
    if status != next {
        sqlx::query("UPDATE tenants SET status=$2,suspended_at=CASE WHEN $2='suspended' THEN clock_timestamp() ELSE NULL END,updated_at=clock_timestamp() WHERE id=$1")
            .bind(id).bind(next).execute(&mut *transaction).await?;
        sqlx::query("INSERT INTO audit_logs(tenant_id,action,resource,resource_id,before,after,source) VALUES($1,$2,'tenant',$1,$3,$4,'system')")
            .bind(id).bind(if suspend {"tenant.suspend"} else {"tenant.resume"})
            .bind(serde_json::json!({"status":status})).bind(serde_json::json!({"status":next})).execute(&mut *transaction).await?;
    }
    transaction.commit().await?;
    Ok(tenant)
}

pub async fn cancel_batch(pool: &PgPool, tenant: TenantId) -> Result<Option<u64>, LifecycleError> {
    let started = std::time::Instant::now();
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let mut transaction = connection.begin().await?;
    lock(&mut transaction, tenant, true).await?;
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(&mut *transaction)
        .await?;
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id=$1")
        .bind(tenant.get())
        .fetch_one(&mut *transaction)
        .await?;
    if status != "suspended" {
        transaction.rollback().await?;
        return Ok(None);
    }
    sqlx::query("WITH candidates AS MATERIALIZED(SELECT identity_id FROM account_leases WHERE expires_at>statement_timestamp() AND identity_id IN(SELECT id FROM identities WHERE tenant_id=$1) ORDER BY identity_id LIMIT $2) UPDATE account_leases lease SET expires_at=statement_timestamp() FROM candidates WHERE lease.identity_id=candidates.identity_id")
        .bind(tenant.get()).bind(BATCH_LIMIT).execute(&mut *transaction).await?;
    let rows=sqlx::query("WITH candidates AS MATERIALIZED(SELECT id FROM delivery_tasks WHERE tenant_id=$1 AND status IN ('PENDING','FAILED') ORDER BY id LIMIT $2)
        UPDATE delivery_tasks task SET status='CANCELLED',next_retry_at=NULL,completed_at=clock_timestamp(),updated_at=clock_timestamp()
        FROM candidates WHERE task.id=candidates.id AND task.tenant_id=$1 AND task.status IN ('PENDING','FAILED')")
        .bind(tenant.get()).bind(BATCH_LIMIT).execute(&mut *transaction).await?.rows_affected();
    sqlx::query("INSERT INTO system_logs(tenant_id,level,module,event,context) VALUES($1,'INFO','tenant_lifecycle','cleanup.done',$2)")
        .bind(tenant.get()).bind(serde_json::json!({"table":"delivery_tasks","rows":rows,"elapsed_ms":u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)}))
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(Some(rows))
}

pub async fn drained(pool: &PgPool, tenant: TenantId) -> Result<bool, LifecycleError> {
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let mut transaction = connection.begin().await?;
    lock(&mut transaction, tenant, true).await?;
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(&mut *transaction)
        .await?;
    let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM delivery_tasks WHERE tenant_id=$1 AND status IN('PENDING','FAILED','SENDING'))")
        .bind(tenant.get()).fetch_one(&mut *transaction).await?;
    let leases: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account_leases WHERE expires_at>statement_timestamp() AND identity_id IN(SELECT id FROM identities WHERE tenant_id=$1))")
        .bind(tenant.get()).fetch_one(&mut *transaction).await?;
    let stopped = workers_stopped(&mut transaction, tenant).await?;
    transaction.commit().await?;
    Ok(!pending && !leases && stopped)
}
