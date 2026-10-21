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
    CredentialError, CustodyConsent, NewBotIdentity, NewMtprotoIdentity, clear_bot_session,
    create_bot_identity, create_mtproto_identity, insert_custody_consent, latest_custody_consent,
    load_bot_credentials, load_identity_secret, load_mtproto_identity,
    reauthorize_mtproto_identity, store_bot_session, store_identity_secret,
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

async fn session_columns(
    admin: &support::AdminOnlyPool,
    identity: IdentityId,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    sqlx::query_as(
        "SELECT bot_session_ciphertext, bot_session_nonce FROM identity_secrets WHERE identity_id = $1",
    )
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("读 session 列失败")
}

/// ADR-0034：Bot session 与 Token 同 DEK、同行加密落库；Token 覆盖时 session 一并清空。
#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_session_roundtrips_and_is_cleared_when_token_is_replaced(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-bot-session", 6701).await;
    let identity = seed_identity(&admin, tenant).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant, identity, &secret(b"123456:token-a"))
        .await
        .expect("写 Token 失败");

    let first = load_bot_credentials(&app, &kek, tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert!(first.session.is_none());
    assert!(
        store_bot_session(
            &app,
            &kek,
            tenant,
            identity,
            &first,
            &secret(b"TGMS-session")
        )
        .await
        .expect("写 session 失败")
    );
    let (ciphertext, nonce) = session_columns(&admin, identity).await;
    let ciphertext = ciphertext.expect("session 密文为空");
    assert!(nonce.is_some());
    assert!(
        !ciphertext.windows(4).any(|window| window == b"TGMS"),
        "session 以明文落库"
    );

    let second = load_bot_credentials(&app, &kek, tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert_eq!(second.token.expose().as_slice(), b"123456:token-a");
    assert_eq!(
        second
            .session
            .as_ref()
            .map(|session| session.expose().clone()),
        Some(b"TGMS-session".to_vec())
    );

    store_identity_secret(&app, &kek, tenant, identity, &secret(b"123456:token-b"))
        .await
        .expect("覆盖 Token 失败");
    assert_eq!(session_columns(&admin, identity).await, (None, None));

    // 读出之后 Token 被换掉：拿旧读数写回的 session 必须被拒，不能搭上新 Token 那一行。
    assert!(
        !store_bot_session(&app, &kek, tenant, identity, &second, &secret(b"stale"))
            .await
            .expect("写 session 失败")
    );
    assert_eq!(session_columns(&admin, identity).await, (None, None));
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn tampered_bot_session_falls_back_and_can_be_cleared(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = seed_tenant(&admin, "cred-bot-session-tamper", 6702).await;
    let identity = seed_identity(&admin, tenant).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant, identity, &secret(b"123456:token"))
        .await
        .expect("写 Token 失败");
    let read = load_bot_credentials(&app, &kek, tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    store_bot_session(&app, &kek, tenant, identity, &read, &secret(b"session"))
        .await
        .expect("写 session 失败");

    // 把 Token 密文与 nonce 抄进 session 列：同 DEK、AAD 不同，必须解不开。
    sqlx::query(
        "UPDATE identity_secrets
            SET bot_session_ciphertext = secret_ciphertext, bot_session_nonce = nonce
          WHERE identity_id = $1",
    )
    .bind(identity)
    .execute(admin.raw())
    .await
    .expect("篡改失败");
    let back = load_bot_credentials(&app, &kek, tenant, identity)
        .await
        .expect("session 坏了不该让 Token 读不出来")
        .expect("应当读到凭据");
    assert_eq!(back.token.expose().as_slice(), b"123456:token");
    assert!(back.session.is_none(), "Token 密文被当成 session 解开了");

    clear_bot_session(&app, tenant, identity)
        .await
        .expect("清 session 失败");
    assert_eq!(session_columns(&admin, identity).await, (None, None));

    let half = sqlx::query(
        "UPDATE identity_secrets SET bot_session_ciphertext = '\\x00' WHERE identity_id = $1",
    )
    .bind(identity)
    .execute(admin.raw())
    .await;
    assert!(half.is_err(), "session 密文与 nonce 只有一半也能写进去");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn bot_session_is_tenant_scoped(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = seed_tenant(&admin, "cred-bot-session-a", 6703).await;
    let tenant_b = seed_tenant(&admin, "cred-bot-session-b", 6704).await;
    let identity = seed_identity(&admin, tenant_a).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant_a, identity, &secret(b"123456:token"))
        .await
        .expect("写 Token 失败");
    assert!(
        load_bot_credentials(&app, &kek, tenant_b, identity)
            .await
            .expect("读取失败")
            .is_none(),
        "别的租户读到了凭据"
    );
    let read = load_bot_credentials(&app, &kek, tenant_a, identity)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    store_bot_session(&app, &kek, tenant_a, identity, &read, &secret(b"session"))
        .await
        .expect("写 session 失败");
    let before = session_columns(&admin, identity).await;
    // 用 A 的读数、以 B 的租户上下文写：KEK 上下文对不上，unwrap 就失败。
    assert!(
        store_bot_session(&app, &kek, tenant_b, identity, &read, &secret(b"hijack"))
            .await
            .is_err()
    );
    clear_bot_session(&app, tenant_b, identity)
        .await
        .expect("清 session 失败");
    assert_eq!(
        session_columns(&admin, identity).await,
        before,
        "别的租户改动了 session"
    );
}

fn new_user(tg_user_id: i64, api_id: i32) -> NewMtprotoIdentity {
    NewMtprotoIdentity {
        tg_user_id,
        display_name: Some("Alice".to_owned()),
        api_id,
        actor_user_id: 7001,
        source: "web".to_owned(),
    }
}

/// QR 登录落库（spec/08 §1.3）：control-plane 只有 wrap 也能建身份；
/// `api_id` 落库、`api_hash` 不落库；session 能被 worker 侧的 KEK 解开；同租户同账号去重。
#[sqlx::test(migrations = "../../migrations-test")]
async fn create_mtproto_identity_is_encrypt_only_audited_and_deduplicated(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "mtproto-create", 6801).await;
    let writer = EncryptOnlyKek::new();
    let session = secret(b"grammers-session-snapshot");

    let identity = create_mtproto_identity(&app, &writer, tenant, new_user(42, 31001), &session)
        .await
        .expect("建 MTProto 身份失败");
    assert_eq!(
        *writer.wrapped.lock().expect("读取上下文失败"),
        vec![KekContext::new(tenant.get(), identity.get())]
    );
    let row: (String, Option<i64>, String, Option<i32>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT i.kind, i.tg_user_id, i.status, s.api_id, s.api_hash_ciphertext
           FROM identities i JOIN identity_secrets s ON s.identity_id = i.id
          WHERE i.tenant_id = $1 AND i.id = $2",
    )
    .bind(tenant)
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("读身份失败");
    assert_eq!(
        (row.0.as_str(), row.1, row.2.as_str(), row.3, row.4),
        ("mtproto", Some(42), "offline", Some(31001), None)
    );
    let back = load_identity_secret(&app, &kek(), tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到 session");
    assert_eq!(back.expose(), session.expose());
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT after FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.create' AND resource_id = $2",
    )
    .bind(tenant)
    .bind(identity.get())
    .fetch_one(admin.raw())
    .await
    .expect("应当有一条 identity.create 审计");
    assert_eq!(after["kind"], "mtproto");
    assert!(!after.to_string().contains("grammers-session"), "{after}");

    let err = create_mtproto_identity(&app, &writer, tenant, new_user(42, 31001), &session)
        .await
        .expect_err("同租户重复接入同一账号应当拒绝");
    assert!(
        matches!(err, CredentialError::AccountAlreadyRegistered),
        "{err:?}"
    );
    let other = seed_tenant(&admin, "mtproto-create-b", 6802).await;
    create_mtproto_identity(&app, &writer, other, new_user(42, 31001), &session)
        .await
        .expect("另一个租户接入同一账号不受影响");
    assert!(
        load_mtproto_identity(&app, other, identity.get())
            .await
            .expect("查询失败")
            .is_none(),
        "别的租户按 id 查不到本租户的身份"
    );
}

/// 重新授权（spec/08 §五）：覆盖 session 与 api_id、unauthorized 回到 offline、
/// 在册租约立即过期但 fencing token 不动；账号不一致拒绝且不改任何东西。
#[sqlx::test(migrations = "../../migrations-test")]
async fn reauthorize_mtproto_identity_requires_same_account_and_expires_lease(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "mtproto-reauth", 6811).await;
    let writer = EncryptOnlyKek::new();
    let identity = create_mtproto_identity(
        &app,
        &writer,
        tenant,
        new_user(42, 31001),
        &secret(b"old-session"),
    )
    .await
    .expect("建身份失败");
    sqlx::query("UPDATE identities SET status = 'unauthorized' WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await
        .expect("置 unauthorized 失败");
    sqlx::query(
        "INSERT INTO account_leases (identity_id, worker_id, fencing_token, heartbeat_at, expires_at)
         VALUES ($1, 'w1', 7, now(), now() + interval '1 hour')",
    )
    .bind(identity)
    .execute(admin.raw())
    .await
    .expect("造租约失败");

    let err = reauthorize_mtproto_identity(
        &app,
        &writer,
        tenant,
        identity,
        new_user(43, 31002),
        &secret(b"intruder-session"),
    )
    .await
    .expect_err("换了账号应当拒绝");
    assert!(matches!(err, CredentialError::AccountMismatch), "{err:?}");
    let back = load_identity_secret(&app, &kek(), tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到 session");
    assert_eq!(
        back.expose().as_slice(),
        b"old-session",
        "被拒的重新授权不得覆盖 session"
    );

    let status = reauthorize_mtproto_identity(
        &app,
        &writer,
        tenant,
        identity,
        new_user(42, 31002),
        &secret(b"new-session"),
    )
    .await
    .expect("同账号重新授权失败");
    assert_eq!(status, "offline");
    let back = load_identity_secret(&app, &kek(), tenant, identity)
        .await
        .expect("读取失败")
        .expect("应当读到 session");
    assert_eq!(back.expose().as_slice(), b"new-session");
    let (api_id, lease_live, fencing): (Option<i32>, bool, i64) = sqlx::query_as(
        "SELECT s.api_id, l.expires_at > clock_timestamp(), l.fencing_token
           FROM identity_secrets s JOIN account_leases l USING (identity_id)
          WHERE s.identity_id = $1",
    )
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("读状态失败");
    assert_eq!((api_id, lease_live, fencing), (Some(31002), false, 7));
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.reauthorize' AND resource_id = $2",
    )
    .bind(tenant)
    .bind(identity.get())
    .fetch_one(admin.raw())
    .await
    .expect("读审计失败");
    assert_eq!(audits, 1, "被拒的那次不该留审计");

    sqlx::query("UPDATE identities SET status = 'banned' WHERE id = $1")
        .bind(identity)
        .execute(admin.raw())
        .await
        .expect("置 banned 失败");
    let status = reauthorize_mtproto_identity(
        &app,
        &writer,
        tenant,
        identity,
        new_user(42, 31002),
        &secret(b"newer-session"),
    )
    .await
    .expect("重新授权失败");
    assert_eq!(status, "banned", "封禁不因重新扫码解除");

    let other = seed_tenant(&admin, "mtproto-reauth-b", 6812).await;
    let err = reauthorize_mtproto_identity(
        &app,
        &writer,
        other,
        identity,
        new_user(42, 31002),
        &secret(b"cross-tenant"),
    )
    .await
    .expect_err("跨租户重新授权应当拒绝");
    assert!(matches!(err, CredentialError::IdentityNotFound), "{err:?}");
}

/// 代持授权按租户隔离，取最近一条。
#[sqlx::test(migrations = "../../migrations-test")]
async fn custody_consent_latest_version_is_tenant_scoped(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "consent-a", 6821).await;
    let other = seed_tenant(&admin, "consent-b", 6822).await;
    sqlx::query("INSERT INTO tenant_members (tenant_id, user_id) VALUES ($1, 6821)")
        .bind(tenant)
        .execute(admin.raw())
        .await
        .expect("加成员失败");
    assert_eq!(
        latest_custody_consent(&app, tenant)
            .await
            .expect("查询失败"),
        None
    );
    for version in ["custody-old", "custody-v0"] {
        insert_custody_consent(
            &app,
            tenant,
            CustodyConsent {
                text_version: version.to_owned(),
                user_id: 6821,
                ip: "203.0.113.7".to_owned(),
                user_agent: Some("test".to_owned()),
            },
        )
        .await
        .expect("写同意失败");
    }
    assert_eq!(
        latest_custody_consent(&app, tenant)
            .await
            .expect("查询失败")
            .as_deref(),
        Some("custody-v0")
    );
    assert_eq!(
        latest_custody_consent(&app, other).await.expect("查询失败"),
        None
    );
    let ip: String = sqlx::query_scalar(
        "SELECT host(ip) FROM tenant_consents WHERE tenant_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("读同意失败");
    assert_eq!(ip, "203.0.113.7");
}
