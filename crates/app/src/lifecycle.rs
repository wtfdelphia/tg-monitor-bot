use sqlx::PgPool;
use std::time::Duration;
use tgm_db::lifecycle::{BATCH_LIMIT, LifecycleError};

pub async fn run(
    pool: &PgPool,
    id: i64,
    suspend: bool,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<u64, LifecycleError> {
    tgm_db::lifecycle::check_connection(pool).await?;
    tokio::pin!(shutdown);
    let tenant = tokio::select! {
        biased;
        _=&mut shutdown=>return Err(LifecycleError::Draining),
        result=tgm_db::lifecycle::transition(pool,id,suspend)=>result?,
    };
    if !suspend {
        return Ok(0);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut total = 0;
    loop {
        let rows = tokio::select! {
            biased;
            _=&mut shutdown=>return Err(LifecycleError::Draining),
            _=tokio::time::sleep_until(deadline)=>return Err(LifecycleError::Draining),
            result=tgm_db::lifecycle::cancel_batch(pool,tenant)=>result?,
        };
        let Some(rows) = rows else {
            return Err(LifecycleError::State);
        };
        total += rows;
        let drained = tokio::select! {
            biased;
            _=&mut shutdown=>return Err(LifecycleError::Draining),
            _=tokio::time::sleep_until(deadline)=>return Err(LifecycleError::Draining),
            result=tgm_db::lifecycle::drained(pool,tenant)=>result?,
        };
        if rows < BATCH_LIMIT as u64 && drained {
            return Ok(total);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(LifecycleError::Draining);
        }
        tokio::select! {
            biased;
            _=&mut shutdown=>return Err(LifecycleError::Draining),
            _=tokio::time::sleep(Duration::from_millis(200))=>{},
        }
    }
}

pub async fn delete(
    pool: &PgPool,
    id: i64,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), LifecycleError> {
    tgm_db::lifecycle::check_connection(pool).await?;
    tokio::pin!(shutdown);
    tokio::select! {
        biased;
        _ = &mut shutdown => Err(LifecycleError::Draining),
        result = tgm_db::lifecycle::logical_delete(pool, id) => result.map(|_| ()),
    }
}
