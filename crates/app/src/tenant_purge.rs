use std::future::Future;

use sqlx::PgPool;

const CATEGORIES: [&str; 7] = [
    "identity_dependents",
    "rule_dependents",
    "configuration",
    "notifications",
    "terminal_delivery",
    "tenant_events",
    "tenant_audit",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub tenant_id: i64,
    pub category: String,
    pub result: String,
    pub blocked: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunResult {
    pub candidates: u64,
    pub completed: u64,
    pub failed: u64,
    pub batches: u64,
    pub rows_deleted: u64,
    pub blocked: u64,
    pub stopped: bool,
    pub failures: Vec<Failure>,
}

impl RunResult {
    pub fn successful(&self) -> bool {
        !self.stopped && self.failed == 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PurgeError {
    #[error("--max-tenants 必须在 1 到 5000 之间")]
    InvalidMaxTenants,
    #[error("租户物理清理数据库调用失败")]
    Database(#[from] tgm_db::tenant_purge::PurgeError),
}

pub async fn run(
    pool: &PgPool,
    tenant_id: Option<i64>,
    max_tenants: i64,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<RunResult, PurgeError> {
    if !(1..=tgm_db::tenant_purge::BATCH_LIMIT).contains(&max_tenants) {
        return Err(PurgeError::InvalidMaxTenants);
    }

    tgm_db::tenant_purge::check_connection(pool).await?;
    tokio::pin!(shutdown);
    let tenant_ids = if let Some(tenant_id) = tenant_id {
        vec![tenant_id]
    } else {
        tokio::select! {
            biased;
            _ = &mut shutdown => return Ok(RunResult { stopped: true, ..RunResult::default() }),
            candidates = tgm_db::tenant_purge::candidates(pool, max_tenants) => candidates?,
        }
    };

    let mut result = RunResult {
        candidates: tenant_ids.len() as u64,
        ..RunResult::default()
    };
    for tenant_id in tenant_ids {
        let tenant = process_tenant(pool, tenant_id);
        tokio::pin!(tenant);
        let report = tokio::select! {
            biased;
            _ = &mut shutdown => {
                result.stopped = true;
                break;
            }
            report = &mut tenant => report?,
        };
        result.batches += report.batches;
        result.rows_deleted += report.rows_deleted;
        result.blocked += report.blocked;
        match report.failure {
            Some(failure) => {
                result.failed += 1;
                result.failures.push(failure);
            }
            None => result.completed += 1,
        }
    }
    Ok(result)
}

#[derive(Debug, Default)]
struct TenantReport {
    batches: u64,
    rows_deleted: u64,
    blocked: u64,
    failure: Option<Failure>,
}

async fn process_tenant(pool: &PgPool, tenant_id: i64) -> Result<TenantReport, PurgeError> {
    let mut report = TenantReport::default();
    for category in CATEGORIES {
        loop {
            let batch = tgm_db::tenant_purge::batch(pool, tenant_id, category).await?;
            report.batches += 1;
            report.rows_deleted += batch.rows_deleted.max(0) as u64;
            report.blocked += batch.blocked.max(0) as u64;
            match batch.result.as_str() {
                "empty" => break,
                "deleted" if batch.rows_deleted > 0 => {
                    if batch.rows_deleted < tgm_db::tenant_purge::BATCH_LIMIT {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                result => {
                    report.failure = Some(Failure {
                        tenant_id,
                        category: category.to_owned(),
                        result: result.to_owned(),
                        blocked: batch.blocked,
                    });
                    return Ok(report);
                }
            }
        }
    }

    let final_result = tgm_db::tenant_purge::finalize(pool, tenant_id).await?;
    if final_result != "deleted" {
        report.failure = Some(Failure {
            tenant_id,
            category: "tenant_finalize".to_owned(),
            result: final_result,
            blocked: 0,
        });
    }
    Ok(report)
}
