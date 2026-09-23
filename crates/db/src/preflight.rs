//! 启动期校验里要连库的两条。定义处是
//! ../../../docs/design/eng/01-配置与Secret.md §4.2（版本下限）与 §4.3（角色自检）。
//!
//! 不连库的 §4.1/§4.5 在 `tgm_core::config`。
//!
//! 与 `audit-rls` 的分工在 §4.6 划得很明确：RLS 配置是否完整属审计口径，
//! 不放启动期（每次重启全表扫描）；而这两条是**本进程自己的连接**的属性，
//! 只有连上之后才知道，且每次重启都可能变（换了连接串、改了角色属性）。

use sqlx::{PgPool, Row};

/// 版本下限。`170011` 是 CVE-2026-14666 的修复版本（spec/05），不是性能考量。
///
/// 各大版本的下限不同（§4.2 列了 180006/170011/160015/150019/140024），
/// 所以判定不是「≥ 170011」这么一句 —— 见 [`version_floor_for`]。
const FLOORS: &[(i32, i32)] = &[
    (18, 180006),
    (17, 170011),
    (16, 160015),
    (15, 150019),
    (14, 140024),
];

/// 启动期校验的失败。每一条都必须让进程**拒绝启动**。
///
/// 不设「警告」一级：§四 的原则是「能在启动时发现的配置错误，绝不留到运行时」，
/// 而一条能被忽略的警告在运行时的形态就是没有这条检查。
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("PostgreSQL {found} 低于安全下限 {floor}（CVE-2026-14666 的修复版本，见 eng/01 §4.2）")]
    VersionTooOld { found: i32, floor: i32 },

    #[error("PostgreSQL 大版本 {0} 不在支持列表里（eng/01 §4.2 只给了 14~18 的下限）")]
    UnknownMajor(i32),

    #[error(
        "运行时角色 {role} 带 {attr} —— RLS 对它形同虚设且不会报错，\
         见 eng/01 §4.3 与 spec/05 §3.3 纪律 1"
    )]
    PrivilegedRole { role: String, attr: &'static str },

    /// §4.3 的第二句 SQL。owner 受 FORCE 约束但通常没有匹配它的策略，
    /// 所以这个形态的失败方向不是「泄漏」而是「静默 0 行」（eng/03 §五 实测）——
    /// 两个方向都不可接受，所以一样拒绝启动。
    #[error(
        "运行时角色 {role} 是 {count} 张 public 表的 owner —— \
         应用角色不得是表 owner，见 eng/01 §4.3"
    )]
    RoleOwnsTables { role: String, count: i64 },

    /// 继承自组的 BYPASSRLS。**实测过它是真能旁路的**：一个 NOLOGIN 组角色带
    /// BYPASSRLS、登录角色 `IN ROLE` 它，则登录角色自己的 `rolbypassrls` 是 `f`，
    /// 单查那一列查不出来；`SET ROLE` 到该组之后 RLS 就被旁路（同一张表 0 行 → 2 行）。
    /// 所以 §4.3 给的那句 SQL 有缺口，这条是补的。
    #[error(
        "运行时角色 {role} 是 {grantor} 的成员，而后者带 {attr} —— \
         SET ROLE 过去即可旁路 RLS。§4.3 给的 SQL 只查角色自身的属性，查不到这一层"
    )]
    PrivilegedViaMembership {
        role: String,
        grantor: String,
        attr: &'static str,
    },

    #[error("启动期校验的 SQL 自身执行失败 —— 这不是配置问题，是校验坏了：{0}")]
    Query(#[from] sqlx::Error),
}

/// 该大版本的下限。
fn version_floor_for(major: i32) -> Option<i32> {
    FLOORS.iter().find(|(m, _)| *m == major).map(|(_, f)| *f)
}

/// 判定本身。与取数分开只为一件事：**否定用例跑不起来**。
/// 手边只有一个 17.11 的库，`VersionTooOld` 这条路径在集成测试里永远走不到，
/// 而走不到的分支与不存在的分支在测试结果上长得一样。
fn evaluate_version(found: i32) -> Result<i32, PreflightError> {
    let major = found / 10000;
    let floor = version_floor_for(major).ok_or(PreflightError::UnknownMajor(major))?;
    if found < floor {
        return Err(PreflightError::VersionTooOld { found, floor });
    }
    Ok(found)
}

/// §4.2。经 PgBouncer 也能读到（实测 `transaction` 模式下透传，返回 `170011`）——
/// 所以这条检查不需要一个绕开连接池的旁路连接。
pub async fn check_version(pool: &PgPool) -> Result<i32, PreflightError> {
    // 用 current_setting 而不是 `SHOW`：`SHOW` 是 utility 命令，
    // 返回的列名依版本而变，而 current_setting 是普通函数调用，可以 query_scalar。
    let found: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(pool)
        .await?;
    evaluate_version(found)
}

/// §4.3。检查本进程这条连接的角色：不是超级用户、不带 BYPASSRLS、不是表 owner、
/// 也不是任何带这两个属性的角色的成员。
///
/// 返回角色名，好让调用方把它打进启动日志 —— 那一行日志是唯一能事后回答
/// 「那次运行到底用的哪个角色」的证据。
pub async fn check_runtime_role(pool: &PgPool) -> Result<String, PreflightError> {
    let row = sqlx::query(
        "SELECT current_user::text AS role, rolsuper, rolbypassrls
           FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await?;
    let role: String = row.get("role");
    for (flag, attr) in [("rolsuper", "SUPERUSER"), ("rolbypassrls", "BYPASSRLS")] {
        if row.get::<bool, _>(flag) {
            return Err(PreflightError::PrivilegedRole { role, attr });
        }
    }

    // 组身份那一层。见 PrivilegedViaMembership 的注释 —— 这一段不在 §4.3 里，
    // 是实测补的。`pg_has_role(..., 'USAGE')` 含间接成员，也含角色自身；
    // 自身已由上面那两句判过，这里把它排掉以免同一件事报两次。
    let escalation = sqlx::query(
        "SELECT r.rolname::text AS grantor, r.rolsuper, r.rolbypassrls
           FROM pg_roles r
          WHERE pg_has_role(current_user, r.oid, 'USAGE')
            AND r.rolname <> current_user
            AND (r.rolsuper OR r.rolbypassrls)
          ORDER BY 1 LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    if let Some(row) = escalation {
        let grantor: String = row.get("grantor");
        let attr = if row.get::<bool, _>("rolsuper") {
            "SUPERUSER"
        } else {
            "BYPASSRLS"
        };
        return Err(PreflightError::PrivilegedViaMembership {
            role,
            grantor,
            attr,
        });
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_tables
          WHERE schemaname = 'public' AND tableowner = current_user",
    )
    .fetch_one(pool)
    .await?;
    if count > 0 {
        return Err(PreflightError::RoleOwnsTables { role, count });
    }

    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 判定部分的测试。连库那部分在 tests/preflight.rs —— 那里才有真角色。
    #[test]
    fn floors_are_per_major_not_a_single_number() {
        // 17.11 过，17.10 不过。这一对是这条检查的全部意义：
        // 写成「≥ 170011」的话 18.0（= 180000）会被误判为合规，
        // 而 18 的下限是 180006 —— 下面那条就是钉这个的。
        assert!(evaluate_version(170011).is_ok());
        assert!(matches!(
            evaluate_version(170010),
            Err(PreflightError::VersionTooOld {
                found: 170010,
                floor: 170011
            })
        ));
        assert!(matches!(
            evaluate_version(180000),
            Err(PreflightError::VersionTooOld {
                found: 180000,
                floor: 180006
            })
        ));
    }

    #[test]
    fn every_listed_major_has_its_own_floor_and_passes_at_it() {
        // 反证锚：先确认每个下限值自己是过的。否则上面那条测试的红
        // 可能只是因为 FLOORS 整个是空的。
        for (major, floor) in FLOORS {
            assert!(
                evaluate_version(*floor).is_ok(),
                "{major} 的下限 {floor} 自己都没过 —— FLOORS 写错了"
            );
            assert!(
                evaluate_version(floor - 1).is_err(),
                "{major} 的 {} 竟然过了 —— 下限没生效",
                floor - 1
            );
        }
    }

    #[test]
    fn unsupported_major_is_refused_not_waved_through() {
        // 13 已 EOL、19 还没有下限数据。两个方向都不能默认放过 ——
        // 「不在列表里就通过」会让这条检查在下一个大版本发布时静默失效。
        assert!(matches!(
            evaluate_version(130021),
            Err(PreflightError::UnknownMajor(13))
        ));
        assert!(matches!(
            evaluate_version(190000),
            Err(PreflightError::UnknownMajor(19))
        ));
    }
}
