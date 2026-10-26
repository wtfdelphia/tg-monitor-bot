use sqlx::{PgConnection, PgPool};
use std::time::Instant;
use tgm_core::ids::TenantId;

pub use crate::auth_retention::BATCH_LIMIT;
pub const TENANT_PAGE_LIMIT: i64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    TenantEvents,
    DeliveryTasks,
    LoginSessions,
    NotifyDedup,
    BotConsoleUpdates,
    DeliveryLogs,
    AuditLogs,
    SystemLogs,
    RuleVersions,
}

impl Category {
    pub fn table(self) -> &'static str {
        match self {
            Self::TenantEvents => "tenant_events",
            Self::DeliveryTasks => "delivery_tasks",
            Self::LoginSessions => "login_sessions",
            Self::NotifyDedup => "notify_dedup",
            Self::BotConsoleUpdates => "bot_console_updates",
            Self::DeliveryLogs => "delivery_logs",
            Self::AuditLogs => "audit_logs",
            Self::SystemLogs => "system_logs",
            Self::RuleVersions => "rule_versions",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error("租户清理数据库角色或版本检查失败")]
    Preflight(#[from] crate::preflight::PreflightError),
    #[error("租户清理必须直接使用无额外属性或成员关系的 tenant_cleanup 登录")]
    WrongRole,
    #[error("租户套餐值无效，拒绝按默认期限清理")]
    InvalidPlan,
    #[error("租户清理批次失败，未提交的删除和日志已回滚")]
    Database(#[from] sqlx::Error),
}

pub async fn check_connection(pool: &PgPool) -> Result<(), RetentionError> {
    crate::preflight::check_version(pool).await?;
    let role = crate::preflight::check_runtime_role(pool).await?;
    let safe: bool = sqlx::query_scalar("SELECT session_user = 'tenant_cleanup' AND rolcanlogin AND NOT (rolsuper OR rolbypassrls OR rolcreatedb OR rolcreaterole OR rolreplication) AND NOT EXISTS (SELECT 1 FROM pg_auth_members WHERE member = pg_roles.oid OR roleid = pg_roles.oid) FROM pg_roles WHERE rolname = current_user")
        .fetch_one(pool).await?;
    if role != "tenant_cleanup" || !safe {
        return Err(RetentionError::WrongRole);
    }
    Ok(())
}

pub async fn list_tenants(pool: &PgPool, after: i64) -> Result<Vec<TenantId>, RetentionError> {
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    Ok(
        sqlx::query_scalar("SELECT id FROM tenants WHERE id > $1 ORDER BY id LIMIT $2")
            .bind(after)
            .bind(TENANT_PAGE_LIMIT)
            .fetch_all(&mut *connection)
            .await?,
    )
}

pub async fn try_lock(
    connection: &mut PgConnection,
    tenant: TenantId,
    category: Category,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 1952935218))")
        .bind(lock_key(tenant, category))
        .fetch_one(connection)
        .await
}

fn lock_key(tenant: TenantId, category: Category) -> String {
    if matches!(category, Category::TenantEvents | Category::DeliveryTasks) {
        return format!("tenant_retention:{}:event_lifecycle", tenant.get());
    }
    format!("tenant_retention:{}:{}", tenant.get(), category.table())
}

pub async fn lock_event_lifecycle_shared(
    connection: &mut PgConnection,
    tenant: TenantId,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 1952935218))")
        .bind(lock_key(tenant, Category::TenantEvents))
        .execute(connection)
        .await?;
    Ok(())
}

pub async fn lock_rule_versions(
    connection: &mut PgConnection,
    tenant: TenantId,
) -> Result<(), sqlx::Error> {
    check_isolation(connection).await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 1952935218))")
        .bind(lock_key(tenant, Category::RuleVersions))
        .execute(connection)
        .await?;
    Ok(())
}

async fn check_isolation(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(connection)
        .await?;
    if isolation != "read committed" {
        return Err(sqlx::Error::Protocol(
            "rule version retention requires READ COMMITTED".to_owned(),
        ));
    }
    Ok(())
}

pub async fn cleanup_batch(
    pool: &PgPool,
    tenant: TenantId,
    category: Category,
) -> Result<Option<u64>, RetentionError> {
    let started = Instant::now();
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    let mut transaction = sqlx::Connection::begin(&mut *connection).await?;
    if matches!(
        category,
        Category::RuleVersions | Category::TenantEvents | Category::DeliveryTasks
    ) {
        check_isolation(&mut transaction).await?;
    }
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.get().to_string())
        .execute(&mut *transaction)
        .await?;
    if !try_lock(&mut transaction, tenant, category).await? {
        transaction.rollback().await?;
        return Ok(None);
    }
    if category == Category::TenantEvents {
        let plan: String = sqlx::query_scalar("SELECT plan FROM tenants WHERE id = $1")
            .bind(tenant.get())
            .fetch_one(&mut *transaction)
            .await?;
        if plan != "free" && plan != "pro" {
            return Err(RetentionError::InvalidPlan);
        }
    }
    let cutoff = sqlx::query_scalar("SELECT statement_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    let deleted = delete_expired(&mut transaction, tenant, category, cutoff).await?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    sqlx::query("INSERT INTO system_logs (tenant_id, level, module, event, context) VALUES ($1, 'INFO', 'tenant_retention', 'cleanup.done', $2)")
        .bind(tenant.get())
        .bind(serde_json::json!({"table": category.table(), "rows": deleted, "elapsed_ms": elapsed_ms}))
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(Some(deleted))
}

async fn delete_expired(
    connection: &mut PgConnection,
    tenant: TenantId,
    category: Category,
    cutoff: sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    let query = match category {
        Category::LoginSessions => {
            "WITH candidates AS MATERIALIZED (SELECT tenant_id, id FROM login_sessions WHERE tenant_id = $1 AND expires_at <= $3 ORDER BY expires_at, id LIMIT $2) DELETE FROM login_sessions target USING candidates WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id AND target.expires_at <= $3"
        }
        Category::NotifyDedup => {
            "WITH candidates AS MATERIALIZED (SELECT tenant_id, fingerprint, target_ref FROM notify_dedup WHERE tenant_id = $1 AND expires_at <= $3 ORDER BY expires_at, fingerprint, target_ref LIMIT $2) DELETE FROM notify_dedup target USING candidates WHERE target.tenant_id = candidates.tenant_id AND target.fingerprint = candidates.fingerprint AND target.target_ref = candidates.target_ref AND target.expires_at <= $3"
        }
        Category::BotConsoleUpdates => {
            "WITH candidates AS MATERIALIZED (SELECT tenant_id, bot_id, update_id FROM bot_console_updates WHERE tenant_id = $1 AND processed_at <= $3::timestamptz - interval '7 days' ORDER BY processed_at, bot_id, update_id LIMIT $2) DELETE FROM bot_console_updates target USING candidates WHERE target.tenant_id = candidates.tenant_id AND target.bot_id = candidates.bot_id AND target.update_id = candidates.update_id AND target.processed_at <= $3::timestamptz - interval '7 days'"
        }
        Category::DeliveryLogs => {
            "WITH candidates AS MATERIALIZED (SELECT logs.id FROM delivery_logs logs JOIN delivery_tasks parent ON parent.id = logs.task_id WHERE parent.tenant_id = $1 AND logs.created_at <= $3::timestamptz - interval '90 days' ORDER BY logs.created_at, logs.id LIMIT $2) DELETE FROM delivery_logs target USING candidates WHERE target.id = candidates.id AND target.created_at <= $3::timestamptz - interval '90 days' AND EXISTS (SELECT 1 FROM delivery_tasks parent WHERE parent.id = target.task_id AND parent.tenant_id = $1)"
        }
        Category::AuditLogs => {
            "WITH candidates AS MATERIALIZED (SELECT logs.tenant_id, logs.id FROM audit_logs logs JOIN tenants parent ON parent.id = logs.tenant_id WHERE logs.tenant_id = $1 AND logs.created_at <= $3::timestamptz - CASE parent.plan WHEN 'free' THEN interval '90 days' WHEN 'pro' THEN interval '365 days' END ORDER BY logs.created_at, logs.id LIMIT $2) DELETE FROM audit_logs target USING candidates WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id AND EXISTS (SELECT 1 FROM tenants parent WHERE parent.id = target.tenant_id AND target.created_at <= $3::timestamptz - CASE parent.plan WHEN 'free' THEN interval '90 days' WHEN 'pro' THEN interval '365 days' END)"
        }
        Category::SystemLogs => {
            "WITH candidates AS MATERIALIZED (SELECT tenant_id, id FROM system_logs WHERE tenant_id = $1 AND created_at <= $3::timestamptz - interval '90 days' ORDER BY created_at, id LIMIT $2) DELETE FROM system_logs target USING candidates WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id AND target.created_at <= $3::timestamptz - interval '90 days'"
        }
        Category::RuleVersions => {
            "WITH candidates AS MATERIALIZED (SELECT versions.tenant_id, versions.rule_id, versions.version FROM rule_versions versions JOIN tenants parent ON parent.id = versions.tenant_id WHERE versions.tenant_id = $1 AND versions.created_at <= $3::timestamptz - CASE parent.plan WHEN 'free' THEN interval '90 days' WHEN 'pro' THEN interval '365 days' END AND NOT EXISTS (SELECT 1 FROM rules current_rule WHERE current_rule.tenant_id = versions.tenant_id AND current_rule.id = versions.rule_id AND current_rule.version = versions.version) AND NOT EXISTS (SELECT 1 FROM delivery_tasks task WHERE task.tenant_id = versions.tenant_id AND task.rule_id = versions.rule_id AND task.rule_version = versions.version) ORDER BY versions.created_at, versions.rule_id, versions.version LIMIT $2) DELETE FROM rule_versions target USING candidates WHERE target.tenant_id = candidates.tenant_id AND target.rule_id = candidates.rule_id AND target.version = candidates.version"
        }
        Category::TenantEvents => {
            "WITH candidates AS MATERIALIZED (SELECT events.tenant_id, events.id FROM tenant_events events JOIN tenants parent ON parent.id = events.tenant_id WHERE events.tenant_id = $1 AND events.created_at < $3::timestamptz - CASE parent.plan WHEN 'free' THEN interval '7 days' WHEN 'pro' THEN interval '30 days' END AND NOT EXISTS (SELECT 1 FROM delivery_tasks task WHERE task.tenant_id = events.tenant_id AND task.tenant_event_id = events.id AND task.status IN ('PENDING', 'SENDING', 'FAILED')) ORDER BY events.created_at, events.id LIMIT $2) DELETE FROM tenant_events target USING candidates, tenants parent WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id AND parent.id = target.tenant_id AND target.created_at < $3::timestamptz - CASE parent.plan WHEN 'free' THEN interval '7 days' WHEN 'pro' THEN interval '30 days' END AND NOT EXISTS (SELECT 1 FROM delivery_tasks task WHERE task.tenant_id = target.tenant_id AND task.tenant_event_id = target.id AND task.status IN ('PENDING', 'SENDING', 'FAILED'))"
        }
        Category::DeliveryTasks => {
            "WITH candidates AS MATERIALIZED (SELECT task.id FROM delivery_tasks task WHERE task.tenant_id = $1 AND task.tenant_event_id IS NULL AND task.status IN ('DONE', 'DEAD', 'CANCELLED') AND NOT EXISTS (SELECT 1 FROM delivery_logs log WHERE log.task_id = task.id) AND ((task.status = 'DEAD' AND task.updated_at <= $3::timestamptz - interval '30 days') OR (task.updated_at <= $3::timestamptz - interval '90 days')) ORDER BY task.updated_at, task.id LIMIT $2) DELETE FROM delivery_tasks target USING candidates WHERE target.tenant_id = $1 AND target.id = candidates.id AND target.tenant_event_id IS NULL AND target.status IN ('DONE', 'DEAD', 'CANCELLED') AND NOT EXISTS (SELECT 1 FROM delivery_logs log WHERE log.task_id = target.id) AND ((target.status = 'DEAD' AND target.updated_at <= $3::timestamptz - interval '30 days') OR (target.updated_at <= $3::timestamptz - interval '90 days'))"
        }
    };
    Ok(sqlx::query(query)
        .bind(tenant.get())
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
    async fn every_category_includes_cutoff_but_preserves_next_microsecond(
        admin: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        sqlx::raw_sql("INSERT INTO tenants (name,owner_user_id,plan) VALUES ('boundary-free',92001,'free'),('boundary-pro',92002,'pro');
            INSERT INTO identities (tenant_id,kind) SELECT id,'bot' FROM tenants;
            INSERT INTO delivery_tasks (tenant_id,sender_identity_id,target_tg_chat_id,action) SELECT tenant_id,id,-1001,'COPY' FROM identities;
            INSERT INTO keywords (tenant_id,word,match_type) SELECT id,'boundary','exact' FROM tenants;
            INSERT INTO target_channels (tenant_id,tg_chat_id,sender_identity_id) SELECT tenant_id,-1002,id FROM identities;
            INSERT INTO rules (tenant_id,keyword_id,target_ref,action,version) SELECT words.tenant_id,words.id,targets.id,'COPY',3 FROM keywords words JOIN target_channels targets ON targets.tenant_id=words.tenant_id;")
            .execute(&admin).await?;
        let tenants: Vec<TenantId> = sqlx::query_scalar("SELECT id FROM tenants ORDER BY id")
            .fetch_all(&admin)
            .await?;
        let cutoff: sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc> =
            sqlx::query_scalar("SELECT statement_timestamp()-interval '1 day'")
                .fetch_one(&admin)
                .await?;
        let cleanup = sqlx::postgres::PgPoolOptions::new()
            .connect_with(
                admin
                    .connect_options()
                    .as_ref()
                    .clone()
                    .username("tenant_cleanup")
                    .password("tcpw"),
            )
            .await?;
        for tenant in tenants {
            for number in [0_i64, 1] {
                let instant = cutoff + std::time::Duration::from_micros(u64::try_from(number)?);
                for query in [
                    "INSERT INTO login_sessions (tenant_id,kind,state,expires_at) VALUES ($1,'qr','expired',$2)",
                    "INSERT INTO notify_dedup (tenant_id,fingerprint,target_ref,last_sent_at,expires_at) VALUES ($1,$3::bigint::text,1,now(),$2)",
                    "INSERT INTO bot_console_updates (tenant_id,bot_id,update_id,processed_at) SELECT tenant_id,id,$3,$2::timestamptz-interval '7 days' FROM identities WHERE tenant_id=$1",
                    "INSERT INTO delivery_logs (task_id,attempt,ok,created_at) SELECT id,1,true,$2::timestamptz-interval '90 days' FROM delivery_tasks WHERE tenant_id=$1",
                    "INSERT INTO audit_logs (tenant_id,action,resource,created_at) SELECT id,'boundary','sentinel',$2::timestamptz-CASE plan WHEN 'free' THEN interval '90 days' WHEN 'pro' THEN interval '365 days' END FROM tenants WHERE id=$1",
                    "INSERT INTO system_logs (tenant_id,level,module,event,created_at) VALUES ($1,'INFO','boundary','sentinel',$2::timestamptz-interval '90 days')",
                    "INSERT INTO rule_versions (tenant_id,rule_id,version,snapshot,created_at) SELECT rules.tenant_id,rules.id,$3::bigint::int+1,'{}',$2::timestamptz-CASE tenants.plan WHEN 'free' THEN interval '90 days' WHEN 'pro' THEN interval '365 days' END FROM rules JOIN tenants ON tenants.id=rules.tenant_id WHERE rules.tenant_id=$1",
                ] {
                    let extra_parameter = query.contains("$3");
                    let query = sqlx::query(query).bind(tenant.get()).bind(instant);
                    if extra_parameter {
                        query.bind(number).execute(&admin).await?;
                    } else {
                        query.execute(&admin).await?;
                    }
                }
            }
            let mut transaction = cleanup.begin().await?;
            sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
                .bind(tenant.get().to_string())
                .execute(&mut *transaction)
                .await?;
            for category in [
                Category::LoginSessions,
                Category::NotifyDedup,
                Category::BotConsoleUpdates,
                Category::DeliveryLogs,
                Category::AuditLogs,
                Category::SystemLogs,
                Category::RuleVersions,
            ] {
                assert_eq!(
                    delete_expired(&mut transaction, tenant, category, cutoff).await?,
                    1,
                    "{category:?}"
                );
            }
            transaction.commit().await?;
        }
        Ok(())
    }
}
