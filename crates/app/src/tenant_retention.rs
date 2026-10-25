use sqlx::PgPool;
use std::time::Duration;
use tgm_db::tenant_retention::{BATCH_LIMIT, Category, RetentionError};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupResult {
    pub deleted: u64,
    pub batches: u64,
    pub tenants: u64,
    pub overlaps: u64,
    pub stopped: bool,
}

pub async fn run(
    pool: &PgPool,
    category: Category,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<CleanupResult, RetentionError> {
    tgm_db::tenant_retention::check_connection(pool).await?;
    tokio::pin!(shutdown);
    let mut result = CleanupResult::default();
    let mut after = 0;
    loop {
        let tenants = tokio::select! {
            biased;
            _ = &mut shutdown => { result.stopped = true; return Ok(result); }
            tenants = tgm_db::tenant_retention::list_tenants(pool, after) => tenants?,
        };
        if tenants.is_empty() {
            return Ok(result);
        }
        for tenant in tenants {
            after = tenant.get();
            result.tenants += 1;
            loop {
                let batch = tokio::select! {
                    biased;
                    _ = &mut shutdown => { result.stopped = true; return Ok(result); }
                    batch = tgm_db::tenant_retention::cleanup_batch(pool, tenant, category) => batch?,
                };
                let Some(deleted) = batch else {
                    result.overlaps += 1;
                    break;
                };
                result.deleted += deleted;
                result.batches += 1;
                if deleted < BATCH_LIMIT as u64 {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut shutdown => { result.stopped = true; return Ok(result); }
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
            }
        }
    }
}
