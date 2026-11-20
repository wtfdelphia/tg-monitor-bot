use sqlx::PgPool;
use std::time::Instant;

pub const BATCH_LIMIT: i64 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Challenges,
    WebSessions,
    BotSessions,
}

impl Category {
    pub fn table(self) -> &'static str {
        match self {
            Self::Challenges => "console_login_challenges",
            Self::WebSessions => "web_sessions",
            Self::BotSessions => "bot_console_sessions",
        }
    }

    pub fn lock_key(self) -> i64 {
        match self {
            Self::Challenges => 0x74676d_617501,
            Self::WebSessions => 0x74676d_617502,
            Self::BotSessions => 0x74676d_617503,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error("清理数据库角色或版本检查失败")]
    Preflight(#[from] crate::preflight::PreflightError),
    #[error("清理命令必须直接使用 auth_cleanup 登录")]
    WrongRole,
    #[error("清理角色带有不允许的属性或成员关系")]
    RolePrivileges,
    #[error("清理批次失败，未提交的删除和日志已回滚")]
    Database(#[from] sqlx::Error),
}

pub async fn check_connection(pool: &PgPool) -> Result<(), RetentionError> {
    crate::preflight::check_version(pool).await?;
    let role = crate::preflight::check_runtime_role(pool).await?;
    let direct: bool = sqlx::query_scalar("SELECT session_user = 'auth_cleanup'")
        .fetch_one(pool)
        .await?;
    if role != "auth_cleanup" || !direct {
        return Err(RetentionError::WrongRole);
    }
    let safe: bool = sqlx::query_scalar("SELECT rolcanlogin AND NOT (rolsuper OR rolbypassrls OR rolcreatedb OR rolcreaterole OR rolreplication) AND NOT EXISTS (SELECT 1 FROM pg_auth_members WHERE member = pg_roles.oid OR roleid = pg_roles.oid) FROM pg_roles WHERE rolname = current_user")
        .fetch_one(pool)
        .await?;
    if !safe {
        return Err(RetentionError::RolePrivileges);
    }
    Ok(())
}

pub async fn cleanup_batch(
    pool: &PgPool,
    category: Category,
) -> Result<Option<u64>, RetentionError> {
    let started = Instant::now();
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let mut transaction = sqlx::Connection::begin(&mut *connection).await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(category.lock_key())
        .fetch_one(&mut *transaction)
        .await?;
    if !acquired {
        transaction.rollback().await?;
        return Ok(None);
    }
    let cutoff = sqlx::query_scalar("SELECT statement_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    let deleted = delete_expired(&mut transaction, category, cutoff).await?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    sqlx::query("INSERT INTO system_logs (tenant_id, level, module, event, context) VALUES (NULL, 'INFO', 'auth_retention', 'cleanup.done', $1)")
        .bind(serde_json::json!({"table": category.table(), "rows": deleted, "elapsed_ms": elapsed_ms}))
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(Some(deleted))
}

async fn delete_expired(
    connection: &mut sqlx::PgConnection,
    category: Category,
    cutoff: sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    let query = match category {
        Category::Challenges => {
            "WITH candidates AS MATERIALIZED (SELECT nonce FROM console_login_challenges WHERE expires_at <= $2 ORDER BY expires_at, nonce LIMIT $1) DELETE FROM console_login_challenges target USING candidates WHERE target.nonce = candidates.nonce AND target.expires_at <= $2"
        }
        Category::WebSessions => {
            "WITH candidates AS MATERIALIZED (SELECT tenant_id, id FROM web_sessions WHERE expires_at <= $2 ORDER BY expires_at, tenant_id, id LIMIT $1) DELETE FROM web_sessions target USING candidates WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id AND target.expires_at <= $2"
        }
        Category::BotSessions => {
            "WITH candidates AS MATERIALIZED (SELECT tg_user_id FROM bot_console_sessions WHERE updated_at <= $2::timestamptz - interval '90 days' ORDER BY updated_at, tg_user_id LIMIT $1) DELETE FROM bot_console_sessions target USING candidates WHERE target.tg_user_id = candidates.tg_user_id AND target.updated_at <= $2::timestamptz - interval '90 days'"
        }
    };
    Ok(sqlx::query(query)
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
    async fn deletion_predicates_include_exact_cutoff_but_not_the_next_microsecond(
        admin: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cutoff: sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc> =
            sqlx::query_scalar("SELECT statement_timestamp() - interval '1 day'")
                .fetch_one(&admin)
                .await?;
        sqlx::raw_sql("INSERT INTO tenants (name, owner_user_id) VALUES ('boundary', 89001); INSERT INTO tenant_members (tenant_id, user_id) SELECT id, 89001 FROM tenants; INSERT INTO tenant_members (tenant_id, user_id) SELECT id, 89002 FROM tenants;")
            .execute(&admin).await?;
        for number in [1_i64, 2] {
            let expires = cutoff + std::time::Duration::from_micros(u64::try_from(number - 1)?);
            sqlx::query("INSERT INTO console_login_challenges (nonce, confirm_code, origin_ip, expires_at) VALUES (int8send($1), 'ABCD', '127.0.0.1', $2)").bind(number).bind(expires).execute(&admin).await?;
            sqlx::query("INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at) SELECT id, int8send($1), 89000 + $1, int8send($1), $2 FROM tenants").bind(number).bind(expires).execute(&admin).await?;
            sqlx::query("INSERT INTO bot_console_sessions (tenant_id, tg_user_id, updated_at) SELECT id, 89000 + $1, $2::timestamptz - interval '90 days' FROM tenants").bind(number).bind(expires).execute(&admin).await?;
        }
        let cleanup = sqlx::postgres::PgPoolOptions::new()
            .connect_with(
                admin
                    .connect_options()
                    .as_ref()
                    .clone()
                    .username("auth_cleanup")
                    .password("cpw"),
            )
            .await?;
        check_connection(&cleanup).await?;
        let mut transaction = cleanup.begin().await?;
        for category in [
            Category::Challenges,
            Category::WebSessions,
            Category::BotSessions,
        ] {
            assert_eq!(delete_expired(&mut transaction, category, cutoff).await?, 1);
        }
        transaction.commit().await?;
        Ok(())
    }
}
