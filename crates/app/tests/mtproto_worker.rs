//! mtproto-worker 的租约与落库纪律（spec/08 §4.3、§五），以受限角色 app_user 连接。
//!
//! 不连 Telegram：建连之后的循环要真机，这里验的是循环依赖的每一个数据库判据。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use tgm_app::mtproto_worker::{
    IdentityExit, MtprotoTarget, MtprotoWorkerConfig, canonical_event, run_identity,
};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::{CredentialError, store_identity_secret_leased};
use tgm_db::repo::{
    LeaseGuard, acquire_lease, insert_canonical_event_leased, list_mtproto_worker_candidates,
    mark_leased_identity_unauthorized, set_leased_identity_status,
};
use tgm_tg::IncomingMessage;
use tokio::sync::watch;

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";

fn kek() -> EnvKek {
    EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败")
}

async fn seed_mtproto(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
    status: &str,
) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, $2) RETURNING id",
    )
    .bind(tenant)
    .bind(status)
    .fetch_one(admin.raw())
    .await
    .expect("创建 MTProto 身份失败")
}

async fn set_api_id(admin: &support::AdminOnlyPool, identity: IdentityId, api_id: i32) {
    sqlx::query("UPDATE identity_secrets SET api_id = $2 WHERE identity_id = $1")
        .bind(identity)
        .bind(api_id)
        .execute(admin.raw())
        .await
        .expect("写入 api_id 失败");
}

async fn take_lease(app: &PgPool, identity: IdentityId, worker: &str) -> LeaseGuard {
    let fencing_token = acquire_lease(app, identity, worker)
        .await
        .expect("抢租约失败")
        .expect("租约应可获取");
    LeaseGuard {
        identity_id: identity,
        worker_id: worker.to_owned(),
        fencing_token,
    }
}

/// 让租约立刻过期，模拟僵尸 worker 卡住超过 TTL。
async fn expire_lease(admin: &support::AdminOnlyPool, identity: IdentityId) {
    sqlx::query(
        "UPDATE account_leases SET expires_at = clock_timestamp() - interval '1 second'
          WHERE identity_id = $1",
    )
    .bind(identity)
    .execute(admin.raw())
    .await
    .expect("让租约过期失败");
}

fn incoming(message_id: i64) -> IncomingMessage {
    IncomingMessage {
        tg_chat_id: -1_000_000_001_234,
        tg_message_id: message_id,
        edited: false,
        grouped_id: None,
        sender_id: None,
        date: 1_700_000_000,
        edit_date: None,
        text: Some("关键词".to_owned()),
        media_type: None,
        file_name: None,
        file_size: None,
        mime_type: None,
    }
}

async fn status_of(admin: &support::AdminOnlyPool, identity: IdentityId) -> String {
    sqlx::query_scalar("SELECT status FROM identities WHERE id = $1")
        .bind(identity)
        .fetch_one(admin.raw())
        .await
        .expect("读身份状态失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn stale_fencing_token_cannot_write_events(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtw-a", 9101).await;
    let identity = seed_mtproto(&admin, tenant, "offline").await;

    let zombie = take_lease(&app, identity, "worker-a").await;
    let event = canonical_event(identity, &incoming(1), 0).expect("构造事件失败");
    let first = insert_canonical_event_leased(&app, &zombie, &event)
        .await
        .expect("写入失败")
        .expect("持有者应能写入");
    assert!(first.inserted);

    expire_lease(&admin, identity).await;
    let successor = take_lease(&app, identity, "worker-b").await;
    assert!(
        successor.fencing_token > zombie.fencing_token,
        "接管必须递增 token"
    );

    let late = canonical_event(identity, &incoming(2), 0).expect("构造事件失败");
    assert!(
        insert_canonical_event_leased(&app, &zombie, &late)
            .await
            .expect("写入不应报错")
            .is_none(),
        "旧 token 必须写不进去"
    );
    assert!(
        insert_canonical_event_leased(&app, &successor, &late)
            .await
            .expect("写入失败")
            .is_some()
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canonical_events WHERE receiver_identity_id = $1 AND adapter = 'mtproto'",
    )
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("计数失败");
    assert_eq!(count, 2);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn redelivered_message_is_deduplicated_but_new_edit_is_kept(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtw-b", 9102).await;
    let identity = seed_mtproto(&admin, tenant, "offline").await;
    let lease = take_lease(&app, identity, "worker-a").await;

    let created = canonical_event(identity, &incoming(5), 0).expect("构造事件失败");
    let first = insert_canonical_event_leased(&app, &lease, &created)
        .await
        .expect("写入失败")
        .expect("应写入");
    let again = insert_canonical_event_leased(&app, &lease, &created)
        .await
        .expect("写入失败")
        .expect("应命中去重");
    assert!(first.inserted);
    assert!(!again.inserted);
    assert_eq!(first.id, again.id);

    let mut edit = incoming(5);
    edit.edited = true;
    edit.edit_date = Some(1_700_000_100);
    let edited = canonical_event(identity, &edit, 1_700_000_100).expect("构造事件失败");
    assert!(
        insert_canonical_event_leased(&app, &lease, &edited)
            .await
            .expect("写入失败")
            .expect("应写入")
            .inserted
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn stale_worker_cannot_overwrite_session(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtw-c", 9103).await;
    let identity = seed_mtproto(&admin, tenant, "offline").await;
    let kek = kek();

    let zombie = take_lease(&app, identity, "worker-a").await;
    store_identity_secret_leased(&app, &kek, tenant, &zombie, &Secret::new(b"old".to_vec()))
        .await
        .expect("持有者应能落库");
    expire_lease(&admin, identity).await;
    let successor = take_lease(&app, identity, "worker-b").await;
    store_identity_secret_leased(
        &app,
        &kek,
        tenant,
        &successor,
        &Secret::new(b"new".to_vec()),
    )
    .await
    .expect("新持有者应能落库");

    let err =
        store_identity_secret_leased(&app, &kek, tenant, &zombie, &Secret::new(b"stale".to_vec()))
            .await
            .expect_err("旧 token 必须写不进去");
    assert!(matches!(err, CredentialError::LeaseLost));
    let stored = tgm_db::credentials::load_identity_secret(&app, &kek, tenant, identity)
        .await
        .expect("读取失败")
        .expect("应有凭据");
    assert_eq!(stored.expose().as_slice(), b"new");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn unauthorized_releases_lease_audits_and_is_not_reverted(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtw-d", 9104).await;
    let identity = seed_mtproto(&admin, tenant, "offline").await;
    let lease = take_lease(&app, identity, "worker-a").await;
    assert!(
        set_leased_identity_status(&app, tenant, &lease, "online")
            .await
            .expect("改状态失败")
    );

    assert!(
        mark_leased_identity_unauthorized(&app, tenant, &lease)
            .await
            .expect("标记失败")
    );
    assert_eq!(status_of(&admin, identity).await, "unauthorized");
    let audit: (String, serde_json::Value) = sqlx::query_as(
        "SELECT source, before FROM audit_logs
          WHERE tenant_id = $1 AND action = 'identity.unauthorized' AND resource_id = $2",
    )
    .bind(tenant)
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("应有审计记录");
    assert_eq!(audit.0, "system");
    assert_eq!(audit.1, serde_json::json!({ "status": "online" }));

    // 释放后立即可接管，且 token 递增（spec/08 §五 第 1 条）。
    let next = take_lease(&app, identity, "worker-b").await;
    assert!(next.fencing_token > lease.fencing_token);
    // 新持有者也不能把 unauthorized 改回 online：只有重新授权能退出这个状态。
    assert!(
        !set_leased_identity_status(&app, tenant, &next, "online")
            .await
            .expect("改状态失败")
    );
    assert_eq!(status_of(&admin, identity).await, "unauthorized");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn stale_worker_cannot_mark_unauthorized_or_change_status(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtw-e", 9105).await;
    let identity = seed_mtproto(&admin, tenant, "offline").await;
    let zombie = take_lease(&app, identity, "worker-a").await;
    expire_lease(&admin, identity).await;
    let _successor = take_lease(&app, identity, "worker-b").await;

    assert!(
        !mark_leased_identity_unauthorized(&app, tenant, &zombie)
            .await
            .expect("标记失败")
    );
    assert!(
        !set_leased_identity_status(&app, tenant, &zombie, "online")
            .await
            .expect("改状态失败")
    );
    assert_eq!(status_of(&admin, identity).await, "offline");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn candidates_need_session_and_api_id_and_stay_in_tenant(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant_a = support::seed_tenant(&admin, "mtw-f", 9106).await;
    let tenant_b = support::seed_tenant(&admin, "mtw-g", 9107).await;
    let kek = kek();
    let ready = seed_mtproto(&admin, tenant_a, "offline").await;
    let no_api_id = seed_mtproto(&admin, tenant_a, "offline").await;
    let banned = seed_mtproto(&admin, tenant_a, "banned").await;
    let _no_session = seed_mtproto(&admin, tenant_a, "offline").await;
    let other_tenant = seed_mtproto(&admin, tenant_b, "offline").await;
    for (tenant, identity) in [
        (tenant_a, ready),
        (tenant_a, no_api_id),
        (tenant_a, banned),
        (tenant_b, other_tenant),
    ] {
        tgm_db::credentials::store_identity_secret(
            &app,
            &kek,
            tenant,
            identity,
            &Secret::new(b"s".to_vec()),
        )
        .await
        .expect("落库失败");
    }
    for identity in [ready, banned, other_tenant] {
        set_api_id(&admin, identity, 932_939).await;
    }

    let candidates = list_mtproto_worker_candidates(&app, tenant_a)
        .await
        .expect("列候选失败");
    assert_eq!(candidates, vec![(ready, 932_939)]);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn identity_without_session_exits_and_releases_lease(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtw-h", 9108).await;
    let identity = seed_mtproto(&admin, tenant, "offline").await;
    let kek = kek();
    let (_tx, shutdown) = watch::channel(false);
    let target = MtprotoTarget {
        tenant,
        identity,
        api_id: 932_939,
    };

    let exit = run_identity(
        &app,
        &kek,
        target,
        "worker-a",
        MtprotoWorkerConfig::default(),
        shutdown.clone(),
    )
    .await
    .expect("运行失败");
    assert_eq!(exit, IdentityExit::NoSession);
    // 主动释放后另一个 worker 立刻能接管，不用等 30s。
    let next = take_lease(&app, identity, "worker-b").await;
    assert_eq!(next.fencing_token, 2);

    let busy = run_identity(
        &app,
        &kek,
        target,
        "worker-a",
        MtprotoWorkerConfig::default(),
        shutdown,
    )
    .await
    .expect("运行失败");
    assert_eq!(busy, IdentityExit::LeaseBusy);
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn media_text_goes_to_caption_like_bot_api(admin: PgPool) {
    let (_app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "mtw-i", 9109).await;
    let identity = seed_mtproto(&admin, tenant, "offline").await;
    let mut photo = incoming(9);
    photo.media_type = Some("PHOTO".to_owned());

    let event = canonical_event(identity, &photo, 0).expect("构造事件失败");
    assert_eq!(event.text, None);
    assert_eq!(event.caption.as_deref(), Some("关键词"));
    let text = canonical_event(identity, &incoming(9), 0).expect("构造事件失败");
    assert_eq!(text.text.as_deref(), Some("关键词"));
    assert_eq!(text.caption, None);
}
