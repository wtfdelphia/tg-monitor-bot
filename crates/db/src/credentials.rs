//! 身份凭据的信封加密落库。定义处是 docs/design/spec/05-安全与租户隔离.md §七，
//! KEK 契约在 docs/design/eng/01-配置与Secret.md §三。
//!
//! 三条形态约定：
//!
//! - **KEK 调用不在事务里。** KMS 一次往返是网络调用，放进 `tenant_tx` 会让事务
//!   跨网络持锁。所以拆成「读 DEK 行 → 事务外 unwrap → 事务内写密文」。
//! - **不缓存解开的 DEK**（eng/01 §三 末段）：每次调用各自 unwrap，用完随
//!   `Secret` 的 `Drop` 清零。
//! - **`identity_secrets` 没有 RLS**（0009 丙类），隔离完全靠这里的 SQL：
//!   读写都经 `identities` 连接并显式 `WHERE tenant_id = $1`，而 `identities`
//!   本身有 RLS —— 应用层与 RLS 两道都在。

use sqlx::PgPool;
use tgm_core::credential::{generate_dek, open_identity_secret, seal_identity_secret};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::{Kek, KekError};
use tgm_core::secret::Secret;

use crate::tenant::tenant_tx;

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Kek(#[from] KekError),
    /// 身份不存在，或不属于该租户。两者不区分，同 `AppError::NotFound` 的口径。
    #[error("身份不存在")]
    IdentityNotFound,
    /// 该租户的 DEK 是另一个 KEK 包的。
    // ponytail: 不支持多 KEK 并存，KEK 轮换落地时改为按行 kek_id 选 KEK
    #[error("租户 DEK 的 kek_id 与当前 KEK 不符，KEK 轮换尚未实现")]
    KekMismatch,
}

/// 取该租户的 DEK 密文；没有就新建一个。
///
/// 并发首次写入时两边都可能各生成一个 DEK，`ON CONFLICT DO NOTHING` 只留先到的
/// 那一行，之后统一以库里那一行为准 —— 输家生成的 DEK 从未用于加密，直接丢弃。
async fn ensure_wrapped_dek(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
) -> Result<Vec<u8>, CredentialError> {
    if let Some(row) = select_wrapped_dek(pool, tenant).await? {
        return check_kek(kek, row);
    }
    let wrapped = kek.wrap(&generate_dek()?).await?;
    let kek_id = kek.kek_id().to_owned();
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO credential_keys (tenant_id, dek_ciphertext, kek_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (tenant_id) DO NOTHING",
            )
            .bind(tenant.get())
            .bind(wrapped)
            .bind(kek_id)
            .execute(conn)
            .await
            .map(|_| ())
        })
    })
    .await?;
    let row = select_wrapped_dek(pool, tenant)
        .await?
        .ok_or(CredentialError::Db(sqlx::Error::RowNotFound))?;
    check_kek(kek, row)
}

async fn select_wrapped_dek(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<Option<(Vec<u8>, String)>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, (Vec<u8>, String)>(
                "SELECT dek_ciphertext, kek_id FROM credential_keys WHERE tenant_id = $1",
            )
            .bind(tenant.get())
            .fetch_optional(conn)
            .await
        })
    })
    .await
}

fn check_kek(
    kek: &dyn Kek,
    (wrapped, kek_id): (Vec<u8>, String),
) -> Result<Vec<u8>, CredentialError> {
    if kek_id == kek.kek_id() {
        Ok(wrapped)
    } else {
        Err(CredentialError::KekMismatch)
    }
}

/// 写入（或覆盖）一个身份的凭据密文。Bot 存 token，MTProto 存 session。
pub async fn store_identity_secret(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
    plaintext: &Secret<Vec<u8>>,
) -> Result<(), CredentialError> {
    // 先确认身份归属，再去建 DEK：否则对不存在的身份写入也会给租户凭空造一行 DEK。
    let owned = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM identities WHERE tenant_id = $1 AND id = $2",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .fetch_optional(conn)
            .await
        })
    })
    .await?;
    if owned.is_none() {
        return Err(CredentialError::IdentityNotFound);
    }

    let wrapped = ensure_wrapped_dek(pool, kek, tenant).await?;
    let dek = kek.unwrap(&wrapped).await?;
    let sealed = seal_identity_secret(&dek, identity.get(), plaintext.expose())?;
    drop(dek);

    let written = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>(
                "INSERT INTO identity_secrets (identity_id, secret_ciphertext, nonce)
                 SELECT id, $3, $4 FROM identities WHERE tenant_id = $1 AND id = $2
                 ON CONFLICT (identity_id) DO UPDATE
                    SET secret_ciphertext = EXCLUDED.secret_ciphertext,
                        nonce = EXCLUDED.nonce,
                        updated_at = now()
                 RETURNING identity_id",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .bind(sealed.ciphertext)
            .bind(sealed.nonce)
            .fetch_optional(conn)
            .await
        })
    })
    .await?;
    written.map(|_| ()).ok_or(CredentialError::IdentityNotFound)
}

/// 读出并解密一个身份的凭据。身份没有存过凭据时返回 `None`。
pub async fn load_identity_secret(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
) -> Result<Option<Secret<Vec<u8>>>, CredentialError> {
    let row = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, (Vec<u8>, Vec<u8>, Vec<u8>, String)>(
                "SELECT s.secret_ciphertext, s.nonce, k.dek_ciphertext, k.kek_id
                   FROM identity_secrets s
                   JOIN identities i ON i.id = s.identity_id
                   JOIN credential_keys k ON k.tenant_id = i.tenant_id
                  WHERE i.tenant_id = $1 AND s.identity_id = $2",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .fetch_optional(conn)
            .await
        })
    })
    .await?;
    let Some((ciphertext, nonce, wrapped, kek_id)) = row else {
        return Ok(None);
    };
    let wrapped = check_kek(kek, (wrapped, kek_id))?;
    let dek = kek.unwrap(&wrapped).await?;
    Ok(Some(open_identity_secret(
        &dek,
        identity.get(),
        &nonce,
        &ciphertext,
    )?))
}
