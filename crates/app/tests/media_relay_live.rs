//! 跨身份媒体中转真机验收（docs/plan/proposals/WP-5-delivery-media.md 片 2「验收」节）。
//!
//! `#[ignore]`：要真实 Bot、扫码登录的测试用户账号、测试频道与平台 `api_id`，CI 与默认测试都不跑。
//! 凭据只经环境变量传入，本文件不打印、不落盘 Token、`api_hash` 与 session。
//!
//! 链路：Bot 1 往测试频道发一个源文件 → 用户账号（采集身份）经 `MediaReader` 重取消息、
//! 分片下载进 MinIO → Bot 1 走 `run_delivery_once` 上传并发出。覆盖短说明一条发完、
//! 超过 1024 拆成「媒体 + 文字」两条、同一 `random_id` 重发去重，以及说明长度的计数口径。
//!
//! ```text
//! TGM_PLATFORM_API_ID / TGM_PLATFORM_API_HASH   平台应用凭据
//! TGM_LIVE_BOT_TOKEN                            测试 Bot 1（测试频道管理员）
//! TGM_LIVE_TARGET_CHAT                          测试频道的 Bot API chat id（源与目标同一个频道）
//! TGM_LIVE_TARGET_USERNAME                      测试频道用户名（生成来源链接用）
//! TGM_LIVE_USER_SESSION_FILE                    用户账号 session（先跑 qr_login 的
//!                                               live_user_session_resume 缓存频道 access_hash）
//! TGM_LIVE_ALBUM_PHOTO_FILE                     相册验收用的本地 JPEG（非空且不超过 1 MiB）
//! ```
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tgm_app::delivery::{
    DeliveryMedia, DeliveryRunOutcome, DeliveryTransport, TelegramDeliveryTransport,
    build_delivery_payload, run_delivery_once,
};
use tgm_app::media_relay::{RelayOutcome, run_relay_once};
use tgm_app::pipeline::process_tenant_event;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::secret::Secret;
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, LeaseGuard, acquire_lease,
    fanout_canonical_event, insert_canonical_event, load_delivery_task_payload,
    load_media_relay_for_task,
};
use tgm_tg::client::TelegramClientConfig;
use tgm_tg::{StoredSession, TelegramClient, UploadMedia, telegram_text_len};

const FILE_NAME: &str = "tgm-relay-live.bin";
const MIME: &str = "application/octet-stream";

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("真机验收缺少环境变量 {name}"))
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos()
}

fn fresh_random_id() -> i64 {
    let mixed = now_nanos() ^ (u128::from(std::process::id()) << 64);
    i64::try_from(mixed & 0x3fff_ffff_ffff_ffff).expect("截断后必为正")
}

/// 每次运行内容不同（避免与上一轮的对象同 hash），线性同余生成，不引依赖。
fn content(seed: i64, len: usize) -> Vec<u8> {
    let mut state = seed as u64;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 56) as u8
        })
        .collect()
}

struct Setup {
    app: PgPool,
    admin: support::AdminOnlyPool,
    tenant: TenantId,
    receiver: IdentityId,
    sender: IdentityId,
}

/// pro 租户：订阅测试频道（采集身份），目标也是测试频道（Bot 1 发送），一条 COPY 规则。
async fn setup(admin: PgPool, chat: i64, username: &str) -> Setup {
    let (app, admin) = support::pools(admin).await;
    let raw = admin.raw();
    let tenant = support::seed_tenant(&admin, "relay-live", 9201).await;
    sqlx::query("UPDATE tenants SET plan = 'pro' WHERE id = $1")
        .bind(tenant)
        .execute(raw)
        .await
        .expect("设置套餐失败");
    let receiver: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'online') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(raw)
    .await
    .expect("创建采集身份失败");
    let sender = support::seed_identity(&admin, tenant).await;
    let source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, username, receiver_identity_id, enabled)
         VALUES ($1, $2, 'Relay Live', $3, $4, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(chat)
    .bind(username)
    .bind(receiver)
    .fetch_one(raw)
    .await
    .expect("创建来源失败");
    let target_ref: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, $2, $3, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(chat)
    .bind(sender)
    .fetch_one(raw)
    .await
    .expect("创建目标失败");
    let keyword_id: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'tgm-relay', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant)
    .fetch_one(raw)
    .await
    .expect("创建关键词失败");
    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rules
            (tenant_id, keyword_id, source_ref, media_type, target_ref, action, priority, version, enabled)
         VALUES ($1, $2, $3, 'ANY', $4, 'COPY', 10, 1, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(keyword_id)
    .bind(source_ref)
    .bind(target_ref)
    .fetch_one(raw)
    .await
    .expect("创建规则失败");
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, '{\"rule\": {}}'::jsonb)",
    )
    .bind(tenant)
    .bind(rule_id)
    .execute(raw)
    .await
    .expect("创建规则快照失败");
    Setup {
        app,
        admin,
        tenant,
        receiver,
        sender,
    }
}

/// 模拟采集侧落一条 canonical event，扇出、跑规则，返回投递任务 id。
async fn enqueue(setup: &Setup, chat: i64, message_id: i64, caption: &str, size: usize) -> i64 {
    let canonical = insert_canonical_event(
        &setup.app,
        &CanonicalEventInput {
            adapter: CanonicalAdapter::MtProto,
            receiver_identity_id: setup.receiver,
            tg_chat_id: chat,
            tg_message_id: message_id,
            event_type: CanonicalEventType::MessageCreated,
            edit_version: 0,
            update_id: None,
            media_group_id: None,
            sender_id: None,
            message_date: Utc::now(),
            text: None,
            caption: Some(caption.to_owned()),
            media_type: Some("DOCUMENT".to_owned()),
            content_hash: None,
            file_unique_id: None,
            file_name: Some(FILE_NAME.to_owned()),
            file_size: Some(size as i64),
            mime_type: Some(MIME.to_owned()),
            raw_file_id: None,
        },
    )
    .await
    .expect("写入事件失败")
    .id;
    let tenant_event = fanout_canonical_event(&setup.app, setup.tenant, canonical)
        .await
        .expect("扇出失败")
        .expect("应生成租户事件");
    let result = process_tenant_event(&setup.app, setup.tenant, tenant_event.id)
        .await
        .expect("规则处理失败");
    assert_eq!(result.tasks.len(), 1, "规则应命中一条");
    result.tasks[0].id
}

/// 同一任务的载荷原样再投一次：Telegram 按 `random_id` 去重，应返回同一条消息。
async fn resend(setup: &Setup, transport: &TelegramDeliveryTransport, task: i64) -> i64 {
    let record = load_delivery_task_payload(&setup.app, setup.tenant, task)
        .await
        .expect("读载荷失败")
        .expect("载荷不存在");
    let mut payload = build_delivery_payload(&record).expect("构造载荷失败");
    let relay = load_media_relay_for_task(&setup.app, setup.tenant, task)
        .await
        .expect("读中转状态失败")
        .expect("中转状态不存在");
    payload.media = Some(DeliveryMedia {
        object_key: relay.object_key.expect("对象未就绪"),
        media_type: relay.media_type.expect("缺媒体类型"),
        mime_type: relay.mime_type,
        file_name: relay.file_name,
    });
    transport.deliver(payload).await.expect("重发失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要测试 Bot、重新扫码的用户 session、测试频道与本地 JPEG 文件"]
async fn live_media_group_relay_end_to_end(admin: PgPool) {
    let api_id = required("TGM_PLATFORM_API_ID")
        .parse()
        .expect("api_id 必须是整数");
    let config = TelegramClientConfig::new(api_id).expect("api_id 无效");
    let hash = Secret::new(required("TGM_PLATFORM_API_HASH"));
    let token = Secret::new(required("TGM_LIVE_BOT_TOKEN"));
    let chat: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("频道 id 无效");
    let (bot, _) = TelegramClient::connect_bot_sender(config.clone(), &hash, &token)
        .await
        .expect("Bot 登录失败");
    drop(token);
    let bot = Arc::new(bot);
    let bytes = std::fs::read(required("TGM_LIVE_USER_SESSION_FILE")).expect("读 session 失败");
    let user = TelegramClient::connect_with_session(
        config.without_catch_up(),
        StoredSession::restore(&bytes).expect("session 无效"),
    )
    .await
    .expect("用户账号建连失败");
    drop(bytes);
    let reader = user.media_reader();
    let photo = std::fs::read(required("TGM_LIVE_ALBUM_PHOTO_FILE")).expect("读测试 JPEG 失败");
    assert!(
        !photo.is_empty() && photo.len() <= 1024 * 1024,
        "测试 JPEG 须为 1 MiB 以内的非空图片"
    );
    let caption = format!("tgm-relay album {}", fresh_random_id() % 1_000_000);
    let mut source_media = Vec::new();
    for _ in 0..3 {
        source_media.push(
            bot.prepare_album_media(
                chat,
                &mut std::io::Cursor::new(&photo),
                photo.len(),
                &UploadMedia {
                    media_type: "PHOTO".to_owned(),
                    mime_type: Some("image/jpeg".to_owned()),
                    file_name: Some("tgm-album.jpg".to_owned()),
                },
            )
            .await
            .expect("上传源相册失败"),
        );
    }
    let base = fresh_random_id();
    let source_random_ids: Vec<_> = (0..3)
        .map(|position| tgm_app::delivery::media_group_random_id(base, position))
        .collect();
    let source_ids = bot
        .send_media_group(chat, &source_random_ids, source_media, &caption)
        .await
        .expect("发送源相册失败");
    assert_eq!(source_ids.len(), 3);
    let incoming = reader
        .read_messages(chat, &source_ids)
        .await
        .expect("用户读取源相册失败");
    let group_id = incoming[0].grouped_id.expect("源消息未组成相册");
    assert!(
        incoming
            .iter()
            .all(|message| message.grouped_id == Some(group_id))
    );
    let username = required("TGM_LIVE_TARGET_USERNAME");
    let setup = setup(admin, chat, username.trim_start_matches('@')).await;
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&setup.app)
        .await
        .expect("读角色失败");
    assert_eq!(role, "app_user");
    for message in &incoming {
        let event = tgm_app::mtproto_worker::canonical_event(setup.receiver, message, 0)
            .expect("规范化成员失败");
        insert_canonical_event(&setup.app, &event)
            .await
            .expect("写源相册失败");
    }
    tgm_app::pipeline::run_fanout_once(&setup.app, 100)
        .await
        .expect("窗口内扇出失败");
    let projections: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_events WHERE tenant_id = $1")
            .bind(setup.tenant)
            .fetch_one(setup.admin.raw())
            .await
            .expect("读投影失败");
    assert_eq!(projections, 0, "窗口未结束不得扇出");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    tgm_app::pipeline::run_fanout_once(&setup.app, 100)
        .await
        .expect("聚合扇出失败");
    tgm_app::pipeline::run_rule_once(&setup.app, 100)
        .await
        .expect("规则处理失败");
    let tasks: Vec<i64> = sqlx::query_scalar("SELECT id FROM delivery_tasks WHERE tenant_id = $1")
        .bind(setup.tenant)
        .fetch_all(setup.admin.raw())
        .await
        .expect("读任务失败");
    assert_eq!(tasks.len(), 1);
    let task = tasks[0];
    let store = support::media_store("tgm-live-album").await;
    let transport = TelegramDeliveryTransport::new(Arc::clone(&bot), store.clone());
    assert_eq!(
        run_delivery_once(&setup.app, setup.tenant, setup.sender, &transport)
            .await
            .expect("等待中转失败"),
        DeliveryRunOutcome::Deferred { task_id: task }
    );
    let guard = LeaseGuard {
        identity_id: setup.receiver,
        worker_id: "album-live".to_owned(),
        fencing_token: acquire_lease(&setup.app, setup.receiver, "album-live")
            .await
            .expect("抢租约失败")
            .expect("租约不可用"),
    };
    for _ in 0..3 {
        assert!(
            tgm_db::repo::renew_lease(
                &setup.app,
                setup.receiver,
                &guard.worker_id,
                guard.fencing_token
            )
            .await
            .expect("续租失败")
        );
        assert!(matches!(
            run_relay_once(&setup.app, &guard, &reader, &store)
                .await
                .expect("成员中转失败"),
            RelayOutcome::Ready { .. }
        ));
    }
    sqlx::query("UPDATE delivery_tasks SET next_retry_at = NULL WHERE id = $1")
        .bind(task)
        .execute(setup.admin.raw())
        .await
        .expect("清退避失败");
    let outcome = run_delivery_once(&setup.app, setup.tenant, setup.sender, &transport)
        .await
        .expect("整组投递失败");
    let DeliveryRunOutcome::Done {
        telegram_message_id,
        ..
    } = outcome
    else {
        let errors = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT error_code, error_msg FROM delivery_logs WHERE task_id = $1 ORDER BY attempt",
        )
        .bind(task)
        .fetch_all(setup.admin.raw())
        .await
        .expect("读取投递失败原因失败");
        panic!("整组未完成：{outcome:?} errors={errors:?}")
    };
    let record = load_delivery_task_payload(&setup.app, setup.tenant, task)
        .await
        .expect("读快照失败")
        .expect("快照不存在");
    let mut payload = build_delivery_payload(&record).expect("载荷无效");
    for state in tgm_db::repo::load_media_group_relay_for_task(&setup.app, setup.tenant, task)
        .await
        .expect("读整组中转失败")
    {
        payload.media_group.push(DeliveryMedia {
            object_key: state.object_key.expect("对象未就绪"),
            media_type: state.media_type.expect("类型不存在"),
            mime_type: state.mime_type,
            file_name: state.file_name,
        });
    }
    let repeated = transport
        .deliver(payload.clone())
        .await
        .expect("相册重发失败");
    assert_eq!(
        repeated, telegram_message_id,
        "相同 random_id 重发必须返回原消息"
    );
    let output = reader
        .read_media_group(chat, telegram_message_id)
        .await
        .expect("读目标相册失败");
    let target_group_id = output[0].grouped_id.expect("目标消息没有相册 id");
    assert_eq!(output.len(), 3, "目标相册必须包含全部成员");
    assert_eq!(output[0].text.as_deref(), Some(caption.as_str()));
    assert_ne!(target_group_id, group_id);
    let forward_base = fresh_random_id();
    let forward_ids: Vec<_> = (0..3)
        .map(|position| tgm_app::delivery::media_group_random_id(forward_base, position))
        .collect();
    let forwarded = bot
        .forward_media_group(chat, &source_ids, chat, &forward_ids)
        .await
        .expect("整组转发失败");
    let forwarded_again = bot
        .forward_media_group(chat, &source_ids, chat, &forward_ids)
        .await
        .expect("转发重试失败");
    assert_eq!(forwarded, forwarded_again);
    let forwarded_messages = reader
        .read_messages(chat, &forwarded)
        .await
        .expect("读取转发相册失败");
    assert!(forwarded_messages[0].grouped_id.is_some());
    assert!(
        forwarded_messages
            .iter()
            .all(|message| message.grouped_id == forwarded_messages[0].grouped_id)
    );
    println!(
        "[G] source_ids={source_ids:?} source_group={group_id} tasks=1 members={} first_target={telegram_message_id} target_group={target_group_id} same_random_id={repeated} forwarded={forwarded:?}",
        payload.media_group.len()
    );
    drop(transport);
    user.shutdown().await.expect("用户客户端关闭失败");
    match Arc::try_unwrap(bot) {
        Ok(bot) => bot.shutdown().await.expect("Bot 客户端关闭失败"),
        Err(_) => panic!("Bot 客户端仍被引用"),
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要测试 Bot、测试用户 session、测试频道与平台 api_id"]
async fn live_media_relay_end_to_end(admin: PgPool) {
    let api_id: i32 = required("TGM_PLATFORM_API_ID")
        .parse()
        .expect("TGM_PLATFORM_API_ID 必须是整数");
    let api_hash = Secret::new(required("TGM_PLATFORM_API_HASH"));
    let chat: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数");
    let username = required("TGM_LIVE_TARGET_USERNAME");
    let username = username.trim_start_matches('@');
    let session_bytes =
        std::fs::read(required("TGM_LIVE_USER_SESSION_FILE")).expect("读 session 失败");
    let config = TelegramClientConfig::new(api_id).expect("api_id 无效");

    let bot_token = Secret::new(required("TGM_LIVE_BOT_TOKEN"));
    let (bot, _) = TelegramClient::connect_bot_sender(config.clone(), &api_hash, &bot_token)
        .await
        .expect("Bot 登录失败");
    drop(bot_token);
    let bot = Arc::new(bot);
    let user = TelegramClient::connect_with_session(
        config.without_catch_up(),
        StoredSession::restore(&session_bytes).expect("session 快照无效"),
    )
    .await
    .expect("用户账号建连失败");
    drop(session_bytes);
    let reader = user.media_reader();

    let store = support::media_store("tgm-live-relay").await;
    let setup = setup(admin, chat, username).await;
    let transport = TelegramDeliveryTransport::new(Arc::clone(&bot), store.clone());
    let fencing_token = acquire_lease(&setup.app, setup.receiver, "relay-live")
        .await
        .expect("抢租约失败")
        .expect("租约应可获取");
    let guard = LeaseGuard {
        identity_id: setup.receiver,
        worker_id: "relay-live".to_owned(),
        fencing_token,
    };
    let run = fresh_random_id() % 1_000_000;

    let short = format!("tgm-relay {run} short 🚀 说明一条发完");
    let long = format!("tgm-relay {run} split {}", "长".repeat(1100));
    for (case, caption) in [("short", short.as_str()), ("split", long.as_str())] {
        let bytes = content(fresh_random_id(), 1024 * 1024 + 7);
        let mut cursor = std::io::Cursor::new(bytes.clone());
        let source_id = bot
            .send_uploaded_media(
                chat,
                fresh_random_id(),
                &mut cursor,
                bytes.len(),
                &UploadMedia {
                    media_type: "DOCUMENT".to_owned(),
                    mime_type: Some(MIME.to_owned()),
                    file_name: Some(FILE_NAME.to_owned()),
                },
                &format!("tgm-relay {run} {case} 源消息"),
            )
            .await
            .expect("发源文件失败");
        let task = enqueue(&setup, chat, source_id, caption, bytes.len()).await;

        let deferred = run_delivery_once(&setup.app, setup.tenant, setup.sender, &transport)
            .await
            .expect("投递失败");
        assert_eq!(deferred, DeliveryRunOutcome::Deferred { task_id: task });

        let started = std::time::Instant::now();
        let relay = run_relay_once(&setup.app, &guard, &reader, &store)
            .await
            .expect("中转失败");
        let relay_took = started.elapsed();
        assert!(
            matches!(relay, RelayOutcome::Ready { reused: false, .. }),
            "[{case}] 中转应就绪：{relay:?}"
        );
        let (key, size): (String, i64) = sqlx::query_as(
            "SELECT a.object_key, a.file_size FROM media_relay_jobs j
               JOIN media_archive a ON a.id = j.media_archive_id
               JOIN canonical_events e ON e.id = j.canonical_event_id
              WHERE e.tg_message_id = $1",
        )
        .bind(source_id)
        .fetch_one(&setup.app)
        .await
        .expect("读档案失败");
        let (mut object, _) = store.get_stream(&key).await.expect("读对象失败");
        let mut stored = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut object, &mut stored)
            .await
            .expect("读对象内容失败");
        assert_eq!(size as usize, bytes.len());
        assert!(
            stored == bytes,
            "[{case}] MinIO 里的对象必须与源文件逐字节一致"
        );

        sqlx::query("UPDATE delivery_tasks SET next_retry_at = NULL WHERE id = $1")
            .bind(task)
            .execute(setup.admin.raw())
            .await
            .expect("清推后时间失败");
        let started = std::time::Instant::now();
        let outcome = run_delivery_once(&setup.app, setup.tenant, setup.sender, &transport)
            .await
            .expect("投递失败");
        let send_took = started.elapsed();
        let DeliveryRunOutcome::Done {
            telegram_message_id,
            ..
        } = outcome
        else {
            panic!("[{case}] 投递应完成：{outcome:?}");
        };
        let again = resend(&setup, &transport, task).await;
        println!(
            "[R-{case}] source_message_id={source_id} caption_utf16={} relay=ready took={relay_took:?} \
             byte_equal=true delivered_message_id={telegram_message_id} send_took={send_took:?} \
             same_random_id_resend_message_id={again}",
            telegram_text_len(caption),
        );
        assert_eq!(
            again, telegram_message_id,
            "[{case}] 同一 random_id 重发必须返回原消息"
        );
    }

    // 计数口径：1021 个 ASCII + 3 个 emoji = 1024 个字符、1027 个 UTF-16 码元。
    // 服务器接受 → 按字符计（本仓按 UTF-16 判定是偏保守，只会多拆不会被拒）；
    // 报 MEDIA_CAPTION_TOO_LONG → 按 UTF-16 计，与 telegram_text_len 一致。
    let probe = format!("{}😀😀😀", "a".repeat(1021));
    let mut cursor = std::io::Cursor::new(b"tgm caption probe".to_vec());
    let result = bot
        .send_uploaded_media(
            chat,
            fresh_random_id(),
            &mut cursor,
            17,
            &UploadMedia {
                media_type: "DOCUMENT".to_owned(),
                mime_type: Some("text/plain".to_owned()),
                file_name: Some("tgm-caption-probe.txt".to_owned()),
            },
            &probe,
        )
        .await;
    println!(
        "[R-probe] chars={} utf16={} result={}",
        probe.chars().count(),
        telegram_text_len(&probe),
        match &result {
            Ok(id) => format!("ok message_id={id}"),
            Err(error) => format!("err {}", error.delivery_error().code),
        }
    );
    println!(
        "[R] 人工核对：每个 tgm-relay {run} 说明只出现一次；split 的媒体不带说明，紧跟一条全文。"
    );

    let _ = user.shutdown().await;
    if let Ok(bot) = Arc::try_unwrap(bot) {
        let _ = bot.shutdown().await;
    }
}
