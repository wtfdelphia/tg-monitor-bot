#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use sqlx::PgPool;
use tgm_app::delivery::{
    DeliveryPayload, DeliveryRunOutcome, DeliverySendError, DeliveryTransport, run_delivery_once,
};
use tgm_core::ids::{IdentityId, TenantId};

struct RestartProcess {
    child: Child,
}

impl RestartProcess {
    fn start(url: &str, tenant: TenantId, directory: &Path, phase: &str) -> Self {
        let child = Command::new(std::env::current_exe().expect("定位测试子进程失败"))
            .args(["--exact", "delivery_restart_child", "--nocapture"])
            .env_clear()
            .env("TGM_RESTART_CHILD_URL", url)
            .env("TGM_RESTART_CHILD_TENANT", tenant.get().to_string())
            .env("TGM_RESTART_CHILD_DIR", directory)
            .env("TGM_RESTART_CHILD_PHASE", phase)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("启动投递恢复测试子进程失败");
        Self { child }
    }

    async fn wait_for_marker(&mut self, path: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !path.exists() {
            assert!(
                self.child.try_wait().expect("检查子进程失败").is_none(),
                "子进程提前退出"
            );
            assert!(tokio::time::Instant::now() < deadline, "等待投递检查点超时");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn kill(&mut self) {
        self.child.kill().expect("强杀投递子进程失败");
        assert!(!self.child.wait().expect("回收投递子进程失败").success());
    }

    async fn wait_for_success(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("等待恢复子进程失败") {
                assert!(status.success());
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "恢复子进程未退出");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for RestartProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct RestartDirectory(PathBuf);

impl Drop for RestartDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct RecordingTransport {
    directory: PathBuf,
    phase: String,
}

#[async_trait::async_trait]
impl DeliveryTransport for RecordingTransport {
    async fn deliver(&self, payload: DeliveryPayload) -> Result<i64, DeliverySendError> {
        let request = serde_json::to_vec(&json!({
            "task_id": payload.task_id,
            "random_id": payload.telegram_random_id,
            "sender": payload.sender_identity_id.get(),
            "target": payload.target_tg_chat_id,
            "action": format!("{:?}", payload.action),
            "source": payload.source,
            "message": payload.message,
        }))
        .expect("序列化模拟请求失败");
        std::fs::write(
            self.directory.join(format!("{}.payload", self.phase)),
            &request,
        )
        .expect("写入请求检查点失败");
        if self.phase == "before-send" {
            std::future::pending::<()>().await;
        }
        std::fs::write(
            self.directory.join(format!("{}.sent", self.phase)),
            &request,
        )
        .expect("写入模拟发送记录失败");
        if self.phase == "after-send" {
            std::future::pending::<()>().await;
        }
        if self.phase == "response-lost" {
            return Err(DeliverySendError {
                code: "MESSAGE_NOT_RETURNED".to_owned(),
                message: None,
                flood_wait_seconds: None,
            });
        }
        Ok(9877)
    }
}

#[tokio::test]
async fn delivery_restart_child() {
    let Ok(url) = std::env::var("TGM_RESTART_CHILD_URL") else {
        return;
    };
    let pool = PgPool::connect(&url).await.expect("连接恢复测试库失败");
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&pool)
        .await
        .expect("读取角色失败");
    assert_eq!(role, "app_user");
    let tenant_value: i64 = std::env::var("TGM_RESTART_CHILD_TENANT")
        .expect("缺少租户")
        .parse()
        .expect("租户无效");
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants WHERE id = $1")
        .bind(tenant_value)
        .fetch_one(&pool)
        .await
        .expect("读取租户失败");
    let sender: IdentityId = tgm_db::tenant::tenant_tx(&pool, tenant, |connection| Box::pin(async move {
        sqlx::query_scalar("SELECT sender_identity_id FROM delivery_tasks WHERE tenant_id = $1 ORDER BY id LIMIT 1")
            .bind(tenant).fetch_one(connection).await
    })).await.expect("读取发送身份失败");
    let directory =
        PathBuf::from(std::env::var_os("TGM_RESTART_CHILD_DIR").expect("缺少检查点路径"));
    let phase = std::env::var("TGM_RESTART_CHILD_PHASE").expect("缺少检查点阶段");
    let outcome = run_delivery_once(
        &pool,
        tenant,
        sender,
        &RecordingTransport {
            directory: directory.clone(),
            phase: phase.clone(),
        },
    )
    .await
    .expect("运行投递失败");
    if phase == "response-lost" {
        assert!(matches!(outcome, DeliveryRunOutcome::Failed { .. }));
        std::fs::write(directory.join("response-lost.recorded"), b"recorded")
            .expect("记录失败状态检查点失败");
        std::future::pending::<()>().await;
    }
    assert!(matches!(
        outcome,
        DeliveryRunOutcome::Done {
            telegram_message_id: 9877,
            ..
        }
    ));
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn frozen_requests_survive_subprocess_kill_before_and_after_simulated_send(admin: PgPool) {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .expect("读取库名失败");
    let url = format!("postgres://app_user:apw@127.0.0.1:55432/{database}");
    let tenant: TenantId = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('delivery-restart', 7890) RETURNING id",
    )
    .fetch_one(&admin)
    .await
    .expect("创建租户失败");
    let sender: IdentityId = sqlx::query_scalar(
        "INSERT INTO identities (tenant_id, kind) VALUES ($1, 'mtproto') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&admin)
    .await
    .expect("创建身份失败");
    let source_ref: i64 = sqlx::query_scalar(
        "INSERT INTO source_subscriptions
            (tenant_id, tg_chat_id, receiver_identity_id)
         VALUES ($1, -1001, $2)
         RETURNING id",
    )
    .bind(tenant)
    .bind(sender)
    .fetch_one(&admin)
    .await
    .expect("创建来源失败");
    let canonical_event_id: i64 = sqlx::query_scalar(
        "INSERT INTO canonical_events
            (adapter, receiver_identity_id, tg_chat_id, tg_message_id, message_date)
         VALUES ('mtproto', $1, -1001, 42, clock_timestamp())
         RETURNING id",
    )
    .bind(sender)
    .fetch_one(&admin)
    .await
    .expect("创建规范化事件失败");
    let tenant_event_id: i64 = sqlx::query_scalar(
        "INSERT INTO tenant_events (tenant_id, canonical_event_id, source_ref)
         VALUES ($1, $2, $3)
         RETURNING id",
    )
    .bind(tenant)
    .bind(canonical_event_id)
    .bind(source_ref)
    .fetch_one(&admin)
    .await
    .expect("创建租户事件失败");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("读取时间失败")
        .as_nanos();
    for phase in ["before-send", "after-send", "response-lost"] {
        let directory = RestartDirectory(std::env::temp_dir().join(format!(
            "tgm-delivery-{}-{timestamp}-{phase}",
            std::process::id()
        )));
        std::fs::create_dir(&directory.0).expect("创建检查点目录失败");
        let task_id: i64 = sqlx::query_scalar(
            "INSERT INTO delivery_tasks
            (tenant_id, tenant_event_id, sender_identity_id, target_tg_chat_id, action,
             telegram_random_id, payload_snapshot)
            VALUES ($1, $2, $3, -2001, 'COPY', 1234567, $4) RETURNING id",
        )
        .bind(tenant)
        .bind(tenant_event_id)
        .bind(sender)
        .bind(json!({
            "source": {"tg_chat_id": -1001, "tg_message_id": 42, "media_type": "TEXT"},
            "message": {"text": "frozen &lt;message&gt;", "parse_mode": "HTML"}
        }))
        .fetch_one(&admin)
        .await
        .expect("创建快照任务失败");
        let mut first = RestartProcess::start(&url, tenant, &directory.0, phase);
        let marker = match phase {
            "before-send" => "before-send.payload",
            "after-send" => "after-send.sent",
            _ => "response-lost.recorded",
        };
        first.wait_for_marker(&directory.0.join(marker)).await;
        first.kill();
        let status: String = sqlx::query_scalar("SELECT status FROM delivery_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(&admin)
            .await
            .expect("读取中断状态失败");
        assert_eq!(
            status,
            if phase == "response-lost" {
                "PENDING"
            } else {
                "SENDING"
            }
        );
        assert_eq!(
            directory.0.join(format!("{phase}.sent")).exists(),
            phase != "before-send"
        );
        sqlx::query(
            "UPDATE delivery_tasks SET next_retry_at = clock_timestamp(),
                     updated_at = clock_timestamp() - interval '120 seconds' WHERE id = $1",
        )
        .bind(task_id)
        .execute(&admin)
        .await
        .expect("设置恢复条件失败");
        let mut restarted = RestartProcess::start(&url, tenant, &directory.0, "restarted");
        restarted.wait_for_success().await;
        assert_eq!(
            std::fs::read(directory.0.join(format!("{phase}.payload"))).expect("读取原请求失败"),
            std::fs::read(directory.0.join("restarted.sent")).expect("读取恢复请求失败")
        );
        let completed: (String, Option<i64>) =
            sqlx::query_as("SELECT status, telegram_message_id FROM delivery_tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&admin)
                .await
                .expect("读取完成状态失败");
        assert_eq!(completed, ("DONE".to_owned(), Some(9877)));
        sqlx::query("DELETE FROM delivery_logs WHERE task_id = $1")
            .bind(task_id)
            .execute(&admin)
            .await
            .expect("清理投递日志失败");
        sqlx::query("DELETE FROM delivery_tasks WHERE id = $1")
            .bind(task_id)
            .execute(&admin)
            .await
            .expect("清理任务失败");
    }
}
