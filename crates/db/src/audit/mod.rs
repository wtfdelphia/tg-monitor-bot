//! RLS 静态审计的执行器。断言全文在同目录的 `queries.sql`，
//! 定义处是 `docs/design/eng/04-CI门禁.md`（ADR-0022：断言替代第二人复核）。
//!
//! 本模块只做三件事：切分 SQL、逐条执行、把违规行原样收集起来。
//! **判据的全部内容在 SQL 里**，Rust 侧不做任何形状判断 ——
//! 这样断言可以整段粘进 psql 复现，排查时不需要读 Rust。
//!
//! 为什么不用 `sqlx::query!` 宏：宏要求编译期已知 SQL 字面量，
//! 而这里是运行时切分出来的多条语句。审计结果只需要「一列文本」这一种形状
//! （`queries.sql` 里每条都套了 `SELECT q::text FROM (...) q`），
//! 用不上宏的类型检查。代价是这些 SQL 不进 `.sqlx/`，
//! 不受 `cargo sqlx prepare --check` 覆盖 —— 改错了要靠本模块的测试发现。

use sqlx::{AssertSqlSafe, PgPool, Row};

/// 一条断言。`id` 与 eng/04 的编号对应（`MAIN` 是 §一 的主查询，
/// `C1`/`C2` 是 §三 的补充断言，`A0` 是本实现新增的元断言）。
#[derive(Debug)]
pub struct Assertion {
    pub id: String,
    pub title: String,
    pub sql: String,
}

/// 一条断言的执行结果。`violations` 非空即失败 ——
/// 这个约定写在 `queries.sql` 的头部，所有断言都查违规项而不是合规项。
///
/// `error` 是第三种状态：这条断言的 SQL 自己报错了，既不是通过也不是违规。
/// 必须与「通过」区分开，否则一条崩掉的断言在汇总里长得跟绿的一样 ——
/// 见 `run()` 上的说明。
#[derive(Debug)]
pub struct Outcome {
    pub id: String,
    pub title: String,
    pub violations: Vec<String>,
    pub error: Option<String>,
}

impl Outcome {
    /// 报错也算失败。断言自己崩了却让退出码为 0，是最坏的一种绿。
    pub fn failed(&self) -> bool {
        !self.violations.is_empty() || self.error.is_some()
    }
}

/// `queries.sql` 在编译期嵌入二进制。
///
/// 不在运行时读文件：`tgm audit-rls` 要能在 CI 容器里跑，
/// 那里只有二进制，没有仓库树。
const QUERIES: &str = include_str!("queries.sql");

/// 按 `-- name: <ID> <标题>` 切分断言。
///
/// 切分而不是整段送给 PG，是为了能逐条报告「哪条断言红了」——
/// 整段执行只会给出最后一个结果集，前面的静默丢弃。
pub fn parse(src: &str) -> Vec<Assertion> {
    let mut out: Vec<Assertion> = Vec::new();
    for line in src.lines() {
        if let Some(rest) = line.strip_prefix("-- name: ") {
            let (id, title) = rest.split_once(' ').unwrap_or((rest, ""));
            out.push(Assertion {
                id: id.to_string(),
                title: title.to_string(),
                sql: String::new(),
            });
        } else if let Some(cur) = out.last_mut() {
            // 首个 `-- name:` 之前的行是文件头注释，`out` 为空时丢弃。
            cur.sql.push_str(line);
            cur.sql.push('\n');
        }
    }
    out
}

/// 逐条执行。**遇到红的不停、遇到报错也不停**，全部跑完再返回 ——
/// 一次修一条会让「改了 A 又碰坏 B」这种情况要跑很多遍才暴露出来。
///
/// 报错那一支原先是 `?` 直接抛出去，与上面这句注释矛盾：
/// 第一条报错的断言会让它后面的断言一条都不执行，而进程退出码仍是 1。
/// 读者看到的是「红了」，看不到「另外 5 条根本没跑」——
/// 又一例「验证手段坏掉时表现得像验证过了」。所以报错收进 `Outcome::error`
/// 继续往下跑，由调用方统一判退出码。
///
/// 连接角色不需要是超级用户：全部断言只读 `pg_catalog`，
/// 而目录对所有角色可读（实测 `app_user` 与 `platform_ops` 都能读到
/// `pg_policy` 的 27 行、`pg_get_expr` 的策略表达式、`pg_get_constraintdef`）。
/// 但**也不该用运行时角色跑**：`has_table_privilege` 等函数查的是别人的权限，
/// 用 `tgm_owner` 跑语义最清楚。这个选择记在 `tgm audit-rls` 的连接串上。
pub async fn run(pool: &PgPool) -> Result<Vec<Outcome>, sqlx::Error> {
    let mut out = Vec::new();
    for a in parse(QUERIES) {
        // sqlx 0.9 的 `SqlSafeStr` 只对 `&'static str` 自动实现，运行时切分出来的
        // String 必须显式 `AssertSqlSafe` 才能执行（编译期报错，不是 warning）。
        // 这里的断言成立：SQL 全部来自编译期 `include_str!` 的常量，
        // 没有任何外部输入参与拼接 —— 唯一的运行时操作是按 `-- name:` 切分。
        // 如果将来有人给断言加参数，必须改成 bind 而不是扩大这个断言的范围。
        let res = sqlx::query(AssertSqlSafe(a.sql.clone()))
            .fetch_all(pool)
            .await
            .and_then(|rows| {
                rows.iter()
                    .map(|r| {
                        r.try_get::<Option<String>, _>(0)
                            .map(|v| v.unwrap_or_default())
                    })
                    .collect::<Result<Vec<_>, _>>()
            });
        let (violations, error) = match res {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        out.push(Outcome {
            id: a.id,
            title: a.title,
            violations,
            error,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 切分器不丢断言、不丢 SQL。
    ///
    /// 这条测试防的是一种静默失效：切分正则写错导致某几条断言根本没被执行，
    /// 而 `tgm audit-rls` 照样退出 0 —— 与 A0 兜的那类「静默全绿」同形。
    #[test]
    fn parse_keeps_every_assertion() {
        let parsed = parse(QUERIES);
        // 数量对得上 eng/04：主查询 1 + A0~A9（A2 拆成 a/b，共 11）
        // + C1/C4/C2/C5 四条 = 16。C4（禁物化视图）与 C5（禁登录角色的组成员身份）
        // 是本步骤新增的，不在 eng/04 原文里
        assert_eq!(parsed.len(), 16, "断言条数变了，同步改这里与 eng/04");
        for a in &parsed {
            assert!(!a.id.is_empty(), "断言缺 id");
            assert!(!a.title.is_empty(), "断言 {} 缺标题", a.id);
            assert!(
                a.sql.contains("SELECT"),
                "断言 {} 的 SQL 是空的 —— 切分把它吃掉了",
                a.id
            );
        }
    }

    /// 每条断言都必须把结果收敛成一列文本。
    ///
    /// `run()` 只取第 0 列并按 `Option<String>` 解读。某条断言若直接返回多列或
    /// 非文本列，运行时才会报类型错 —— 而那是在 CI 里、在活库上才发生的。
    /// 这条测试把它提前到 `cargo test`。
    #[test]
    fn every_assertion_returns_single_text_column() {
        for a in parse(QUERIES) {
            assert!(
                a.sql.contains("::text"),
                "断言 {} 没有把结果转成文本列，run() 取值会失败",
                a.id
            );
        }
    }

    /// id 不重复。重复会让报告里出现两条同名断言，修的时候改错文件。
    #[test]
    fn ids_are_unique() {
        let ids: Vec<String> = parse(QUERIES).into_iter().map(|a| a.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "断言 id 有重复");
    }
}
