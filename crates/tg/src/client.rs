//! `grammers` 的最小接入边界。
//!
//! session 用 [`StoredSession`]：`connect` 起一个空 session（P0 spike 与无凭据验证），
//! `connect_with_session` 接解密后恢复的 session；[`TelegramClient::snapshot`]
//! 产出明文快照，由调用方交给凭据层信封加密落库（spec/08 §1.3）。

use std::sync::Arc;

use grammers_client::client::{UpdateStream, UpdatesConfiguration};
use grammers_client::session::types::{PeerId, PeerRef};
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool, parsers, tl};
use tgm_core::secret::Secret;
use thiserror::Error;

use crate::session::{StoredSession, StoredSessionError};

#[derive(Debug, Error)]
pub enum TelegramClientError {
    #[error("Telegram api_id 必须为正整数")]
    InvalidApiId,
    #[error("无法建立 Telegram 更新流: {0}")]
    UpdateStream(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("无法解析 Telegram peer: {0}")]
    PeerResolution(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("无法同步 Telegram 更新状态: {0}")]
    SessionSync(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("Telegram 调用失败: {0}")]
    Invocation(#[source] grammers_client::InvocationError),
    #[error("无效 Telegram peer id: {0}")]
    InvalidPeer(i64),
    #[error("Telegram 未返回发送后的消息")]
    MessageNotReturned,
    #[error("Telegram random_id 必须为正整数: {0}")]
    InvalidRandomId(i64),
    #[error("当前适配器尚未支持媒体 COPY")]
    MediaCopyUnavailable,
    #[error("当前适配器尚未支持媒体 RESEND")]
    MediaResendUnavailable,
    #[error("仅发送客户端不接收更新")]
    SendOnly,
    #[error("session 快照失败: {0}")]
    Snapshot(#[source] StoredSessionError),
    #[error("Telegram runner 关闭失败: {0}")]
    Runner(#[source] tokio::task::JoinError),
}

#[derive(Debug, Clone)]
pub struct TelegramClientConfig {
    api_id: i32,
    catch_up: bool,
}

impl TelegramClientConfig {
    pub fn new(api_id: i32) -> Result<Self, TelegramClientError> {
        if api_id <= 0 {
            return Err(TelegramClientError::InvalidApiId);
        }

        Ok(Self {
            api_id,
            catch_up: true,
        })
    }

    pub fn without_catch_up(mut self) -> Self {
        self.catch_up = false;
        self
    }
}

pub struct TelegramClient {
    client: Client,
    session: Arc<StoredSession>,
    handle: grammers_client::sender::SenderPoolHandle,
    /// `None` = 仅发送客户端（delivery）：更新通道的接收端已丢弃，
    /// runner 推送的更新直接丢掉，不在无界队列里堆积。
    updates: Option<UpdateStream>,
    runner_task: tokio::task::JoinHandle<()>,
}

impl TelegramClient {
    pub async fn connect(config: TelegramClientConfig) -> Result<Self, TelegramClientError> {
        Self::connect_with_session(config, StoredSession::default()).await
    }

    /// 用已恢复的 session 建连（通常来自 `StoredSession::restore`）。
    pub async fn connect_with_session(
        config: TelegramClientConfig,
        session: StoredSession,
    ) -> Result<Self, TelegramClientError> {
        let session = Arc::new(session);
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(Arc::clone(&session), config.api_id);
        let client = Client::new(handle.clone());
        let runner_task = tokio::spawn(async move {
            runner.run().await;
        });
        let updates = match client
            .stream_updates(
                updates,
                UpdatesConfiguration {
                    catch_up: config.catch_up,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(updates) => updates,
            Err(error) => {
                handle.quit();
                let _ = runner_task.await;
                return Err(TelegramClientError::UpdateStream(error));
            }
        };

        Ok(Self {
            client,
            session,
            handle: handle.thin,
            updates: Some(updates),
            runner_task,
        })
    }

    /// 以 Bot Token 经 MTProto 登录的仅发送客户端（ADR-0033）。
    ///
    /// session 只在内存里，不落库：Bot 凭 Token 随时可重新登录，持久化它换不来恢复能力。
    /// `api_id` / `api_hash` 是平台自己的；返回 Bot 的 user id。
    pub async fn connect_bot_sender(
        config: TelegramClientConfig,
        api_hash: &Secret<String>,
        bot_token: &Secret<String>,
    ) -> Result<(Self, i64), TelegramClientError> {
        let session = Arc::new(StoredSession::default());
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(Arc::clone(&session), config.api_id);
        drop(updates);
        let client = Client::new(handle.clone());
        let runner_task = tokio::spawn(async move {
            runner.run().await;
        });
        let me = match client
            .bot_sign_in(bot_token.expose(), api_hash.expose())
            .await
        {
            Ok(me) => me,
            Err(error) => {
                handle.quit();
                let _ = runner_task.await;
                return Err(TelegramClientError::Invocation(error));
            }
        };
        Ok((
            Self {
                client,
                session,
                handle: handle.thin,
                updates: None,
                runner_task,
            },
            me.id().bare_id().unwrap_or_default(),
        ))
    }

    pub async fn next_update(&mut self) -> Result<Update, TelegramClientError> {
        self.updates
            .as_mut()
            .ok_or(TelegramClientError::SendOnly)?
            .next()
            .await
            .map_err(TelegramClientError::Invocation)
    }

    /// 等下一条别人发的新消息或编辑。自己发出的消息与其他更新类型直接跳过（spec/08 §1.4）。
    pub async fn next_incoming(&mut self) -> Result<IncomingMessage, TelegramClientError> {
        loop {
            let (raw, edited) = match self.next_update().await? {
                Update::NewMessage(message) => (message.into_inner().raw, false),
                Update::MessageEdited(message) => (message.into_inner().raw, true),
                _ => continue,
            };
            if let Some(incoming) = incoming_from_raw(&raw, edited) {
                return Ok(incoming);
            }
        }
    }

    pub async fn copy_text(
        &self,
        target_tg_chat_id: i64,
        telegram_random_id: i64,
        html_text: &str,
    ) -> Result<i64, TelegramClientError> {
        validate_random_id(telegram_random_id)?;
        let target = self.resolve_peer_ref(target_tg_chat_id).await?;
        let (message, entities) = parsers::parse_html_message(html_text);
        let updates = self
            .client
            .invoke(&tl::functions::messages::SendMessage {
                no_webpage: true,
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                allow_paid_floodskip: false,
                peer: target.into(),
                reply_to: None,
                message,
                random_id: telegram_random_id,
                reply_markup: None,
                entities: (!entities.is_empty()).then_some(entities),
                schedule_date: None,
                schedule_repeat_period: None,
                send_as: None,
                quick_reply_shortcut: None,
                effect: None,
                allow_paid_stars: None,
                suggested_post: None,
                rich_message: None,
            })
            .await
            .map_err(TelegramClientError::Invocation)?;
        message_id_for_random_id(&updates, telegram_random_id)
            .ok_or(TelegramClientError::MessageNotReturned)
    }

    pub async fn forward_message(
        &self,
        source_tg_chat_id: i64,
        source_tg_message_id: i64,
        target_tg_chat_id: i64,
        telegram_random_id: i64,
    ) -> Result<i64, TelegramClientError> {
        validate_random_id(telegram_random_id)?;
        let source = self.resolve_peer_ref(source_tg_chat_id).await?;
        let target = self.resolve_peer_ref(target_tg_chat_id).await?;
        let source_message_id = i32::try_from(source_tg_message_id)
            .map_err(|_| TelegramClientError::MessageNotReturned)?;
        let updates = self
            .client
            .invoke(&tl::functions::messages::ForwardMessages {
                silent: false,
                background: false,
                with_my_score: false,
                drop_author: false,
                drop_media_captions: false,
                noforwards: false,
                allow_paid_floodskip: false,
                from_peer: source.into(),
                id: vec![source_message_id],
                random_id: vec![telegram_random_id],
                to_peer: target.into(),
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
            .map_err(TelegramClientError::Invocation)?;
        message_id_for_random_id(&updates, telegram_random_id)
            .ok_or(TelegramClientError::MessageNotReturned)
    }

    pub async fn deliver_text_or_forward(
        &self,
        action: TelegramDeliveryAction,
        source_tg_chat_id: i64,
        source_tg_message_id: i64,
        target_tg_chat_id: i64,
        telegram_random_id: i64,
        html_text: &str,
        media_type: Option<&str>,
    ) -> Result<i64, TelegramClientError> {
        match action {
            TelegramDeliveryAction::Copy => {
                if media_type.is_some_and(|value| value != "TEXT") {
                    return Err(TelegramClientError::MediaCopyUnavailable);
                }
                self.copy_text(target_tg_chat_id, telegram_random_id, html_text)
                    .await
            }
            TelegramDeliveryAction::Forward => {
                self.forward_message(
                    source_tg_chat_id,
                    source_tg_message_id,
                    target_tg_chat_id,
                    telegram_random_id,
                )
                .await
            }
            TelegramDeliveryAction::Resend => {
                if media_type.is_some_and(|value| value != "TEXT") {
                    return Err(TelegramClientError::MediaResendUnavailable);
                }
                self.copy_text(target_tg_chat_id, telegram_random_id, html_text)
                    .await
            }
        }
    }

    /// 把更新流状态同步进 session 后取快照。输出含 auth key，只能交给信封加密落库。
    pub async fn snapshot(&self) -> Result<Secret<Vec<u8>>, TelegramClientError> {
        self.updates
            .as_ref()
            .ok_or(TelegramClientError::SendOnly)?
            .sync_update_state()
            .await
            .map_err(TelegramClientError::SessionSync)?;
        self.session
            .snapshot()
            .map_err(TelegramClientError::Snapshot)
    }

    pub async fn shutdown(self) -> Result<(), TelegramClientError> {
        let sync_result = match &self.updates {
            Some(updates) => updates.sync_update_state().await,
            None => Ok(()),
        };
        self.handle.quit();
        let runner_result = self.runner_task.await;

        if let Err(error) = sync_result {
            return Err(TelegramClientError::SessionSync(error));
        }

        runner_result.map_err(TelegramClientError::Runner)
    }

    async fn resolve_peer_ref(&self, tg_chat_id: i64) -> Result<PeerRef, TelegramClientError> {
        let peer = peer_ref(tg_chat_id)?;
        let resolved = self
            .client
            .resolve_peer(peer)
            .await
            .map_err(TelegramClientError::Invocation)?;
        resolved
            .to_ref()
            .await
            .map_err(TelegramClientError::PeerResolution)?
            .ok_or(TelegramClientError::InvalidPeer(tg_chat_id))
    }
}

/// 采集到的一条消息，只含落 `canonical_events` 需要的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMessage {
    /// Bot API 形式的对话 id（频道为 `-100…`），与 `source_subscriptions.tg_chat_id` 同一口径。
    pub tg_chat_id: i64,
    pub tg_message_id: i64,
    pub edited: bool,
    pub grouped_id: Option<i64>,
    pub sender_id: Option<i64>,
    /// Unix 秒。
    pub date: i64,
    /// 编辑时间（Unix 秒），只有编辑事件有。
    pub edit_date: Option<i64>,
    pub text: Option<String>,
    pub media_type: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
    pub mime_type: Option<String>,
}

/// 只认普通消息：空消息、服务消息与自己发出的消息返回 `None`。
fn incoming_from_raw(raw: &tl::enums::Message, edited: bool) -> Option<IncomingMessage> {
    let tl::enums::Message::Message(message) = raw else {
        return None;
    };
    if message.out {
        return None;
    }
    let tg_chat_id = PeerId::from(&message.peer_id).bot_api_dialog_id()?;
    let sender_id = message
        .from_id
        .as_ref()
        .and_then(|peer| PeerId::from(peer).bot_api_dialog_id());
    let media = message.media.as_ref().and_then(media_fields);
    let (media_type, file_name, file_size, mime_type) = match media {
        Some(fields) => (
            Some(fields.media_type.to_owned()),
            fields.file_name,
            fields.file_size,
            fields.mime_type,
        ),
        None => (None, None, None, None),
    };
    // 有媒体时 message 字段是说明文字，与 Bot API 的 caption 对应；这里统一放 text，
    // 由调用方按 media_type 决定落 text 还是 caption。
    Some(IncomingMessage {
        tg_chat_id,
        tg_message_id: i64::from(message.id),
        edited,
        grouped_id: message.grouped_id,
        sender_id,
        date: i64::from(message.date),
        edit_date: message.edit_date.map(i64::from),
        text: (!message.message.is_empty()).then(|| message.message.clone()),
        media_type,
        file_name,
        file_size,
        mime_type,
    })
}

struct MediaFields {
    media_type: &'static str,
    file_name: Option<String>,
    file_size: Option<i64>,
    mime_type: Option<String>,
}

/// 媒体类型名与 Bot API 采集路径一致（`bot_gateway` 的 PHOTO / VIDEO / …），
/// 规则的 `media_type` 条件才能对两种来源一视同仁。不认识的媒体返回 `None`，按纯文本处理。
fn media_fields(media: &tl::enums::MessageMedia) -> Option<MediaFields> {
    match media {
        tl::enums::MessageMedia::Photo(_) => Some(MediaFields {
            media_type: "PHOTO",
            file_name: None,
            file_size: None,
            mime_type: None,
        }),
        tl::enums::MessageMedia::Document(document) => {
            let Some(tl::enums::Document::Document(document)) = document.document.as_ref() else {
                return None;
            };
            let mut media_type = "DOCUMENT";
            let mut file_name = None;
            for attribute in &document.attributes {
                match attribute {
                    tl::enums::DocumentAttribute::Sticker(_) => media_type = "STICKER",
                    tl::enums::DocumentAttribute::Animated => media_type = "ANIMATION",
                    tl::enums::DocumentAttribute::Video(video) if media_type == "DOCUMENT" => {
                        media_type = if video.round_message {
                            "VIDEO_NOTE"
                        } else {
                            "VIDEO"
                        };
                    }
                    tl::enums::DocumentAttribute::Audio(audio) if media_type == "DOCUMENT" => {
                        media_type = if audio.voice { "VOICE" } else { "AUDIO" };
                    }
                    tl::enums::DocumentAttribute::Filename(name) => {
                        file_name = Some(name.file_name.clone());
                    }
                    _ => {}
                }
            }
            Some(MediaFields {
                media_type,
                file_name,
                file_size: Some(document.size),
                mime_type: Some(document.mime_type.clone()),
            })
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramDeliveryAction {
    Copy,
    Forward,
    Resend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramDeliveryError {
    pub code: String,
    pub message: Option<String>,
    pub flood_wait_seconds: Option<i64>,
}

impl TelegramClientError {
    /// Telegram 判定授权已失效（spec/08 §五 的 `unauthorized`）。这类错误不得自动重试。
    pub fn is_unauthorized(&self) -> bool {
        matches!(
            self,
            Self::Invocation(grammers_client::InvocationError::Rpc(error))
                if error.code == 401
                    || error.is("AUTH_KEY_UNREGISTERED")
                    || error.is("SESSION_REVOKED")
                    || error.is("USER_DEACTIVATED*")
        )
    }

    /// `auth.importBotAuthorization` 拒绝了 Bot Token（吊销、重置或格式不对）。
    pub fn is_bot_token_rejected(&self) -> bool {
        matches!(
            self,
            Self::Invocation(grammers_client::InvocationError::Rpc(error))
                if error.is("ACCESS_TOKEN_INVALID") || error.is("ACCESS_TOKEN_EXPIRED")
        )
    }

    pub fn delivery_error(&self) -> TelegramDeliveryError {
        match self {
            Self::Invocation(grammers_client::InvocationError::Rpc(error)) => {
                TelegramDeliveryError {
                    code: error.name.clone(),
                    message: Some(error.to_string()),
                    flood_wait_seconds: (error.name == "FLOOD_WAIT")
                        .then(|| error.value.map(i64::from))
                        .flatten(),
                }
            }
            Self::InvalidPeer(_) => TelegramDeliveryError {
                code: "PEER_ID_INVALID".to_owned(),
                message: Some(self.to_string()),
                flood_wait_seconds: None,
            },
            Self::MediaCopyUnavailable => TelegramDeliveryError {
                code: "MEDIA_COPY_UNAVAILABLE".to_owned(),
                message: Some(self.to_string()),
                flood_wait_seconds: None,
            },
            Self::MediaResendUnavailable => TelegramDeliveryError {
                code: "MEDIA_RESEND_UNAVAILABLE".to_owned(),
                message: Some(self.to_string()),
                flood_wait_seconds: None,
            },
            Self::MessageNotReturned => TelegramDeliveryError {
                code: "MESSAGE_NOT_RETURNED".to_owned(),
                message: Some(self.to_string()),
                flood_wait_seconds: None,
            },
            Self::InvalidRandomId(_) => TelegramDeliveryError {
                code: "INVALID_RANDOM_ID".to_owned(),
                message: Some(self.to_string()),
                flood_wait_seconds: None,
            },
            _ => TelegramDeliveryError {
                code: "TELEGRAM_CLIENT_ERROR".to_owned(),
                message: Some(self.to_string()),
                flood_wait_seconds: None,
            },
        }
    }
}

fn peer_ref(tg_chat_id: i64) -> Result<PeerRef, TelegramClientError> {
    let peer = PeerId::from_bot_api_dialog_id(tg_chat_id)
        .ok_or(TelegramClientError::InvalidPeer(tg_chat_id))?;
    Ok(peer.to_ambient_ref())
}

fn validate_random_id(random_id: i64) -> Result<(), TelegramClientError> {
    if random_id <= 0 {
        return Err(TelegramClientError::InvalidRandomId(random_id));
    }
    Ok(())
}

fn message_id_for_random_id(updates: &tl::enums::Updates, random_id: i64) -> Option<i64> {
    let raw_updates = match updates {
        tl::enums::Updates::Updates(updates) => &updates.updates,
        tl::enums::Updates::Combined(updates) => &updates.updates,
        tl::enums::Updates::UpdateShortSentMessage(update) => return Some(i64::from(update.id)),
        _ => return None,
    };
    raw_updates.iter().find_map(|update| match update {
        tl::enums::Update::MessageId(update) if update.random_id == random_id => {
            Some(i64::from(update.id))
        }
        _ => None,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        TelegramClient, TelegramClientConfig, TelegramClientError, incoming_from_raw,
        message_id_for_random_id, peer_ref, validate_random_id,
    };
    use grammers_client::tl;

    #[test]
    fn rejects_invalid_api_id() {
        let result = TelegramClientConfig::new(0);

        assert!(matches!(result, Err(TelegramClientError::InvalidApiId)));
    }

    #[test]
    fn rejects_invalid_peer_ids_before_network_calls() {
        let result = peer_ref(0);

        assert!(matches!(result, Err(TelegramClientError::InvalidPeer(0))));
    }

    #[test]
    fn rejects_non_positive_random_ids_before_network_calls() {
        assert!(matches!(
            validate_random_id(0),
            Err(TelegramClientError::InvalidRandomId(0))
        ));
    }

    #[test]
    fn maps_only_the_exact_random_id_update() {
        let updates = tl::enums::Updates::Updates(tl::types::Updates {
            updates: vec![
                tl::enums::Update::MessageId(tl::types::UpdateMessageId {
                    id: 101,
                    random_id: 7,
                }),
                tl::enums::Update::MessageId(tl::types::UpdateMessageId {
                    id: 202,
                    random_id: 8,
                }),
            ],
            users: Vec::new(),
            chats: Vec::new(),
            date: 0,
            seq: 0,
        });

        assert_eq!(message_id_for_random_id(&updates, 8), Some(202));
        assert_eq!(message_id_for_random_id(&updates, 9), None);
    }

    #[test]
    fn maps_short_sent_message_from_the_current_request() {
        let updates =
            tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                out: true,
                id: 303,
                pts: 1,
                pts_count: 1,
                date: 0,
                media: None,
                entities: None,
                ttl_period: None,
            });

        assert_eq!(message_id_for_random_id(&updates, 8), Some(303));
        assert_eq!(
            message_id_for_random_id(&tl::enums::Updates::TooLong, 8),
            None
        );
    }

    fn raw_message(
        out: bool,
        peer: tl::enums::Peer,
        media: Option<tl::enums::MessageMedia>,
    ) -> tl::enums::Message {
        tl::enums::Message::Message(tl::types::Message {
            out,
            mentioned: false,
            media_unread: false,
            silent: false,
            post: true,
            from_scheduled: false,
            legacy: false,
            edit_hide: false,
            pinned: false,
            noforwards: false,
            invert_media: false,
            offline: false,
            video_processing_pending: false,
            paid_suggested_post_stars: false,
            paid_suggested_post_ton: false,
            id: 77,
            from_id: None,
            from_boosts_applied: None,
            from_rank: None,
            peer_id: peer,
            saved_peer_id: None,
            fwd_from: None,
            via_bot_id: None,
            via_business_bot_id: None,
            guestchat_via_from: None,
            reply_to: None,
            date: 1_700_000_000,
            message: "关键词 命中".to_owned(),
            media,
            reply_markup: None,
            entities: None,
            views: None,
            forwards: None,
            replies: None,
            edit_date: None,
            post_author: None,
            grouped_id: Some(9),
            reactions: None,
            restriction_reason: None,
            ttl_period: None,
            quick_reply_shortcut_id: None,
            effect: None,
            factcheck: None,
            report_delivery_until_date: None,
            paid_message_stars: None,
            suggested_post: None,
            schedule_repeat_period: None,
            summary_from_language: None,
            rich_message: None,
        })
    }

    fn channel(id: i64) -> tl::enums::Peer {
        tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: id })
    }

    #[test]
    fn incoming_channel_message_uses_bot_api_chat_id() {
        let incoming = incoming_from_raw(&raw_message(false, channel(1234), None), false)
            .expect("频道消息应被采集");

        assert_eq!(incoming.tg_chat_id, -1_000_000_001_234);
        assert_eq!(incoming.tg_message_id, 77);
        assert_eq!(incoming.grouped_id, Some(9));
        assert_eq!(incoming.text.as_deref(), Some("关键词 命中"));
        assert_eq!(incoming.media_type, None);
        assert!(!incoming.edited);
    }

    #[test]
    fn outgoing_and_service_messages_are_not_collected() {
        assert!(incoming_from_raw(&raw_message(true, channel(1234), None), false).is_none());
        let service = tl::enums::Message::Empty(tl::types::MessageEmpty {
            id: 1,
            peer_id: Some(channel(1234)),
        });
        assert!(incoming_from_raw(&service, false).is_none());
    }

    #[test]
    fn document_attributes_map_to_bot_api_media_names() {
        let document = |attributes: Vec<tl::enums::DocumentAttribute>| {
            Some(tl::enums::MessageMedia::Document(
                tl::types::MessageMediaDocument {
                    nopremium: false,
                    spoiler: false,
                    video: false,
                    round: false,
                    voice: false,
                    document: Some(tl::enums::Document::Document(tl::types::Document {
                        id: 1,
                        access_hash: 2,
                        file_reference: Vec::new(),
                        date: 0,
                        mime_type: "audio/ogg".to_owned(),
                        size: 4096,
                        thumbs: None,
                        video_thumbs: None,
                        dc_id: 4,
                        attributes,
                    })),
                    alt_documents: None,
                    video_cover: None,
                    video_timestamp: None,
                    ttl_seconds: None,
                },
            ))
        };
        let voice = tl::enums::DocumentAttribute::Audio(tl::types::DocumentAttributeAudio {
            voice: true,
            duration: 3,
            title: None,
            performer: None,
            waveform: None,
        });
        let name = tl::enums::DocumentAttribute::Filename(tl::types::DocumentAttributeFilename {
            file_name: "a.ogg".to_owned(),
        });

        let incoming = incoming_from_raw(
            &raw_message(false, channel(1), document(vec![voice, name.clone()])),
            true,
        )
        .expect("语音消息应被采集");
        assert_eq!(incoming.media_type.as_deref(), Some("VOICE"));
        assert_eq!(incoming.file_name.as_deref(), Some("a.ogg"));
        assert_eq!(incoming.file_size, Some(4096));
        assert_eq!(incoming.mime_type.as_deref(), Some("audio/ogg"));
        assert!(incoming.edited);

        let plain = incoming_from_raw(&raw_message(false, channel(1), document(vec![name])), false)
            .expect("文件消息应被采集");
        assert_eq!(plain.media_type.as_deref(), Some("DOCUMENT"));
    }

    #[test]
    fn auth_key_errors_are_unauthorized_but_flood_wait_is_not() {
        let rpc = |code, name: &str| {
            TelegramClientError::Invocation(grammers_client::InvocationError::Rpc(
                grammers_client::sender::RpcError {
                    code,
                    name: name.to_owned(),
                    value: None,
                    caused_by: None,
                },
            ))
        };
        assert!(rpc(401, "AUTH_KEY_UNREGISTERED").is_unauthorized());
        assert!(rpc(401, "SESSION_REVOKED").is_unauthorized());
        assert!(!rpc(420, "FLOOD_WAIT").is_unauthorized());
        assert!(rpc(400, "ACCESS_TOKEN_INVALID").is_bot_token_rejected());
        assert!(!rpc(420, "FLOOD_WAIT").is_bot_token_rejected());
    }

    #[test]
    fn maps_media_resend_to_a_permanent_delivery_error() {
        let error = TelegramClientError::MediaResendUnavailable.delivery_error();

        assert_eq!(error.code, "MEDIA_RESEND_UNAVAILABLE");
        assert_eq!(error.flood_wait_seconds, None);
    }

    #[tokio::test]
    async fn opens_and_closes_memory_session_without_network() -> Result<(), TelegramClientError> {
        let config = TelegramClientConfig::new(932_939)?.without_catch_up();

        let client = TelegramClient::connect(config).await?;
        client.shutdown().await?;

        Ok(())
    }

    #[tokio::test]
    async fn restored_session_snapshot_survives_a_client_without_network()
    -> Result<(), TelegramClientError> {
        use crate::session::StoredSession;
        use grammers_session::Session;

        let seeded = StoredSession::default();
        seeded
            .set_home_dc_id(4)
            .await
            .map_err(TelegramClientError::Snapshot)?;
        let bytes = seeded.snapshot().map_err(TelegramClientError::Snapshot)?;
        let restored =
            StoredSession::restore(bytes.expose()).map_err(TelegramClientError::Snapshot)?;
        let config = TelegramClientConfig::new(932_939)?.without_catch_up();

        let client = TelegramClient::connect_with_session(config, restored).await?;
        let after = client.snapshot().await?;
        client.shutdown().await?;

        let reloaded =
            StoredSession::restore(after.expose()).map_err(TelegramClientError::Snapshot)?;
        assert_eq!(
            reloaded
                .home_dc_id()
                .map_err(TelegramClientError::Snapshot)?,
            4
        );
        Ok(())
    }
}
