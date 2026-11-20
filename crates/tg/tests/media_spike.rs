//! 媒体投递可行性 spike（提案 `docs/plan/proposals/WP-5-delivery-media.md` 片 1）。
//!
//! `#[ignore]`：要真实 Bot、测试频道与平台 `api_id`，CI 与默认测试都不跑。不进生产代码，
//! 只回答片 2 动工前的事实问题。凭据只经环境变量传入，本文件不打印 Token 与 `api_hash`。
//!
//! ```text
//! M1 Bot 经 MTProto upload + messages.sendMedia（带 random_id）发媒体；同一 random_id 重发
//! M2 caption 超 1024 时服务器报什么
//! M3 Bot 从频道流式下载媒体（upload.getFile），与上传内容逐字节比对，记内存峰值与耗时
//! M4 Bot 在源频道里时 FORWARD 媒体（带 random_id）；同一 random_id 重发
//! M5 发送 Bot 不在源频道时：读源消息、FORWARD 各报什么（提案「事实问题 1」）
//! ```
//!
//! 环境变量：
//!
//! ```text
//! TGM_PLATFORM_API_ID / TGM_PLATFORM_API_HASH   平台应用凭据
//! TGM_LIVE_BOT_TOKEN                            源频道里的 Bot（频道管理员）
//! TGM_LIVE_BOT2_TOKEN                           不在源频道里的 Bot（M5）
//! TGM_LIVE_TARGET_CHAT                          测试频道的 Bot API chat id，如 -100...
//! TGM_LIVE_TARGET_USERNAME                      可选，公开源频道用户名（M5b）
//! TGM_SPIKE_MEDIA_MB                            M1/M3 的文件大小（MB），默认 50（pro 档上限）
//! ```
#![allow(clippy::expect_used, clippy::panic)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use grammers_client::media::Uploaded;
use grammers_client::session::types::{PeerId, PeerRef};
use grammers_client::{Client, InvocationError, SenderPool, tl};
use tgm_tg::StoredSession;
use tokio::io::{AsyncRead, ReadBuf};

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("spike 缺少环境变量 {name}"))
}

fn fresh_random_id() -> i64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos();
    let mixed = nanos ^ (u128::from(std::process::id()) << 64);
    i64::try_from(mixed & 0x3fff_ffff_ffff_ffff).expect("截断后必为正")
}

/// 进程内存（KiB）：当前 RSS 与峰值 RSS。
fn memory_kib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

/// 确定性伪随机字节流（xorshift64），边读边生成，不在内存里放整份文件。
/// 同一种子再生成一遍就能逐字节比对下载结果。
struct Pattern {
    state: u64,
    remaining: usize,
}

impl Pattern {
    fn new(seed: u64, len: usize) -> Self {
        Self {
            state: seed | 1,
            remaining: len,
        }
    }

    fn next_byte(&mut self) -> u8 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state.to_le_bytes()[0]
    }
}

impl AsyncRead for Pattern {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let take = buf.remaining().min(self.remaining);
        for _ in 0..take {
            let byte = self.next_byte();
            buf.put_slice(&[byte]);
        }
        self.remaining -= take;
        Poll::Ready(Ok(()))
    }
}

struct Bot {
    client: Client,
    handle: grammers_client::sender::SenderPoolFatHandle,
    runner: tokio::task::JoinHandle<()>,
}

impl Bot {
    async fn sign_in(token_var: &str) -> Self {
        let api_id: i32 = required("TGM_PLATFORM_API_ID")
            .parse()
            .expect("TGM_PLATFORM_API_ID 必须是整数");
        let session = Arc::new(StoredSession::default());
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(Arc::clone(&session), api_id);
        drop(updates);
        let client = Client::new(handle.clone());
        let runner = tokio::spawn(async move { runner.run().await });
        client
            .bot_sign_in(&required(token_var), &required("TGM_PLATFORM_API_HASH"))
            .await
            .expect("bot_sign_in 失败");
        Self {
            client,
            handle,
            runner,
        }
    }

    async fn shutdown(self) {
        self.handle.quit();
        let _ = self.runner.await;
    }
}

fn channel_ref(chat: i64) -> PeerRef {
    PeerId::from_bot_api_dialog_id(chat)
        .expect("TGM_LIVE_TARGET_CHAT 不是合法 chat id")
        .to_ambient_ref()
}

fn rpc_name(error: &InvocationError) -> String {
    match error {
        InvocationError::Rpc(rpc) => format!("{} {}", rpc.code, rpc.name),
        other => format!("non-rpc: {other}"),
    }
}

fn message_id_for_random_id(updates: &tl::enums::Updates, random_id: i64) -> Option<i32> {
    let raw = match updates {
        tl::enums::Updates::Updates(updates) => &updates.updates,
        tl::enums::Updates::Combined(updates) => &updates.updates,
        tl::enums::Updates::UpdateShortSentMessage(update) => return Some(update.id),
        _ => return None,
    };
    raw.iter().find_map(|update| match update {
        tl::enums::Update::MessageId(update) if update.random_id == random_id => Some(update.id),
        _ => None,
    })
}

fn uploaded_document(file: Uploaded, name: &str) -> tl::enums::InputMedia {
    tl::types::InputMediaUploadedDocument {
        nosound_video: false,
        force_file: true,
        spoiler: false,
        file: file.raw,
        thumb: None,
        mime_type: "application/octet-stream".to_owned(),
        attributes: vec![
            tl::types::DocumentAttributeFilename {
                file_name: name.to_owned(),
            }
            .into(),
        ],
        stickers: None,
        ttl_seconds: None,
        video_cover: None,
        video_timestamp: None,
    }
    .into()
}

async fn send_media(
    client: &Client,
    peer: PeerRef,
    media: tl::enums::InputMedia,
    caption: &str,
    random_id: i64,
) -> Result<tl::enums::Updates, InvocationError> {
    client
        .invoke(&tl::functions::messages::SendMedia {
            silent: false,
            background: false,
            clear_draft: false,
            peer: peer.into(),
            reply_to: None,
            media,
            message: caption.to_owned(),
            random_id,
            reply_markup: None,
            entities: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_floodskip: false,
            allow_paid_stars: None,
            suggested_post: None,
        })
        .await
}

async fn forward(
    client: &Client,
    from: PeerRef,
    message_id: i32,
    to: PeerRef,
    random_id: i64,
) -> Result<tl::enums::Updates, InvocationError> {
    client
        .invoke(&tl::functions::messages::ForwardMessages {
            silent: false,
            background: false,
            with_my_score: false,
            drop_author: false,
            drop_media_captions: false,
            noforwards: false,
            allow_paid_floodskip: false,
            from_peer: from.into(),
            id: vec![message_id],
            random_id: vec![random_id],
            to_peer: to.into(),
            top_msg_id: None,
            reply_to: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            video_timestamp: None,
            allow_paid_stars: None,
            suggested_post: None,
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "真机：需要两个测试 Bot、测试频道与平台 api_id"]
async fn live_media_spike() {
    let chat: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数");
    let megabytes: usize = std::env::var("TGM_SPIKE_MEDIA_MB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50);
    let size = megabytes * 1024 * 1024;
    let seed = u64::try_from(fresh_random_id()).expect("random_id 为正");

    let bot = Bot::sign_in("TGM_LIVE_BOT_TOKEN").await;
    let peer = bot
        .client
        .resolve_peer(channel_ref(chat))
        .await
        .expect("Bot 1 解析测试频道失败")
        .to_ref()
        .await
        .expect("取频道引用失败")
        .expect("频道引用为空");
    let (rss_before, _) = memory_kib();

    // ── M1 上传 + sendMedia，同一 random_id 重发 ──
    let started = Instant::now();
    let uploaded = bot
        .client
        .upload_stream(
            &mut Pattern::new(seed, size),
            size,
            format!("tgm-spike-{megabytes}mb.bin"),
        )
        .await
        .expect("[M1] upload_stream 失败");
    let upload_took = started.elapsed();
    let (_, hwm_after_upload) = memory_kib();
    let random_id = fresh_random_id();
    let name = format!("tgm-spike-{megabytes}mb.bin");
    let updates = send_media(
        &bot.client,
        peer,
        uploaded_document(uploaded.clone(), &name),
        "tgm-media-spike M1：Bot 经 MTProto 上传发送",
        random_id,
    )
    .await
    .expect("[M1] sendMedia 失败");
    let source_id = message_id_for_random_id(&updates, random_id).expect("[M1] 未返回 message id");
    let resend = send_media(
        &bot.client,
        peer,
        uploaded_document(uploaded, &name),
        "tgm-media-spike M1：Bot 经 MTProto 上传发送",
        random_id,
    )
    .await;
    let resend_result = match &resend {
        Ok(updates) => format!(
            "ok message_id={:?}",
            message_id_for_random_id(updates, random_id)
        ),
        Err(error) => rpc_name(error),
    };
    println!(
        "[M1] size={megabytes}MB upload={upload_took:?} message_id={source_id} \
         same_random_id_resend={resend_result} rss_before={rss_before}KiB hwm_after_upload={hwm_after_upload}KiB"
    );

    // ── M2 caption 超 1024 ──
    let small = bot
        .client
        .upload_stream(
            &mut Pattern::new(seed ^ 0xff, 4096),
            4096,
            "tgm-spike-caption.bin".to_owned(),
        )
        .await
        .expect("[M2] 上传失败");
    let long_caption = "长".repeat(1100);
    let caption_result = send_media(
        &bot.client,
        peer,
        uploaded_document(small.clone(), "tgm-spike-caption.bin"),
        &long_caption,
        fresh_random_id(),
    )
    .await;
    let exact_caption = "界".repeat(1024);
    let exact_result = send_media(
        &bot.client,
        peer,
        uploaded_document(small, "tgm-spike-caption.bin"),
        &exact_caption,
        fresh_random_id(),
    )
    .await;
    println!(
        "[M2] caption_1100_chars={} caption_1024_chars={}",
        caption_result
            .as_ref()
            .map(|_| "ok".to_owned())
            .unwrap_or_else(rpc_name),
        exact_result
            .as_ref()
            .map(|_| "ok".to_owned())
            .unwrap_or_else(rpc_name),
    );

    // ── M3 流式下载并逐字节比对 ──
    let message = bot
        .client
        .get_messages_by_id(peer, &[source_id])
        .await
        .expect("[M3] 读源消息失败")
        .into_iter()
        .next()
        .flatten()
        .expect("[M3] 源消息不存在");
    let media = message.media().expect("[M3] 源消息没有媒体");
    let started = Instant::now();
    let mut download = bot.client.iter_download(&media);
    let mut expected = Pattern::new(seed, size);
    let mut received = 0_usize;
    let mut chunks = 0_usize;
    while let Some(chunk) = download.next().await.expect("[M3] 下载分片失败") {
        for byte in &chunk {
            assert_eq!(
                *byte,
                expected.next_byte(),
                "[M3] 第 {received} 字节与上传内容不一致"
            );
            received += 1;
        }
        chunks += 1;
    }
    let download_took = started.elapsed();
    let (_, hwm_after_download) = memory_kib();
    assert_eq!(received, size, "[M3] 下载字节数不对");
    println!(
        "[M3] downloaded={received}B chunks={chunks} took={download_took:?} byte_equal=true \
         hwm_after_download={hwm_after_download}KiB"
    );

    // ── M4 源频道里的 Bot FORWARD 媒体，同一 random_id 重发 ──
    let forward_id = fresh_random_id();
    let first = forward(&bot.client, peer, source_id, peer, forward_id).await;
    let second = forward(&bot.client, peer, source_id, peer, forward_id).await;
    let describe = |result: &Result<tl::enums::Updates, InvocationError>| match result {
        Ok(updates) => format!(
            "ok message_id={:?}",
            message_id_for_random_id(updates, forward_id)
        ),
        Err(error) => rpc_name(error),
    };
    println!(
        "[M4] forward={} same_random_id_forward={}",
        describe(&first),
        describe(&second)
    );
    bot.shutdown().await;

    // ── M5 不在源频道里的 Bot ──
    let outsider = Bot::sign_in("TGM_LIVE_BOT2_TOKEN").await;
    let ambient = channel_ref(chat);
    let resolved = outsider.client.resolve_peer(ambient).await;
    let read = outsider
        .client
        .get_messages_by_id(ambient, &[source_id])
        .await;
    let outsider_forward = forward(
        &outsider.client,
        ambient,
        source_id,
        ambient,
        fresh_random_id(),
    )
    .await;
    println!(
        "[M5] resolve_peer={} read_source={} forward={}",
        resolved
            .as_ref()
            .map(|_| "ok".to_owned())
            .unwrap_or_else(rpc_name),
        read.as_ref()
            .map(|messages| format!("ok found={}", messages.iter().flatten().count()))
            .unwrap_or_else(rpc_name),
        outsider_forward
            .as_ref()
            .map(|_| "ok".to_owned())
            .unwrap_or_else(rpc_name),
    );

    // ── M5b 公开源频道：经用户名解析拿真实 access_hash 后再读、再 FORWARD ──
    if let Some(username) = std::env::var("TGM_LIVE_TARGET_USERNAME")
        .ok()
        .filter(|value| !value.is_empty())
    {
        let username = username.trim_start_matches('@');
        match outsider.client.resolve_username(username).await {
            Ok(Some(found)) => {
                let full = found
                    .to_ref()
                    .await
                    .expect("[M5b] 取频道引用失败")
                    .expect("[M5b] 频道引用为空");
                let read = outsider.client.get_messages_by_id(full, &[source_id]).await;
                let forwarded =
                    forward(&outsider.client, full, source_id, full, fresh_random_id()).await;
                println!(
                    "[M5b] resolve_username=ok read_source={} forward_into_source={}",
                    read.as_ref()
                        .map(|messages| format!("ok found={}", messages.iter().flatten().count()))
                        .unwrap_or_else(rpc_name),
                    forwarded
                        .as_ref()
                        .map(|_| "ok".to_owned())
                        .unwrap_or_else(rpc_name),
                );
            }
            Ok(None) => println!("[M5b] resolve_username=none"),
            Err(error) => println!("[M5b] resolve_username={}", rpc_name(&error)),
        }
    }
    outsider.shutdown().await;
}
