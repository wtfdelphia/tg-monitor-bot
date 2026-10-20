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
use tgm_db::repo::{
    clear_media_archive_object, delete_stale_media_archive, list_expired_media_objects,
};

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
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Store(#[from] MediaStoreError),
}

/// 跑一轮清理。`staging_max_age` 只在测试里调小。
// ponytail: 清理结果只进 tracing、不进 system_logs（spec/04 §1.1 第 2 条）—— app_user 写不进
// tenant_id IS NULL 的行（0010 末尾的注释），平台级日志写入策略定下来后改为写库
pub async fn run_cleanup_once(
    pool: &PgPool,
    store: &MediaStore,
    staging_max_age: Duration,
) -> Result<CleanupOutcome, CleanupError> {
    let mut outcome = CleanupOutcome::default();

    let started = Instant::now();
    loop {
        let expired = list_expired_media_objects(pool, BATCH).await?;
        let mut progressed = false;
        for (archive_id, key) in &expired {
            match store.delete(key).await {
                Ok(()) => {
                    if clear_media_archive_object(pool, *archive_id, key).await? {
                        outcome.objects_deleted += 1;
                        progressed = true;
                    }
                }
                Err(error) => {
                    outcome.object_failures += 1;
                    tracing::warn!(archive_id, error = %error, "删除过期媒体对象失败，下一轮再试");
                }
            }
        }
        // 一批里全部失败时不再重读同一批，免得在对象存储故障时空转。
        if (expired.len() as i64) < BATCH || !progressed {
            break;
        }
        tokio::time::sleep(BATCH_PAUSE).await;
    }
    log_done("media_archive.object", outcome.objects_deleted, started);

    let started = Instant::now();
    loop {
        let deleted = delete_stale_media_archive(pool, ARCHIVE_RETENTION_DAYS, BATCH).await?;
        outcome.archives_deleted += deleted;
        if (deleted as i64) < BATCH {
            break;
        }
        tokio::time::sleep(BATCH_PAUSE).await;
    }
    log_done("media_archive", outcome.archives_deleted, started);

    let started = Instant::now();
    let cutoff = std::time::SystemTime::now()
        .checked_sub(staging_max_age)
        .unwrap_or(std::time::UNIX_EPOCH);
    let cutoff = S3DateTime::from(cutoff);
    for key in store.list_older_than(STAGING_PREFIX, cutoff).await? {
        match store.delete(&key).await {
            Ok(()) => outcome.staging_deleted += 1,
            Err(error) => {
                outcome.object_failures += 1;
                tracing::warn!(key = %key, error = %error, "删除 staging 孤儿对象失败，下一轮再试");
            }
        }
    }
    log_done("staging", outcome.staging_deleted, started);

    Ok(outcome)
}

fn log_done(table: &str, rows: u64, started: Instant) {
    tracing::info!(
        event = "cleanup.done",
        table,
        rows,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "清理完成"
    );
}

/// 生产默认的 staging 孤儿判定年龄。
pub fn default_staging_max_age() -> Duration {
    STAGING_MAX_AGE
}
