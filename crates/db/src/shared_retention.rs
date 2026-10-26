use sqlx::{PgConnection, PgPool};
use std::time::Instant;

pub use crate::auth_retention::BATCH_LIMIT;

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error("共享事件清理数据库角色或版本检查失败")]
    Preflight(#[from] crate::preflight::PreflightError),
    #[error("共享事件清理必须直接使用无额外属性或成员关系的 shared_cleanup 登录")]
    WrongRole,
    #[error("共享事件清理批次失败，未提交的删除和日志已回滚")]
    Database(#[from] sqlx::Error),
}

pub async fn check_connection(pool: &PgPool) -> Result<(), RetentionError> {
    crate::preflight::check_version(pool).await?;
    let role = crate::preflight::check_runtime_role(pool).await?;
    let safe: bool = sqlx::query_scalar(
        "SELECT session_user = 'shared_cleanup'
          AND rolcanlogin
          AND NOT (rolsuper OR rolbypassrls OR rolcreatedb OR rolcreaterole OR rolreplication)
          AND NOT EXISTS (
                SELECT 1 FROM pg_auth_members
                 WHERE member = pg_roles.oid OR roleid = pg_roles.oid
          )
           FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await?;
    if role != "shared_cleanup" || !safe {
        return Err(RetentionError::WrongRole);
    }
    Ok(())
}

pub async fn lock_writer(connection: &mut PgConnection, identity: i64) -> Result<(), sqlx::Error> {
    check_isolation(connection).await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 1952935218))")
        .bind(format!("shared_retention:identity:{identity}"))
        .execute(connection)
        .await?;
    Ok(())
}

async fn check_isolation(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut *connection)
        .await?;
    if isolation != "read committed" {
        return Err(sqlx::Error::Protocol(
            "shared event retention requires READ COMMITTED".to_owned(),
        ));
    }
    Ok(())
}

async fn try_lock(connection: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('shared_retention:canonical_events', 1952935218))",
    )
    .fetch_one(&mut *connection)
    .await
}

pub async fn cleanup_batch(pool: &PgPool) -> Result<Option<u64>, RetentionError> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        match cleanup_batch_once(pool).await {
            Err(RetentionError::Database(sqlx::Error::Database(error)))
                if error.is_foreign_key_violation()
                    && matches!(
                        error.constraint(),
                        Some(
                            "tenant_events_canonical_event_id_fkey"
                                | "media_group_members_member_event_id_fkey"
                        )
                    )
                    && attempts < 3 => {}
            result => return result,
        }
    }
}

async fn cleanup_batch_once(pool: &PgPool) -> Result<Option<u64>, RetentionError> {
    let started = Instant::now();
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let mut transaction = sqlx::Connection::begin(&mut *connection).await?;
    check_isolation(&mut transaction).await?;
    if !try_lock(&mut transaction).await? {
        transaction.rollback().await?;
        return Ok(None);
    }
    let cutoff = sqlx::query_scalar("SELECT statement_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    let candidates = sqlx::query_as::<_, (i64, i64, i64, Option<String>)>(
        "SELECT event.id, event.receiver_identity_id, event.tg_chat_id, event.media_group_id
           FROM canonical_events event
          WHERE event.fanned_out
            AND event.created_at < $1::timestamptz - CASE WHEN event.matched_any
                  THEN interval '30 days' ELSE interval '24 hours' END
            AND NOT EXISTS (SELECT 1 FROM tenant_events ref WHERE ref.canonical_event_id = event.id)
            AND NOT EXISTS (SELECT 1 FROM media_group_members ref WHERE ref.member_event_id = event.id)
            AND NOT EXISTS (SELECT 1 FROM media_relay_jobs job
                            WHERE job.canonical_event_id = event.id AND job.status = 'DOWNLOADING'
                              AND (job.next_retry_at IS NULL OR job.next_retry_at > $1))
          ORDER BY event.created_at, event.id LIMIT $2",
    )
    .bind(cutoff)
    .bind(BATCH_LIMIT)
    .fetch_all(&mut *transaction)
    .await?;
    let identities: std::collections::BTreeSet<_> =
        candidates.iter().map(|candidate| candidate.1).collect();
    for identity in identities {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 1952935218))")
            .bind(format!("shared_retention:identity:{identity}"))
            .execute(&mut *transaction)
            .await?;
    }
    let groups: std::collections::BTreeSet<_> = candidates
        .iter()
        .filter_map(|(_, identity, chat, group)| {
            group
                .as_ref()
                .map(|group| format!("media-group:{identity}:{chat}:{group}"))
        })
        .collect();
    for group in groups {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(group)
            .execute(&mut *transaction)
            .await?;
    }
    let ids: Vec<_> = candidates.iter().map(|candidate| candidate.0).collect();
    let deleted = delete_expired(&mut transaction, cutoff, &ids).await?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    sqlx::query(
        "INSERT INTO system_logs (tenant_id, level, module, event, context)
         VALUES (NULL, 'INFO', 'shared_retention', 'cleanup.done', $1)",
    )
    .bind(serde_json::json!({
        "table": "canonical_events",
        "rows": deleted,
        "elapsed_ms": elapsed_ms,
    }))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Some(deleted))
}

async fn delete_expired(
    connection: &mut PgConnection,
    cutoff: sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
    ids: &[i64],
) -> Result<u64, sqlx::Error> {
    let query = r#"
        DELETE FROM canonical_events AS target
         WHERE target.id = ANY($2)
           AND target.fanned_out
           AND (
                (NOT target.matched_any
                 AND target.created_at < $1::timestamptz - interval '24 hours')
                OR
                (target.matched_any
                 AND target.created_at < $1::timestamptz - interval '30 days')
           )
           AND NOT EXISTS (
                SELECT 1 FROM tenant_events AS direct_ref
                 WHERE direct_ref.canonical_event_id = target.id
           )
           AND NOT EXISTS (
                SELECT 1 FROM media_relay_jobs AS job
                 WHERE job.canonical_event_id = target.id
                   AND job.status = 'DOWNLOADING'
                   AND (job.next_retry_at IS NULL OR job.next_retry_at > $1)
           )
           AND NOT EXISTS (
                SELECT 1 FROM media_group_members member_ref
                 WHERE member_ref.member_event_id = target.id
           )
    "#;
    Ok(sqlx::query(query)
        .bind(cutoff)
        .bind(ids)
        .execute(connection)
        .await?
        .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../../migrations-test")]
    async fn exact_cutoff_is_kept_and_previous_microsecond_is_deleted(
        admin: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let identity: i64 =
            sqlx::query_scalar("INSERT INTO identities (kind) VALUES ('mtproto') RETURNING id")
                .fetch_one(&admin)
                .await?;
        let cutoff: sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc> =
            sqlx::query_scalar("SELECT statement_timestamp()-interval '1 day'")
                .fetch_one(&admin)
                .await?;
        let ids: Vec<i64> = sqlx::query_scalar(
            "INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,
                 message_date,created_at,fanned_out,matched_any)
             SELECT 'mtproto',$1,-1001,number,now(),
                    $2::timestamptz-CASE WHEN number>3 THEN interval '30 days' ELSE interval '24 hours' END
                    + ((number-1)%3-1)*interval '1 microsecond',true,number>3
               FROM generate_series(1,6) number RETURNING id",
        )
        .bind(identity)
        .bind(cutoff)
        .fetch_all(&admin)
        .await?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_with(
                admin
                    .connect_options()
                    .as_ref()
                    .clone()
                    .username("shared_cleanup")
                    .password("scpw"),
            )
            .await?;
        check_connection(&pool).await?;
        let mut transaction = pool.begin().await?;
        assert_eq!(delete_expired(&mut transaction, cutoff, &ids).await?, 2);
        transaction.commit().await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM canonical_events")
                .fetch_one(&admin)
                .await?,
            4
        );
        Ok(())
    }
}
