//! 单实例租约防误双开（`ops/01` §一 门槛、G5 提案 §2.2）。
//!
//! 三个场景分开记录，不混为一个判据（提案 §四）：
//!
//! 1. **同名 systemd unit 重复 start**：同一 user manager 里对同一 unit 连续
//!    `start` 两次，只有一个 MainPID，第二次是 no-op。需要可用的
//!    `systemd --user` manager；CI runner 没有用户会话时该场景记为未运行
//!    （打印说明并通过），不冒充通过。
//! 2. **unit 外第二进程竞争**：直接命令路径起两个 `rule` 进程处理同一事件，
//!    `processed_at` 行锁与 `idem_key` 唯一约束保证恰好一条投递任务，
//!    两个进程都存活且可优雅停止。
//! 3. **租约接管与陈旧 token**：池级「僵尸持有者」持租约，真实
//!    `mtproto-worker` 进程在租约有效期内抢不到；强制过期（等价于等满
//!    30s TTL，隔离测试里由 admin 连接制造同一状态）后由真实进程接管、
//!    fencing token 递增；僵尸的陈旧 token 写不进事件。
//!
//! 全部使用隔离数据库与合成身份：不连接 Telegram，不使用生产凭据。
//! 目标机（Rocky）上的同名 unit 复验是独立子项，需单独授权，
//! 本文件的通过不构成目标机通过。

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use anyhow::{Context, Result, ensure};
use sqlx::PgPool;
use sqlx::types::chrono::{TimeZone, Utc};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_core::kek::EnvKek;
use tgm_core::profile::Profile;
use tgm_core::secret::Secret;
use tgm_db::credentials::store_identity_secret_leased;
use tgm_db::repo::{
    CanonicalAdapter, CanonicalEventInput, CanonicalEventType, LeaseGuard, acquire_lease,
    fanout_canonical_event, insert_canonical_event, insert_canonical_event_leased,
};
use tokio::time::{Instant, sleep};

mod support;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// 直接命令路径起的角色进程（不经 systemd）：场景 2 与 3 的启动入口。
struct DirectProcess {
    child: Child,
    stderr: PathBuf,
}

impl DirectProcess {
    fn spawn_role(database_url: &str, role: &str) -> Result<Self> {
        let stderr = std::env::temp_dir().join(format!(
            "tgm-single-instance-{}-{}.log",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        let output = std::fs::File::create(&stderr)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_tgm"));
        command
            .args(["serve", "--role", role])
            .env_clear()
            .env("TGM_PROFILE", "development")
            .env("TGM_DB_URL_APP", database_url)
            .env("TGM_REDIS_URL", "redis://127.0.0.1:56379")
            .env("RUST_LOG", "info,sqlx=warn")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(stderr.with_extension("stdout.log"))?)
            .stderr(output);
        if role == "mtproto-worker" {
            command
                .env("TGM_KEK_BACKEND", "env")
                .env(
                    "TGM_LOCAL_KEK",
                    "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
                )
                .env("TGM_S3_ENDPOINT", "http://127.0.0.1:59000")
                .env("TGM_S3_BUCKET", "tgm-single-instance")
                .env("TGM_S3_ACCESS_KEY_ID", "minioadmin")
                .env("TGM_S3_SECRET_ACCESS_KEY", "minioadmin");
        }
        let child = command
            .spawn()
            .with_context(|| format!("启动 {role} 子进程失败"))?;
        Ok(Self { child, stderr })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn assert_running(&mut self, role: &str) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            let mut log = String::new();
            if let Ok(mut file) = std::fs::File::open(&self.stderr) {
                let _ = file.read_to_string(&mut log);
            }
            anyhow::bail!("{role} 子进程提前退出：{status}\n{log}");
        }
        Ok(())
    }

    async fn terminate_and_wait_exit(mut self, role: &str) -> Result<()> {
        nix_free_kill(self.child.id())?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
                ensure!(
                    status.success(),
                    "{role} 收到 SIGTERM 后未干净退出：{status}"
                );
                std::mem::forget(self);
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "{role} 30s 内未响应 SIGTERM");
            sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for DirectProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// SIGTERM（不用 libc 依赖：kill 命令足够）。
fn nix_free_kill(pid: u32) -> Result<()> {
    ensure!(
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()?
            .success(),
        "SIGTERM 未发送成功"
    );
    Ok(())
}

fn user_manager_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 场景 1：同一 user manager 对同名 unit 连续 start 两次。
///
/// 无可用 `systemd --user` manager 时（典型：CI runner 无用户会话），
/// 记录为未运行并通过 —— 该场景的证据只能在有 manager 的环境产生，
/// 不拿别的场景冒充。
#[sqlx::test(migrations = "../../migrations-test")]
async fn same_name_unit_second_start_is_noop_with_single_main_pid(admin: PgPool) -> Result<()> {
    if !user_manager_available() {
        eprintln!("场景 1 未运行：当前环境没有可用的 systemd --user manager");
        return Ok(());
    }
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let port = support::pg_port()?;
    let app_url = format!("postgres://app_user:apw@127.0.0.1:{port}/{database}");

    let directory = std::env::temp_dir().join(format!(
        "tgm-single-instance-unit-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let env_path = directory.join("rule.env");
    std::fs::write(
        &env_path,
        format!(
            "TGM_PROFILE=development\nTGM_DB_URL_APP={app_url}\nTGM_REDIS_URL=redis://127.0.0.1:56379\nRUST_LOG=info,sqlx=warn\n"
        ),
    )?;
    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600))?;

    let unit = format!("tgm-g5-single-instance-{}.service", std::process::id());
    let cleanup = |status: Result<()>| -> Result<()> {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &unit])
            .status();
        let _ = Command::new("systemctl")
            .args(["--user", "reset-failed", &unit])
            .status();
        let _ = std::fs::remove_dir_all(&directory);
        status
    };

    let started = Command::new("systemd-run")
        .args([
            "--user",
            &format!("--unit={unit}"),
            "--property=Restart=no",
            env!("CARGO_BIN_EXE_tgm"),
            "serve",
            "--role",
            "rule",
            "--env-file",
        ])
        .arg(&env_path)
        .status();
    ensure!(
        started.is_ok_and(|status| status.success()),
        "systemd-run 启动临时 unit 失败"
    );

    let show = |property: &str| -> Result<String> {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                &unit,
                &format!("--property={property}"),
                "--value",
            ])
            .output()?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let first_pid = loop {
        let active = show("ActiveState")?;
        let pid = show("MainPID")?;
        if active == "active" && pid != "0" {
            break pid;
        }
        ensure!(Instant::now() < deadline, "unit 30s 内未 active：{active}");
        sleep(Duration::from_millis(100)).await;
    };

    // 第二次 start：systemd 语义下是 no-op，退出 0，MainPID 不变。
    let second = Command::new("systemctl")
        .args(["--user", "start", &unit])
        .status()?;
    ensure!(second.success(), "同名 unit 第二次 start 应成功（no-op）");
    ensure!(
        show("MainPID")? == first_pid,
        "第二次 start 后 MainPID 必须不变（只有一个进程）"
    );
    ensure!(show("ActiveState")? == "active");

    let stop = Command::new("systemctl")
        .args(["--user", "stop", &unit])
        .status()?;
    ensure!(stop.success(), "unit 停止失败");
    cleanup(Ok(()))
}

async fn seed_rule_world(admin: &PgPool, app: &PgPool) -> Result<(TenantId, IdentityId)> {
    let tenant: TenantId = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('single-instance', 9301) RETURNING id",
    )
    .fetch_one(admin)
    .await?;
    let receiver: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin)
    .await?;
    let sender: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('bot', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin)
    .await?;
    let source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, title, username, receiver_identity_id, enabled)
         VALUES ($1, -1001, 'Source SI', 'source_si', $2, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(receiver)
    .fetch_one(admin)
    .await?;
    let target_ref: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, -2001, $2, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(sender)
    .fetch_one(admin)
    .await?;
    let keyword_id: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'urgent', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin)
    .await?;
    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rules
            (tenant_id, keyword_id, source_ref, media_type, target_ref, action, priority, version, enabled)
         VALUES ($1, $2, $3, 'TEXT', $4, 'COPY', 10, 1, TRUE)
         RETURNING id",
    )
    .bind(tenant)
    .bind(keyword_id)
    .bind(source_ref)
    .bind(target_ref)
    .fetch_one(admin)
    .await?;
    sqlx::query(
        "INSERT INTO rule_versions (tenant_id, rule_id, version, snapshot)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(tenant)
    .bind(rule_id)
    .bind(serde_json::json!({
        "rule": { "template": "{matched_keywords}" },
        "replacements": []
    }))
    .execute(admin)
    .await?;
    let canonical = insert_canonical_event(
        app,
        &CanonicalEventInput {
            receiver_tenant_id: Some(tenant),
            adapter: CanonicalAdapter::MtProto,
            receiver_identity_id: receiver,
            tg_chat_id: -1001,
            tg_message_id: 71,
            event_type: CanonicalEventType::MessageCreated,
            edit_version: 0,
            update_id: None,
            media_group_id: None,
            sender_id: Some(7),
            message_date: Utc.timestamp_opt(1_760_000_000, 0).single().unwrap(),
            text: Some("urgent update".to_owned()),
            caption: None,
            media_type: Some("TEXT".to_owned()),
            content_hash: Some("single-instance-hash".to_owned()),
            file_unique_id: None,
            file_name: None,
            file_size: None,
            mime_type: None,
            raw_file_id: None,
        },
    )
    .await?;
    fanout_canonical_event(app, tenant, canonical.id)
        .await?
        .context("扇出应生成租户事件")?;
    Ok((tenant, receiver))
}

/// 场景 2：unit 外两个直接进程竞争同一事件，恰好产出一条投递任务。
#[sqlx::test(migrations = "../../migrations-test")]
async fn two_direct_rule_processes_produce_exactly_one_delivery(admin: PgPool) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let port = support::pg_port()?;
    let app_url =
        format!("postgres://app_user:apw@127.0.0.1:{port}/{database}?application_name=tgm-si-rule");
    let app = PgPool::connect(&app_url).await?;
    let (tenant, _receiver) = seed_rule_world(&admin, &app).await?;

    let first = DirectProcess::spawn_role(&app_url, "rule")?;
    let second = DirectProcess::spawn_role(&app_url, "rule")?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let processed = loop {
        let processed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tenant_events
                            WHERE tenant_id = $1 AND processed_at IS NOT NULL)",
        )
        .bind(tenant)
        .fetch_one(&admin)
        .await?;
        if processed {
            break true;
        }
        ensure!(Instant::now() < deadline, "30s 内事件未被处理");
        sleep(Duration::from_millis(100)).await;
    };
    assert!(processed);

    // 等一个完整轮询周期，捕获迟到的重复入队（若有）。
    sleep(Duration::from_secs(3)).await;
    let task_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_tasks WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&admin)
            .await?;
    assert_eq!(task_count, 1, "双开只允许恰好一条投递任务（idem_key 去重）");
    let event_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_events WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&admin)
            .await?;
    assert_eq!(event_rows, 1, "不得产生重复租户事件");

    // 两个进程都还活着，且都能优雅停止（记录进程 PID 与退出状态）。
    let mut first = first;
    let mut second = second;
    first.assert_running("rule-1")?;
    second.assert_running("rule-2")?;
    eprintln!(
        "场景 2：双进程 pid={} pid={}，事件处理一次、任务一条",
        first.pid(),
        second.pid()
    );
    first.terminate_and_wait_exit("rule-1").await?;
    second.terminate_and_wait_exit("rule-2").await?;
    Ok(())
}

/// 场景 3：真实进程在有效租约前拿不到身份；租约过期后接管并递增
/// fencing token；陈旧 token 的写入被拒。
#[sqlx::test(migrations = "../../migrations-test")]
async fn real_worker_cannot_take_live_lease_but_takes_over_after_expiry(
    admin: PgPool,
) -> Result<()> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await?;
    let port = support::pg_port()?;
    let app_url = format!(
        "postgres://app_user:apw@127.0.0.1:{port}/{database}?application_name=tgm-si-mtproto"
    );
    let app = PgPool::connect(&app_url).await?;
    let tenant: TenantId = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('single-instance-lease', 9302) RETURNING id",
    )
    .fetch_one(&admin)
    .await?;
    let identity: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('mtproto', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await?;
    // 候选查询（list_mtproto_worker_candidates）JOIN identity_secrets 且要求
    // api_id 非空：没有凭据行的身份不会被 worker 发现。经租约持有者加密写入，
    // 内容只需能被 load_session 读到（不真连 Telegram，内容不会外发）。
    let kek = EnvKek::new(
        Profile::Development,
        "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=",
    )
    .expect("构造测试 KEK 失败");
    let seed_lease = LeaseGuard {
        tenant_id: Some(tenant),
        identity_id: identity,
        worker_id: "seed".to_owned(),
        fencing_token: acquire_lease(&app, Some(tenant), identity, "seed")
            .await?
            .context("种子租约必须可取得")?,
    };
    store_identity_secret_leased(
        &app,
        &kek,
        tenant,
        &seed_lease,
        &Secret::new(b"single-instance-session".to_vec()),
    )
    .await
    .expect("写入合成凭据失败");
    // 候选查询还要求 `identity_secrets.api_id IS NOT NULL`（mtproto_worker.rs
    // 同款手法）：不补这一列，身份不会进入接管扫描。
    sqlx::query("UPDATE identity_secrets SET api_id = 1 WHERE identity_id = $1")
        .bind(identity)
        .execute(&admin)
        .await?;
    // 凭据落库后释放种子租约：身份重新可被接管（与扫码登录后的初始态一致）。
    tgm_db::repo::release_lease(&app, identity, "seed", seed_lease.fencing_token).await?;

    // 池级「僵尸持有者」：模拟卡住但未死的旧进程。
    let zombie_token = acquire_lease(&app, Some(tenant), identity, "zombie-worker")
        .await?
        .context("僵尸持有者应取得租约")?;
    let zombie = LeaseGuard {
        tenant_id: Some(tenant),
        identity_id: identity,
        worker_id: "zombie-worker".to_owned(),
        fencing_token: zombie_token,
    };

    let mut worker = DirectProcess::spawn_role(&app_url, "mtproto-worker")?;
    // 租约有效期内：真实进程抢不到（LeaseBusy），lease 行仍归僵尸。
    sleep(Duration::from_secs(5)).await;
    worker.assert_running("mtproto-worker")?;
    let holder: String =
        sqlx::query_scalar("SELECT worker_id FROM account_leases WHERE identity_id = $1")
            .bind(identity)
            .fetch_one(&admin)
            .await?;
    assert_eq!(holder, "zombie-worker", "有效租约不得被第二进程抢走");

    // 强制过期：与等满 30s TTL 到达同一数据库状态（lease.rs 同款手法）。
    sqlx::query(
        "UPDATE account_leases SET expires_at = clock_timestamp() - interval '1 second'
          WHERE identity_id = $1",
    )
    .bind(identity)
    .execute(&admin)
    .await?;

    // successor 接管：LeaseBusy 后该身份进 30s 冷却（restart_delay），
    // discover 周期 15s，最坏 ~45s 后才会再尝试 —— 窗口给 70s。
    let deadline = Instant::now() + Duration::from_secs(70);
    let successor = loop {
        worker.assert_running("mtproto-worker")?;
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT worker_id, fencing_token FROM account_leases WHERE identity_id = $1",
        )
        .bind(identity)
        .fetch_optional(&admin)
        .await?;
        if let Some((worker_id, token)) = row
            && worker_id != "zombie-worker"
        {
            break (worker_id, token);
        }
        ensure!(Instant::now() < deadline, "70s 内未完成接管");
        sleep(Duration::from_millis(250)).await;
    };
    assert!(
        successor.0.ends_with(&format!(":{}", worker.pid())),
        "接管者必须是真实进程（worker_id 含其 PID）：{}",
        successor.0
    );
    assert!(
        successor.1 > zombie_token,
        "接管必须递增 fencing token：{} -> {}",
        zombie_token,
        successor.1
    );

    // 陈旧 token 写入被拒：僵尸的 LeaseGuard 已不是持有者。
    let stale_event = CanonicalEventInput {
        receiver_tenant_id: Some(tenant),
        adapter: CanonicalAdapter::MtProto,
        receiver_identity_id: identity,
        tg_chat_id: -1001,
        tg_message_id: 91,
        event_type: CanonicalEventType::MessageCreated,
        edit_version: 0,
        update_id: None,
        media_group_id: None,
        sender_id: None,
        message_date: Utc.timestamp_opt(1_760_000_100, 0).single().unwrap(),
        text: Some("stale write".to_owned()),
        caption: None,
        media_type: Some("TEXT".to_owned()),
        content_hash: None,
        file_unique_id: None,
        file_name: None,
        file_size: None,
        mime_type: None,
        raw_file_id: None,
    };
    assert!(
        insert_canonical_event_leased(&app, &zombie, &stale_event)
            .await?
            .is_none(),
        "陈旧 fencing token 不得写入事件"
    );
    eprintln!(
        "场景 3：僵尸持租 → 真实进程 pid={} 接管（token {} → {}），陈旧写入被拒",
        worker.pid(),
        zombie_token,
        successor.1
    );
    worker.terminate_and_wait_exit("mtproto-worker").await?;
    Ok(())
}
