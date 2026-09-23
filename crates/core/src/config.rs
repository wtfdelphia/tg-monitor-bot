//! 启动期配置校验。定义处是 docs/design/eng/01-配置与Secret.md §四，
//! 变量清单与「哪个角色需要哪些」在同文档 §二。
//!
//! 本模块只管**能在不连库时判定**的两条：§4.1（必填项与角色匹配）与
//! §4.5（`RUST_LOG` 过滤器）。要连库的 §4.2/§4.3 在 `tgm-db` 的 `preflight`
//! 里 —— 拆开的理由是本模块可以是纯函数，而纯函数的断言不需要跑容器。
//!
//! §四 的元规则：**把全部缺失项收集齐再一次性报错**。所以这里返回的是
//! `Vec<ConfigProblem>` 而不是在第一个问题上 `return Err` —— 按角色配十几个
//! 变量，逐个试错要重启十几次。

use crate::profile::Profile;

/// 一条配置问题。`Display` 出来的文本就是运维要读的那一行。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigProblem {
    #[error("缺少 {0}（该角色必填，见 eng/01 §二）")]
    Missing(&'static str),

    /// §4.1 明写「多配的项也要报」。理由不是整洁：`control-plane` 上出现
    /// `TGM_LOCAL_KEK`，说明部署模板把所有变量喂给了所有角色 ——
    /// 那让 §二 的权限收敛整个失效，而系统照样启动得起来。
    #[error("多配了 {0}（该角色不需要它，配了说明部署模板没按角色收敛，见 eng/01 §4.1）")]
    Unexpected(&'static str),

    /// §4.5。production 下 `RUST_LOG` 显式给了却不含 `sqlx=warn` 时拒绝启动。
    #[error(
        "production 下 RUST_LOG 显式指定却不含 sqlx=warn（现为 {0:?}）—— \
         sqlx 把绑定参数打进日志，identity_secrets 的写入绑的是密文与 nonce，\
         见 eng/01 §4.5"
    )]
    SqlxLogNotSilenced(String),

    /// `TGM_PROFILE` 缺失。它本可以走 `Missing`，但单独一条是因为它的**后果**
    /// 不同：profile 定不下来时 §4.5 那条判不了（见 `check` 里对 `None` 的处理），
    /// 所以这一条同时是「本轮有一条检查没跑」的记号。
    #[error("缺少 TGM_PROFILE（见 eng/01 §二）—— 它缺失时 §4.5 的日志过滤器检查跳过")]
    ProfileMissing,

    /// `TGM_PROFILE` 取值不认识。**不兜底成 development** ——
    /// 那会让拼错的生产部署拿到宽松档（`profile.rs` 的 `near_misses` 那条测试）。
    #[error(
        "TGM_PROFILE 只接受 production 或 development，收到 {0:?} —— \
         取值不认识时 §4.5 的日志过滤器检查跳过"
    )]
    ProfileUnknown(String),
}

/// 进程角色。取值与 `tgm serve --role` 一一对应（eng/00 §一）。
///
/// 这里再定一份而不是复用 bin 里那个 clap enum：`tgm-core` 不依赖 clap，
/// 而必填集是**配置规则**，属于领域知识，不该住在参数解析层。
/// 两份的一致性由 `bins/tgm` 那边的 `From` 转换承担（一处映射，可 grep）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    ControlPlane,
    BotGateway,
    MtprotoWorker,
    Fanout,
    Rule,
    Delivery,
}

/// 全部角色都必填的项（eng/01 §二 的「全部」行）。
///
/// `TGM_DB_URL_OWNER` **不在任何角色的必填集里**，而且在下面被显式禁掉 ——
/// §二 那句「常驻进程不得持有」是硬纪律：拿到它就等于可以用 owner 身份连进来，
/// spec/05 §3.3 纪律 1（应用角色非 owner）在运行时失去意义。
const COMMON: &[&str] = &[
    "TGM_PROFILE",
    "TGM_DB_URL_APP",
    "TGM_REDIS_URL",
    // RUST_LOG 不在必填里：§二 给了默认值 `info,sqlx=warn`。
    // 「显式给了却不含 sqlx=warn」是 §4.5 的事，由 check_rust_log 管。
];

/// 该角色额外必填的项。
fn extra_required(role: Role) -> &'static [&'static str] {
    match role {
        Role::ControlPlane => &[
            "TGM_DB_URL_AUTH",
            "TGM_DB_URL_OPS",
            "TGM_OIDC_BOT_ID",
            "TGM_OIDC_ALG",
            "TGM_OIDC_REDIRECT_URL",
            "TGM_CONTROL_BOT_TOKEN",
            // TGM_LISTEN_ADDR 不必填：§二 给了默认 127.0.0.1:8080，
            // 且那个默认值本身是安全选择（配错时失败方向是「连不上」而非「上公网」）
        ],
        Role::BotGateway => &["TGM_DB_URL_AUTH", "TGM_KEK_BACKEND"],
        Role::MtprotoWorker => &["TGM_KEK_BACKEND", "TGM_S3_ENDPOINT", "TGM_S3_BUCKET"],
        Role::Delivery => &["TGM_KEK_BACKEND", "TGM_S3_ENDPOINT", "TGM_S3_BUCKET"],
        // fanout 与 rule 只要 COMMON。它们不碰凭据也不碰对象存储 ——
        // 这正是 §二 那句「某个角色不需要的项，就不该配给它」想达到的形状
        Role::Fanout | Role::Rule => &[],
    }
}

/// 这个角色**允许**出现的项。不在这里面的都算多配（§4.1）。
///
/// 与必填集的差集是「可选项」：`TGM_LISTEN_ADDR`（有默认值）、
/// `TGM_KMS_KEY_ARN` / `TGM_LOCAL_KEK`（取决于 `TGM_KEK_BACKEND`）、
/// 以及 `RUST_LOG` 和 AWS 那两个。
fn allowed(role: Role) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = COMMON.to_vec();
    v.push("RUST_LOG");
    v.extend_from_slice(extra_required(role));
    match role {
        Role::ControlPlane => v.push("TGM_LISTEN_ADDR"),
        Role::BotGateway | Role::MtprotoWorker | Role::Delivery => {
            v.extend_from_slice(&["TGM_KMS_KEY_ARN", "TGM_LOCAL_KEK"]);
        }
        Role::Fanout | Role::Rule => {}
    }
    if matches!(role, Role::MtprotoWorker | Role::Delivery) {
        // 名字固定，由 aws-config 读（§二）。所以是「允许」而非「必填」——
        // 用 IAM 角色跑的时候这两个本来就不该存在
        v.extend_from_slice(&["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]);
    }
    v
}

/// 无论什么角色都不得出现的项。
///
/// 单独列出来而不是靠 `allowed` 的补集，是因为这一条的性质不同：
/// 多配 `TGM_LOCAL_KEK` 是部署模板不收敛，而常驻进程持有 owner 连接串
/// 是一条硬纪律被破（§二 末段）。两者都报 `Unexpected`，但前者可能是疏忽，
/// 后者一定要停下来看。
const NEVER: &[&str] = &["TGM_DB_URL_OWNER"];

/// §4.1 + §4.5 的全部检查。`present` 是「这些变量存在且非空」的集合。
///
/// `raw_profile` 收的是**未解析的原始值**（`None` = 变量缺失），解析在本函数内。
/// 早先的签名要一个解析好的 `Profile`，于是调用方必须先解析、失败就早退 ——
/// 那让 `TGM_PROFILE` 缺失或拼错时只报它自己一条，同时缺的别的变量下一轮才看见，
/// 与 §四 的元规则「缺失项收齐再一次报」不同形。收进来之后 profile 出问题
/// 也只是一条 problem，别的检查照跑。
///
/// 代价写明：profile 定不下来时 §4.5 那条**判不了**（它以 `is_production()`
/// 为条件），所以那两条 problem 的文案里带了「§4.5 跳过」——
/// 一条没跑的检查不能长得像一条通过的检查。
///
/// 取 `present`/`rust_log` 作参数而不是自己读 `std::env`：与 `EnvKek::new`
/// 同一条理由 —— 读 env 的版本只能靠改进程环境来测，而 env 是进程全局的，
/// 并行测试会互相干扰，于是这条最关键的断言会变成一条偶尔跑的测试。
/// 真正读 env 的那一层在 `bins/tgm`，只有一处，可 grep。
pub fn check(
    role: Role,
    raw_profile: Option<&str>,
    present: &[&str],
    rust_log: Option<&str>,
) -> Vec<ConfigProblem> {
    let mut problems = Vec::new();

    // profile 先解析，但失败不早退 —— 收成 problem，让后面的检查照跑。
    let profile = match raw_profile {
        None => {
            problems.push(ConfigProblem::ProfileMissing);
            None
        }
        Some(raw) => match Profile::parse(raw) {
            Ok(p) => Some(p),
            Err(_) => {
                problems.push(ConfigProblem::ProfileUnknown(raw.to_string()));
                None
            }
        },
    };

    let mut required: Vec<&'static str> = COMMON.to_vec();
    required.extend_from_slice(extra_required(role));
    for name in required {
        // TGM_PROFILE 也在 COMMON 里（它确实是必填项，那张表不该为本函数的
        // 实现细节挖个洞），但它缺失时上面已经报了 ProfileMissing ——
        // 同一件事报两行会让运维以为是两个问题。
        if name == "TGM_PROFILE" {
            continue;
        }
        if !present.contains(&name) {
            problems.push(ConfigProblem::Missing(name));
        }
    }

    let ok = allowed(role);
    for name in NEVER {
        if present.contains(name) {
            problems.push(ConfigProblem::Unexpected(name));
        }
    }
    // 多配的判定只对**已知**变量名生效。未知名字一律放过 ——
    // 进程环境里本来就有 PATH、HOME、LANG 一堆东西，按「不在白名单就报」
    // 会让这条检查在任何真实环境里都报几十条，那等于没有这条检查。
    for name in known_names() {
        if present.contains(name) && !ok.contains(name) && !NEVER.contains(name) {
            problems.push(ConfigProblem::Unexpected(name));
        }
    }

    // profile 是 None 时这一条判不了，跳过。跳过这件事本身已经在
    // ProfileMissing / ProfileUnknown 的文案里说了 —— 否则 profile 拼错的
    // production 部署会看到一份「RUST_LOG 没报错」的输出，而那条根本没跑。
    if let Some(filter) = rust_log
        && profile.is_some_and(Profile::is_production)
        && !filter.contains("sqlx=warn")
    {
        problems.push(ConfigProblem::SqlxLogNotSilenced(filter.to_string()));
    }

    problems
}

/// eng/01 §二 那张表里的全部变量名，加上 `NEVER` 里的。
///
/// 这份清单是「多配」判定的定义域。表里加了新变量而这里漏了，后果是
/// **那个变量的多配永远不会被报** —— 一处静默的覆盖缺口。
/// 没有机器化的办法把它和文档表格绑住（同 eng/03 §八 末条），所以它在这里，
/// 紧挨着上面那个 `allowed`，改一处时另一处在同屏。
fn known_names() -> &'static [&'static str] {
    &[
        "TGM_PROFILE",
        "RUST_LOG",
        "TGM_DB_URL_APP",
        "TGM_DB_URL_AUTH",
        "TGM_DB_URL_OPS",
        "TGM_DB_URL_OWNER",
        "TGM_REDIS_URL",
        "TGM_S3_ENDPOINT",
        "TGM_S3_BUCKET",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "TGM_KEK_BACKEND",
        "TGM_KMS_KEY_ARN",
        "TGM_LOCAL_KEK",
        "TGM_OIDC_BOT_ID",
        "TGM_OIDC_ALG",
        "TGM_OIDC_REDIRECT_URL",
        "TGM_CONTROL_BOT_TOKEN",
        "TGM_LISTEN_ADDR",
    ]
}

#[cfg(test)]
// 同 kek.rs：测试里的 expect/unwrap 就是断言本身。
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{COMMON, ConfigProblem, Role, check, extra_required};

    /// 该角色的一份「刚好合规」的环境。用它当基线，每条测试只动一处。
    fn minimal(role: Role) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = COMMON.to_vec();
        v.extend_from_slice(extra_required(role));
        v
    }

    /// 反证的锚：基线本身必须干净。没有这一条，下面每一条「恰好报 N 项」
    /// 都可能是基线自带问题凑出来的。
    #[test]
    fn minimal_env_is_clean() {
        for role in [
            Role::ControlPlane,
            Role::BotGateway,
            Role::MtprotoWorker,
            Role::Fanout,
            Role::Rule,
            Role::Delivery,
        ] {
            let p = check(role, Some("development"), &minimal(role), None);
            assert!(p.is_empty(), "{role:?} 的最小环境不该有问题：{p:?}");
        }
    }

    /// §四 的元规则：全部缺失项一次性收齐，不是撞到第一个就退。
    #[test]
    fn all_missing_are_collected_at_once() {
        let p = check(Role::ControlPlane, Some("development"), &[], None);
        let missing: Vec<_> = p
            .iter()
            .filter(|x| matches!(x, ConfigProblem::Missing(_)))
            .collect();
        // TGM_PROFILE 走的是 ProfileMissing 那条，不重复报 Missing —— 这里按名字
        // 排掉而不是写 `COMMON.len() - 1`：往 COMMON 里加变量时这个数字要跟着动，
        // 而一个写死的减法看不出它减的是谁。
        let expect = COMMON.iter().filter(|n| **n != "TGM_PROFILE").count()
            + extra_required(Role::ControlPlane).len();
        assert_eq!(
            missing.len(),
            expect,
            "空环境应当一次报齐 {expect} 项，实际 {missing:?}"
        );
    }

    /// 元规则对 `TGM_PROFILE` 自己也要成立。这条钉的是一个真出现过的形状：
    /// `check` 早先收解析好的 `Profile`，于是调用方在解析失败时就早退了 ——
    /// profile 缺失或拼错时只报它一条，同时缺的别的变量下一轮才看见。
    #[test]
    fn profile_problems_do_not_hide_other_missing_vars() {
        for raw in [None, Some("prod")] {
            let p = check(Role::Rule, raw, &[], None);
            assert!(
                p.contains(&ConfigProblem::Missing("TGM_DB_URL_APP"))
                    && p.contains(&ConfigProblem::Missing("TGM_REDIS_URL")),
                "raw_profile={raw:?} 时别的缺失项被 profile 的问题挡住了：{p:?}"
            );
        }
    }

    /// 同一件事只报一行。`TGM_PROFILE` 同时在 `COMMON` 里，所以缺失时有两条
    /// 路径想报它 —— 报两行会让运维以为是两个问题。
    #[test]
    fn missing_profile_is_reported_once() {
        let env = minimal(Role::Rule);
        let p = check(Role::Rule, None, &env, None);
        assert_eq!(p, vec![ConfigProblem::ProfileMissing], "应当恰好这一条");
    }

    /// profile 定不下来时 §4.5 **判不了**（它以 `is_production` 为条件），
    /// 于是一个拼错 profile 的 production 部署会看到「RUST_LOG 没被报」——
    /// 那条根本没跑。代价换不掉，但不能长得像一条通过的检查：
    /// 这里钉住那条 problem 的文案必须把跳过说出来。
    #[test]
    fn a_skipped_rust_log_check_says_so() {
        let env = minimal(Role::Rule);
        let p = check(Role::Rule, Some("prod"), &env, Some("debug"));
        assert_eq!(
            p,
            vec![ConfigProblem::ProfileUnknown("prod".to_string())],
            "拼错的 profile 应当只报它自己，§4.5 无从判定：{p:?}"
        );
        assert!(
            p[0].to_string().contains("§4.5"),
            "没跑的检查必须在文案里现身：{}",
            p[0]
        );
    }

    /// §4.1「多配的项也要报」。用的例子就是那条原话里的：
    /// control-plane 上出现 TGM_LOCAL_KEK。
    #[test]
    fn control_plane_with_local_kek_is_reported() {
        let mut env = minimal(Role::ControlPlane);
        env.push("TGM_LOCAL_KEK");
        let p = check(Role::ControlPlane, Some("development"), &env, None);
        assert_eq!(
            p,
            vec![ConfigProblem::Unexpected("TGM_LOCAL_KEK")],
            "应当恰好报这一项"
        );
    }

    /// 反证上一条不是「凡是 TGM_LOCAL_KEK 都报」：bot-gateway 上它是允许的。
    /// 没有这一条，一个把 TGM_LOCAL_KEK 写进 NEVER 的版本也会让上一条绿，
    /// 而那会让 backend=env 的本地开发整个起不来。
    #[test]
    fn same_var_is_allowed_on_a_role_that_needs_it() {
        let mut env = minimal(Role::BotGateway);
        env.push("TGM_LOCAL_KEK");
        let p = check(Role::BotGateway, Some("development"), &env, None);
        assert!(p.is_empty(), "bot-gateway 带 TGM_LOCAL_KEK 不该报：{p:?}");
    }

    /// §二 末段那条硬纪律：常驻进程不得持有 owner 连接串。
    /// 对每个角色都验一遍 —— 它不是某个角色的规则。
    #[test]
    fn owner_url_is_refused_for_every_role() {
        for role in [
            Role::ControlPlane,
            Role::BotGateway,
            Role::MtprotoWorker,
            Role::Fanout,
            Role::Rule,
            Role::Delivery,
        ] {
            let mut env = minimal(role);
            env.push("TGM_DB_URL_OWNER");
            let p = check(role, Some("development"), &env, None);
            assert!(
                p.contains(&ConfigProblem::Unexpected("TGM_DB_URL_OWNER")),
                "{role:?} 持有 owner 连接串竟然没报：{p:?}"
            );
        }
    }

    /// 未知变量名不报。这条是保护检查本身的可用性：
    /// 真实环境里有 PATH/HOME/LANG 一堆东西，按「不在白名单就报」
    /// 会让这条检查在任何地方都报几十条，那等于没有它。
    #[test]
    fn unknown_names_are_ignored() {
        let mut env = minimal(Role::Rule);
        env.extend_from_slice(&["PATH", "HOME", "LANG", "HOSTNAME"]);
        let p = check(Role::Rule, Some("development"), &env, None);
        assert!(p.is_empty(), "无关的环境变量不该报：{p:?}");
    }

    /// §4.5。三种形态都要覆盖：不给（走默认，放过）、给了含 sqlx=warn（放过）、
    /// 给了不含（production 下报）。
    #[test]
    fn rust_log_must_silence_sqlx_in_production() {
        let env = minimal(Role::Rule);
        let cases: [(Option<&str>, bool); 4] = [
            (None, false),
            (Some("info,sqlx=warn"), false),
            (Some("debug"), true),
            (Some("info,sqlx=debug"), true),
        ];
        for (filter, should_report) in cases {
            let p = check(Role::Rule, Some("production"), &env, filter);
            let reported = p
                .iter()
                .any(|x| matches!(x, ConfigProblem::SqlxLogNotSilenced(_)));
            assert_eq!(
                reported, should_report,
                "RUST_LOG={filter:?} 的判定不对：{p:?}"
            );
        }
    }

    /// 上一条只在 production 下成立。development 下同一个 RUST_LOG 必须放过 ——
    /// 否则本地开发调 sqlx 日志就得改 profile，而那条路通向「production 也开着」。
    #[test]
    fn rust_log_is_not_checked_in_development() {
        let env = minimal(Role::Rule);
        let p = check(Role::Rule, Some("development"), &env, Some("debug"));
        assert!(p.is_empty(), "development 下不该管 RUST_LOG：{p:?}");
    }
}
