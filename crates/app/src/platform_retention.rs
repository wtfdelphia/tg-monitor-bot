use sqlx::PgPool;
use std::time::Duration;
use tgm_db::platform_retention::{BATCH_LIMIT, RetentionError};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupResult {
    pub deleted: u64,
    pub batches: u64,
    pub overlapped: bool,
    pub stopped: bool,
}

pub async fn run(
    pool: &PgPool,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<CleanupResult, RetentionError> {
    tgm_db::platform_retention::check_connection(pool).await?;
    tokio::pin!(shutdown);
    let mut result = CleanupResult::default();
    loop {
        let batch = tokio::select! {
            biased;
            _ = &mut shutdown => {
                result.stopped = true;
                return Ok(result);
            }
            batch = tgm_db::platform_retention::cleanup_batch(pool) => batch?,
        };
        let Some(deleted) = batch else {
            result.overlapped = true;
            return Ok(result);
        };
        result.deleted += deleted;
        result.batches += 1;
        if deleted < BATCH_LIMIT as u64 {
            return Ok(result);
        }
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                result.stopped = true;
                return Ok(result);
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
}
