//! 单二进制多 subcommand（eng/00 §一）。7 个组件共用一个 bin，
//! 靠 `--role` 区分进程角色 —— 首发部署形态就是单机跑多进程（ops/00）。
//!
//! 本文件目前只有命令骨架：每个子命令都还没有实现体。
//! 骨架先落地是因为 eng/04 §四 的五条卡口里有三条要求 `tgm` 这个二进制存在。

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "tgm", version, about = "Telegram 多租户内容监控与分发平台")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动一个进程角色
    Serve {
        #[arg(long)]
        role: Role,
    },
    /// 跑数据库迁移（与 `sqlx migrate run --source migrations` 同一份文件）
    Migrate,
    /// RLS 静态审计：主查询 + A1~A9 + eng/04 §三 三条
    AuditRls,
    /// 导出 OpenAPI 契约；`--check` 则与仓库内的 openapi.json 比对
    Openapi {
        #[arg(long)]
        check: bool,
    },
}

/// 角色取值与 eng/00 §一 的 `tgm serve --role` 列表一一对应。
#[derive(Clone, ValueEnum)]
enum Role {
    ControlPlane,
    BotGateway,
    MtprotoWorker,
    Fanout,
    Rule,
    Delivery,
}

impl Role {
    fn as_str(&self) -> &'static str {
        match self {
            Role::ControlPlane => "control-plane",
            Role::BotGateway => "bot-gateway",
            Role::MtprotoWorker => "mtproto-worker",
            Role::Fanout => "fanout",
            Role::Rule => "rule",
            Role::Delivery => "delivery",
        }
    }
}

/// 未实现的子命令统一走这里，且**退出码是 0**。
///
/// 这是个有意的、且危险的选择，写清理由：eng/04 §四 的卡口清单要求
/// `tgm audit-rls` 与 `tgm openapi --check` 可执行，而动工顺序第 6 步的判据是
/// 「命令存在且退出 0」—— 真正的断言要到第 7 步才接活库。
///
/// 危险在于：这期间流水线里那两条是**绿的但什么都没验**，
/// 正是 eng/04 §五 说的 "false sense of security"。所以提示打到 stderr 且
/// 带 NOT IMPLEMENTED 字样，让人读日志时能看见 —— 但机器看不见。
/// 第 7 步落地 audit-rls 之后，这个函数的调用点要同步减少；
/// 减到零之前，「流水线全绿」这句话不携带信息。
fn not_implemented(what: &str) {
    eprintln!("tgm: {what}: NOT IMPLEMENTED（骨架阶段，退出码 0 不代表校验通过）");
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve { role } => not_implemented(&format!("serve --role {}", role.as_str())),
        Command::Migrate => not_implemented("migrate"),
        Command::AuditRls => not_implemented("audit-rls"),
        Command::Openapi { check } => {
            not_implemented(if check { "openapi --check" } else { "openapi" })
        }
    }
    Ok(())
}
