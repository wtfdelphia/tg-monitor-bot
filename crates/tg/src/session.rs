//! 可落库的 MTProto session（spec/08 §1.3：不落文件，落库）。
//!
//! grammers 的 `MemorySession` 不暴露内部状态，`SqliteSession` 落本地文件，都不能
//! 直接接信封加密。这里用同样的 `SessionData` 实现 `Session`，另加一份带版本号的
//! 二进制快照：[`StoredSession::snapshot`] 产出明文字节交给凭据层加密落库，
//! [`StoredSession::restore`] 从解密后的字节恢复。
//!
//! 不开 grammers-session 的 `serde` feature：那会为四类字段多拉二十余个间接依赖。
//! 快照里含各 DC 的 auth key（等同登录凭据），所以输出只以 `Secret` 形式给出。

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::sync::{Mutex, MutexGuard};

use grammers_session::types::{
    ChannelKind, ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState,
};
use grammers_session::{BoxFuture, Session, SessionData};
use tgm_core::secret::Secret;
use thiserror::Error;

const MAGIC: &[u8; 4] = b"TGMS";
const VERSION: u8 = 1;
const AUTH_KEY_LEN: usize = 256;

#[derive(Debug, Error)]
pub enum StoredSessionError {
    #[error("session 锁已中毒")]
    Poisoned,
    #[error("session 快照格式无效")]
    Malformed,
    #[error("session 快照版本不受支持: {0}")]
    UnsupportedVersion(u8),
}

/// 可快照的 session。存储形状同 grammers 的 `MemorySession`；查 `PeerId::self_user()`
/// 时按 `is_self` 找登录用户，与 `SqliteSession` 一致（`MemorySession` 按键直查，查不到）。
/// grammers 靠这次查询判断是否已登录（`stream_updates` 是否拉初始更新状态）。
#[derive(Default)]
pub struct StoredSession(Mutex<SessionData>);

/// 内含 auth key，`Debug` 一律脱敏（同 `Secret`）。
impl std::fmt::Debug for StoredSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoredSession([REDACTED])")
    }
}

impl StoredSession {
    fn data(&self) -> Result<MutexGuard<'_, SessionData>, StoredSessionError> {
        self.0.lock().map_err(|_| StoredSessionError::Poisoned)
    }

    /// 是否已绑定登录用户（grammers 以缓存里有 self 用户为判据）。
    pub fn is_signed_in(&self) -> Result<bool, StoredSessionError> {
        Ok(self.data()?.peer_infos.values().any(|peer| {
            matches!(
                peer,
                PeerInfo::User {
                    is_self: Some(true),
                    ..
                }
            )
        }))
    }

    /// 序列化当前状态。输出含 auth key，只能交给信封加密落库。
    pub fn snapshot(&self) -> Result<Secret<Vec<u8>>, StoredSessionError> {
        let data = self.data()?;
        // 一次分配到位：扩容会在释放的旧缓冲里留下 auth key 副本（同 secret.rs 的说明）。
        let mut out = Vec::with_capacity(
            32 + data.dc_options.len() * (AUTH_KEY_LEN + 48)
                + data.peer_infos.len() * 24
                + data.updates_state.channels.len() * 12,
        );
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&data.home_dc.to_be_bytes());

        let mut dcs: Vec<&DcOption> = data.dc_options.values().collect();
        dcs.sort_by_key(|dc| dc.id);
        put_len(&mut out, dcs.len())?;
        for dc in dcs {
            out.extend_from_slice(&dc.id.to_be_bytes());
            out.extend_from_slice(&dc.ipv4.ip().octets());
            out.extend_from_slice(&dc.ipv4.port().to_be_bytes());
            out.extend_from_slice(&dc.ipv6.ip().octets());
            out.extend_from_slice(&dc.ipv6.port().to_be_bytes());
            out.extend_from_slice(&dc.ipv6.flowinfo().to_be_bytes());
            out.extend_from_slice(&dc.ipv6.scope_id().to_be_bytes());
            match &dc.auth_key {
                Some(key) => {
                    out.push(1);
                    out.extend_from_slice(key);
                }
                None => out.push(0),
            }
        }

        // ponytail: peer 缓存不设上限，快照随会话见过的 peer 线性增长；单份超过 1 MiB 时按最近使用裁剪
        let mut peers: Vec<&PeerInfo> = data.peer_infos.values().collect();
        peers.sort_by_key(|peer| peer.id().bot_api_dialog_id_unchecked());
        put_len(&mut out, peers.len())?;
        for peer in peers {
            match peer {
                PeerInfo::User {
                    id,
                    auth,
                    bot,
                    is_self,
                } => {
                    out.push(0);
                    out.extend_from_slice(&id.to_be_bytes());
                    put_auth(&mut out, *auth);
                    put_flag(&mut out, *bot);
                    put_flag(&mut out, *is_self);
                }
                PeerInfo::Chat { id } => {
                    out.push(1);
                    out.extend_from_slice(&id.to_be_bytes());
                }
                PeerInfo::Channel { id, auth, kind } => {
                    out.push(2);
                    out.extend_from_slice(&id.to_be_bytes());
                    put_auth(&mut out, *auth);
                    out.push(match kind {
                        None => 0,
                        Some(ChannelKind::Broadcast) => 1,
                        Some(ChannelKind::Megagroup) => 2,
                        Some(ChannelKind::Gigagroup) => 3,
                    });
                }
            }
        }

        let state = &data.updates_state;
        for value in [state.pts, state.qts, state.date, state.seq] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        let mut channels: Vec<&ChannelState> = state.channels.iter().collect();
        channels.sort_by_key(|channel| channel.id);
        put_len(&mut out, channels.len())?;
        for channel in channels {
            out.extend_from_slice(&channel.id.to_be_bytes());
            out.extend_from_slice(&channel.pts.to_be_bytes());
        }
        Ok(Secret::new(out))
    }

    /// 从快照恢复。任何越界、未知标签或尾随字节都拒绝，不做部分恢复。
    pub fn restore(bytes: &[u8]) -> Result<Self, StoredSessionError> {
        let mut input = Reader(bytes);
        if input.take(MAGIC.len())? != MAGIC {
            return Err(StoredSessionError::Malformed);
        }
        let version = input.u8()?;
        if version != VERSION {
            return Err(StoredSessionError::UnsupportedVersion(version));
        }
        let mut data = SessionData {
            home_dc: input.i32()?,
            ..SessionData::default()
        };

        data.dc_options.clear();
        for _ in 0..input.len()? {
            let id = input.i32()?;
            let ipv4 = SocketAddrV4::new(Ipv4Addr::from(input.array::<4>()?), input.u16()?);
            let ip6 = Ipv6Addr::from(input.array::<16>()?);
            let ipv6 = SocketAddrV6::new(ip6, input.u16()?, input.u32()?, input.u32()?);
            let auth_key = match input.u8()? {
                0 => None,
                1 => Some(input.array::<AUTH_KEY_LEN>()?),
                _ => return Err(StoredSessionError::Malformed),
            };
            let option = DcOption {
                id,
                ipv4,
                ipv6,
                auth_key,
            };
            if data.dc_options.insert(id, option).is_some() {
                return Err(StoredSessionError::Malformed);
            }
        }

        for _ in 0..input.len()? {
            let peer = match input.u8()? {
                0 => PeerInfo::User {
                    id: valid_id(input.i64()?, PeerId::user)?,
                    auth: input.auth()?,
                    bot: input.flag()?,
                    is_self: input.flag()?,
                },
                1 => PeerInfo::Chat {
                    id: valid_id(input.i64()?, PeerId::chat)?,
                },
                2 => PeerInfo::Channel {
                    id: valid_id(input.i64()?, PeerId::channel)?,
                    auth: input.auth()?,
                    kind: match input.u8()? {
                        0 => None,
                        1 => Some(ChannelKind::Broadcast),
                        2 => Some(ChannelKind::Megagroup),
                        3 => Some(ChannelKind::Gigagroup),
                        _ => return Err(StoredSessionError::Malformed),
                    },
                },
                _ => return Err(StoredSessionError::Malformed),
            };
            if data.peer_infos.insert(peer.id(), peer).is_some() {
                return Err(StoredSessionError::Malformed);
            }
        }

        let (pts, qts, date, seq) = (input.i32()?, input.i32()?, input.i32()?, input.i32()?);
        let mut channels = Vec::new();
        for _ in 0..input.len()? {
            channels.push(ChannelState {
                id: input.i64()?,
                pts: input.i32()?,
            });
        }
        data.updates_state = UpdatesState {
            pts,
            qts,
            date,
            seq,
            channels,
        };
        if !input.0.is_empty() {
            return Err(StoredSessionError::Malformed);
        }
        Ok(Self(Mutex::new(data)))
    }
}

fn valid_id(id: i64, check: fn(i64) -> Option<PeerId>) -> Result<i64, StoredSessionError> {
    check(id).map(|_| id).ok_or(StoredSessionError::Malformed)
}

fn put_len(out: &mut Vec<u8>, len: usize) -> Result<(), StoredSessionError> {
    let len = u32::try_from(len).map_err(|_| StoredSessionError::Malformed)?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

fn put_auth(out: &mut Vec<u8>, auth: Option<PeerAuth>) {
    match auth {
        Some(auth) => {
            out.push(1);
            out.extend_from_slice(&auth.hash().to_be_bytes());
        }
        None => out.push(0),
    }
}

fn put_flag(out: &mut Vec<u8>, flag: Option<bool>) {
    out.push(match flag {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    });
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], StoredSessionError> {
        if self.0.len() < n {
            return Err(StoredSessionError::Malformed);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], StoredSessionError> {
        self.take(N)?
            .try_into()
            .map_err(|_| StoredSessionError::Malformed)
    }

    fn u8(&mut self) -> Result<u8, StoredSessionError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, StoredSessionError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, StoredSessionError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, StoredSessionError> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, StoredSessionError> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    /// 元素个数。每个元素至少占一个字节，个数超过剩余字节即判定格式无效，
    /// 免得被一个伪造的大计数骗去循环。
    fn len(&mut self) -> Result<u32, StoredSessionError> {
        let len = self.u32()?;
        if usize::try_from(len).map_err(|_| StoredSessionError::Malformed)? > self.0.len() {
            return Err(StoredSessionError::Malformed);
        }
        Ok(len)
    }

    fn auth(&mut self) -> Result<Option<PeerAuth>, StoredSessionError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(PeerAuth::from_hash(self.i64()?))),
            _ => Err(StoredSessionError::Malformed),
        }
    }

    fn flag(&mut self) -> Result<Option<bool>, StoredSessionError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(false)),
            2 => Ok(Some(true)),
            _ => Err(StoredSessionError::Malformed),
        }
    }
}

impl Session for StoredSession {
    type Error = StoredSessionError;

    fn home_dc_id(&self) -> Result<i32, StoredSessionError> {
        Ok(self.data()?.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), StoredSessionError>> {
        Box::pin(async move {
            self.data()?.home_dc = dc_id;
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, StoredSessionError> {
        Ok(self.data()?.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), StoredSessionError>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            self.data()?.dc_options.insert(dc_option.id, dc_option);
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, StoredSessionError>> {
        Box::pin(async move {
            let data = self.data()?;
            if peer == PeerId::self_user() {
                return Ok(data
                    .peer_infos
                    .values()
                    .find(|info| {
                        matches!(
                            info,
                            PeerInfo::User {
                                is_self: Some(true),
                                ..
                            }
                        )
                    })
                    .cloned());
            }
            Ok(data.peer_infos.get(&peer).cloned())
        })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), StoredSessionError>> {
        let peer = peer.clone();
        Box::pin(async move {
            self.data()?
                .peer_infos
                .entry(peer.id())
                .or_insert_with(|| peer.clone())
                .extend_info(&peer);
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, StoredSessionError>> {
        Box::pin(async move { Ok(self.data()?.updates_state.clone()) })
    }

    fn set_update_state(
        &self,
        update: UpdateState,
    ) -> BoxFuture<'_, Result<(), StoredSessionError>> {
        Box::pin(async move {
            let mut data = self.data()?;
            match update {
                UpdateState::All(state) => data.updates_state = state,
                UpdateState::Primary { pts, date, seq } => {
                    data.updates_state.pts = pts;
                    data.updates_state.date = date;
                    data.updates_state.seq = seq;
                }
                UpdateState::Secondary { qts } => data.updates_state.qts = qts,
                UpdateState::Channel { id, pts } => {
                    data.updates_state
                        .channels
                        .retain(|channel| channel.id != id);
                    data.updates_state.channels.push(ChannelState { id, pts });
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AUTH_KEY_LEN, StoredSession, StoredSessionError};
    use grammers_session::Session;
    use grammers_session::types::{
        ChannelKind, ChannelState, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState,
    };

    async fn signed_in_session() -> Result<StoredSession, StoredSessionError> {
        let session = StoredSession::default();
        session.set_home_dc_id(4).await?;
        let mut dc = session.dc_option(4)?.ok_or(StoredSessionError::Malformed)?;
        dc.auth_key = Some([7u8; AUTH_KEY_LEN]);
        session.set_dc_option(&dc).await?;
        session
            .cache_peer(&PeerInfo::User {
                id: 4242,
                auth: Some(PeerAuth::from_hash(-99)),
                bot: Some(false),
                is_self: Some(true),
            })
            .await?;
        session.cache_peer(&PeerInfo::Chat { id: 77 }).await?;
        session
            .cache_peer(&PeerInfo::Channel {
                id: 1_234_567,
                auth: Some(PeerAuth::from_hash(5)),
                kind: Some(ChannelKind::Megagroup),
            })
            .await?;
        session
            .set_update_state(UpdateState::All(UpdatesState {
                pts: 10,
                qts: 20,
                date: 30,
                seq: 40,
                channels: vec![ChannelState {
                    id: 1_234_567,
                    pts: 50,
                }],
            }))
            .await?;
        Ok(session)
    }

    #[tokio::test]
    async fn snapshot_roundtrips_auth_key_peers_and_update_state() -> Result<(), StoredSessionError>
    {
        let session = signed_in_session().await?;
        let bytes = session.snapshot()?;

        let restored = StoredSession::restore(bytes.expose())?;

        assert_eq!(restored.home_dc_id()?, 4);
        assert_eq!(
            restored.dc_option(4)?.and_then(|dc| dc.auth_key),
            Some([7u8; AUTH_KEY_LEN])
        );
        assert_eq!(restored.dc_option(2)?.and_then(|dc| dc.auth_key), None);
        assert!(restored.is_signed_in()?);
        let self_user = restored.peer(PeerId::self_user()).await?;
        assert!(
            matches!(self_user, Some(PeerInfo::User { id: 4242, .. })),
            "恢复后按 self_user 查不到登录用户，grammers 会当成未登录"
        );
        let channel = PeerId::channel(1_234_567).ok_or(StoredSessionError::Malformed)?;
        assert_eq!(restored.peer(channel).await?, session.peer(channel).await?);
        assert_eq!(
            restored.updates_state().await?,
            session.updates_state().await?
        );
        assert_eq!(restored.snapshot()?.expose(), bytes.expose());
        Ok(())
    }

    #[tokio::test]
    async fn fresh_session_is_not_signed_in_after_restore() -> Result<(), StoredSessionError> {
        let bytes = StoredSession::default().snapshot()?;
        let restored = StoredSession::restore(bytes.expose())?;

        assert!(!restored.is_signed_in()?);
        assert_eq!(restored.peer(PeerId::self_user()).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn every_truncation_and_trailing_byte_is_rejected() -> Result<(), StoredSessionError> {
        let bytes = signed_in_session().await?.snapshot()?;
        let full = bytes.expose();

        for cut in 0..full.len() {
            assert!(
                matches!(
                    StoredSession::restore(&full[..cut]),
                    Err(StoredSessionError::Malformed)
                ),
                "截断到 {cut} 字节仍被接受"
            );
        }
        let mut trailing = full.clone();
        trailing.push(0);
        assert!(matches!(
            StoredSession::restore(&trailing),
            Err(StoredSessionError::Malformed)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn unknown_version_and_magic_are_rejected() -> Result<(), StoredSessionError> {
        let bytes = StoredSession::default().snapshot()?;
        let mut version = bytes.expose().clone();
        version[4] = 9;
        assert!(matches!(
            StoredSession::restore(&version),
            Err(StoredSessionError::UnsupportedVersion(9))
        ));
        let mut magic = bytes.expose().clone();
        magic[0] = b'X';
        assert!(matches!(
            StoredSession::restore(&magic),
            Err(StoredSessionError::Malformed)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn debug_never_prints_session_contents() -> Result<(), StoredSessionError> {
        let session = signed_in_session().await?;

        assert_eq!(format!("{session:?}"), "StoredSession([REDACTED])");
        Ok(())
    }

    #[test]
    fn oversized_count_is_rejected_without_looping() {
        let mut bytes = b"TGMS\x01".to_vec();
        bytes.extend_from_slice(&2i32.to_be_bytes());
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());

        assert!(matches!(
            StoredSession::restore(&bytes),
            Err(StoredSessionError::Malformed)
        ));
    }
}
