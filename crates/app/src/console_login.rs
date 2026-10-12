//! 控制台扫码登录的挑战材料与状态机。
//!
//! 这条链路和 MTProto QR 登录不同：二维码只携带随机 nonce，Bot 应答后还必须
//! 经过同一账号的确认动作，才能进入会话签发阶段。数据库里的原子状态迁移在
//! `tgm_db::repo`，本模块只负责不依赖数据库的安全状态与材料生成。

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use getrandom::Error as RandomError;
use thiserror::Error;

const NONCE_LEN: usize = 32;
const CONFIRM_CODE_LEN: usize = 4;
const CONFIRM_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const LOGIN_START_PREFIX: &str = "login_";
const LOGIN_CALLBACK_PREFIX: &str = "console_login";
const LOGIN_CALLBACK_COMPACT_PREFIX: &str = "cl";

#[derive(Debug, Error)]
pub enum ChallengeMaterialError {
    #[error("生成登录挑战随机材料失败")]
    Random(#[from] RandomError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeMaterial {
    nonce: [u8; NONCE_LEN],
    confirm_code: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleLoginAction {
    Start {
        nonce: Vec<u8>,
    },
    Confirm {
        nonce: Vec<u8>,
        confirm_code: String,
    },
    Reject {
        nonce: Vec<u8>,
    },
}

pub fn encode_nonce(nonce: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(nonce)
}

pub fn parse_start_command(text: &str) -> Option<ConsoleLoginAction> {
    parse_start_command_inner(text, None)
}

pub fn parse_start_command_for_bot(text: &str, bot_username: &str) -> Option<ConsoleLoginAction> {
    parse_start_command_inner(text, Some(bot_username))
}

fn parse_start_command_inner(text: &str, bot_username: Option<&str>) -> Option<ConsoleLoginAction> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    if command != "/start" {
        let username = command.strip_prefix("/start@")?;
        if bot_username.is_some_and(|expected| !username.eq_ignore_ascii_case(expected)) {
            return None;
        }
    }
    let payload = parts.next()?.strip_prefix(LOGIN_START_PREFIX)?;
    if parts.next().is_some() {
        return None;
    }
    decode_nonce(payload).map(|nonce| ConsoleLoginAction::Start { nonce })
}

pub fn parse_callback_data(data: &str) -> Option<ConsoleLoginAction> {
    let mut parts = data.split(':');
    let prefix = parts.next()?;
    let action = parts.next()?;
    let (confirm_action, reject_action) = match prefix {
        LOGIN_CALLBACK_PREFIX => ("confirm", "reject"),
        LOGIN_CALLBACK_COMPACT_PREFIX => ("c", "r"),
        _ => return None,
    };
    let nonce = decode_nonce(parts.next()?)?;
    match action {
        action if action == confirm_action => {
            let confirm_code = parts.next()?.to_owned();
            if parts.next().is_some()
                || confirm_code.len() != CONFIRM_CODE_LEN
                || !confirm_code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric())
            {
                return None;
            }
            Some(ConsoleLoginAction::Confirm {
                nonce,
                confirm_code,
            })
        }
        action if action == reject_action && parts.next().is_none() => {
            Some(ConsoleLoginAction::Reject { nonce })
        }
        _ => None,
    }
}

pub fn confirm_callback_data(nonce: &[u8], confirm_code: &str) -> String {
    format!(
        "{LOGIN_CALLBACK_COMPACT_PREFIX}:c:{}:{confirm_code}",
        encode_nonce(nonce)
    )
}

pub fn reject_callback_data(nonce: &[u8]) -> String {
    format!("{LOGIN_CALLBACK_COMPACT_PREFIX}:r:{}", encode_nonce(nonce))
}

pub fn decode_nonce(encoded: &str) -> Option<Vec<u8>> {
    let nonce = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    (nonce.len() == NONCE_LEN).then_some(nonce)
}

impl ChallengeMaterial {
    pub fn generate() -> Result<Self, ChallengeMaterialError> {
        let mut nonce = [0_u8; NONCE_LEN];
        getrandom::fill(&mut nonce)?;

        let mut confirm_code = String::with_capacity(CONFIRM_CODE_LEN);
        let alphabet_len = CONFIRM_ALPHABET.len() as u16;
        let limit = u16::from(u8::MAX) + 1 - (u16::from(u8::MAX) + 1) % alphabet_len;
        while confirm_code.len() < CONFIRM_CODE_LEN {
            let mut byte = [0_u8; 1];
            getrandom::fill(&mut byte)?;
            let value = u16::from(byte[0]);
            if value >= limit {
                continue;
            }
            confirm_code.push(CONFIRM_ALPHABET[(value % alphabet_len) as usize] as char);
        }

        Ok(Self {
            nonce,
            confirm_code,
        })
    }

    pub fn nonce(&self) -> &[u8; NONCE_LEN] {
        &self.nonce
    }

    pub fn confirm_code(&self) -> &str {
        &self.confirm_code
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeState {
    Pending,
    AwaitingConfirm { tg_user_id: i64 },
    Confirmed { tg_user_id: i64 },
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeEvent {
    Scanned { tg_user_id: i64 },
    Confirmed { tg_user_id: i64 },
    Rejected { tg_user_id: i64 },
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChallengeTransitionError {
    #[error("控制台登录挑战状态迁移无效")]
    InvalidTransition,
    #[error("控制台登录挑战操作人不匹配")]
    ActorMismatch,
    #[error("Telegram 用户 id 无效")]
    InvalidUserId,
}

impl ChallengeState {
    pub fn transition(self, event: ChallengeEvent) -> Result<Self, ChallengeTransitionError> {
        match (self, event) {
            (Self::Pending, ChallengeEvent::Scanned { tg_user_id }) => {
                valid_user_id(tg_user_id)?;
                Ok(Self::AwaitingConfirm { tg_user_id })
            }
            (
                Self::AwaitingConfirm {
                    tg_user_id: expected,
                },
                ChallengeEvent::Confirmed { tg_user_id },
            ) => {
                ensure_actor(expected, tg_user_id)?;
                Ok(Self::Confirmed { tg_user_id })
            }
            (
                Self::AwaitingConfirm {
                    tg_user_id: expected,
                },
                ChallengeEvent::Rejected { tg_user_id },
            ) => {
                ensure_actor(expected, tg_user_id)?;
                Ok(Self::Rejected)
            }
            (Self::Pending | Self::AwaitingConfirm { .. }, ChallengeEvent::Expired) => {
                Ok(Self::Expired)
            }
            _ => Err(ChallengeTransitionError::InvalidTransition),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::AwaitingConfirm { .. } => "awaiting_confirm",
            Self::Confirmed { .. } => "confirmed",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }

    pub fn from_db(state: &str, tg_user_id: Option<i64>) -> Result<Self, ChallengeTransitionError> {
        match (state, tg_user_id) {
            ("pending", None) => Ok(Self::Pending),
            ("awaiting_confirm", Some(tg_user_id)) => {
                valid_user_id(tg_user_id)?;
                Ok(Self::AwaitingConfirm { tg_user_id })
            }
            ("confirmed", Some(tg_user_id)) => {
                valid_user_id(tg_user_id)?;
                Ok(Self::Confirmed { tg_user_id })
            }
            ("rejected", None) | ("rejected", Some(_)) => Ok(Self::Rejected),
            ("expired", None) | ("expired", Some(_)) => Ok(Self::Expired),
            _ => Err(ChallengeTransitionError::InvalidTransition),
        }
    }
}

fn valid_user_id(tg_user_id: i64) -> Result<(), ChallengeTransitionError> {
    (tg_user_id > 0)
        .then_some(())
        .ok_or(ChallengeTransitionError::InvalidUserId)
}

fn ensure_actor(expected: i64, actual: i64) -> Result<(), ChallengeTransitionError> {
    valid_user_id(actual)?;
    (expected == actual)
        .then_some(())
        .ok_or(ChallengeTransitionError::ActorMismatch)
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIRM_ALPHABET, CONFIRM_CODE_LEN, ChallengeEvent, ChallengeMaterial, ChallengeState,
        ChallengeTransitionError, ConsoleLoginAction, NONCE_LEN, confirm_callback_data,
        encode_nonce, parse_callback_data, parse_start_command, parse_start_command_for_bot,
        reject_callback_data,
    };

    #[test]
    fn generated_material_has_expected_shape() -> Result<(), Box<dyn std::error::Error>> {
        let material = ChallengeMaterial::generate()?;
        assert_eq!(material.nonce().len(), NONCE_LEN);
        assert_eq!(material.confirm_code().len(), CONFIRM_CODE_LEN);
        assert!(
            material
                .confirm_code()
                .bytes()
                .all(|byte| CONFIRM_ALPHABET.contains(&byte))
        );
        Ok(())
    }

    #[test]
    fn scan_then_confirm_binds_the_same_actor() -> Result<(), ChallengeTransitionError> {
        let state =
            ChallengeState::Pending.transition(ChallengeEvent::Scanned { tg_user_id: 42 })?;
        assert_eq!(
            state.transition(ChallengeEvent::Confirmed { tg_user_id: 7 }),
            Err(ChallengeTransitionError::ActorMismatch)
        );
        assert_eq!(
            state.transition(ChallengeEvent::Confirmed { tg_user_id: 42 })?,
            ChallengeState::Confirmed { tg_user_id: 42 }
        );
        Ok(())
    }

    #[test]
    fn expired_and_terminal_states_cannot_be_reused() -> Result<(), ChallengeTransitionError> {
        let expired = ChallengeState::Pending.transition(ChallengeEvent::Expired)?;
        assert_eq!(
            expired.transition(ChallengeEvent::Scanned { tg_user_id: 42 }),
            Err(ChallengeTransitionError::InvalidTransition)
        );

        let rejected = ChallengeState::Pending
            .transition(ChallengeEvent::Scanned { tg_user_id: 42 })
            .and_then(|state| state.transition(ChallengeEvent::Rejected { tg_user_id: 42 }))?;
        assert_eq!(
            rejected.transition(ChallengeEvent::Confirmed { tg_user_id: 42 }),
            Err(ChallengeTransitionError::InvalidTransition)
        );
        Ok(())
    }

    #[test]
    fn database_state_requires_actor_for_active_states() -> Result<(), ChallengeTransitionError> {
        assert_eq!(
            ChallengeState::from_db("awaiting_confirm", None),
            Err(ChallengeTransitionError::InvalidTransition)
        );
        assert_eq!(
            ChallengeState::from_db("confirmed", Some(99))?.as_str(),
            "confirmed"
        );
        Ok(())
    }

    #[test]
    fn login_start_and_callback_data_round_trip_without_extra_authority() {
        let nonce = vec![7_u8; NONCE_LEN];
        let encoded = encode_nonce(&nonce);
        assert_eq!(
            parse_start_command(&format!("/start login_{encoded}")),
            Some(ConsoleLoginAction::Start {
                nonce: nonce.clone()
            })
        );
        assert_eq!(
            parse_start_command(&format!("/start@control_bot login_{encoded}")),
            Some(ConsoleLoginAction::Start {
                nonce: nonce.clone()
            })
        );
        assert_eq!(
            parse_start_command_for_bot(
                &format!("/start@control_bot login_{encoded}"),
                "CONTROL_BOT"
            ),
            Some(ConsoleLoginAction::Start {
                nonce: nonce.clone()
            })
        );
        assert_eq!(
            parse_start_command_for_bot(
                &format!("/start@other_bot login_{encoded}"),
                "control_bot"
            ),
            None
        );
        assert_eq!(
            parse_callback_data(&format!("console_login:confirm:{encoded}:A7K4")),
            Some(ConsoleLoginAction::Confirm {
                nonce: nonce.clone(),
                confirm_code: "A7K4".to_owned()
            })
        );
        let compact_confirm = confirm_callback_data(&nonce, "A7K4");
        assert!(compact_confirm.len() <= 64);
        assert_eq!(
            parse_callback_data(&compact_confirm),
            Some(ConsoleLoginAction::Confirm {
                nonce: nonce.clone(),
                confirm_code: "A7K4".to_owned()
            })
        );
        let compact_reject = reject_callback_data(&nonce);
        assert!(compact_reject.len() <= 64);
        assert_eq!(
            parse_callback_data(&compact_reject),
            Some(ConsoleLoginAction::Reject { nonce })
        );
    }

    #[test]
    fn login_commands_reject_malformed_or_overspecified_values() {
        let encoded = encode_nonce(&[9_u8; NONCE_LEN]);
        assert_eq!(parse_start_command("/help login_value"), None);
        assert_eq!(
            parse_start_command(&format!("/start login_{encoded} extra")),
            None
        );
        assert_eq!(
            parse_callback_data(&format!("console_login:confirm:{encoded}:123")),
            None
        );
        assert_eq!(
            parse_callback_data(&format!("console_login:confirm:{encoded}:A7K4:extra")),
            None
        );
    }
}
