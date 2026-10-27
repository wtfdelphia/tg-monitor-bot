use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use tgm_core::ids::{IdentityId, TenantId};

use crate::tenant::{tenant_tx, tenant_write_tx};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TargetProbe {
    pub generation: i64,
    pub state: String,
    pub can_send: Option<bool>,
    pub content_protected: Option<bool>,
    pub bot_role: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct ProbeClaim {
    pub id: i64,
    pub tg_chat_id: i64,
    pub sender_identity_id: IdentityId,
    pub generation: i64,
    pub token: String,
    pub attempts: i32,
    pub tg_user_id: Option<i64>,
    pub kind: String,
    pub identity_status: String,
    pub credential_version: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSuccess {
    pub can_send: bool,
    pub content_protected: bool,
    pub bot_role: String,
}

pub enum ProbeOutcome {
    Success(ProbeSuccess),
    Failure {
        code: &'static str,
        retry_seconds: Option<i32>,
        rate_limited: bool,
    },
}

pub async fn read_probe(
    pool: &PgPool,
    tenant: TenantId,
    id: i64,
) -> Result<Option<TargetProbe>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| Box::pin(async move {
        sqlx::query_as("SELECT probe_generation AS generation, probe_state AS state, can_send, content_protected, bot_role, checked_at, probe_error_code AS error_code FROM target_channels WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.get()).bind(id).fetch_optional(conn).await
    })).await
}

pub async fn request_probe(
    pool: &PgPool,
    tenant: TenantId,
    id: i64,
) -> Result<Option<TargetProbe>, sqlx::Error> {
    tenant_write_tx(pool, tenant, |conn| Box::pin(async move {
        sqlx::query_as("UPDATE target_channels SET
            probe_generation = probe_generation + CASE WHEN probe_state IN ('PENDING', 'RUNNING') THEN 0 ELSE 1 END,
            probe_attempts = CASE WHEN probe_state IN ('PENDING', 'RUNNING') THEN probe_attempts ELSE 0 END,
            probe_due_at = CASE WHEN probe_state IN ('PENDING', 'RUNNING') THEN probe_due_at ELSE GREATEST(clock_timestamp(), probe_due_at) END,
            probe_state = CASE WHEN probe_state = 'RUNNING' THEN probe_state ELSE 'PENDING' END,
            can_send = NULL, content_protected = NULL, bot_role = NULL, checked_at = NULL,
            probe_error_code = CASE WHEN probe_state IN ('PENDING', 'RUNNING') THEN probe_error_code ELSE NULL END
          WHERE tenant_id = $1 AND id = $2
          RETURNING probe_generation AS generation, probe_state AS state, can_send, content_protected, bot_role, checked_at, probe_error_code AS error_code")
            .bind(tenant.get()).bind(id).fetch_optional(conn).await
    })).await
}

pub async fn claim_probe(
    pool: &PgPool,
    tenant: TenantId,
    token: String,
) -> Result<Option<ProbeClaim>, sqlx::Error> {
    tenant_write_tx(pool, tenant, |conn| Box::pin(async move {
        let sender = sqlx::query_scalar::<_, IdentityId>("SELECT i.id FROM identities i
          WHERE i.tenant_id = $1 AND i.target_probe_not_before <= clock_timestamp()
            AND EXISTS (SELECT 1 FROM target_channels t WHERE t.tenant_id = $1 AND t.sender_identity_id = i.id
              AND ((t.probe_state = 'PENDING' AND t.probe_due_at <= clock_timestamp()) OR (t.probe_state = 'RUNNING' AND t.probe_expires_at <= clock_timestamp())))
            AND NOT EXISTS (SELECT 1 FROM target_channels t WHERE t.tenant_id = $1 AND t.sender_identity_id = i.id
              AND t.probe_state = 'RUNNING' AND t.probe_expires_at > clock_timestamp())
          ORDER BY i.id FOR UPDATE OF i SKIP LOCKED LIMIT 1")
            .bind(tenant.get()).fetch_optional(&mut *conn).await?;
        let Some(sender) = sender else { return Ok(None); };
        sqlx::query("UPDATE identities SET target_probe_not_before = clock_timestamp() + interval '60 seconds' WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.get()).bind(sender.get()).execute(&mut *conn).await?;
        sqlx::query_as("WITH candidate AS (
            SELECT id FROM target_channels WHERE tenant_id = $1 AND sender_identity_id = $2
              AND ((probe_state = 'PENDING' AND probe_due_at <= clock_timestamp()) OR (probe_state = 'RUNNING' AND probe_expires_at <= clock_timestamp()))
            ORDER BY probe_due_at, id FOR UPDATE SKIP LOCKED LIMIT 1
          ), claimed AS (
            UPDATE target_channels t SET probe_state = 'RUNNING', probe_token = $3,
              probe_expires_at = clock_timestamp() + interval '60 seconds', probe_attempts = probe_attempts + 1
            FROM candidate WHERE t.tenant_id = $1 AND t.id = candidate.id RETURNING t.*
          ) SELECT t.id, t.tg_chat_id, t.sender_identity_id, t.probe_generation AS generation,
              t.probe_token AS token, t.probe_attempts AS attempts, i.tg_user_id, i.kind,
              i.status AS identity_status, s.dek_ciphertext AS credential_version
            FROM claimed t JOIN identities i ON i.id = t.sender_identity_id AND i.tenant_id = $1
            LEFT JOIN identity_secrets s ON s.identity_id = i.id")
            .bind(tenant.get()).bind(sender.get()).bind(token).fetch_optional(conn).await
    })).await
}

pub async fn finish_probe(
    pool: &PgPool,
    tenant: TenantId,
    claim: ProbeClaim,
    outcome: ProbeOutcome,
) -> Result<bool, sqlx::Error> {
    tenant_write_tx(pool, tenant, |conn| Box::pin(async move {
        sqlx::query("SELECT id FROM identities WHERE tenant_id = $1 AND id = $2 FOR UPDATE")
            .bind(tenant.get()).bind(claim.sender_identity_id.get()).execute(&mut *conn).await?;
        let (state, success, code, retry, rate_limited) = match outcome {
            ProbeOutcome::Success(value) => ("SUCCEEDED", Some(value), None, None, false),
            ProbeOutcome::Failure { code, retry_seconds, rate_limited } => {
                let retry = retry_seconds.filter(|_| rate_limited || claim.attempts < 3);
                (if retry.is_some() { "PENDING" } else { "FAILED" }, None, Some(code), retry, rate_limited)
            }
        };
        if rate_limited {
            sqlx::query("UPDATE identities SET target_probe_not_before = GREATEST(target_probe_not_before, clock_timestamp() + make_interval(secs => $3)) WHERE tenant_id = $1 AND id = $2")
                .bind(tenant.get()).bind(claim.sender_identity_id.get()).bind(f64::from(retry.unwrap_or(60))).execute(&mut *conn).await?;
        }
        let updated = sqlx::query("UPDATE target_channels t SET probe_state = $7, probe_token = NULL, probe_expires_at = NULL,
              can_send = $8, content_protected = $9, bot_role = $10,
              checked_at = CASE WHEN $7 = 'SUCCEEDED' THEN clock_timestamp() ELSE NULL END,
              probe_error_code = $11, probe_due_at = clock_timestamp() + make_interval(secs => $12)
            WHERE t.tenant_id = $1 AND t.id = $2 AND t.sender_identity_id = $3 AND t.tg_chat_id = $4
              AND t.probe_generation = $5 AND t.probe_token = $6 AND t.probe_state = 'RUNNING'
              AND t.probe_expires_at > clock_timestamp()
              AND (SELECT s.dek_ciphertext FROM identity_secrets s JOIN identities i ON i.id = s.identity_id WHERE i.tenant_id = $1 AND i.id = $3) IS NOT DISTINCT FROM $13
            ")
            .bind(tenant.get()).bind(claim.id).bind(claim.sender_identity_id.get()).bind(claim.tg_chat_id)
            .bind(claim.generation).bind(claim.token).bind(state)
            .bind(success.as_ref().map(|value| value.can_send)).bind(success.as_ref().map(|value| value.content_protected))
            .bind(success.as_ref().map(|value| value.bot_role.as_str())).bind(code).bind(f64::from(retry.unwrap_or(0)))
            .bind(claim.credential_version).execute(&mut *conn).await?;
        if updated.rows_affected() == 1 && !rate_limited {
            sqlx::query("UPDATE identities SET target_probe_not_before = clock_timestamp() WHERE tenant_id = $1 AND id = $2")
                .bind(tenant.get()).bind(claim.sender_identity_id.get()).execute(conn).await?;
        }
        Ok(updated.rows_affected() == 1)
    })).await
}
