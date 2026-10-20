//! delivery 常驻长跑（真机，ADR-0033 代价节「重启风暴下的限流没有测过」）。
//!
//! `#[ignore]`：要真实 Bot、测试频道与平台 `api_id`，CI 与默认测试都不跑。
//! 凭据只经环境变量传给子进程，本文件不打印、不落盘 Token 与 `api_hash`。
//!
//! 起真实的 `tgm serve --role delivery` 子进程，持续往库里放投递任务，期间按固定节奏
//! 交替用 SIGTERM（优雅）与 SIGKILL（强杀）重启进程。结束后断言：
//!
//! ```text
//! S1 每条任务都到 DONE，且各自拿到不同的 telegram_message_id（不丢、不重）
//! S2 没有任务进 DEAD / FAILED 残留，delivery_logs 里没有 FLOOD_WAIT 以外的失败
//! S3 每次 SIGTERM 都在 30s 内以 0 退出
//! S4 身份不被标成 unauthorized；限流（`bot_sign_in` 的 FLOOD_WAIT）只记录不判失败。
//! S5 只有第一次启动用 Token 登录，之后每次重启都复用落库的 session，不再调 `auth.importBotAuthorization`（ADR-0034）。
//!
//! 默认重启间隔 600 秒，是 session 落库之前的取值：当时几分钟内第 4 次左右登录
//! 就被限流 66–305 秒。S5 成立后可以调短（`TGM_SOAK_RESTART_SECS`）验证重启风暴。
//! ```
//!
//! 环境变量：
//!
//! ```text
//! TGM_PLATFORM_API_ID / TGM_PLATFORM_API_HASH   平台应用凭据
//! TGM_LIVE_BOT_TOKEN                            测试 Bot 的 Token
//! TGM_LIVE_TARGET_CHAT                          测试频道的 Bot API chat id，如 -100...
//! TGM_SOAK_MINUTES                              长跑时长（分钟），默认 30
//! TGM_SOAK_RESTART_SECS                         两次重启的间隔（秒），默认 600
//! TGM_SOAK_TASK_SECS                            放一条任务的间隔（秒），默认 20
//! ```
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::store_identity_secret;
use tokio::time::{Instant, sleep};

const LOCAL_KEK: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=";
const LOGIN_LINE: &str = "delivery 发送身份已登录";
const VIA_TOKEN: &str = "delivery 发送身份已登录（Token）";
const FLOOD_LINE: &str = "bot_sign_in 被限流";

/// 缺变量直接失败：`--ignored` 跑起来却因为没配凭据而「通过」就是假绿。
fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("长跑缺少环境变量 {name}"))
}

fn knob(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("{name} 必须是正整数"))
        })
        .unwrap_or(default)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos()
}

fn fresh_random_id(sequence: u64) -> i64 {
    let mixed = now_nanos() ^ (u128::from(sequence) << 48) ^ (u128::from(std::process::id()) << 64);
    i64::try_from(mixed & 0x3fff_ffff_ffff_ffff).expect("截断后必为正")
}

struct DeliveryProcess {
    child: Child,
}

#[derive(Default)]
struct Counters {
    logins: Arc<AtomicUsize>,
    token_logins: Arc<AtomicUsize>,
    floods: Arc<AtomicUsize>,
}

impl DeliveryProcess {
    fn start(app_url: &str, counters: &Counters) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tgm"))
            .args(["serve", "--role", "delivery"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TGM_PROFILE", "development")
            .env("TGM_DB_URL_APP", app_url)
            .env("TGM_REDIS_URL", "redis://127.0.0.1:56379")
            .env("RUST_LOG", "info,sqlx=warn")
            .env("TGM_KEK_BACKEND", "env")
            .env("TGM_LOCAL_KEK", LOCAL_KEK)
            .env("TGM_S3_ENDPOINT", "http://127.0.0.1:59000")
            .env("TGM_S3_BUCKET", "tgm-soak")
            .env("TGM_PLATFORM_API_ID", required("TGM_PLATFORM_API_ID"))
            .env("TGM_PLATFORM_API_HASH", required("TGM_PLATFORM_API_HASH"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("启动 delivery 子进程失败");
        for stream in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let counter = Arc::clone(&counters.logins);
            let token_counter = Arc::clone(&counters.token_logins);
            let flood_counter = Arc::clone(&counters.floods);
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if line.contains(LOGIN_LINE) {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if line.contains(VIA_TOKEN) {
                            token_counter.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    if line.contains(FLOOD_LINE) {
                        flood_counter.fetch_add(1, Ordering::SeqCst);
                    }
                    println!("[child] {line}");
                }
            });
        }
        Self { child }
    }

    fn assert_running(&mut self) {
        if let Some(status) = self.child.try_wait().expect("查询子进程状态失败") {
            panic!("delivery 子进程意外退出：{status}");
        }
    }

    async fn terminate(mut self) -> Duration {
        let started = Instant::now();
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("发送 SIGTERM 失败");
        assert!(sent.success(), "kill -TERM 未成功");
        loop {
            if let Some(status) = self.child.try_wait().expect("查询子进程状态失败") {
                assert!(
                    status.success(),
                    "[S3] SIGTERM 后应以 0 退出，实际 {status}"
                );
                return started.elapsed();
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "[S3] SIGTERM 后 30s 内未退出"
            );
            sleep(Duration::from_millis(100)).await;
        }
    }

    fn kill(mut self) {
        self.child.kill().expect("强杀 delivery 子进程失败");
        let status = self.child.wait().expect("回收 delivery 子进程失败");
        assert!(!status.success(), "必须实际强杀，不能用正常退出替代");
    }
}

impl Drop for DeliveryProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn seed_task(
    admin: &PgPool,
    tenant: TenantId,
    bot: IdentityId,
    target: i64,
    seq: u64,
) -> i64 {
    let random_id = fresh_random_id(seq);
    sqlx::query_scalar(
        "INSERT INTO delivery_tasks
            (tenant_id, sender_identity_id, target_tg_chat_id, action, telegram_random_id, payload_snapshot)
         VALUES ($1, $2, $3, 'COPY', $4,
                 jsonb_build_object(
                     'source', jsonb_build_object('tg_chat_id', $3, 'tg_message_id', 1, 'media_type', 'TEXT'),
                     'message', jsonb_build_object('text', $5::text, 'parse_mode', 'HTML')))
         RETURNING id",
    )
    .bind(tenant)
    .bind(bot)
    .bind(target)
    .bind(random_id)
    .bind(format!("tgm-soak #{seq} {random_id}"))
    .fetch_one(admin)
    .await
    .expect("创建投递任务失败")
}

#[sqlx::test(migrations = "../../migrations-test")]
#[ignore = "真机：需要测试 Bot、测试频道与平台 api_id"]
async fn live_delivery_soak(admin: PgPool) {
    let token = required("TGM_LIVE_BOT_TOKEN");
    let target: i64 = required("TGM_LIVE_TARGET_CHAT")
        .parse()
        .expect("TGM_LIVE_TARGET_CHAT 必须是整数");
    let minutes = knob("TGM_SOAK_MINUTES", 30);
    let restart_every = Duration::from_secs(knob("TGM_SOAK_RESTART_SECS", 600));
    let task_every = Duration::from_secs(knob("TGM_SOAK_TASK_SECS", 20));

    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .expect("读取库名失败");
    let app_url = format!("postgres://app_user:apw@127.0.0.1:55432/{database}");
    let app = PgPoolOptions::new()
        .max_connections(2)
        .connect(&app_url)
        .await
        .expect("连接 app_user 失败");
    let tenant: TenantId = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('delivery-soak', 9301) RETURNING id",
    )
    .fetch_one(&admin)
    .await
    .expect("创建租户失败");
    let bot_id = token
        .split_once(':')
        .and_then(|(id, _)| id.parse::<i64>().ok())
        .expect("Token 格式无效");
    let bot: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status, tg_user_id)
         VALUES ('bot', $1, 'offline', $2) RETURNING id",
    )
    .bind(tenant)
    .bind(bot_id)
    .fetch_one(&admin)
    .await
    .expect("创建 Bot 身份失败");
    let kek = EnvKek::new(Profile::Development, LOCAL_KEK).expect("构造 EnvKek 失败");
    store_identity_secret(&app, &kek, tenant, bot, &Secret::new(token.into_bytes()))
        .await
        .expect("写入 Token 失败");

    let counters = Counters::default();
    let mut starts = 1_usize;
    let mut process = DeliveryProcess::start(&app_url, &counters);
    let mut graceful_exits = Vec::new();
    let mut hard_kills = 0_usize;
    let mut tasks = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    let mut next_task = Instant::now();
    let mut next_restart = Instant::now() + restart_every;
    let mut seq = 0_u64;

    while Instant::now() < deadline {
        process.assert_running();
        if Instant::now() >= next_task {
            seq += 1;
            tasks.push(seed_task(&admin, tenant, bot, target, seq).await);
            next_task += task_every;
        }
        if Instant::now() >= next_restart {
            if starts % 2 == 1 {
                let took = process.terminate().await;
                println!("[soak] 第 {starts} 次重启：SIGTERM，{took:?} 后退出");
                graceful_exits.push(took);
            } else {
                process.kill();
                hard_kills += 1;
                println!("[soak] 第 {starts} 次重启：SIGKILL");
            }
            starts += 1;
            process = DeliveryProcess::start(&app_url, &counters);
            next_restart += restart_every;
        }
        sleep(Duration::from_millis(500)).await;
    }

    // 最后一次重启可能正撞上登录限流（实测最长 305s），收尾窗口要盖住它。
    let drain_deadline = Instant::now() + Duration::from_secs(600);
    loop {
        process.assert_running();
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM delivery_tasks
              WHERE tenant_id = $1 AND status NOT IN ('DONE', 'DEAD')",
        )
        .bind(tenant)
        .fetch_one(&admin)
        .await
        .expect("统计未完成任务失败");
        if open == 0 {
            break;
        }
        assert!(
            Instant::now() < drain_deadline,
            "长跑结束后 10 分钟仍有 {open} 条任务未完成"
        );
        sleep(Duration::from_secs(2)).await;
    }
    let final_exit = process.terminate().await;
    graceful_exits.push(final_exit);

    let (done, distinct_messages, dead): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'DONE'),
                count(DISTINCT telegram_message_id) FILTER (WHERE status = 'DONE'),
                count(*) FILTER (WHERE status = 'DEAD')
           FROM delivery_tasks WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await
    .expect("统计任务结果失败");
    let failures: Vec<(String, i64)> = sqlx::query_as(
        "SELECT coalesce(l.error_code, '-'), count(*)
           FROM delivery_logs l JOIN delivery_tasks t ON t.id = l.task_id
          WHERE t.tenant_id = $1 AND NOT l.ok
          GROUP BY 1 ORDER BY 1",
    )
    .bind(tenant)
    .fetch_all(&admin)
    .await
    .expect("统计失败日志失败");
    let (status, unauthorized): (String, i64) = sqlx::query_as(
        "SELECT i.status,
                (SELECT count(*) FROM audit_logs a
                  WHERE a.resource = 'identity' AND a.resource_id = i.id
                    AND a.action = 'identity.unauthorized')
           FROM identities i WHERE i.id = $1",
    )
    .bind(bot)
    .fetch_one(&admin)
    .await
    .expect("读取身份状态失败");
    let login_count = counters.logins.load(Ordering::SeqCst);
    let token_login_count = counters.token_logins.load(Ordering::SeqCst);
    let flood_count = counters.floods.load(Ordering::SeqCst);
    let slowest_exit = graceful_exits.iter().max().copied().unwrap_or_default();

    println!(
        "[soak] minutes={minutes} tasks={} done={done} distinct_message_ids={distinct_messages} dead={dead} \
         starts={starts} logins={login_count} token_logins={token_login_count} login_flood_waits={flood_count} sigterm={} sigkill={hard_kills} slowest_sigterm_exit={slowest_exit:?} \
         failures={failures:?} identity_status={status} unauthorized_audits={unauthorized}",
        tasks.len(),
        graceful_exits.len(),
    );
    println!("[soak] 人工核对：测试频道里 tgm-soak #1..#{seq} 每条只出现一次。");

    let task_count = i64::try_from(tasks.len()).expect("任务数溢出");
    assert_eq!(done, task_count, "[S1] 有任务没到 DONE");
    assert_eq!(
        distinct_messages, task_count,
        "[S1] 有两条任务拿到同一 message_id"
    );
    assert_eq!(dead, 0, "[S2] 有任务进了死信");
    assert!(
        failures.iter().all(|(code, _)| code == "FLOOD_WAIT"),
        "[S2] 出现 FLOOD_WAIT 以外的失败：{failures:?}"
    );
    assert_eq!(unauthorized, 0, "[S4] 身份被标为 unauthorized");
    assert_eq!(login_count, starts, "[S5] 有一次启动没登录上");
    assert_eq!(
        token_login_count, 1,
        "[S5] 重启后没有复用落库的 session，又用 Token 登录了"
    );
}
