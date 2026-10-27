use sqlx::PgPool;
use std::time::Instant;

pub const BATCH_LIMIT: i64 = 5000;
pub const LOCK_KEY: i64 = 0x74676d_706c01;

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error("平台日志清理数据库角色或版本检查失败")]
    Preflight(#[from] crate::preflight::PreflightError),
    #[error("平台日志清理必须直接使用无额外权限的 platform_cleanup 登录")]
    WrongRole,
    #[error("平台日志清理只支持 READ COMMITTED")]
    Isolation,
    #[error("平台日志清理批次失败，未提交的删除和日志已回滚")]
    Database(#[from] sqlx::Error),
}

pub async fn check_connection(pool: &PgPool) -> Result<(), RetentionError> {
    crate::preflight::check_version(pool).await?;
    let role = crate::preflight::check_runtime_role(pool).await?;
    let safe: bool = sqlx::query_scalar(
        "SELECT session_user = 'platform_cleanup' AND rolcanlogin
                AND NOT (rolsuper OR rolbypassrls OR rolcreatedb OR rolcreaterole OR rolreplication)
                AND NOT EXISTS (SELECT 1 FROM pg_auth_members
                                 WHERE member = pg_roles.oid OR roleid = pg_roles.oid)
           FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await?;
    if role != "platform_cleanup" || !safe {
        return Err(RetentionError::WrongRole);
    }
    Ok(())
}

pub async fn cleanup_batch(pool: &PgPool) -> Result<Option<u64>, RetentionError> {
    let started = Instant::now();
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let mut transaction = sqlx::Connection::begin(&mut *connection).await?;
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut *transaction)
        .await?;
    if isolation != "read committed" {
        return Err(RetentionError::Isolation);
    }
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(LOCK_KEY)
        .fetch_one(&mut *transaction)
        .await?;
    if !acquired {
        transaction.rollback().await?;
        return Ok(None);
    }
    let cutoff = sqlx::query_scalar("SELECT statement_timestamp() - interval '90 days'")
        .fetch_one(&mut *transaction)
        .await?;
    let deleted = delete_expired(&mut transaction, cutoff).await?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    sqlx::query(
        "INSERT INTO system_logs (tenant_id, level, module, event, context)
                 VALUES (NULL, 'INFO', 'platform_retention', 'cleanup.done', $1)",
    )
    .bind(serde_json::json!({"table": "system_logs", "rows": deleted, "elapsed_ms": elapsed_ms}))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Some(deleted))
}

async fn delete_expired(
    connection: &mut sqlx::PgConnection,
    cutoff: sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "WITH candidates AS MATERIALIZED (
             SELECT id FROM system_logs
              WHERE tenant_id IS NULL AND created_at <= $2
              ORDER BY created_at, id LIMIT $1
         ) DELETE FROM system_logs target USING candidates
            WHERE target.id = candidates.id AND target.tenant_id IS NULL AND target.created_at <= $2",
    )
    .bind(BATCH_LIMIT)
    .bind(cutoff)
    .execute(connection)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../../migrations-test")]
    async fn exact_cutoff_is_deleted_and_next_microsecond_is_kept(
        admin: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cutoff = sqlx::query_scalar("SELECT now() - interval '91 days'")
            .fetch_one(&admin)
            .await?;
        sqlx::query("INSERT INTO system_logs (level,module,event,created_at)
                     VALUES ('INFO','test','exact',$1), ('INFO','test','next',$1::timestamptz + interval '1 microsecond')")
            .bind(cutoff).execute(&admin).await?;
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&admin)
            .await?;
        let pool = PgPool::connect(&format!(
            "postgres://platform_cleanup:pcpw@127.0.0.1:55432/{database}"
        ))
        .await?;
        check_connection(&pool).await?;
        let mut connection = pool.acquire().await?;
        assert_eq!(delete_expired(&mut connection, cutoff).await?, 1);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT event FROM system_logs")
                .fetch_one(&admin)
                .await?,
            "next"
        );
        Ok(())
    }
}
