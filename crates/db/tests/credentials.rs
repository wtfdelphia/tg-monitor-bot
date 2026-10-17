//! 身份凭据信封加密落库的受限角色测试（spec/05 §七）。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::{EnvKek, KekError};
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::{CredentialError, load_identity_secret, store_identity_secret};

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

fn secret(value: &[u8]) -> Secret<Vec<u8>> {
    Secret::new(value.to_vec())
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

async fn dek_rows(admin: &support::AdminOnlyPool, tenant: TenantId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM credential_keys WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(admin.raw())
        .await
        .expect("计数失败")
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
    let (dek_len, kek_id): (i32, String) = sqlx::query_as(
        "SELECT length(dek_ciphertext), kek_id FROM credential_keys WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("取 DEK 行失败");
    assert_eq!(kek_id, "local:env:v1");
    assert_ne!(dek_len, 32, "DEK 疑似以明文入库");

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
    assert_eq!(
        dek_rows(&admin, tenant).await,
        1,
        "覆盖写入不该再建一个 DEK"
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn other_tenant_can_neither_write_nor_read_and_gets_no_dek(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant_a = seed_tenant(&admin, "cred-cross-a", 6201).await;
    let tenant_b = seed_tenant(&admin, "cred-cross-b", 6202).await;
    let identity_a = seed_identity(&admin, tenant_a).await;
    let kek = kek();
    store_identity_secret(&app, &kek, tenant_a, identity_a, &secret(b"token-a"))
        .await
        .expect("写入失败");

    let err = store_identity_secret(&app, &kek, tenant_b, identity_a, &secret(b"hijack"))
        .await
        .expect_err("跨租户写入应当失败");
    assert!(matches!(err, CredentialError::IdentityNotFound), "{err:?}");
    assert!(
        load_identity_secret(&app, &kek, tenant_b, identity_a)
            .await
            .expect("读取不该报错")
            .is_none(),
        "跨租户读到了凭据"
    );
    assert_eq!(
        dek_rows(&admin, tenant_b).await,
        0,
        "对别家身份的写入给本租户造了 DEK"
    );
    let back = load_identity_secret(&app, &kek, tenant_a, identity_a)
        .await
        .expect("读取失败")
        .expect("应当读到凭据");
    assert_eq!(back.expose().as_slice(), b"token-a", "被跨租户写入覆盖了");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn ciphertext_copied_to_another_identity_does_not_decrypt(admin: PgPool) {
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
        "INSERT INTO identity_secrets (identity_id, secret_ciphertext, nonce)
         SELECT $2, secret_ciphertext, nonce FROM identity_secrets WHERE identity_id = $1",
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
    sqlx::query("UPDATE credential_keys SET kek_id = 'arn:aws:kms:other' WHERE tenant_id = $1")
        .bind(tenant)
        .execute(admin.raw())
        .await
        .expect("改 kek_id 失败");

    let err = load_identity_secret(&app, &kek, tenant, identity)
        .await
        .expect_err("kek_id 不符应当拒绝");
    assert!(matches!(err, CredentialError::KekMismatch), "{err:?}");
    let err = store_identity_secret(&app, &kek, tenant, identity, &secret(b"next"))
        .await
        .expect_err("kek_id 不符应当拒绝写入");
    assert!(matches!(err, CredentialError::KekMismatch), "{err:?}");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn concurrent_first_writes_share_one_dek(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "cred-race", 6501).await;
    let first = seed_identity(&admin, tenant).await;
    let second = seed_identity(&admin, tenant).await;
    let kek = kek();
    let (one, two) = (secret(b"token-1"), secret(b"token-2"));
    let (left, right) = tokio::join!(
        store_identity_secret(&app, &kek, tenant, first, &one),
        store_identity_secret(&app, &kek, tenant, second, &two),
    );
    left.expect("第一路写入失败");
    right.expect("第二路写入失败");
    assert_eq!(dek_rows(&admin, tenant).await, 1);
    for (identity, expected) in [
        (first, b"token-1".as_slice()),
        (second, b"token-2".as_slice()),
    ] {
        let back = load_identity_secret(&app, &kek, tenant, identity)
            .await
            .expect("读取失败")
            .expect("应当读到凭据");
        assert_eq!(back.expose().as_slice(), expected);
    }
}
