//! 跨身份媒体中转（docs/plan/proposals/WP-5-delivery-media.md 片 2，spec/02 §4.2）。
//!
//! 采集身份（mtproto-worker）下载 → 边下边算 sha256 → S3 multipart → `media_archive`；
//! 发送身份（delivery）从 S3 流式读 → 上传 → `sendMedia`。
//!
//! 两条纪律：
//!
//! - 流式，不落磁盘：内存里最多一片（8 MB，S3 multipart 最小分片 5 MB 之上）。
//! - S3 用独立静态凭据（`TGM_S3_ACCESS_KEY_ID` / `TGM_S3_SECRET_ACCESS_KEY`），
//!   不走 AWS 默认凭据链 —— 默认链承载 KMS 身份（片 2 批准记录第 3 条）。

use std::time::Duration;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tgm_db::repo::{
    LeaseGuard, MediaArchiveInput, MediaRelayJobRecord, claim_media_relay_job,
    complete_media_relay_job, fail_media_relay_job,
};
use tgm_tg::{MediaReader, TelegramClientError};

/// pro 档媒体中转单文件上限（spec/01 §6.1）。
pub const MEDIA_RELAY_MAX_BYTES: i64 = 50 * 1024 * 1024;
/// multipart 分片大小。S3 要求除最后一片外不小于 5 MiB。
const PART_SIZE: usize = 8 * 1024 * 1024;
/// 下载失败后的退避（片 2 提案：30s / 5min / 30min，第 4 次失败判死）。
const RELAY_RETRY_SECONDS: [i64; 3] = [30, 5 * 60, 30 * 60];

/// 对象存储配置，全部来自 eng/01 §二 的 `TGM_S3_*`。
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: tgm_core::secret::Secret<String>,
}

impl S3Config {
    /// 从环境变量读取。必填项缺失时由启动期校验先拦住，这里只兜底报错。
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).map_err(|_| format!("缺少 {name}"));
        Ok(Self {
            endpoint: var("TGM_S3_ENDPOINT")?,
            bucket: var("TGM_S3_BUCKET")?,
            region: std::env::var("TGM_S3_REGION").unwrap_or_else(|_| "us-east-1".to_owned()),
            access_key_id: var("TGM_S3_ACCESS_KEY_ID")?,
            secret_access_key: tgm_core::secret::Secret::new(var("TGM_S3_SECRET_ACCESS_KEY")?),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MediaStoreError {
    #[error("对象存储请求失败: {0}")]
    S3(String),
    #[error("媒体源读取失败: {0}")]
    Source(#[source] TelegramClientError),
    #[error("媒体超过中转上限 {MEDIA_RELAY_MAX_BYTES} 字节")]
    TooLarge,
}

/// SDK 错误的 `Display` 只有一句 "service error"，要带上下文链才看得出是哪一层失败。
fn s3_error<E: std::error::Error>(error: E) -> MediaStoreError {
    MediaStoreError::S3(aws_sdk_s3::error::DisplayErrorContext(error).to_string())
}

/// 显式静态凭据、path-style 的 S3 客户端（MinIO 与 AWS S3 都适用）。
#[derive(Clone)]
pub struct MediaStore {
    client: aws_sdk_s3::Client,
    bucket: String,
}

/// 一个已写完的对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    pub key: String,
    pub content_hash: String,
    pub size: i64,
}

impl MediaStore {
    pub fn new(config: &S3Config) -> Self {
        let credentials = Credentials::new(
            config.access_key_id.clone(),
            config.secret_access_key.expose().clone(),
            None,
            None,
            "tgm-s3-static",
        );
        let s3 = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region.clone()))
            .endpoint_url(config.endpoint.clone())
            .credentials_provider(credentials)
            .force_path_style(true)
            .build();
        Self {
            client: aws_sdk_s3::Client::from_conf(s3),
            bucket: config.bucket.clone(),
        }
    }

    /// 本地与测试用：桶不存在就建。生产桶由部署方建好，进程不持有建桶权限。
    pub async fn ensure_bucket(&self) -> Result<(), MediaStoreError> {
        if self
            .client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }
        // 多个进程（或并行测试）同时发现桶不存在会一起建，后到的收到 409 BucketAlreadyOwnedByYou：
        // 桶已经在了，就是成功。
        match self
            .client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|service| service.is_bucket_already_owned_by_you()) =>
            {
                Ok(())
            }
            Err(error) => Err(s3_error(error)),
        }
    }

    /// 把分片流写成一个对象，途中算 sha256。超过上限时中止 multipart、报 `TooLarge`。
    pub async fn put_stream(
        &self,
        key: &str,
        source: &mut dyn MediaSource,
    ) -> Result<StoredObject, MediaStoreError> {
        let upload = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_error)?;
        let upload_id = upload
            .upload_id()
            .ok_or_else(|| MediaStoreError::S3("create_multipart_upload 未返回 upload_id".into()))?
            .to_owned();
        match self.put_parts(key, &upload_id, source).await {
            Ok(stored) => Ok(stored),
            Err(error) => {
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(error)
            }
        }
    }

    async fn put_parts(
        &self,
        key: &str,
        upload_id: &str,
        source: &mut dyn MediaSource,
    ) -> Result<StoredObject, MediaStoreError> {
        let mut hasher = Sha256::new();
        let mut parts = Vec::new();
        let mut buffer = Vec::with_capacity(PART_SIZE);
        let mut size: i64 = 0;
        loop {
            let chunk = source.next_chunk().await.map_err(MediaStoreError::Source)?;
            let done = chunk.is_none();
            if let Some(chunk) = chunk {
                size += chunk.len() as i64;
                if size > MEDIA_RELAY_MAX_BYTES {
                    return Err(MediaStoreError::TooLarge);
                }
                hasher.update(&chunk);
                buffer.extend_from_slice(&chunk);
            }
            // 空对象也要传一片：S3 不接受零分片的 complete。
            if buffer.len() >= PART_SIZE || (done && (!buffer.is_empty() || parts.is_empty())) {
                let number = i32::try_from(parts.len() + 1).unwrap_or(i32::MAX);
                let body = std::mem::replace(&mut buffer, Vec::with_capacity(PART_SIZE));
                let part = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(number)
                    .body(ByteStream::from(body))
                    .send()
                    .await
                    .map_err(s3_error)?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(number)
                        .set_e_tag(part.e_tag().map(str::to_owned))
                        .build(),
                );
            }
            if done {
                break;
            }
        }
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(s3_error)?;
        Ok(StoredObject {
            key: key.to_owned(),
            content_hash: hex(&hasher.finalize()),
            size,
        })
    }

    /// 流式读一个对象，返回可直接交给 `upload_stream` 的 `AsyncRead` 与对象长度。
    pub async fn get_stream(
        &self,
        key: &str,
    ) -> Result<(impl tokio::io::AsyncRead + Unpin + use<>, usize), MediaStoreError> {
        let object = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_error)?;
        let size = object
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .ok_or_else(|| MediaStoreError::S3("get_object 未返回 content_length".into()))?;
        Ok((object.body.into_async_read(), size))
    }

    /// 删对象。S3 对不存在的 key 也返回成功（MinIO 同），所以重复删是幂等的。
    pub async fn delete(&self, key: &str) -> Result<(), MediaStoreError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map(|_| ())
            .map_err(s3_error)
    }

    /// 列出 `prefix` 下最后修改时间早于 `cutoff` 的对象 key（分页读完）。
    pub async fn list_older_than(
        &self,
        prefix: &str,
        cutoff: aws_sdk_s3::primitives::DateTime,
    ) -> Result<Vec<String>, MediaStoreError> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(token.take())
                .send()
                .await
                .map_err(s3_error)?;
            keys.extend(page.contents().iter().filter_map(|object| {
                let old = object.last_modified().is_some_and(|at| *at < cutoff);
                old.then(|| object.key().map(str::to_owned)).flatten()
            }));
            match page.next_continuation_token() {
                Some(next) => token = Some(next.to_owned()),
                None => return Ok(keys),
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 下载的分片来源。生产是 [`tgm_tg::MediaDownload`]，L2 测试注入假流。
#[async_trait::async_trait]
pub trait MediaSource: Send {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TelegramClientError>;
}

#[async_trait::async_trait]
impl MediaSource for tgm_tg::MediaDownload {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TelegramClientError> {
        tgm_tg::MediaDownload::next_chunk(self).await
    }
}

/// 打开一条源消息的媒体下载。生产实现是采集身份的 [`MediaReader`]。
#[async_trait::async_trait]
pub trait MediaOpener: Send + Sync {
    async fn open(
        &self,
        tg_chat_id: i64,
        tg_message_id: i64,
    ) -> Result<Box<dyn MediaSource>, TelegramClientError>;
}

#[async_trait::async_trait]
impl MediaOpener for MediaReader {
    async fn open(
        &self,
        tg_chat_id: i64,
        tg_message_id: i64,
    ) -> Result<Box<dyn MediaSource>, TelegramClientError> {
        Ok(Box::new(
            MediaReader::open(self, tg_chat_id, tg_message_id).await?,
        ))
    }
}

/// 一轮处理的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayOutcome {
    Idle,
    Ready {
        canonical_event_id: i64,
        reused: bool,
    },
    Failed {
        canonical_event_id: i64,
        dead: bool,
        error_code: String,
    },
    LeaseLost,
}

/// 领一条本身份到期的 job，下载并归档。持租约的 mtproto-worker 调。
pub async fn run_relay_once(
    pool: &PgPool,
    lease: &LeaseGuard,
    opener: &dyn MediaOpener,
    store: &MediaStore,
) -> Result<RelayOutcome, sqlx::Error> {
    let _admission = match lease.tenant_id {
        Some(tenant) => match tgm_db::lifecycle::WorkerGuard::acquire(pool, tenant).await? {
            Some(guard) => Some(guard),
            None => return Ok(RelayOutcome::Idle),
        },
        None => None,
    };
    let Some(job) = claim_media_relay_job(pool, lease).await? else {
        return Ok(RelayOutcome::Idle);
    };
    if job
        .file_size
        .is_some_and(|size| size > MEDIA_RELAY_MAX_BYTES)
    {
        return fail(pool, lease, &job, true, "MEDIA_TOO_LARGE").await;
    }
    // ponytail: 下载完才知道 content_hash，spec/02 §4.2 第 3 条「命中档案跳过下载」做不到，
    // 只在下载后去重存储；中转流量成为成本瓶颈时按 document id 预判跳过
    let mut source = match opener.open(job.tg_chat_id, job.tg_message_id).await {
        Ok(source) => source,
        Err(error) => return fail_with(pool, lease, &job, &error).await,
    };
    let staging = format!(
        "staging/{}-{}-{}",
        job.canonical_event_id,
        job.attempt_count,
        Utc::now().timestamp_millis()
    );
    let stored = match store.put_stream(&staging, source.as_mut()).await {
        Ok(stored) => stored,
        Err(MediaStoreError::TooLarge) => {
            return fail(pool, lease, &job, true, "MEDIA_TOO_LARGE").await;
        }
        Err(MediaStoreError::Source(error)) => return fail_with(pool, lease, &job, &error).await,
        Err(MediaStoreError::S3(message)) => {
            tracing::warn!(canonical_event_id = job.canonical_event_id, error = %message, "媒体写入对象存储失败");
            return fail(pool, lease, &job, false, "MEDIA_STORE_FAILED").await;
        }
    };
    let archive = MediaArchiveInput {
        content_hash: stored.content_hash.clone(),
        media_type: job.media_type.clone(),
        mime_type: job.mime_type.clone(),
        file_name: job.file_name.clone(),
        file_size: stored.size,
        object_key: stored.key.clone(),
    };
    // 同 hash 已有有效对象时 upsert 保留旧对象：job 指向旧档案，删掉刚写的这份。
    match complete_media_relay_job(pool, lease, job.canonical_event_id, &archive).await? {
        Some((_, kept_key)) => {
            let reused = kept_key != stored.key;
            if reused {
                let _ = store.delete(&stored.key).await;
            }
            Ok(RelayOutcome::Ready {
                canonical_event_id: job.canonical_event_id,
                reused,
            })
        }
        None => {
            let _ = store.delete(&stored.key).await;
            Ok(RelayOutcome::LeaseLost)
        }
    }
}

/// 源消息被删或没有媒体是永久失败；其余（网络、FLOOD_WAIT、file_reference 过期）可重试。
fn relay_error_is_permanent(error: &TelegramClientError) -> bool {
    matches!(
        error,
        TelegramClientError::SourceMessageMissing
            | TelegramClientError::SourceMediaMissing
            | TelegramClientError::InvalidPeer(_)
    ) || ["CHANNEL_PRIVATE", "CHANNEL_INVALID", "PEER_ID_INVALID"]
        .contains(&error.delivery_error().code.as_str())
}

async fn fail_with(
    pool: &PgPool,
    lease: &LeaseGuard,
    job: &MediaRelayJobRecord,
    error: &TelegramClientError,
) -> Result<RelayOutcome, sqlx::Error> {
    let code = error.delivery_error().code;
    tracing::warn!(canonical_event_id = job.canonical_event_id, error = %error, "媒体下载失败");
    fail(pool, lease, job, relay_error_is_permanent(error), &code).await
}

async fn fail(
    pool: &PgPool,
    lease: &LeaseGuard,
    job: &MediaRelayJobRecord,
    permanent: bool,
    code: &str,
) -> Result<RelayOutcome, sqlx::Error> {
    let retry = relay_retry_after(job.attempt_count);
    let dead = permanent || retry.is_none();
    let next_retry_at = (!dead)
        .then(|| retry.map(|seconds| Utc::now() + Duration::from_secs(seconds as u64)))
        .flatten();
    if !fail_media_relay_job(
        pool,
        lease,
        job.canonical_event_id,
        dead,
        next_retry_at,
        code,
    )
    .await?
    {
        return Ok(RelayOutcome::LeaseLost);
    }
    Ok(RelayOutcome::Failed {
        canonical_event_id: job.canonical_event_id,
        dead,
        error_code: code.to_owned(),
    })
}

/// 第 `attempt` 次失败后的退避秒数；`None` = 第 4 次失败，判死。
pub fn relay_retry_after(attempt: i32) -> Option<i64> {
    usize::try_from(attempt - 1)
        .ok()
        .and_then(|index| RELAY_RETRY_SECONDS.get(index).copied())
}

#[cfg(test)]
mod tests {
    use super::relay_retry_after;

    #[test]
    fn relay_retry_ladder_dies_on_fourth_failure() {
        assert_eq!(relay_retry_after(1), Some(30));
        assert_eq!(relay_retry_after(2), Some(300));
        assert_eq!(relay_retry_after(3), Some(1800));
        assert_eq!(relay_retry_after(4), None);
    }
}
