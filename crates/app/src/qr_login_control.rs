//! 控制面 QR 登录的进程内登记簿与限流（spec/07 §3.3，提案 `WP-2-qr-login-control` D1）。
//!
//! 登录中的 MTProto 连接与 `login_token` 只在内存里：bearer 凭据不进数据库。
//! 每条登录绑定发起者的租户与用户，查不到与不属于本人一律同口径（调用方映射 404）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// 单次登录总时长上限。到点即丢弃连接，前端看到 `expired`。
pub const LOGIN_MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// 同一 `login_id` 两次轮询（或两次提交密码）的最小间隔。
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 每租户同时进行中的登录数上限。
pub const MAX_CONCURRENT_PER_TENANT: usize = 2;
/// 每租户每小时发起次数上限。
pub const MAX_STARTS_PER_HOUR: usize = 10;
const START_WINDOW: Duration = Duration::from_secs(60 * 60);
const LOGIN_ID_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRejected {
    TooManyConcurrent,
    TooManyPerHour,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Touch {
    Poll,
    Password,
}

pub enum Lookup<T> {
    Live(Arc<tokio::sync::Mutex<T>>),
    /// 已超过 [`LOGIN_MAX_AGE`]，条目已移出登记簿；调用方负责关闭连接。
    Expired(Arc<tokio::sync::Mutex<T>>),
    NotFound,
    TooFast,
}

struct Slot<T> {
    tenant_id: i64,
    user_id: i64,
    deadline: Instant,
    last_poll: Option<Instant>,
    last_password: Option<Instant>,
    login: Arc<tokio::sync::Mutex<T>>,
}

struct Inner<T> {
    logins: HashMap<String, Slot<T>>,
    starts: HashMap<i64, VecDeque<Instant>>,
}

// ponytail: 进程内登记簿与限流，只在单实例 control-plane（ADR-0025 形态 A）下成立；
// 上多实例前改为粘性路由或共享存储
pub struct QrLoginStore<T> {
    inner: Mutex<Inner<T>>,
}

impl<T> Default for QrLoginStore<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                logins: HashMap::new(),
                starts: HashMap::new(),
            }),
        }
    }
}

/// 32 字节随机数，base64url 无 padding。不可枚举（spec/07 §3.3）。
pub fn new_login_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; LOGIN_ID_BYTES];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

impl<T> QrLoginStore<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 按限流登记一条新登录。并发数与小时配额在同一把锁里判定，不会被并行请求同时穿过。
    pub fn start(
        &self,
        login_id: String,
        tenant_id: i64,
        user_id: i64,
        now: Instant,
        login: T,
    ) -> Result<Arc<tokio::sync::Mutex<T>>, StartRejected> {
        let mut inner = self.lock();
        let live = inner
            .logins
            .values()
            .filter(|slot| slot.tenant_id == tenant_id && slot.deadline > now)
            .count();
        if live >= MAX_CONCURRENT_PER_TENANT {
            return Err(StartRejected::TooManyConcurrent);
        }
        let starts = inner.starts.entry(tenant_id).or_default();
        while starts
            .front()
            .is_some_and(|started| now.duration_since(*started) >= START_WINDOW)
        {
            starts.pop_front();
        }
        if starts.len() >= MAX_STARTS_PER_HOUR {
            return Err(StartRejected::TooManyPerHour);
        }
        starts.push_back(now);
        let login = Arc::new(tokio::sync::Mutex::new(login));
        inner.logins.insert(
            login_id,
            Slot {
                tenant_id,
                user_id,
                deadline: now + LOGIN_MAX_AGE,
                last_poll: None,
                last_password: None,
                login: Arc::clone(&login),
            },
        );
        Ok(login)
    }

    /// 取一条登录并记一次访问。别人的登录与不存在的登录返回同一个 `NotFound`。
    pub fn touch(
        &self,
        login_id: &str,
        tenant_id: i64,
        user_id: i64,
        now: Instant,
        kind: Touch,
    ) -> Lookup<T> {
        let mut inner = self.lock();
        let Some(slot) = inner.logins.get_mut(login_id) else {
            return Lookup::NotFound;
        };
        if slot.tenant_id != tenant_id || slot.user_id != user_id {
            return Lookup::NotFound;
        }
        if now >= slot.deadline {
            let login = Arc::clone(&slot.login);
            inner.logins.remove(login_id);
            return Lookup::Expired(login);
        }
        let last = match kind {
            Touch::Poll => &mut slot.last_poll,
            Touch::Password => &mut slot.last_password,
        };
        if last.is_some_and(|last| now.duration_since(last) < MIN_POLL_INTERVAL) {
            return Lookup::TooFast;
        }
        *last = Some(now);
        Lookup::Live(Arc::clone(&slot.login))
    }

    /// 移出所有已超时的登录。返回它们，调用方负责关闭连接。
    ///
    /// 被丢弃的登录（用户关了页面）不会再被访问，靠每次发起与轮询顺手清。
    pub fn sweep(&self, now: Instant) -> Vec<Arc<tokio::sync::Mutex<T>>> {
        let mut inner = self.lock();
        let expired: Vec<String> = inner
            .logins
            .iter()
            .filter(|(_, slot)| now >= slot.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .iter()
            .filter_map(|id| inner.logins.remove(id).map(|slot| slot.login))
            .collect()
    }

    /// 移出登记簿（完成、失败或超时）。返回被移出的登录，调用方负责关闭连接。
    pub fn remove(&self, login_id: &str) -> Option<Arc<tokio::sync::Mutex<T>>> {
        self.lock().logins.remove(login_id).map(|slot| slot.login)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn store() -> QrLoginStore<()> {
        QrLoginStore::default()
    }

    #[test]
    fn login_id_is_32_random_bytes() {
        let first = new_login_id().expect("random");
        let second = new_login_id().expect("random");
        assert_ne!(first, second);
        assert_eq!(
            URL_SAFE_NO_PAD.decode(&first).expect("base64url").len(),
            LOGIN_ID_BYTES
        );
    }

    #[test]
    fn other_tenant_or_user_sees_not_found() {
        let store = store();
        let now = Instant::now();
        store.start("a".into(), 1, 10, now, ()).expect("start");
        assert!(matches!(
            store.touch("a", 2, 10, now, Touch::Poll),
            Lookup::NotFound
        ));
        assert!(matches!(
            store.touch("a", 1, 11, now, Touch::Poll),
            Lookup::NotFound
        ));
        assert!(matches!(
            store.touch("b", 1, 10, now, Touch::Poll),
            Lookup::NotFound
        ));
        assert!(matches!(
            store.touch("a", 1, 10, now, Touch::Poll),
            Lookup::Live(_)
        ));
    }

    #[test]
    fn poll_faster_than_interval_is_rejected_per_kind() {
        let store = store();
        let now = Instant::now();
        store.start("a".into(), 1, 10, now, ()).expect("start");
        assert!(matches!(
            store.touch("a", 1, 10, now, Touch::Poll),
            Lookup::Live(_)
        ));
        let soon = now + Duration::from_millis(1900);
        assert!(matches!(
            store.touch("a", 1, 10, soon, Touch::Poll),
            Lookup::TooFast
        ));
        assert!(matches!(
            store.touch("a", 1, 10, soon, Touch::Password),
            Lookup::Live(_)
        ));
        let later = now + MIN_POLL_INTERVAL;
        assert!(matches!(
            store.touch("a", 1, 10, later, Touch::Poll),
            Lookup::Live(_)
        ));
    }

    #[test]
    fn concurrent_logins_per_tenant_are_capped() {
        let store = store();
        let now = Instant::now();
        store.start("a".into(), 1, 10, now, ()).expect("first");
        store.start("b".into(), 1, 11, now, ()).expect("second");
        assert_eq!(
            store.start("c".into(), 1, 10, now, ()).err(),
            Some(StartRejected::TooManyConcurrent)
        );
        store
            .start("d".into(), 2, 20, now, ())
            .expect("other tenant unaffected");
        store.remove("a").expect("removed");
        store.start("c".into(), 1, 10, now, ()).expect("slot freed");
    }

    #[test]
    fn starts_per_hour_are_capped_on_a_sliding_window() {
        let store = store();
        let base = Instant::now();
        for index in 0..MAX_STARTS_PER_HOUR {
            let id = format!("id-{index}");
            store
                .start(
                    id.clone(),
                    1,
                    10,
                    base + Duration::from_secs(index as u64),
                    (),
                )
                .expect("within quota");
            store.remove(&id);
        }
        let at = base + Duration::from_secs(60);
        assert_eq!(
            store.start("x".into(), 1, 10, at, ()).err(),
            Some(StartRejected::TooManyPerHour)
        );
        let after = base + START_WINDOW;
        store
            .start("x".into(), 1, 10, after, ())
            .expect("oldest start aged out");
    }

    #[test]
    fn expired_login_is_removed_and_frees_concurrency() {
        let store = store();
        let now = Instant::now();
        store.start("a".into(), 1, 10, now, ()).expect("start");
        store.start("b".into(), 1, 10, now, ()).expect("start");
        let late = now + LOGIN_MAX_AGE;
        store
            .start("c".into(), 1, 10, late, ())
            .expect("expired slots do not count");
        assert!(matches!(
            store.touch("a", 1, 10, late, Touch::Poll),
            Lookup::Expired(_)
        ));
        assert!(matches!(
            store.touch("a", 1, 10, late, Touch::Poll),
            Lookup::NotFound
        ));
        assert_eq!(store.sweep(late).len(), 1, "b 已超时，c 未超时");
        assert!(matches!(
            store.touch("c", 1, 10, late, Touch::Poll),
            Lookup::Live(_)
        ));
    }
}
