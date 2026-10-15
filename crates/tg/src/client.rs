//! `grammers` 的最小接入边界。
//!
//! 当前 session 使用 `MemorySession` 仅用于 P0 spike 和本地验证；它不是生产凭据存储。
//! 生产路径必须在 session 落库前接入信封加密和授权生命周期管理。

use std::sync::Arc;

use grammers_client::client::{UpdateStream, UpdatesConfiguration};
use grammers_client::session::types::{PeerId, PeerRef};
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool, parsers, tl};
use grammers_session::storages::MemorySession;
use thiserror::Error;

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
    handle: grammers_client::sender::SenderPoolHandle,
    updates: UpdateStream,
    runner_task: tokio::task::JoinHandle<()>,
}

impl TelegramClient {
    pub async fn connect(config: TelegramClientConfig) -> Result<Self, TelegramClientError> {
        let session = Arc::new(MemorySession::default());
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(session, config.api_id);
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
            handle: handle.thin,
            updates,
            runner_task,
        })
    }

    pub async fn next_update(&mut self) -> Result<Update, TelegramClientError> {
        self.updates
            .next()
            .await
            .map_err(TelegramClientError::Invocation)
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

    pub async fn shutdown(self) -> Result<(), TelegramClientError> {
        let sync_result = self.updates.sync_update_state().await;
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
mod tests {
    use super::{
        TelegramClient, TelegramClientConfig, TelegramClientError, message_id_for_random_id,
        peer_ref, validate_random_id,
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
}
