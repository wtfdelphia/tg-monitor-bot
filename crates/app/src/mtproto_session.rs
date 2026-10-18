//! MTProto session 的加密落库与恢复（spec/08 §1.3）。
//!
//! 只做拼接：快照与恢复在 `tgm_tg::session`，信封加密与租户隔离在
//! `tgm_db::credentials`。明文快照只存在于这两步之间，随 `Secret` 的 `Drop` 清零。

use sqlx::PgPool;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::Kek;
use tgm_db::credentials::{CredentialError, load_identity_secret, store_identity_secret};
use tgm_tg::{StoredSession, StoredSessionError, TelegramClient, TelegramClientError};

#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Session(#[from] StoredSessionError),
    #[error(transparent)]
    Client(#[from] TelegramClientError),
}

/// 取当前客户端的 session 快照，加密写入该身份的凭据行。
pub async fn save_session(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
    client: &TelegramClient,
) -> Result<(), SessionStoreError> {
    let snapshot = client.snapshot().await?;
    store_identity_secret(pool, kek, tenant, identity, &snapshot).await?;
    Ok(())
}

/// 读出并解密该身份的 session。从未落库过时返回 `None`，调用方应走登录流程。
pub async fn load_session(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
) -> Result<Option<StoredSession>, SessionStoreError> {
    let Some(plain) = load_identity_secret(pool, kek, tenant, identity).await? else {
        return Ok(None);
    };
    Ok(Some(StoredSession::restore(plain.expose())?))
}
