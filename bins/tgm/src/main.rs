//! 单二进制多 subcommand（eng/00 §一）。7 个组件共用一个 bin，
//! 靠 `--role` 区分进程角色 —— 首发部署形态就是单机跑多进程（ops/00）。
//!
//! 本文件目前只有命令骨架：每个子命令都还没有实现体。
//! 骨架先落地是因为 eng/04 §四 的五条卡口里有三条要求 `tgm` 这个二进制存在。

use clap::{Parser, Subcommand, ValueEnum};
use tgm_core::config::Role as CoreRole;
use tgm_core::kek::Kek;

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

/// clap 的角色 → 领域层的角色。**全仓库唯一一处映射**。
///
/// 两份枚举的存在理由在 `tgm_core::config::Role` 的注释里（core 不依赖 clap，
/// 而必填集是领域知识）。代价就是这里：加一个角色时若漏了这一处，编译器会报
/// non-exhaustive match —— 这是为什么不写 `_ => ` 兜底。
impl From<&Role> for CoreRole {
    fn from(r: &Role) -> Self {
        match r {
            Role::ControlPlane => CoreRole::ControlPlane,
            Role::BotGateway => CoreRole::BotGateway,
            Role::MtprotoWorker => CoreRole::MtprotoWorker,
            Role::Fanout => CoreRole::Fanout,
            Role::Rule => CoreRole::Rule,
            Role::Delivery => CoreRole::Delivery,
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
/// `openapi --check` 也移出去了，但**不是因为实现了**：它在卡口清单里，
/// 所以改成 `exit(1)`。恒 0 的退出码对 CI 来说与「通过」不可区分，
/// 而这个函数恰恰只会把话说给人听。
/// `migrate` 随后也移出去了（真接了库）。
///
/// 剩下两个调用点 —— `serve` 与 `openapi`（不带 `--check` 的那支）。
fn not_implemented(what: &str) {
    eprintln!("tgm: {what}: NOT IMPLEMENTED（骨架阶段，退出码 0 不代表校验通过）");
}

/// 跑迁移。ops/00 §2.1 的第 1 步：用 `TGM_DB_URL_OWNER`，跑完即结束。
///
/// **迁移文件在编译期嵌进二进制**（`sqlx::migrate!`），不在运行时读磁盘。
/// 理由是部署形态：那台机器上只有一个二进制，没有仓库，`--source migrations`
/// 无从指向。副作用刚好是 eng/00 §一 想要的那条保证 —— `tgm migrate` 与
/// `sqlx migrate run --source migrations` 指向同一份文件，由编译期路径保证，
/// 不靠人记得同步。
///
/// 这里**不做** eng/01 §4.2 的 PG 版本下限检查。那条是 `serve` 的启动期校验，
/// 而这条命令跑在业务进程之前：版本不够时该拦住的是流量，不是 DDL。
/// 真正的结构性把关是 §2.1 的第 2 步 `audit-rls`，它在这条之后立刻跑。
///
/// 已应用过的文件被改动时 sqlx 在这里就会报
/// `migration N was previously applied but has been modified` 并且**一条都不执行**
/// —— 校验和存的是整个文件的哈希，不区分注释与 DDL（eng/02 §3.2）。
async fn migrate() -> anyhow::Result<()> {
    let url = std::env::var(MIGRATE_URL_ENV).map_err(|_| {
        anyhow::anyhow!("{MIGRATE_URL_ENV} 未设置 —— 迁移要 owner 身份（ops/00 §2.1）")
    })?;
    let pool = sqlx::PgPool::connect(&url).await?;
    // 每个 DDL 文件自己 `SET ROLE tgm_owner`（见 0002 开头的注释）——
    // 表的 owner 决定 FORCE 那一层是真的还是装饰，所以不能靠连接身份碰巧对。
    let outcome = sqlx::migrate!("../../migrations").run(&pool).await;
    let applied =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE success")
            .fetch_one(&pool)
            .await;
    pool.close().await;
    outcome?;
    // 打出条数而不是只说「成功」：一次什么都没做的 run 和一次跑了 12 个文件的 run
    // 在退出码上同形，而前者在「忘了加新迁移」时正是会发生的事。
    eprintln!("tgm: 迁移完成：{} 个版本已应用", applied?);
    Ok(())
}

/// 迁移用的连接串。ops/00 §2.1 与 eng/01 §二 都指定 owner 身份 ——
/// 建表、开 RLS、发 GRANT 都不是 `app_user` 能做的事。
/// 与 `AUDIT_URL_ENV` 是同一个变量名，但两条命令的用途不同，各自写出来
/// 比共享一个常量清楚：将来审计若改用更低权限的角色，这里不该跟着变。
const MIGRATE_URL_ENV: &str = "TGM_DB_URL_OWNER";

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

/// **全仓库唯一一处读 `std::env` 的地方**（`tgm_core::config` 的注释承诺了这一点，
/// 上面的 `audit_rls` 是例外：它是运维命令，不是常驻进程的启动路径）。
///
/// 收的是「存在且非空」的变量名。空串算不存在，因为部署模板里
/// `TGM_LOCAL_KEK=` 这种写法和没写是一个意思，而按「存在」判会让它躲过必填检查。
fn present_env_names() -> Vec<String> {
    std::env::vars()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, _)| k)
        .collect()
}

/// eng/01 §四 的启动期校验。全部六项按文档顺序，**但缺失项收齐再一次报**。
///
/// 顺序不是随意的：不连库的两条（§4.1/§4.5）先跑，因为连库那两条要用
/// `TGM_DB_URL_APP`，而那个变量缺失时 §4.2 的报错会是「连不上」——
/// 指向了错的方向。
///
/// 返回校验过的 `Profile` 给调用方装 subscriber 用。**不在这里装** ——
/// 本函数自己的输出必须在 subscriber 存在之前就可见（配置错误是它要报的第一件事），
/// 所以这一路全是 `eprintln!`，而 `telemetry::init` 只能在它返回之后。
async fn preflight(role: &Role) -> anyhow::Result<tgm_core::profile::Profile> {
    let core_role = CoreRole::from(role);

    // TGM_PROFILE 不在这里解析：连原始值一起交给 check()，让它失败时也只是
    // 一条 problem。早先这里是 `Profile::parse(...)?`，于是 profile 缺失或拼错
    // 时只报它一条，同时缺的别的变量下一轮才看见 —— §四 的元规则被自己的
    // 实现顺序破掉了。空串按缺失算，与 present_env_names 同一口径。
    let raw_profile = std::env::var("TGM_PROFILE").ok().filter(|s| !s.is_empty());

    let names = present_env_names();
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let rust_log = std::env::var("RUST_LOG").ok();
    let problems = tgm_core::config::check(
        core_role,
        raw_profile.as_deref(),
        &borrowed,
        rust_log.as_deref(),
    );
    if !problems.is_empty() {
        // 一次性打印，不 `?` 在第一条上 —— §四 的元规则。
        for p in &problems {
            eprintln!("tgm: 配置错误：{p}");
        }
        anyhow::bail!("{} 项配置错误（见上），拒绝启动", problems.len());
    }

    // 到这里 profile 一定是合法的（上面那两条 problem 会先让我们退出），
    // 但类型上还是 Option<&str> —— 再解析一次而不是让 check 把它返回出来：
    // check 的返回值是「问题清单」，往里塞一个成功值会让它同时承担两种语义。
    let raw_profile =
        raw_profile.ok_or_else(|| anyhow::anyhow!("TGM_PROFILE 缺失 —— 上一步本该拦住"))?;
    let profile = tgm_core::profile::Profile::parse(&raw_profile)?;

    // §4.2 与 §4.3 都走业务连接池的那条连接串：检查的对象就是**这条连接**
    // 的属性，换一条连接去查等于没查（实测 server_version_num 经 PgBouncer
    // 的 transaction 模式可以透传，所以不需要旁路直连）。
    let url = std::env::var("TGM_DB_URL_APP")
        .map_err(|_| anyhow::anyhow!("缺少 TGM_DB_URL_APP —— 上一步本该拦住，这里是兜底"))?;
    let pool = sqlx::PgPool::connect(&url).await?;
    let version = tgm_db::preflight::check_version(&pool).await;
    let db_role = tgm_db::preflight::check_runtime_role(&pool).await;
    pool.close().await;
    let version = version?;
    // 这一行是事后唯一能回答「那次运行到底用的哪个角色」的证据。
    eprintln!(
        "tgm: 启动期校验通过：profile={raw_profile} pg={version} db_role={}",
        db_role?
    );

    // §4.4。`backend=env` 时顺带校验 TGM_LOCAL_KEK 确实是 32 字节 ——
    // 那是 EnvKek::new 做的事，这里只负责调它。
    // backend=kms 的自测要等 KmsKek 落地（它依赖 aws-sdk-kms，不在 tgm-core）。
    match std::env::var("TGM_KEK_BACKEND").as_deref() {
        Ok("env") => {
            let raw = std::env::var("TGM_LOCAL_KEK")
                .map_err(|_| anyhow::anyhow!("TGM_KEK_BACKEND=env 但缺少 TGM_LOCAL_KEK"))?;
            let kek = tgm_core::kek::EnvKek::new(profile, &raw)?;
            tgm_core::kek::self_test(&kek).await?;
            // kek_id 打进日志是安全的（它是标识，不是密钥材料），而且必要：
            // 轮换之后要能从日志里回答「那条密文是哪个 KEK 包的」。
            eprintln!(
                "tgm: KEK 往返自测通过：backend=env kek_id={}",
                Kek::kek_id(&kek)
            );
        }
        Ok("kms") => {
            // 这里是空的，而且必须被看见 —— 一个静默跳过的自测与通过的自测同形。
            eprintln!(
                "tgm: KEK 往返自测：NOT IMPLEMENTED（backend=kms 待 KmsKek 落地，eng/01 §4.4）"
            );
        }
        Ok(other) => anyhow::bail!("TGM_KEK_BACKEND 只接受 kms 或 env，收到 {other:?}"),
        // 不配这个变量的角色（fanout / rule）就不该做 KEK 自测。
        // 该配却没配的情况由上面的 §4.1 检查拦。
        Err(_) => {}
    }

    Ok(profile)
}

fn main() -> anyhow::Result<()> {
    let cmd = Cli::parse().command;
    if let Command::Migrate = cmd {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(migrate())?;
        return Ok(());
    }
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
    if let Command::Serve { role } = &cmd {
        // 启动期校验要跑，**哪怕 serve 本体还没实现**。
        // 理由：这六项校验管的是「这个部署的配置对不对」，与业务代码是否落地无关；
        // 而且 `?` 往外抛让退出码非 0 —— 这是 serve 这条路径上第一个
        // 不是空壳的断言（对比 not_implemented 那个恒 0 的退出码）。
        let rt = tokio::runtime::Runtime::new()?;
        let profile = rt.block_on(preflight(role))?;
        // subscriber 在校验之后装：装它要 profile，而 profile 的合法性是校验的产出。
        // 代价是 preflight 的输出走 stderr 而非结构化日志 —— 接受，因为
        // 「配置错误导致拒绝启动」这条消息的读者是人，不是日志管道。
        tgm_core::telemetry::init(profile);
        tracing::info!(
            role = role.as_str(),
            "subscriber 已装（eng/00 §5.1）—— 本行之前的输出都在 stderr"
        );
    }
    match cmd {
        Command::Serve { role } => not_implemented(&format!("serve --role {}", role.as_str())),
        Command::Migrate => unreachable!("已在上面处理"),
        Command::AuditRls => unreachable!("已在上面处理"),
        Command::Openapi { check } => {
            if check {
                // **这一条退 1，不退 0。** 它是 eng/04 §四 卡口清单里的一员，
                // 而一条恒 0 的卡口与一条通过的卡口在 CI 里完全同形 ——
                // 契约漂移检查还不存在这件事，必须由退出码说出来，
                // 不能只靠一行没人看的 stderr（`not_implemented` 的注释里
                // 写着「机器看不见」，这就是那个代价兑现的地方）。
                // 代价是这条卡口在 WP-5 落地前进不了 CI；ci.yml 里据此
                // 显式注掉并注明原因，而不是让它假绿。
                eprintln!(
                    "tgm: openapi --check: NOT IMPLEMENTED（openapi.json 不存在，\
                     契约导出随 WP-5 落地 —— 退出码 1 是刻意的，见 eng/04 §六）"
                );
                std::process::exit(1);
            }
            not_implemented("openapi");
        }
    }
    Ok(())
}
