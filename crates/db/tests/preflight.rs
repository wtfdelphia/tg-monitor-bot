//! 启动期校验连库那两条的实测。定义处是 eng/01 §4.2 与 §4.3。
//!
//! 与 pgbouncer.rs 同一个理由用 `#[tokio::test]` 跑常驻库：这里要比对
//! **两个不同角色**（app_user 应绿、postgres 应红），而 `#[sqlx::test]`
//! 注入的那个池只有超级用户一个身份。也因此本文件不灌任何数据。

// 同 isolation.rs：反向测试里的 expect 与 panic 就是断言本身。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tgm_db::preflight::{PreflightError, check_runtime_role, check_version};

const PREFLIGHT_CATALOG_LOCK: i64 = 0x5447_4d50_5246_4c54;

async fn lock_preflight_catalog(admin: &PgPool) {
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(PREFLIGHT_CATALOG_LOCK)
        .execute(admin)
        .await
        .expect("获取 preflight 目录锁失败");
}

/// 超级用户直连。**只用来证明校验会红** —— 它是本文件的否定用例，
/// 不是给别处取数用的，所以不进 support/mod.rs。
async fn superuser_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect("postgres://postgres:pw@127.0.0.1:55432/tgm")
        .await
        .expect("以 postgres 连 tgm:55432 失败")
}

#[tokio::test]
async fn version_check_passes_on_the_running_server() {
    // 走 PgBouncer，因为业务进程就是走那条 —— 顺带钉住
    // `current_setting('server_version_num')` 在 transaction 模式下确实透传。
    let pool = support::app_pool_via_bouncer().await;
    let found = check_version(&pool)
        .await
        .expect("版本检查在本地 17.11 上应当通过");
    assert!(found >= 170011, "读到的版本号 {found} 不对");
}

#[tokio::test]
async fn runtime_role_check_passes_for_app_user() {
    // 正向锚。这一条绿才能说明下面那条 postgres 的红是角色差异造成的，
    // 而不是这段 SQL 本来就跑不通。
    let pool = support::app_pool_via_bouncer().await;
    support::assert_role(&pool, "app_user").await;
    let role = check_runtime_role(&pool)
        .await
        .expect("app_user 应当通过运行时角色自检");
    assert_eq!(role, "app_user");
}

#[tokio::test]
async fn runtime_role_check_refuses_a_superuser() {
    let pool = superuser_pool().await;
    // 先确认这个池真是超级用户 —— 否则下面的红可能来自别的原因。
    let is_super: bool =
        sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&pool)
            .await
            .expect("查 rolsuper 失败");
    assert!(is_super, "这个池不是超级用户，本测试的否定用例不成立");

    match check_runtime_role(&pool).await {
        Err(PreflightError::PrivilegedRole { role, attr }) => {
            assert_eq!(role, "postgres");
            assert_eq!(attr, "SUPERUSER");
        }
        other => panic!("超级用户竟然通过了运行时角色自检：{other:?}"),
    }
}

#[tokio::test]
async fn role_owning_a_public_table_is_refused() {
    // 这一条不能拿 postgres 来测：它在 rolsuper 那句就红了，
    // RoleOwnsTables 这个分支永远走不到，而走不到的分支与不存在的分支同形。
    // 所以另起一个普通角色，给它一张表。名字前缀 pf_ 与别处的探针错开；
    // 不碰 app_user，否则会和上面那条正向测试抢同一份状态。
    let admin = superuser_pool().await;
    lock_preflight_catalog(&admin).await;
    let suffix = std::process::id();
    let role_name = format!("pf_owner_{suffix}");
    let table_name = format!("pf_owned_{suffix}");
    let cleanup = |admin: PgPool| {
        let role_name = role_name.clone();
        let table_name = table_name.clone();
        async move {
            // 先表后角色：角色拥有对象时 DROP ROLE 会报 2BP01。
            sqlx::query(AssertSqlSafe(format!("DROP TABLE IF EXISTS {table_name}")))
                .execute(&admin)
                .await
                .expect("清理 owner 探针失败");
            let role_exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                    .bind(&role_name)
                    .fetch_one(&admin)
                    .await
                    .expect("检查 owner 探针是否存在失败");
            if role_exists {
                for stmt in [
                    format!("REVOKE CONNECT ON DATABASE tgm FROM {role_name}"),
                    format!("DROP ROLE {role_name}"),
                ] {
                    sqlx::query(AssertSqlSafe(stmt))
                        .execute(&admin)
                        .await
                        .expect("清理 owner 探针失败");
                }
            }
        }
    };
    cleanup(admin.clone()).await;

    for stmt in [
        format!("CREATE ROLE {role_name} LOGIN PASSWORD 'ppw'"),
        format!("GRANT CONNECT ON DATABASE tgm TO {role_name}"),
        format!("CREATE TABLE {table_name} (id bigint)"),
        format!("ALTER TABLE {table_name} OWNER TO {role_name}"),
    ] {
        sqlx::query(AssertSqlSafe(stmt))
            .execute(&admin)
            .await
            .expect("建 owner 探针失败");
    }

    let owner = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("postgres://{role_name}:ppw@127.0.0.1:55432/tgm"))
        .await
        .expect("以 pf_owner 连库失败");
    let verdict = check_runtime_role(&owner).await;
    owner.close().await;
    cleanup(admin.clone()).await;
    admin.close().await;

    match verdict {
        Err(PreflightError::RoleOwnsTables { role, count }) => {
            assert_eq!(role, role_name);
            assert_eq!(count, 1, "数出来的表数不对");
        }
        other => panic!("public 表的 owner 竟然通过了运行时角色自检：{other:?}"),
    }
}

#[tokio::test]
async fn membership_in_a_bypassrls_role_is_caught() {
    // §4.3 给的 SQL 只查角色自身的 rolbypassrls，查不到这一层。
    // 实测过它是真能旁路的：组角色带 BYPASSRLS 时成员自身是 f，
    // `SET ROLE` 过去后同一张表从 0 行变 2 行。这条测试钉的是补上的那段。
    //
    // 探针角色由本测试自己建自己删。名字带 pf_ 前缀避免撞上别处的探针。
    let admin = superuser_pool().await;
    lock_preflight_catalog(&admin).await;
    let suffix = std::process::id();
    let member_name = format!("pf_member_{suffix}");
    let group_name = format!("pf_group_{suffix}");
    let cleanup = |admin: PgPool| {
        let member_name = member_name.clone();
        let group_name = group_name.clone();
        async move {
            for role_name in [&member_name, &group_name] {
                let role_exists: bool =
                    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                        .bind(role_name)
                        .fetch_one(&admin)
                        .await
                        .expect("检查探针角色是否存在失败");
                if role_exists {
                    if role_name == &member_name {
                        sqlx::query(AssertSqlSafe(format!(
                            "REVOKE CONNECT ON DATABASE tgm FROM {role_name}"
                        )))
                        .execute(&admin)
                        .await
                        .expect("撤销探针角色 CONNECT 失败");
                    }
                    sqlx::query(AssertSqlSafe(format!("DROP ROLE {role_name}")))
                        .execute(&admin)
                        .await
                        .expect("清理探针角色失败");
                }
            }
        }
    };
    cleanup(admin.clone()).await; // 上一轮若中途 panic 会留下残留

    sqlx::query(AssertSqlSafe(format!(
        "CREATE ROLE {group_name} NOLOGIN BYPASSRLS"
    )))
    .execute(&admin)
    .await
    .expect("建组角色失败");
    sqlx::query(AssertSqlSafe(format!(
        "CREATE ROLE {member_name} LOGIN PASSWORD 'ppw' IN ROLE {group_name}"
    )))
    .execute(&admin)
    .await
    .expect("建成员角色失败");
    sqlx::query(AssertSqlSafe(format!(
        "GRANT CONNECT ON DATABASE tgm TO {member_name}"
    )))
    .execute(&admin)
    .await
    .expect("给成员角色授 CONNECT 失败");

    let member = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("postgres://{member_name}:ppw@127.0.0.1:55432/tgm"))
        .await
        .expect("以 pf_member 连库失败");

    // 自检：成员自身那一列必须是 f，否则这个测试测的是 PrivilegedRole 那条，
    // 不是 membership 这条。
    let own: bool =
        sqlx::query_scalar("SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&member)
            .await
            .expect("查成员自身 rolbypassrls 失败");
    assert!(!own, "pf_member 自身就带 BYPASSRLS —— 缺口用例不成立");

    let verdict = check_runtime_role(&member).await;
    member.close().await;
    cleanup(admin.clone()).await;
    admin.close().await;

    match verdict {
        Err(PreflightError::PrivilegedViaMembership {
            role,
            grantor,
            attr,
        }) => {
            assert_eq!(role, member_name);
            assert_eq!(grantor, group_name);
            assert_eq!(attr, "BYPASSRLS");
        }
        other => panic!("继承来的 BYPASSRLS 没被查出来：{other:?}"),
    }
}
