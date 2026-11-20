//! 媒体对象清理（docs/plan/proposals/WP-5-delivery-media.md 片 3 B 部分；spec/04 §一 `media_archive` 行）。
//!
//! 三步，每一步都幂等，多个 fanout 实例同时跑也安全：
//!
//! 1. 过期对象：先删对象，再把档案的 `object_key` 置空（顺序不能反，见 repo 层注释）。
//! 2. 档案记录：超过 180 天、已无对象且无 job 引用的物理删除。
//! 3. `staging/` 孤儿：片 2 删除临时对象失败时留下的，超过 1 天的删掉。
//!
//! 不用 S3 生命周期规则：同 hash 去重会复用未过期的旧对象，对象的有效期由
//! `media_archive.object_expires_at` 决定而不是创建时间（片 3 批准记录第 6 条）。

use std::time::{Duration, Instant};

use aws_sdk_s3::primitives::DateTime as S3DateTime;
use sqlx::PgPool;
use tgm_db::repo::list_expired_media_objects;

use crate::media_relay::{MediaStore, MediaStoreError};

/// spec/04 §1.1：每批不超过 5000 行，批间 200 ms。
const BATCH: i64 = 5000;
const BATCH_PAUSE: Duration = Duration::from_millis(200);
/// spec/04 §一：`media_archive` 记录保留 180 天。
const ARCHIVE_RETENTION_DAYS: i32 = 180;
/// `staging/` 对象超过这个年龄就视为孤儿。片 2 的下载最长也在租约回收期（10 分钟）内结束。
const STAGING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
pub const STAGING_PREFIX: &str = "staging/";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupOutcome {
    pub objects_deleted: u64,
    pub archives_deleted: u64,
    pub staging_deleted: u64,
    /// 删对象失败的个数。失败的档案保持原样，下一轮再试。
    pub object_failures: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error(transparent)]
    Role(#[from] tgm_db::media_cleanup::MediaCleanupError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Store(#[from] MediaStoreError),
}

/// 跑一轮清理。`staging_max_age` 只在测试里调小。
pub async fn run_cleanup_once(
    pool: &PgPool,
    store: &MediaStore,
    staging_max_age: Duration,
) -> Result<CleanupOutcome, CleanupError> {
    tgm_db::media_cleanup::check_connection(pool).await?;
    let mut outcome = CleanupOutcome::default();

    let mut object_log_written = false;
    loop {
        let started = Instant::now();
        let expired = list_expired_media_objects(pool, BATCH).await?;
        let mut progressed = false;
        let mut deleted = Vec::new();
        for (archive_id, key) in &expired {
            match store.delete(key).await {
                Ok(()) => deleted.push((*archive_id, key.clone())),
                Err(error) => {
                    outcome.object_failures += 1;
                    tracing::warn!(archive_id, error = %error, "删除过期媒体对象失败，下一轮再试");
                }
            }
        }
        if !deleted.is_empty() {
            let cleared = tgm_db::media_cleanup::clear_expired_objects_and_log(
                pool,
                &deleted,
                elapsed_ms(started),
            )
            .await?;
            outcome.objects_deleted += cleared;
            progressed = cleared > 0;
            object_log_written = true;
        }
        // 一批里全部失败时不再重读同一批，免得在对象存储故障时空转。
        if (expired.len() as i64) < BATCH || !progressed {
            break;
        }
        tokio::time::sleep(BATCH_PAUSE).await;
    }
    if !object_log_written {
        tgm_db::media_cleanup::log_cleanup_batch(pool, "media_archive.object", 0, 0).await?;
    }

    loop {
        let started = Instant::now();
        let deleted = tgm_db::media_cleanup::delete_stale_archives_and_log(
            pool,
            ARCHIVE_RETENTION_DAYS,
            BATCH,
            elapsed_ms(started),
        )
        .await?;
        outcome.archives_deleted += deleted;
        if (deleted as i64) < BATCH {
            break;
        }
        tokio::time::sleep(BATCH_PAUSE).await;
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(staging_max_age)
        .unwrap_or(std::time::UNIX_EPOCH);
    let cutoff = S3DateTime::from(cutoff);
    let staging_keys = store.list_older_than(STAGING_PREFIX, cutoff).await?;
    let mut staging_log_written = false;
    for chunk in staging_keys.chunks(BATCH as usize) {
        let started = Instant::now();
        let mut deleted = 0_u64;
        for key in chunk {
            match store.delete(key).await {
                Ok(()) => deleted += 1,
                Err(error) => {
                    outcome.object_failures += 1;
                    tracing::warn!(key = %key, error = %error, "删除 staging 孤儿对象失败，下一轮再试");
                }
            }
        }
        outcome.staging_deleted += deleted;
        tgm_db::media_cleanup::log_cleanup_batch(pool, "staging", deleted, elapsed_ms(started))
            .await?;
        staging_log_written = true;
    }
    if !staging_log_written {
        tgm_db::media_cleanup::log_cleanup_batch(pool, "staging", 0, 0).await?;
    }

    Ok(outcome)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// 生产默认的 staging 孤儿判定年龄。
pub fn default_staging_max_age() -> Duration {
    STAGING_MAX_AGE
}
