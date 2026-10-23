//! 身份凭据的信封加密落库。定义处是 docs/design/spec/05-安全与租户隔离.md §七，
//! KEK 契约在 docs/design/eng/01-配置与Secret.md §三，DEK 粒度见 ADR-0032。
//!
//! 四条形态约定：
//!
//! - **每身份一个 DEK，写入只 wrap 不 unwrap。** 每次写入都本地新生成 DEK，
//!   加密凭据后用 KEK 包裹，DEK 密文与凭据密文同行存放。所以写入方（control-plane）
//!   只需要 `kms:Encrypt`；`kms:Decrypt` 只给读取方（ADR-0032）。
//! - **KEK 调用带加密上下文 `{tenant_id, identity_id}`。** DEK 密文搬到别的身份上
//!   解不开；凭据密文另有 AAD 绑定身份（`tgm_core::credential`）。
//! - **KEK 调用不在事务里。** KMS 一次往返是网络调用，放进 `tenant_tx` 会让事务
//!   跨网络持锁。所以拆成「事务外加密 → 事务内写密文」与「事务内读 → 事务外解密」。
//! - **`identity_secrets` 没有 RLS**（0009 丙类），隔离完全靠这里的 SQL：
//!   读写都经 `identities` 连接并显式 `WHERE tenant_id = $1`，而 `identities`
//!   本身有 RLS —— 应用层与 RLS 两道都在。
//!
//! 不缓存解开的 DEK（eng/01 §三 末段）：每次读取各自 unwrap，用完随 `Secret` 的 `Drop` 清零。

use sqlx::PgPool;
use tgm_core::credential::{
    Sealed, generate_dek, open_bot_session, open_identity_secret, seal_bot_session,
    seal_identity_secret,
};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::{Kek, KekContext, KekError};
use tgm_core::secret::Secret;

use crate::repo::{LeaseGuard, lease_is_held};
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
    /// 同一租户已接入同一个 Bot。
    #[error("该 Bot 已接入")]
    BotAlreadyRegistered,
    /// 该行 DEK 是另一个 KEK 包的。
    // ponytail: 不支持多 KEK 并存，KEK 轮换落地时改为按行 kek_id 选 KEK
    #[error("凭据 DEK 的 kek_id 与当前 KEK 不符，KEK 轮换尚未实现")]
    KekMismatch,
    /// 持租约写入时发现租约已被接管或过期（spec/08 §4.3）。
    #[error("身份租约已丢失")]
    LeaseLost,
    /// 同一租户已接入同一个 Telegram 用户账号。
    #[error("该账号已接入")]
    AccountAlreadyRegistered,
    /// 重新授权登录到的账号与身份原账号不一致。
    #[error("登录的账号与该身份不一致")]
    AccountMismatch,
}

/// 一份待落库的凭据：凭据密文 + 被 KEK 包裹的 DEK。明文 DEK 在返回前已清零。
struct SealedCredential {
    sealed: Sealed,
    dek_ciphertext: Vec<u8>,
    kek_id: String,
}

async fn seal_with_fresh_dek(
    kek: &dyn Kek,
    tenant: TenantId,
    identity_id: i64,
    plaintext: &[u8],
) -> Result<SealedCredential, KekError> {
    let dek = generate_dek()?;
    let sealed = seal_identity_secret(&dek, identity_id, plaintext)?;
    let dek_ciphertext = kek
        .wrap(&dek, &KekContext::new(tenant.get(), identity_id))
        .await?;
    drop(dek);
    Ok(SealedCredential {
        sealed,
        dek_ciphertext,
        kek_id: kek.kek_id().to_owned(),
    })
}

const UPSERT_SECRET: &str = "INSERT INTO identity_secrets
        (identity_id, secret_ciphertext, nonce, dek_ciphertext, kek_id)
     SELECT id, $3, $4, $5, $6 FROM identities WHERE tenant_id = $1 AND id = $2
     ON CONFLICT (identity_id) DO UPDATE
        SET secret_ciphertext = EXCLUDED.secret_ciphertext,
            nonce = EXCLUDED.nonce,
            dek_ciphertext = EXCLUDED.dek_ciphertext,
            kek_id = EXCLUDED.kek_id,
            bot_session_ciphertext = NULL,
            bot_session_nonce = NULL,
            updated_at = now()
     RETURNING identity_id";

/// 写入（或覆盖）一个已有身份的凭据密文。Bot 存 token，MTProto 存 session。
///
/// 覆盖时一并清空 Bot session（ADR-0034）：新凭据换了 DEK，旧 session 既解不开、
/// 也不该在 Token 轮换后继续代表这个 Bot 登录。
pub async fn store_identity_secret(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
    plaintext: &Secret<Vec<u8>>,
) -> Result<(), CredentialError> {
    store_secret(pool, kek, tenant, identity, plaintext, None).await
}

/// 同 [`store_identity_secret`]，写入事务内先校验租约（mtproto-worker 落 session 用）。
/// 僵尸 worker 不能用旧快照覆盖接管方的 session。
pub async fn store_identity_secret_leased(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    lease: &LeaseGuard,
    plaintext: &Secret<Vec<u8>>,
) -> Result<(), CredentialError> {
    store_secret(
        pool,
        kek,
        tenant,
        lease.identity_id,
        plaintext,
        Some(lease.clone()),
    )
    .await
}

async fn store_secret(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
    plaintext: &Secret<Vec<u8>>,
    lease: Option<LeaseGuard>,
) -> Result<(), CredentialError> {
    // 先确认身份归属再调 KEK：对不存在的身份写入不该产生一次 KMS 调用。
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

    let credential = seal_with_fresh_dek(kek, tenant, identity.get(), plaintext.expose()).await?;
    let written = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            if let Some(lease) = &lease
                && !lease_is_held(&mut *conn, lease).await?
            {
                return Ok(Err(CredentialError::LeaseLost));
            }
            sqlx::query_scalar::<_, i64>(UPSERT_SECRET)
                .bind(tenant.get())
                .bind(identity.get())
                .bind(credential.sealed.ciphertext)
                .bind(credential.sealed.nonce)
                .bind(credential.dek_ciphertext)
                .bind(credential.kek_id)
                .fetch_optional(conn)
                .await
                .map(Ok)
        })
    })
    .await??;
    written.map(|_| ()).ok_or(CredentialError::IdentityNotFound)
}

/// 新建 Bot 身份时的公开字段（来自 getMe，不含 token）。
#[derive(Debug, Clone)]
pub struct NewBotIdentity {
    pub tg_user_id: i64,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub actor_user_id: i64,
    pub source: String,
}

/// 新建 Bot 身份并写入其 token 密文，同一事务内记 `identity.create` 审计。
///
/// 身份 id 先从序列取出：凭据的 AAD 与 KEK 加密上下文都要绑定它，
/// 而 KEK 调用必须在事务外。取出后没用上的 id 只是序列空洞。
pub async fn create_bot_identity(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    input: NewBotIdentity,
    token: &Secret<Vec<u8>>,
) -> Result<IdentityId, CredentialError> {
    let tg_user_id = input.tg_user_id;
    let (identity_id, exists) = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM identities
                                WHERE tenant_id = $1 AND kind = 'bot' AND tg_user_id = $2)",
            )
            .bind(tenant.get())
            .bind(tg_user_id)
            .fetch_one(&mut *conn)
            .await?;
            let id = sqlx::query_scalar::<_, i64>(
                "SELECT nextval(pg_get_serial_sequence('identities', 'id'))",
            )
            .fetch_one(&mut *conn)
            .await?;
            Ok((id, exists))
        })
    })
    .await?;
    if exists {
        return Err(CredentialError::BotAlreadyRegistered);
    }

    let credential = seal_with_fresh_dek(kek, tenant, identity_id, token.expose()).await?;
    let created = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            // 上面那次查重在事务外，只为免掉一次无用的 KMS 调用；真正的去重在这里：
            // 同租户同 Bot 的并发提交按事务级 advisory lock 串行，锁内再查一次。
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!(
                    "tgm:identity.bot:{}:{}",
                    tenant.get(),
                    input.tg_user_id
                ))
                .execute(&mut *conn)
                .await?;
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM identities
                                WHERE tenant_id = $1 AND kind = 'bot' AND tg_user_id = $2)",
            )
            .bind(tenant.get())
            .bind(input.tg_user_id)
            .fetch_one(&mut *conn)
            .await?;
            if exists {
                return Ok(None);
            }
            let id = sqlx::query_scalar::<_, IdentityId>(
                "INSERT INTO identities (id, kind, tenant_id, tg_user_id, display_name, status)
                 OVERRIDING SYSTEM VALUE
                 VALUES ($2, 'bot', $1, $3, $4, 'offline')
                 RETURNING id",
            )
            .bind(tenant.get())
            .bind(identity_id)
            .bind(input.tg_user_id)
            .bind(input.display_name.as_deref())
            .fetch_one(&mut *conn)
            .await?;
            sqlx::query_scalar::<_, i64>(UPSERT_SECRET)
                .bind(tenant.get())
                .bind(identity_id)
                .bind(credential.sealed.ciphertext)
                .bind(credential.sealed.nonce)
                .bind(credential.dek_ciphertext)
                .bind(credential.kek_id)
                .fetch_one(&mut *conn)
                .await?;
            sqlx::query(
                "INSERT INTO audit_logs
                     (tenant_id, actor_user_id, action, resource, resource_id, after, source)
                 VALUES ($1, $2, 'identity.create', 'identity', $3, $4, $5)",
            )
            .bind(tenant.get())
            .bind(input.actor_user_id)
            .bind(identity_id)
            .bind(serde_json::json!({
                "kind": "bot",
                "tg_user_id": input.tg_user_id,
                "username": input.username,
                "display_name": input.display_name,
                "credential_set": true,
            }))
            .bind(input.source)
            .execute(&mut *conn)
            .await?;
            Ok(Some(id))
        })
    })
    .await?;
    created.ok_or(CredentialError::BotAlreadyRegistered)
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
                "SELECT s.secret_ciphertext, s.nonce, s.dek_ciphertext, s.kek_id
                   FROM identity_secrets s
                   JOIN identities i ON i.id = s.identity_id
                  WHERE i.tenant_id = $1 AND s.identity_id = $2",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .fetch_optional(conn)
            .await
        })
    })
    .await?;
    let Some((ciphertext, nonce, dek_ciphertext, kek_id)) = row else {
        return Ok(None);
    };
    if kek_id != kek.kek_id() {
        return Err(CredentialError::KekMismatch);
    }
    let dek = kek
        .unwrap(
            &dek_ciphertext,
            &KekContext::new(tenant.get(), identity.get()),
        )
        .await?;
    Ok(Some(open_identity_secret(
        &dek,
        identity.get(),
        &nonce,
        &ciphertext,
    )?))
}

/// 读出该身份的凭据与 Bot session（ADR-0034）。一次 unwrap 解两份密文：
/// 两者同一个 DEK，分开读会多一次 KMS 往返。身份没有存过凭据时返回 `None`。
pub async fn load_bot_credentials(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
) -> Result<Option<BotCredentials>, CredentialError> {
    type Row = (
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        String,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    );
    let row = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, Row>(
                "SELECT s.secret_ciphertext, s.nonce, s.dek_ciphertext, s.kek_id,
                        s.bot_session_ciphertext, s.bot_session_nonce
                   FROM identity_secrets s
                   JOIN identities i ON i.id = s.identity_id
                  WHERE i.tenant_id = $1 AND s.identity_id = $2 AND i.kind = 'bot'",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .fetch_optional(conn)
            .await
        })
    })
    .await?;
    let Some((ciphertext, nonce, dek_ciphertext, kek_id, session_ciphertext, session_nonce)) = row
    else {
        return Ok(None);
    };
    if kek_id != kek.kek_id() {
        return Err(CredentialError::KekMismatch);
    }
    let dek = kek
        .unwrap(
            &dek_ciphertext,
            &KekContext::new(tenant.get(), identity.get()),
        )
        .await?;
    let token = open_identity_secret(&dek, identity.get(), &nonce, &ciphertext)?;
    // session 解不开（被篡改、被抄自别的身份）不是致命错误：退回 Token 重新登录即可。
    let session = match (session_ciphertext, session_nonce) {
        (Some(ciphertext), Some(nonce)) => {
            open_bot_session(&dek, identity.get(), &nonce, &ciphertext).ok()
        }
        _ => None,
    };
    Ok(Some(BotCredentials {
        token,
        session,
        dek_ciphertext,
    }))
}

/// [`load_bot_credentials`] 的结果。`dek_ciphertext` 是读出时的 DEK 密文，
/// 写回 session 时用它确认这期间 Token 没被换掉。
pub struct BotCredentials {
    pub token: Secret<Vec<u8>>,
    pub session: Option<Secret<Vec<u8>>>,
    dek_ciphertext: Vec<u8>,
}

impl BotCredentials {
    pub fn version(&self) -> &[u8] {
        &self.dek_ciphertext
    }
}

impl std::fmt::Debug for BotCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BotCredentials([REDACTED])")
    }
}

/// 把 Bot session 加密写回（ADR-0034）。需要 KEK 的 unwrap：session 必须用 Token 那份 DEK
/// 加密，否则两份密文的 DEK 各在一处、对不上。调用方是 delivery，它本来就持有 Decrypt。
///
/// 只在 DEK 未变时写入：读出之后 Token 被 control-plane 覆盖过，这份 session 属于旧 Token，
/// 丢弃。返回是否写入。
pub async fn store_bot_session(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
    credentials: &BotCredentials,
    session: &Secret<Vec<u8>>,
) -> Result<bool, CredentialError> {
    let dek = kek
        .unwrap(
            &credentials.dek_ciphertext,
            &KekContext::new(tenant.get(), identity.get()),
        )
        .await?;
    let sealed = seal_bot_session(&dek, identity.get(), session.expose())?;
    drop(dek);
    let dek_ciphertext = credentials.dek_ciphertext.clone();
    let written = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, i64>(
                "UPDATE identity_secrets AS s
                    SET bot_session_ciphertext = $3, bot_session_nonce = $4
                   FROM identities i
                  WHERE i.id = s.identity_id AND i.tenant_id = $1 AND s.identity_id = $2
                    AND i.kind = 'bot' AND s.dek_ciphertext = $5
                 RETURNING s.identity_id",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .bind(sealed.ciphertext)
            .bind(sealed.nonce)
            .bind(dek_ciphertext)
            .fetch_optional(conn)
            .await
        })
    })
    .await?;
    Ok(written.is_some())
}

/// 丢弃 Bot session（auth key 被 Telegram 判失效时）。下次启动改用 Token 登录。
pub async fn clear_bot_session(
    pool: &PgPool,
    tenant: TenantId,
    identity: IdentityId,
) -> Result<(), CredentialError> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query(
                "UPDATE identity_secrets AS s
                    SET bot_session_ciphertext = NULL, bot_session_nonce = NULL
                   FROM identities i
                  WHERE i.id = s.identity_id AND i.tenant_id = $1 AND s.identity_id = $2",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .execute(conn)
            .await
        })
    })
    .await?;
    Ok(())
}

/// QR 登录完成后新建 MTProto 身份的公开字段（来自 getMe，不含 session）。
#[derive(Debug, Clone)]
pub struct NewMtprotoIdentity {
    pub tg_user_id: i64,
    pub display_name: Option<String>,
    /// 租户自带（ADR-0013）。`api_hash` 不落库：control-plane 解不开密文，worker 不需要它。
    pub api_id: i32,
    pub actor_user_id: i64,
    pub source: String,
}

fn mtproto_lock_key(tenant: TenantId, tg_user_id: i64) -> String {
    format!("tgm:identity.mtproto:{}:{tg_user_id}", tenant.get())
}

/// 新建 MTProto 身份并写入 session 密文与 `api_id`，同一事务内记 `identity.create` 审计。
///
/// 去重形状照 [`create_bot_identity`]：事务外查重省一次 KMS 调用，事务内 advisory lock 再查一次。
pub async fn create_mtproto_identity(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    input: NewMtprotoIdentity,
    session: &Secret<Vec<u8>>,
) -> Result<IdentityId, CredentialError> {
    let tg_user_id = input.tg_user_id;
    let (identity_id, exists) = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let exists = mtproto_account_exists(&mut *conn, tenant, tg_user_id).await?;
            let id = sqlx::query_scalar::<_, i64>(
                "SELECT nextval(pg_get_serial_sequence('identities', 'id'))",
            )
            .fetch_one(&mut *conn)
            .await?;
            Ok((id, exists))
        })
    })
    .await?;
    if exists {
        return Err(CredentialError::AccountAlreadyRegistered);
    }

    let credential = seal_with_fresh_dek(kek, tenant, identity_id, session.expose()).await?;
    let created = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(mtproto_lock_key(tenant, input.tg_user_id))
                .execute(&mut *conn)
                .await?;
            if mtproto_account_exists(&mut *conn, tenant, input.tg_user_id).await? {
                return Ok(None);
            }
            let id = sqlx::query_scalar::<_, IdentityId>(
                "INSERT INTO identities (id, kind, tenant_id, tg_user_id, display_name, status)
                 OVERRIDING SYSTEM VALUE
                 VALUES ($2, 'mtproto', $1, $3, $4, 'offline')
                 RETURNING id",
            )
            .bind(tenant.get())
            .bind(identity_id)
            .bind(input.tg_user_id)
            .bind(input.display_name.as_deref())
            .fetch_one(&mut *conn)
            .await?;
            write_mtproto_secret(&mut *conn, tenant, identity_id, credential, input.api_id).await?;
            sqlx::query(
                "INSERT INTO audit_logs
                     (tenant_id, actor_user_id, action, resource, resource_id, after, source)
                 VALUES ($1, $2, 'identity.create', 'identity', $3, $4, $5)",
            )
            .bind(tenant.get())
            .bind(input.actor_user_id)
            .bind(identity_id)
            .bind(serde_json::json!({
                "kind": "mtproto",
                "tg_user_id": input.tg_user_id,
                "display_name": input.display_name,
                "api_id_set": true,
                "credential_set": true,
            }))
            .bind(input.source)
            .execute(&mut *conn)
            .await?;
            Ok(Some(id))
        })
    })
    .await?;
    created.ok_or(CredentialError::AccountAlreadyRegistered)
}

/// 对已有 MTProto 身份重新授权：覆盖 session 与 `api_id`，`unauthorized` / `offline` 转 `offline`，
/// 让在册租约立即过期，记 `identity.reauthorize` 审计（spec/08 §五「用户重新扫码 → 回到 offline」）。
/// 返回落库后的状态（`banned` 保持不变）。
///
/// 登录到的账号必须与身份原账号一致，否则拒绝 —— 不让别的账号顶替老身份上已有的订阅。
/// 租约置过期而不删：fencing token 保持单调，仍持旧 token 的 worker 写不进新 session（spec/08 §4.3）。
pub async fn reauthorize_mtproto_identity(
    pool: &PgPool,
    kek: &dyn Kek,
    tenant: TenantId,
    identity: IdentityId,
    input: NewMtprotoIdentity,
    session: &Secret<Vec<u8>>,
) -> Result<String, CredentialError> {
    let tg_user_id = input.tg_user_id;
    let current = tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, (Option<i64>,)>(
                "SELECT tg_user_id FROM identities
                  WHERE tenant_id = $1 AND id = $2 AND kind = 'mtproto'",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .fetch_optional(conn)
            .await
        })
    })
    .await?;
    let Some((current,)) = current else {
        return Err(CredentialError::IdentityNotFound);
    };
    if current.is_some_and(|current| current != tg_user_id) {
        return Err(CredentialError::AccountMismatch);
    }

    let credential = seal_with_fresh_dek(kek, tenant, identity.get(), session.expose()).await?;
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            let before = sqlx::query_as::<_, (String, Option<i64>)>(
                "SELECT status, tg_user_id FROM identities
                  WHERE tenant_id = $1 AND id = $2 AND kind = 'mtproto'
                  FOR UPDATE",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .fetch_optional(&mut *conn)
            .await?;
            let Some((status, current)) = before else {
                return Ok(Err(CredentialError::IdentityNotFound));
            };
            if current.is_some_and(|current| current != tg_user_id) {
                return Ok(Err(CredentialError::AccountMismatch));
            }
            sqlx::query(
                "UPDATE account_leases SET expires_at = clock_timestamp()
                  WHERE identity_id = $1 AND expires_at > clock_timestamp()",
            )
            .bind(identity.get())
            .execute(&mut *conn)
            .await?;
            let after = if status == "banned" { "banned" } else { "offline" };
            sqlx::query(
                "UPDATE identities
                    SET tg_user_id = $3, status = $4, updated_at = clock_timestamp()
                  WHERE tenant_id = $1 AND id = $2",
            )
            .bind(tenant.get())
            .bind(identity.get())
            .bind(tg_user_id)
            .bind(after)
            .execute(&mut *conn)
            .await?;
            write_mtproto_secret(&mut *conn, tenant, identity.get(), credential, input.api_id)
                .await?;
            sqlx::query(
                "INSERT INTO audit_logs
                     (tenant_id, actor_user_id, action, resource, resource_id, before, after, source)
                 VALUES ($1, $2, 'identity.reauthorize', 'identity', $3, $4, $5, $6)",
            )
            .bind(tenant.get())
            .bind(input.actor_user_id)
            .bind(identity.get())
            .bind(serde_json::json!({ "status": status }))
            .bind(serde_json::json!({
                "status": after,
                "tg_user_id": tg_user_id,
                "api_id_set": true,
                "credential_set": true,
            }))
            .bind(input.source)
            .execute(&mut *conn)
            .await?;
            Ok(Ok(after.to_owned()))
        })
    })
    .await?
}

/// 按 id 取该租户的 MTProto 身份。重新授权在扫码前先核一次，免得扫完码才发现 id 不对。
pub async fn load_mtproto_identity(
    pool: &PgPool,
    tenant: TenantId,
    identity: i64,
) -> Result<Option<IdentityId>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, IdentityId>(
                "SELECT id FROM identities
                  WHERE tenant_id = $1 AND id = $2 AND kind = 'mtproto'",
            )
            .bind(tenant.get())
            .bind(identity)
            .fetch_optional(conn)
            .await
        })
    })
    .await
}

async fn mtproto_account_exists(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    tg_user_id: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM identities
                        WHERE tenant_id = $1 AND kind = 'mtproto' AND tg_user_id = $2)",
    )
    .bind(tenant.get())
    .bind(tg_user_id)
    .fetch_one(conn)
    .await
}

async fn write_mtproto_secret(
    conn: &mut sqlx::PgConnection,
    tenant: TenantId,
    identity_id: i64,
    credential: SealedCredential,
    api_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, i64>(UPSERT_SECRET)
        .bind(tenant.get())
        .bind(identity_id)
        .bind(credential.sealed.ciphertext)
        .bind(credential.sealed.nonce)
        .bind(credential.dek_ciphertext)
        .bind(credential.kek_id)
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE identity_secrets SET api_id = $2, api_hash_ciphertext = NULL
          WHERE identity_id = $1",
    )
    .bind(identity_id)
    .bind(api_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// 租户凭据代持授权（spec/06 §2.2，`tenant_consents.consent_type = 'credential_custody'`）。
#[derive(Debug, Clone)]
pub struct CustodyConsent {
    pub text_version: String,
    pub user_id: i64,
    pub ip: String,
    pub user_agent: Option<String>,
}

/// 该租户最近一条 `credential_custody` 同意的文本版本。没有同意过时返回 `None`。
pub async fn latest_custody_consent(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<Option<String>, sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, String>(
                "SELECT text_version FROM tenant_consents
                  WHERE tenant_id = $1 AND consent_type = 'credential_custody'
                  ORDER BY agreed_at DESC, id DESC
                  LIMIT 1",
            )
            .bind(tenant.get())
            .fetch_optional(conn)
            .await
        })
    })
    .await
}

/// 记一条代持授权同意。表只给 `SELECT, INSERT`（R22），凭证不可改。
pub async fn insert_custody_consent(
    pool: &PgPool,
    tenant: TenantId,
    consent: CustodyConsent,
) -> Result<(), sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO tenant_consents
                     (tenant_id, consent_type, text_version, user_id, ip, user_agent)
                 VALUES ($1, 'credential_custody', $2, $3, $4::inet, $5)",
            )
            .bind(tenant.get())
            .bind(consent.text_version)
            .bind(consent.user_id)
            .bind(consent.ip)
            .bind(consent.user_agent)
            .execute(conn)
            .await?;
            Ok(())
        })
    })
    .await
}
