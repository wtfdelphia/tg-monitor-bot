use sqlx::PgPool;

#[derive(Debug, thiserror::Error)]
pub enum MediaCleanupError {
    #[error("媒体清理数据库版本检查失败")]
    Preflight(#[from] crate::preflight::PreflightError),
    #[error("媒体清理必须直接使用 media_cleanup 登录")]
    WrongRole,
    #[error("媒体清理角色带有不允许的属性或成员关系")]
    RolePrivileges,
    #[error("媒体清理角色权限检查失败")]
    Database(#[from] sqlx::Error),
}

pub async fn check_connection(pool: &PgPool) -> Result<(), MediaCleanupError> {
    crate::preflight::check_version(pool).await?;
    let role = crate::preflight::check_runtime_role(pool).await?;
    let direct: bool = sqlx::query_scalar("SELECT session_user = 'media_cleanup'")
        .fetch_one(pool)
        .await?;
    if role != "media_cleanup" || !direct {
        return Err(MediaCleanupError::WrongRole);
    }
    let safe: bool = sqlx::query_scalar(
        "SELECT rolcanlogin
                AND NOT (rolsuper OR rolbypassrls OR rolcreatedb OR rolcreaterole OR rolreplication)
                AND NOT EXISTS (
                    SELECT 1 FROM pg_auth_members
                     WHERE member = pg_roles.oid OR roleid = pg_roles.oid
                )
           FROM pg_roles
          WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await?;
    if !safe {
        return Err(MediaCleanupError::RolePrivileges);
    }
    Ok(())
}

pub async fn clear_expired_objects_and_log(
    pool: &PgPool,
    candidates: &[(i64, String)],
    elapsed_ms: u64,
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let mut cleared = 0_u64;
    for (archive_id, object_key) in candidates {
        let updated = sqlx::query(
            "UPDATE media_archive
                SET object_key = NULL,
                    object_expires_at = NULL
              WHERE id = $1
                AND object_key = $2
                AND object_expires_at < clock_timestamp()",
        )
        .bind(archive_id)
        .bind(object_key)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        cleared += u64::from(updated == 1);
    }
    insert_cleanup_log(
        &mut transaction,
        "media_archive.object",
        cleared,
        elapsed_ms,
    )
    .await?;
    transaction.commit().await?;
    Ok(cleared)
}

pub async fn delete_stale_archives_and_log(
    pool: &PgPool,
    retention_days: i32,
    limit: i64,
    elapsed_ms: u64,
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let deleted = sqlx::query(
        "DELETE FROM media_archive
          WHERE id IN (
                SELECT archive.id
                  FROM media_archive AS archive
                 WHERE archive.first_seen_at < clock_timestamp() - make_interval(days => $1)
                   AND archive.object_key IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM media_relay_jobs AS job
                        WHERE job.media_archive_id = archive.id)
                 ORDER BY archive.first_seen_at, archive.id
                 LIMIT $2)",
    )
    .bind(retention_days)
    .bind(limit)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    insert_cleanup_log(&mut transaction, "media_archive", deleted, elapsed_ms).await?;
    transaction.commit().await?;
    Ok(deleted)
}

pub async fn log_cleanup_batch(
    pool: &PgPool,
    table: &str,
    rows: u64,
    elapsed_ms: u64,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    insert_cleanup_log(&mut transaction, table, rows, elapsed_ms).await?;
    transaction.commit().await?;
    Ok(())
}

async fn insert_cleanup_log(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    rows: u64,
    elapsed_ms: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO system_logs (tenant_id, level, module, event, context)
         VALUES (NULL, 'INFO', 'media_retention', 'cleanup.done', $1)",
    )
    .bind(serde_json::json!({
        "table": table,
        "rows": rows,
        "elapsed_ms": elapsed_ms,
    }))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}
