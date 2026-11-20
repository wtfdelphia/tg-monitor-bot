use sqlx::{Acquire, FromRow, PgPool};

pub const BATCH_LIMIT: i64 = 5000;

#[derive(Debug, thiserror::Error)]
pub enum PurgeError {
    #[error("租户物理清理数据库版本或角色检查失败")]
    Preflight(#[from] crate::preflight::PreflightError),
    #[error("租户物理清理必须直接使用 tenant_purge 登录角色")]
    WrongRole,
    #[error("租户物理清理必须使用 READ COMMITTED")]
    WrongIsolation,
    #[error("租户物理清理必须使用保持同一后端连接的直连会话")]
    NotDirect,
    #[error("租户物理清理数据库查询失败")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct BatchResult {
    pub result: String,
    pub rows_deleted: i64,
    pub blocked: i64,
}

pub async fn check_connection(pool: &PgPool) -> Result<(), PurgeError> {
    crate::preflight::check_version(pool).await?;
    let role = crate::preflight::check_runtime_role(pool).await?;
    if role != "tenant_purge" {
        return Err(PurgeError::WrongRole);
    }

    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let (session_user, current_user, isolation, before_pid): (String, String, String, i32) =
        sqlx::query_as(
            "SELECT session_user::text, current_user::text,
                    current_setting('transaction_isolation'), pg_backend_pid()",
        )
        .fetch_one(&mut *connection)
        .await?;
    if session_user != "tenant_purge" || current_user != "tenant_purge" {
        return Err(PurgeError::WrongRole);
    }
    if isolation != "read committed" {
        return Err(PurgeError::WrongIsolation);
    }
    let mut transaction = connection.begin().await?;
    let transaction_isolation: String =
        sqlx::query_scalar("SELECT current_setting('transaction_isolation')")
            .fetch_one(&mut *transaction)
            .await?;
    if transaction_isolation != "read committed" {
        return Err(PurgeError::WrongIsolation);
    }
    transaction.commit().await?;
    let after_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await?;
    if before_pid != after_pid {
        return Err(PurgeError::NotDirect);
    }
    Ok(())
}

pub async fn candidates(pool: &PgPool, limit: i64) -> Result<Vec<i64>, PurgeError> {
    Ok(
        sqlx::query_scalar("SELECT public.tenant_purge_candidates($1)")
            .bind(limit)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn batch(
    pool: &PgPool,
    tenant_id: i64,
    category: &str,
) -> Result<BatchResult, PurgeError> {
    Ok(sqlx::query_as(
        "SELECT result, rows_deleted, blocked
           FROM public.tenant_purge_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(category)
    .fetch_one(pool)
    .await?)
}

pub async fn finalize(pool: &PgPool, tenant_id: i64) -> Result<String, PurgeError> {
    Ok(
        sqlx::query_scalar("SELECT public.tenant_purge_finalize($1)")
            .bind(tenant_id)
            .fetch_one(pool)
            .await?,
    )
}
