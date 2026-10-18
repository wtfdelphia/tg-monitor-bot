//! 身份凭据信封加密落库的受限角色测试（spec/05 §七，DEK 粒度见 ADR-0032）。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Mutex;

use sqlx::PgPool;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::{DEK_LEN, EnvKek, Kek, KekContext, KekError};
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::{
    CredentialError, NewBotIdentity, create_bot_identity, load_identity_secret,
    store_identity_secret,
};

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

fn secret(value: &[u8]) -> Secret<Vec<u8>> {
    Secret::new(value.to_vec())
}

/// control-plane 在生产上的形状：只能 wrap，unwrap 一律被拒（KMS 侧 AccessDenied）。
/// 同时记下每次 wrap 的上下文，用来断言写入路径带的是哪个身份。
struct EncryptOnlyKek {
    inner: EnvKek,
    wrapped: Mutex<Vec<KekContext>>,
}

impl EncryptOnlyKek {
    fn new() -> Self {
        Self {
            inner: kek(),
            wrapped: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl Kek for EncryptOnlyKek {
    async fn wrap(
        &self,
        dek: &Secret<[u8; DEK_LEN]>,
        context: &KekContext,
    ) -> Result<Vec<u8>, KekError> {
        self.wrapped.lock().expect("记录上下文失败").push(*context);
        self.inner.wrap(dek, context).await
    }
    async fn unwrap(
        &self,
        _ciphertext: &[u8],
        _context: &KekContext,
    ) -> Result<Secret<[u8; DEK_LEN]>, KekError> {
        Err(KekError::Crypto)
    }
    fn kek_id(&self) -> &str {
        self.inner.kek_id()
    }
}

async fn seed_tenant(admin: &support::AdminOnlyPool, name: &str, owner: i64) -> TenantId {
    sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(owner)
    .fetch_one(admin.raw())
    .await
    .expect("创建租户失败")
}

async fn seed_identity(admin: &support::AdminOnlyPool, tenant: TenantId) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('bot', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建身份失败")
}

async fn dek_of(admin: &support::AdminOnlyPool, identity: IdentityId) -> (Vec<u8>, String) {
    sqlx::query_as("SELECT dek_ciphertext, kek_id FROM identity_secrets WHERE identity_id = $1")
        .bind(identity)
        .fetch_one(admin.raw())
        .await
        .expect("取 DEK 失败")
}

async fn credential_key_rows(admin: &support::AdminOnlyPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM credential_keys")
        .fetch_one(admin.raw())
        .await
        .expect("计数失败")
}

fn new_bot(tg_user_id: i64) -> NewBotIdentity {
    NewBotIdentity {
        tg_user_id,
        username: Some("data_bot".to_owned()),
        display_name: Some("Data".to_owned()),
        actor_user_id: 7001,
        source: "web".to_owned(),
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn stores_ciphertext_only_and_roundtrips_as_app_user(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    support::assert_not_bypassrls(&app).await;
    let tenant = seed_tenant(&admin, "cred-a", 6101).await;
    let identity = seed_identity(&admin, tenant).await;
    let kek = kek();

    assert!(
        load_identity_secret(&app, &kek, tenant, identity)
            .await
            .expect("读取失败")
            .is_none()
    );
    store_identity_secret(&app, &kek, tenant, identity, &secret(b"123456:bot-token-a"))
        .await
        .expect("写入失败");

    let (ciphertext, nonce): (Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT secret_ciphertext, nonce FROM identity_secrets WHERE identity_id = $1",
    )
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("取密文失败");
    assert_eq!(nonce.len(), 12);
    assert!(
        !ciphertext
            .windows(b"bot-token-a".len())
            .any(|window| window == b"bot-token-a"),
        "库里出现了明文"
    );
    let (first_dek, kek_id) = dek_of(&admin, identity).await;
    assert_eq!(kek_id, "local:env:v1");
    assert_ne!(first_dek.len(), DEK_LEN, "DEK 疑似以明文入库");

    let back = load_identity_secret(&app, &kek, tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert_eq!(back.expose().as_slice(), b"123456:bot-token-a");

    store_identity_secret(&app, &kek, tenant, identity, &secret(b"rotated-token"))
        .await
        .expect("覆盖写入失败");
    let back = load_identity_secret(&app, &kek, tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert_eq!(back.expose().as_slice(), b"rotated-token");
    assert_ne!(
        dek_of(&admin, identity).await.0,
        first_dek,
        "覆盖写入应当换新 DEK（写入方不解旧 DEK）"
    );
    assert_eq!(
        credential_key_rows(&admin).await,
        0,
        "credential_keys 已停写（ADR-0032）"
    );
}

/// ADR-0032 的核心性质：写入路径一次 unwrap 都不调。
/// 反证：把 store 改回「先 unwrap 租户 DEK」，这条红。
#[sqlx::test(migrations = "../../migrations-test")]
async fn writes_succeed_with_an_encrypt_only_kek(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-encrypt-only", 6151).await;
    let first = seed_identity(&admin, tenant).await;
    let second = seed_identity(&admin, tenant).await;
    let writer = EncryptOnlyKek::new();

    for identity in [first, second, first] {
        store_identity_secret(&app, &writer, tenant, identity, &secret(b"token"))
            .await
            .expect("只有加密权的 KEK 也必须能写入");
    }
    assert_eq!(
        *writer.wrapped.lock().expect("读取上下文失败"),
        vec![
            KekContext::new(tenant.get(), first.get()),
            KekContext::new(tenant.get(), second.get()),
            KekContext::new(tenant.get(), first.get()),
        ],
        "wrap 必须带写入身份的加密上下文"
    );
    let err = load_identity_secret(&app, &writer, tenant, first)
        .await
        .expect_err("只有加密权的 KEK 不该读出凭据");
    assert!(
        matches!(err, CredentialError::Kek(KekError::Crypto)),
        "{err:?}"
    );
    let back = load_identity_secret(&app, &kek(), tenant, first)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert_eq!(back.expose().as_slice(), b"token");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn other_tenant_can_neither_write_nor_read(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant_a = seed_tenant(&admin, "cred-cross-a", 6201).await;
    let tenant_b = seed_tenant(&admin, "cred-cross-b", 6202).await;
    let identity_a = seed_identity(&admin, tenant_a).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant_a, identity_a, &secret(b"token-a"))
        .await
        .expect("写入失败");

    let writer = EncryptOnlyKek::new();
    let err = store_identity_secret(&app, &writer, tenant_b, identity_a, &secret(b"hijack"))
        .await
        .expect_err("跨租户写入应当失败");
    assert!(matches!(err, CredentialError::IdentityNotFound), "{err:?}");
    assert!(
        writer.wrapped.lock().expect("读取上下文失败").is_empty(),
        "对别家身份的写入不该触发 KEK 调用"
    );
    assert!(
        load_identity_secret(&app, &kek, tenant_b, identity_a)
            .await
            .expect("读取不该报错")
            .is_none(),
        "跨租户读到了凭据"
    );
    let back = load_identity_secret(&app, &kek, tenant_a, identity_a)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert_eq!(back.expose().as_slice(), b"token-a", "被跨租户写入覆盖了");
}

/// 整行搬运（凭据密文 + nonce + DEK 密文）到同租户另一个身份：凭据 AAD 与 KEK
/// 加密上下文两道都绑定身份，任何一道在都解不开。
#[sqlx::test(migrations = "../../migrations-test")]
async fn row_copied_to_another_identity_does_not_decrypt(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-copy", 6301).await;
    let first = seed_identity(&admin, tenant).await;
    let second = seed_identity(&admin, tenant).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant, first, &secret(b"token-first"))
        .await
        .expect("写入失败");
    sqlx::query(
        "INSERT INTO identity_secrets (identity_id, secret_ciphertext, nonce, dek_ciphertext, kek_id)
         SELECT $2, secret_ciphertext, nonce, dek_ciphertext, kek_id
           FROM identity_secrets WHERE identity_id = $1",
    )
    .bind(first)
    .bind(second)
    .execute(admin.raw())
    .await
    .expect("搬运密文失败");

    let err = load_identity_secret(&app, &kek, tenant, second)
        .await
        .expect_err("搬运过来的密文不该解开");
    assert!(
        matches!(err, CredentialError::Kek(KekError::Crypto)),
        "{err:?}"
    );
}

/// 库里一行 DEK 密文换到别的身份（或别的租户）的上下文下必须解不开：
/// 这一条单测 KEK 加密上下文那一道，与凭据密文的 AAD 无关。
/// 反证：把 `KekContext::aad` 改成返回空切片，这条红（2026-10-18 实测）。
/// 早先的写法是「把 A 的 DEK 密文抄给 B 再读 B」—— 那样 B 的凭据密文配上 A 的 DEK，
/// GCM 本来就解不开，AAD 置空后它照样绿，测不到上下文这一层。
#[sqlx::test(migrations = "../../migrations-test")]
async fn stored_dek_does_not_unwrap_under_another_context(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-dek-ctx", 6351).await;
    let first = seed_identity(&admin, tenant).await;
    let second = seed_identity(&admin, tenant).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant, first, &secret(b"token-first"))
        .await
        .expect("写入失败");
    let (dek_ciphertext, _) = dek_of(&admin, first).await;

    assert!(
        kek.unwrap(&dek_ciphertext, &KekContext::new(tenant.get(), first.get()))
            .await
            .is_ok(),
        "本身份上下文下应当解得开"
    );
    for other in [
        KekContext::new(tenant.get(), second.get()),
        KekContext::new(tenant.get() + 1, first.get()),
    ] {
        assert!(
            kek.unwrap(&dek_ciphertext, &other).await.is_err(),
            "DEK 在 {other:?} 下竟然解开了"
        );
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn dek_wrapped_by_another_kek_is_refused(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-kek", 6401).await;
    let identity = seed_identity(&admin, tenant).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant, identity, &secret(b"token"))
        .await
        .expect("写入失败");
    sqlx::query("UPDATE identity_secrets SET kek_id = 'arn:aws:kms:other' WHERE identity_id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await
        .expect("改 kek_id 失败");

    let err = load_identity_secret(&app, &kek, tenant, identity)
        .await
        .expect_err("kek_id 不符应当拒绝");
    assert!(matches!(err, CredentialError::KekMismatch), "{err:?}");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn create_bot_identity_is_atomic_audited_and_encrypt_only(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-create", 6601).await;
    let writer = EncryptOnlyKek::new();
    let token = secret(b"123456:AAAAAAAAAAAAAAAAAAAAAAAAAAAA");

    let identity = create_bot_identity(&app, &writer, tenant, new_bot(123456), &token)
        .await
        .expect("建 Bot 身份失败");
    assert_eq!(
        *writer.wrapped.lock().expect("读取上下文失败"),
        vec![KekContext::new(tenant.get(), identity.get())],
        "DEK 必须绑定新身份自己的 id"
    );
    let (kind, tg_user_id, display_name): (String, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT kind, tg_user_id, display_name FROM identities WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("读身份失败");
    assert_eq!(
        (kind.as_str(), tg_user_id, display_name.as_deref()),
        ("bot", Some(123456), Some("Data"))
    );
    let back = load_identity_secret(&app, &kek(), tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert_eq!(back.expose(), token.expose());

    let (actor, source, after): (Option<i64>, String, serde_json::Value) = sqlx::query_as(
        "SELECT actor_user_id, source, after FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.create' AND resource_id = $2",
    )
    .bind(tenant)
    .bind(identity.get())
    .fetch_one(admin.raw())
    .await
    .expect("应当有一条 identity.create 审计");
    assert_eq!((actor, source.as_str()), (Some(7001), "web"));
    assert_eq!(after["credential_set"], true);
    assert!(
        !after.to_string().contains("AAAAAAAAAAAA"),
        "审计里出现了 token：{after}"
    );

    let err = create_bot_identity(&app, &writer, tenant, new_bot(123456), &token)
        .await
        .expect_err("同租户重复接入同一个 Bot 应当拒绝");
    assert!(
        matches!(err, CredentialError::BotAlreadyRegistered),
        "{err:?}"
    );
    let other = seed_tenant(&admin, "cred-create-b", 6602).await;
    create_bot_identity(&app, &writer, other, new_bot(123456), &token)
        .await
        .expect("另一个租户接入同一个 Bot 不受影响");
}

/// 并发提交同一个 Bot：advisory lock 下恰好一个成功，库里恰好一行。
#[sqlx::test(migrations = "../../migrations-test")]
async fn concurrent_create_of_same_bot_yields_one_identity(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-create-race", 6701).await;
    let kek = kek();
    let token = secret(b"777:AAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let (left, right) = tokio::join!(
        create_bot_identity(&app, &kek, tenant, new_bot(777), &token),
        create_bot_identity(&app, &kek, tenant, new_bot(777), &token),
    );
    assert_eq!(
        [left.is_ok(), right.is_ok()]
            .iter()
            .filter(|ok| **ok)
            .count(),
        1,
        "应当恰好一个成功：{left:?} / {right:?}"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM identities WHERE tenant_id = $1 AND tg_user_id = 777",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("计数失败");
    assert_eq!(rows, 1);
}
