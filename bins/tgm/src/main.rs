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
    /// RLS 静态审计：主查询 + eng/04 §二 + §三（C3 除外，它比对文件不查库）
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
///
/// 第 7 步进展：`audit-rls` 已接活库，从这里移出去了。
/// 剩下三个调用点 —— `serve`、`migrate`、`openapi` —— 其中 `openapi --check`
/// 仍在 eng/04 §四 的卡口清单里，所以那一条的绿依然是空的。
fn not_implemented(what: &str) {
    eprintln!("tgm: {what}: NOT IMPLEMENTED（骨架阶段，退出码 0 不代表校验通过）");
}

/// 审计用的连接串。**不复用 `TGM_DB_URL_APP`**：
/// 断言里的 `has_table_privilege('app_user', ...)` 查的是别人的权限，
/// 用被查的那个角色自己去跑，语义上是绕的。
/// 也不接 PgBouncer —— 审计读目录，不需要连接池，且 `transaction` 模式下
/// 会话语义会让排查变复杂（eng/02 §一）。
const AUDIT_URL_ENV: &str = "TGM_DB_URL_OWNER";

/// 跑 RLS 静态审计，返回是否有断言报红。
///
/// 报告格式刻意做成「红的打全部违规行，绿的只打一行」：
/// eng/04 §五 说得很直接 —— 全绿这个状态本身不携带信息，
/// 所以绿的输出越短越好，红的输出要能直接拿去修。
async fn audit_rls() -> anyhow::Result<bool> {
    let url = std::env::var(AUDIT_URL_ENV)
        .map_err(|_| anyhow::anyhow!("{AUDIT_URL_ENV} 未设置 —— 审计需要一个跑完全部迁移的库"))?;
    let pool = sqlx::PgPool::connect(&url).await?;
    let outcomes = tgm_db::audit::run(&pool).await?;
    pool.close().await;

    let mut failed = 0usize;
    let mut errored = 0usize;
    for o in &outcomes {
        // 报错与违规分开打。两者都让退出码非 0，但修法完全不同：
        // 违规要改库，报错是断言自己写坏了 —— 混成一句会把人引向错的方向
        if let Some(e) = &o.error {
            failed += 1;
            errored += 1;
            println!("ERR  {} {} —— 断言自身执行失败", o.id, o.title);
            println!("       {e}");
        } else if o.failed() {
            failed += 1;
            println!("FAIL {} {} —— {} 项违规", o.id, o.title, o.violations.len());
            for v in &o.violations {
                println!("       {v}");
            }
        } else {
            println!("ok   {} {}", o.id, o.title);
        }
    }
    println!("\n{} 条断言，{failed} 条报红", outcomes.len());
    if errored > 0 {
        println!("其中 {errored} 条是断言自身报错 —— 这些断言什么都没检查，先修它们");
    }
    if failed == 0 {
        // 这句不是客套。ADR-0022 的代价一栏第一条就是「断言全绿不等于隔离正确」，
        // 而读 CI 日志的人看到的就是这最后一行 —— 把限度写在它旁边。
        println!("注意：全绿只说明配置存在，不说明策略写对（eng/04 §五）。");
        println!("      USING (true) 能通过全部断言。隔离由 eng/03 §三 的反向测试证明。");
    }
    Ok(failed > 0)
}

fn main() -> anyhow::Result<()> {
    let cmd = Cli::parse().command;
    if let Command::AuditRls = cmd {
        // 只有这一条子命令需要异步运行时，故在此处而不是 main 上建 ——
        // `serve` 落地时会各自需要不同的运行时配置（eng/00 §二 的 tokio features）。
        let rt = tokio::runtime::Runtime::new()?;
        if rt.block_on(audit_rls())? {
            // 退出码 1 是这条卡口的全部意义所在。
            std::process::exit(1);
        }
        return Ok(());
    }
    match cmd {
        Command::Serve { role } => not_implemented(&format!("serve --role {}", role.as_str())),
        Command::Migrate => not_implemented("migrate"),
        Command::AuditRls => unreachable!("已在上面处理"),
        Command::Openapi { check } => {
            not_implemented(if check { "openapi --check" } else { "openapi" })
        }
    }
    Ok(())
}
