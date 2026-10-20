//! 全部业务逻辑：fanout / rule / delivery / control。
//!
//! spec/02 §一 划的 7 个组件在这里是模块，不是 crate ——
//! 1 人全职下 7 个 crate 是净负担（eng/00 §一）。
//!
//! 一条评审纪律：本 crate 里出现 `admin_tx` 就是评审阻塞项，
//! 它意味着某段业务逻辑绕过了租户谓词（eng/00 §4.2）。

pub mod bot_console;
pub mod bot_gateway;
pub mod console;
pub mod console_login;
pub mod control;
pub mod delivery;
pub mod delivery_runtime;
pub mod media_relay;
pub mod mtproto_session;
pub mod mtproto_worker;
pub mod pipeline;
pub mod rule;

/// 退出信号：`SIGTERM`（`docker stop` / systemd）或 Ctrl-C，先到先完成。
/// ops/00 §六：收到后停止拉取新任务，等在途请求结束。
///
/// 监听在调用时立即注册，不等返回的 future 被首次轮询 —— 否则启动后、
/// 任务首次被调度前到达的 SIGTERM 会走默认处置直接杀进程。
pub fn shutdown_signal() -> impl std::future::Future<Output = ()> + Send + 'static {
    use tokio::signal::unix::{SignalKind, signal};
    let terminate = signal(SignalKind::terminate())
        .map_err(|error| tracing::warn!(error = %error, "无法监听 SIGTERM，只响应 Ctrl-C"))
        .ok();
    async move {
        match terminate {
            Some(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
}
