//! MTProto session 加密落库与恢复（spec/08 §1.3），以受限角色 app_user 连接。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use grammers_session::Session;
use grammers_session::types::{PeerAuth, PeerId, PeerInfo};
use sqlx::PgPool;
use tgm_app::mtproto_session::{SessionStoreError, load_session, save_session};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_db::credentials::{CredentialError, store_identity_secret};
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{StoredSession, TelegramClient};

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

async fn seed_mtproto(admin: &support::AdminOnlyPool, tenant: TenantId) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建 MTProto 身份失败")
}

async fn signed_in_session() -> StoredSession {
    let session = StoredSession::default();
    session.set_home_dc_id(4).await.expect("设置 home DC 失败");
    let mut dc = session
        .dc_option(4)
        .expect("读取 DC 失败")
        .expect("DC 4 缺失");
    dc.auth_key = Some([9u8; 256]);
    session
        .set_dc_option(&dc)
        .await
        .expect("写入 auth key 失败");
    session
        .cache_peer(&PeerInfo::User {
            id: 4242,
            auth: Some(PeerAuth::from_hash(11)),
            bot: Some(false),
            is_self: Some(true),
        })
        .await
        .expect("缓存登录用户失败");
    session
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn session_saved_by_a_client_restores_after_restart(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtproto-a", 9001).await;
    let identity = seed_mtproto(&admin, tenant).await;
    let kek = kek();

    assert!(
        load_session(&app, &kek, tenant, identity)
            .await
            .expect("读取空 session 失败")
            .is_none(),
        "未落库的身份应返回 None"
    );

    let seeded = StoredSession::default();
    seeded.set_home_dc_id(4).await.expect("设置 home DC 失败");
    let config = TelegramClientConfig::new(932_939)
        .expect("api_id")
        .without_catch_up();
    let client = TelegramClient::connect_with_session(config.clone(), seeded)
        .await
        .expect("建连失败");
    save_session(&app, &kek, tenant, identity, &client)
        .await
        .expect("落库失败");
    let before = client.snapshot().await.expect("快照失败");
    client.shutdown().await.expect("关闭失败");
    drop(app);

    let (app, admin) = support::pools(admin.raw().clone()).await;
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT secret_ciphertext FROM identity_secrets WHERE identity_id = $1")
            .bind(identity)
            .fetch_one(admin.raw())
            .await
            .expect("读密文失败");
    assert_ne!(stored, *before.expose(), "库里不应是明文快照");

    let restored = load_session(&app, &kek, tenant, identity)
        .await
        .expect("恢复失败")
        .expect("应有已落库 session");
    assert_eq!(restored.home_dc_id().expect("home DC"), 4);
    let client = TelegramClient::connect_with_session(config, restored)
        .await
        .expect("用恢复的 session 建连失败");
    let after = client.snapshot().await.expect("恢复后快照失败");
    client.shutdown().await.expect("关闭失败");
    assert_eq!(after.expose(), before.expose());
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn signed_in_session_keeps_auth_key_and_self_user(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtproto-b", 9002).await;
    let identity = seed_mtproto(&admin, tenant).await;
    let kek = kek();

    let snapshot = signed_in_session().await.snapshot().expect("快照失败");
    store_identity_secret(&app, &kek, tenant, identity, &snapshot)
        .await
        .expect("落库失败");

    let restored = load_session(&app, &kek, tenant, identity)
        .await
        .expect("恢复失败")
        .expect("应有 session");
    assert!(restored.is_signed_in().expect("登录态"));
    assert_eq!(
        restored
            .dc_option(4)
            .expect("DC")
            .and_then(|dc| dc.auth_key),
        Some([9u8; 256])
    );
    assert!(
        restored
            .peer(PeerId::self_user())
            .await
            .expect("self 查询")
            .is_some()
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn other_tenant_cannot_load_or_overwrite_session(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "mtproto-c", 9003).await;
    let tenant_b = support::seed_tenant(&admin, "mtproto-d", 9004).await;
    let identity = seed_mtproto(&admin, tenant_a).await;
    let kek = kek();
    let snapshot = signed_in_session().await.snapshot().expect("快照失败");
    store_identity_secret(&app, &kek, tenant_a, identity, &snapshot)
        .await
        .expect("落库失败");

    assert!(
        load_session(&app, &kek, tenant_b, identity)
            .await
            .expect("跨租户读取不应报错")
            .is_none()
    );
    let err = store_identity_secret(&app, &kek, tenant_b, identity, &snapshot)
        .await
        .expect_err("跨租户覆盖必须失败");
    assert!(matches!(err, CredentialError::IdentityNotFound));
    assert!(
        load_session(&app, &kek, tenant_a, identity)
            .await
            .expect("恢复失败")
            .expect("应有 session")
            .is_signed_in()
            .expect("登录态")
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn corrupted_plaintext_is_refused_not_partially_restored(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtproto-e", 9005).await;
    let identity = seed_mtproto(&admin, tenant).await;
    let kek = kek();
    let mut bytes = signed_in_session()
        .await
        .snapshot()
        .expect("快照失败")
        .expose()
        .clone();
    bytes.truncate(bytes.len() - 1);
    store_identity_secret(
        &app,
        &kek,
        tenant,
        identity,
        &tgm_core::secret::Secret::new(bytes),
    )
    .await
    .expect("落库失败");

    let err = load_session(&app, &kek, tenant, identity)
        .await
        .expect_err("截断的快照必须被拒");
    assert!(matches!(err, SessionStoreError::Session(_)));
}
