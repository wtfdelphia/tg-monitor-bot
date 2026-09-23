//! 反向测试：跨租户隔离。清单与编号在 docs/design/eng/03-测试策略.md §三。
//!
//! 每条测试的名字带 `rN_` 前缀，对应那份清单的编号 —— 清单是唯一归口处，
//! 这里不重复叙述测试意图，只写「怎么验」。
//!
//! 迁移路径写成 `../../migrations`：那份迁移在仓库根，不在本 crate 内
//! （见 src/lib.rs 的同一条说明）。宏的默认推断是 crate 根下的 `migrations/`，
//! 不写这个参数则整个 27 表一张都不会建，而测试会以「表不存在」失败 ——
//! 那种失败看起来像测试写错了，其实是路径没配。

// workspace 的 expect_used / panic 两条针对的是生产路径（eng/00 §三 纪律 3：
// sqlx::Error 不得透给客户端）。在反向测试里 expect 与 panic 就是断言本身，
// 换成 `?` 会把失败变成静默的 Err —— 那正好是这些测试要防的形状。
// 范围限在本文件，不动 [workspace.lints]。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use support::AdminOnlyPool;
use tgm_core::ids::TenantId;

/// 两个租户加各自一行数据。返回 (租户 A, 租户 B)。
///
/// 以超级用户身份灌（eng/03 §2.2：种子不能在 RLS 启用后以受限角色插入）。
/// 这不是偶然安全 —— fixture 只灌数据、不改 RLS 配置，是那节明确约定的。
async fn seed_two_tenants(admin: &AdminOnlyPool) -> (i64, i64) {
    let mut ids = Vec::new();
    for name in ["tenant-a", "tenant-b"] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(name)
        .bind(if name == "tenant-a" { 1001_i64 } else { 1002 })
        .fetch_one(admin.raw())
        .await
        .expect("建租户失败");
        sqlx::query("INSERT INTO keywords (tenant_id, word) VALUES ($1, $2)")
            .bind(id)
            .bind(format!("kw-{name}"))
            .execute(admin.raw())
            .await
            .expect("灌关键词失败");
        ids.push(id);
    }
    (ids[0], ids[1])
}

/// R1 不设 `app.tenant_id` 直接查 → 报错，非 0 行。
///
/// 这条同时是整套脚手架的自检：断言的后半段特意在**超级用户池**上跑同一个查询，
/// 并要求它成功。两段合起来才证明受限角色池真的换了角色 ——
/// 只有前半段的话，一个连都连不上的池也会让它「通过」。
#[sqlx::test(migrations = "../../migrations")]
async fn r1_no_tenant_context_errors(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    support::assert_not_bypassrls(&app).await;

    let err = sqlx::query("SELECT count(*) FROM rules")
        .fetch_one(&app)
        .await
        .expect_err("不设 app.tenant_id 竟然查成功了 —— 隔离是坏的");

    // eng/03 §四 第 2 条：全新连接的报错是 unrecognized configuration parameter，
    // 与 R3（复用连接、残留空串）的 invalid input syntax 不是同一条文本。
    // 所以这里不能写成匹配「任意报错」，也不能与 R3 共用断言。
    let msg = err.to_string();
    assert!(
        msg.contains("unrecognized configuration parameter"),
        "报错是 fail-loud 的，但文本不是预期的那条：{msg}"
    );

    // 脚手架自检：同一个查询在超级用户池上必须成功。
    // 它失败意味着表没建起来，上面那个 expect_err 是因为别的原因过的。
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM rules")
        .fetch_one(admin.raw())
        .await
        .expect("超级用户查 rules 失败 —— 迁移没跑起来，上面那条断言是空的");
    assert_eq!(n, 0, "干净的测试库里 rules 应该是空的");
}

/// R2 租户 A 写 B 的 `tenant_id` → `WITH CHECK` 拒绝。
///
/// 顺带验 USING：设了 A 的上下文只看得到 A 那一行。
/// 两段一起才有意义 —— 只验 WITH CHECK 的话，一张空表也会让它通过。
#[sqlx::test(migrations = "../../migrations")]
async fn r2_cross_tenant_write_rejected(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, b) = seed_two_tenants(&admin).await;

    let mut tx = app.begin().await.expect("开事务失败");
    // set_config 的第三参数是 true（事务级）。false 会让它活到连接归还之后，
    // 那正是 R3 那条泄漏的成因（eng/03 §四）。
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .expect("设租户上下文失败");

    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM keywords")
        .fetch_one(&mut *tx)
        .await
        .expect("查本租户关键词失败");
    assert_eq!(seen, 1, "USING 谓词不对：A 的上下文下应当只见 A 的 1 行");

    let err = sqlx::query("INSERT INTO keywords (tenant_id, word) VALUES ($1, 'stolen')")
        .bind(b)
        .execute(&mut *tx)
        .await
        .expect_err("A 竟然写进了 B 的 tenant_id —— WITH CHECK 没起作用");
    let msg = err.to_string();
    assert!(
        msg.contains("row-level security policy"),
        "拒绝了，但不是 RLS 拒的：{msg}"
    );
}

/// R8 唯一约束探测 → 成功，不得 `duplicate key`。
///
/// 这条测的是侧信道而非读权限：`uq_keyword` 是 (tenant_id, word, match_type)，
/// 带了 tenant_id，所以 A 插一个 B 已有的 word 应当成功。
/// 若唯一约束漏了 tenant_id，这里会得到 duplicate key ——
/// 而那个报错本身就是「B 有这个词」的 oracle，RLS 补不了。
#[sqlx::test(migrations = "../../migrations")]
async fn r8_unique_probe_no_oracle(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, b) = seed_two_tenants(&admin).await;

    // B 的那行用的词。A 拿它来探测。
    let bs_word: String = sqlx::query_scalar("SELECT word FROM keywords WHERE tenant_id = $1")
        .bind(b)
        .fetch_one(admin.raw())
        .await
        .expect("取 B 的词失败");

    let mut tx = app.begin().await.expect("开事务失败");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .expect("设租户上下文失败");

    sqlx::query("INSERT INTO keywords (tenant_id, word) VALUES ($1, $2)")
        .bind(a)
        .bind(&bs_word)
        .execute(&mut *tx)
        .await
        .expect("插 B 已有的词失败 —— 唯一约束漏了 tenant_id，这是跨租户 oracle");
}

/// R22 `app_user` 对 `tenant_consents` 的 `UPDATE`/`DELETE` → denied。
///
/// 这条验的是 GRANT 层而不是 RLS 层：0011 只给了 SELECT, INSERT。
/// 所以报错应当是 permission denied，不是 row-level security policy ——
/// 两者混淆会让「凭证不可篡改」这条保证落在错误的机制上。
#[sqlx::test(migrations = "../../migrations")]
async fn r22_consents_immutable(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, _) = seed_two_tenants(&admin).await;
    sqlx::query("INSERT INTO tenant_members (tenant_id, user_id) VALUES ($1, 1001)")
        .bind(a)
        .execute(admin.raw())
        .await
        .expect("建成员关系失败");
    sqlx::query(
        "INSERT INTO tenant_consents (tenant_id, consent_type, text_version, user_id)
         VALUES ($1, 'tos', 'v1', 1001)",
    )
    .bind(a)
    .execute(admin.raw())
    .await
    .expect("灌同意记录失败");

    for sql in [
        "UPDATE tenant_consents SET text_version = 'v2'",
        "DELETE FROM tenant_consents",
    ] {
        let mut tx = app.begin().await.expect("开事务失败");
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(a.to_string())
            .execute(&mut *tx)
            .await
            .expect("设租户上下文失败");
        let err = sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .expect_err(&format!("`{sql}` 竟然成功了 —— 同意凭证可被篡改"));
        assert!(
            err.to_string().contains("permission denied"),
            "被拒了，但不是 GRANT 层拒的：{err}"
        );
    }
}

/// R5 **关掉 RLS 跑跨租户查询 → 应仍不泄漏**。§三 说它是清单里最有价值的一条。
///
/// 它测的不是 RLS，是 `repo::count_keywords` 里那句显式 `WHERE tenant_id = $1`
/// —— 也就是 eng/00 §四 那条纪律是否真被执行了。其余测试在 RLS 开着时都会绿，
/// 唯有这条能证明第一道防线存在。
///
/// 注意 `TenantId` 只能从库里解码（eng/00 §4.1 没有 `From<i64>`），
/// 所以下面那句 `query_scalar` 是必需的绕路，不是啰嗦。
#[sqlx::test(migrations = "../../migrations")]
async fn r5_no_leak_with_rls_disabled(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, _b) = seed_two_tenants(&admin).await;

    let ta: TenantId = sqlx::query_scalar("SELECT id FROM tenants WHERE id = $1")
        .bind(a)
        .fetch_one(admin.raw())
        .await
        .expect("解码 TenantId 失败");

    // 第一遍：RLS 开着。两道防线都在。
    let with_rls = tgm_db::repo::count_keywords(&app, ta)
        .await
        .expect("RLS 开着时查询失败");
    assert_eq!(with_rls, 1, "本租户应当只有 1 行");

    // 第二遍：关掉 RLS，只剩应用层那句 WHERE。
    sqlx::query("ALTER TABLE keywords DISABLE ROW LEVEL SECURITY")
        .execute(admin.raw())
        .await
        .expect("关 RLS 失败");

    // 先确认 RLS 真的关掉了 —— 否则这一遍与第一遍没有区别，
    // 「第二遍也绿」这句话就是空的（与 audit-double-pass.sh 的判据同形）。
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM keywords")
        .fetch_one(&app)
        .await
        .expect("RLS 关掉后裸查应当成功");
    assert_eq!(
        leaked, 2,
        "RLS 没真关掉，这一遍与第一遍无异 —— 本条测试没测到任何东西"
    );

    let without_rls = tgm_db::repo::count_keywords(&app, ta)
        .await
        .expect("RLS 关掉后查询失败");
    assert_eq!(
        without_rls, 1,
        "关掉 RLS 后泄漏了 —— Repo 里漏了显式 WHERE tenant_id，第一道防线不存在"
    );
}

/// R6 owner 的 `SECURITY DEFINER` 函数 / 普通视图。
///
/// §五 的实测结论改变了这条的断言形状：主断言是**静态**的
/// 「带 `tenant_id` 的表上不存在普通视图与 `SECURITY DEFINER` 函数」，
/// 那就是审计断言 A8。这里保留的是动态部分，防止有人补一条 owner 策略把 A8 绕开
/// —— 补了之后 A8 仍绿（它只查对象存不存在），而泄漏是真的。
#[sqlx::test(migrations = "../../migrations")]
async fn r6_owner_paths_return_empty(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, _b) = seed_two_tenants(&admin).await;

    // 视图必须以 tgm_owner 身份建。**这一步不是样板**：
    // 用超级用户建出来的视图 owner 是 postgres，而超级用户旁路 RLS ——
    // 实测那样建出来的视图返回全部 2 行，本条断言会以「FORCE 没生效」失败，
    // 而真正的原因只是视图 owner 错了。与「表的 owner 决定 FORCE 是真的还是装饰」
    // 同形，只是对象换成了视图。
    //
    // SET ROLE 是会话级动作，所以三句必须在同一条连接上，不能分别走池。
    let mut ddl_tx = admin.raw().begin().await.expect("开 DDL 事务失败");
    for ddl in [
        "SET ROLE tgm_owner",
        "CREATE VIEW v_kw AS SELECT * FROM keywords",
        "GRANT SELECT ON v_kw TO app_user",
        "RESET ROLE",
    ] {
        sqlx::query(ddl)
            .execute(&mut *ddl_tx)
            .await
            .unwrap_or_else(|e| panic!("`{ddl}` 失败：{e}"));
    }
    ddl_tx.commit().await.expect("提交 DDL 失败");

    let mut tx = app.begin().await.expect("开事务失败");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .expect("设租户上下文失败");

    // §五：不是「安全且可用」，而是「安全但功能坏掉」—— 静默 0 行。
    // 这类 bug 在功能测试里表现为「查不到数据」，容易被误诊成数据没写进去。
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM v_kw")
        .fetch_one(&mut *tx)
        .await
        .expect("查 owner 视图失败");
    assert_eq!(
        seen, 0,
        "owner 的普通视图返回了 {seen} 行 —— FORCE 没在 owner 身上生效"
    );
}

/// R19 **租户指定 `tenant_id IS NULL` 的演示身份建源 → 403**。§三 标它是承重墙。
///
/// 数据库层面这条**不成立而且必须不成立**：`shared_identity_read` 策略让
/// `app_user` 看得见演示身份，而 `source_subscriptions.receiver_identity_id`
/// 是普通单列外键 —— 于是 INSERT 会成功。
/// 所以这条断言在这里的形态是「记录下 DB 不拦」，403 必须由应用层出。
///
/// 这不是把测试写松：把它写成「DB 会拦」才是假绿，因为 DB 确实不拦（本测试即证据），
/// 而那样写出来的绿会让应用层那道检查看起来可以省。
#[sqlx::test(migrations = "../../migrations")]
async fn r19_db_does_not_block_demo_identity(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, _b) = seed_two_tenants(&admin).await;

    let demo: i64 = sqlx::query_scalar(
        "INSERT INTO identities (kind, tenant_id, display_name)
         VALUES ('bot', NULL, 'demo') RETURNING id",
    )
    .fetch_one(admin.raw())
    .await
    .expect("建演示身份失败");

    let mut tx = app.begin().await.expect("开事务失败");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .expect("设租户上下文失败");

    // 前提：演示身份对租户是可见的（shared_identity_read 策略）。
    // 这一句同时防止下面那个 INSERT 因为「看不见所以不知道 id」而失去意义。
    let visible: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identities WHERE tenant_id IS NULL")
            .fetch_one(&mut *tx)
            .await
            .expect("查演示身份失败");
    assert_eq!(
        visible, 1,
        "演示身份对租户不可见 —— shared_identity_read 没生效"
    );

    let r = sqlx::query(
        "INSERT INTO source_subscriptions (tenant_id, tg_chat_id, receiver_identity_id)
         VALUES ($1, -100123, $2)",
    )
    .bind(a)
    .bind(demo)
    .execute(&mut *tx)
    .await;

    assert!(
        r.is_ok(),
        "DB 竟然拦住了演示身份建源 —— 那说明 shared_identity_read 或外键形态变了，\
         本测试所记录的前提（403 必须由应用层出）需要重新核对：{r:?}"
    );
}

/// R13 `bot_console_sessions` 业务角色 `SELECT`/`INSERT` 均 denied。
///
/// 与 R22 同形（都验 GRANT 层封死），成本近零，故按 §三 的分批一起写。
/// 0011 对这张表是 `REVOKE ALL ... FROM app_user`。
#[sqlx::test(migrations = "../../migrations")]
async fn r13_console_sessions_denied(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, _) = seed_two_tenants(&admin).await;

    for sql in [
        "SELECT count(*) FROM bot_console_sessions",
        "INSERT INTO bot_console_sessions (tg_user_id, tenant_id) VALUES (1, 1)",
    ] {
        let mut tx = app.begin().await.expect("开事务失败");
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(a.to_string())
            .execute(&mut *tx)
            .await
            .expect("设租户上下文失败");
        let err = sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .expect_err(&format!("`{sql}` 竟然成功了 —— 会话表对业务角色没封死"));
        assert!(
            err.to_string().contains("permission denied"),
            "被拒了，但不是 GRANT 层拒的：{err}"
        );
    }
}

/// R24 `ensure_rls` 不得把 FORCE RLS 打到临时表上。
///
/// 这条的来历与清单里其余各条不同：它不是照设计推出来的，是被一条无关探针
/// 撞出来的（想用 `CREATE TEMP TABLE` 存两个 id，结果写入被 RLS 拒了）。
///
/// **它为什么能潜伏过全套验证：超级用户 bypass RLS，而迁移、`tgm audit-rls`、
/// 双遍脚本全都以 `postgres` 跑。** 以超级用户建临时表能正常写入，
/// 换成 `tgm_owner` 或 `app_user` 才被拒 —— 对现有每条验证路径都是隐形的。
/// 所以这条测试必须用**非超级用户**的池，用 `admin` 跑会永远绿。
///
/// 反向那半条（普通表仍须被开 FORCE）也在这里：只验临时表能写的话，
/// 把 0013 的判断放宽成「全都跳过」同样会绿 —— 那正是 0012 想防的形状。
#[sqlx::test(migrations = "../../migrations")]
async fn r24_event_trigger_skips_temp_tables(admin: PgPool) {
    let (app, _admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;

    let mut tx = app.begin().await.expect("开事务失败");
    sqlx::query("CREATE TEMP TABLE r24_probe (a int)")
        .execute(&mut *tx)
        .await
        .expect("建临时表失败");
    sqlx::query("INSERT INTO r24_probe VALUES (1)")
        .execute(&mut *tx)
        .await
        .expect(
            "临时表写入被拒 —— ensure_rls 又把 FORCE RLS 打到临时表上了（迁移 0013）。\
             注意本条不会在以超级用户跑的任何检查里报红",
        );
    let forced: bool =
        sqlx::query_scalar("SELECT relforcerowsecurity FROM pg_class WHERE relname = 'r24_probe'")
            .fetch_one(&mut *tx)
            .await
            .expect("查临时表的 relforcerowsecurity 失败");
    assert!(!forced, "临时表被打上了 FORCE RLS");

    // 反向：普通表仍须被 ensure_rls 开上 ENABLE + FORCE。
    // 用 admin 建 —— app_user 没有 public schema 的 CREATE 权限（0001）。
    let mut atx = _admin.raw().begin().await.expect("开 admin 事务失败");
    sqlx::query("CREATE TABLE r24_perm_probe (a int)")
        .execute(&mut *atx)
        .await
        .expect("建普通表失败");
    let (enabled, forced): (bool, bool) = sqlx::query_as(
        "SELECT relrowsecurity, relforcerowsecurity FROM pg_class
         WHERE relname = 'r24_perm_probe'",
    )
    .fetch_one(&mut *atx)
    .await
    .expect("查普通表的 RLS 标志失败");
    assert!(
        enabled && forced,
        "普通表没被 ensure_rls 开上 RLS（enabled={enabled} forced={forced}）—— \
         0013 的临时表判断放得太宽，把持久表也跳过了"
    );
}

/// R11 同 `update_id` 重投三次 → 一次副作用。
///
/// 定义处 `../spec/02-总体架构.md` §6.7。幂等键是 `bot_console_updates` 的主键
/// `(tenant_id, bot_id, update_id)`，这条测它真的挡得住重投。
///
/// **副作用落在 `audit_logs` 而不是 `rules`。** 后者有 `uq_rule_conflict`
/// （`tenant_id, keyword_id, COALESCE(source_ref,0), COALESCE(media_type,'ANY'), target_ref`
/// WHERE enabled），而那个约束存在的目的正是给 API 判 409 —— 也就是说
/// 应用层本来就会吞掉它的冲突。实测同一条 `/addrule` 跑三遍：
/// 裸 INSERT 报 `duplicate key ... uq_rule_conflict`，
/// 走 409 语义（`ON CONFLICT DO NOTHING`）则 `INSERT 0 0` 两次、最终 1 行。
/// 于是幂等键**完全删掉**这条测试仍然绿。`audit_logs` 没有任何业务唯一约束
/// （主键是 `GENERATED ALWAYS` 的 `id`，每次插入都是新行），
/// 是这库里能真的数出「三次副作用」的落点。
///
/// 三次重投的形态按 §6.7 那段 SQL 注释分开写：裸 INSERT 拿 `duplicate key`
/// 是「发现重投」的机制，`ON CONFLICT DO NOTHING` 的 `INSERT 0 0` 是
/// 「据此判定已处理，直接返回」的机制。两者都要验 —— 只验后者的话，
/// 主键退化成 `(tenant_id, update_id)` 仍然会绿。
#[sqlx::test(migrations = "../../migrations")]
async fn r11_replayed_update_id_has_one_effect(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let (a, _b) = seed_two_tenants(&admin).await;

    // 两个 Bot 身份。第二个用来验 bot_id 真在键里（§6.7：update_id 按 Bot 独立计数）。
    let mut bots = Vec::new();
    for name in ["bot-ctl", "bot-data"] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO identities (kind, tenant_id, display_name)
             VALUES ('bot', $1, $2) RETURNING id",
        )
        .bind(a)
        .bind(name)
        .fetch_one(admin.raw())
        .await
        .expect("建 Bot 身份失败");
        bots.push(id);
    }
    let (ctl, data) = (bots[0], bots[1]);
    const UPD: i64 = 9001;

    // 一次「处理一个 update」：先占幂等键，占到了才写副作用。
    // 返回是否真的处理了（false = 已处理过，直接返回）。
    async fn handle(pool: &PgPool, tenant: i64, bot: i64, upd: i64) -> bool {
        let mut tx = pool.begin().await.expect("开事务失败");
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant.to_string())
            .execute(&mut *tx)
            .await
            .expect("设租户上下文失败");
        let claimed = sqlx::query(
            "INSERT INTO bot_console_updates (tenant_id, bot_id, update_id)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(tenant)
        .bind(bot)
        .bind(upd)
        .execute(&mut *tx)
        .await
        .expect("占幂等键失败")
        .rows_affected();
        if claimed == 1 {
            sqlx::query(
                "INSERT INTO audit_logs (tenant_id, action, resource, source)
                 VALUES ($1, 'rule.create', 'rules', 'bot')",
            )
            .bind(tenant)
            .execute(&mut *tx)
            .await
            .expect("写副作用失败");
        }
        tx.commit().await.expect("提交失败");
        claimed == 1
    }

    // 同一个 update 投三次。
    assert!(handle(&app, a, ctl, UPD).await, "第一次投递没被处理");
    assert!(
        !handle(&app, a, ctl, UPD).await,
        "第二次重投被当成新 update"
    );
    assert!(
        !handle(&app, a, ctl, UPD).await,
        "第三次重投被当成新 update"
    );

    let effects: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE tenant_id = $1")
        .bind(a)
        .fetch_one(admin.raw())
        .await
        .expect("数副作用失败");
    assert_eq!(
        effects, 1,
        "同一个 update_id 投三次产生了 {effects} 条副作用 —— \
         bot_console_updates 的主键没挡住重投"
    );

    // 裸 INSERT 的那半条：`ON CONFLICT DO NOTHING` 只报 0 行，
    // 而 §6.7 记的是「重投同一 update_id → duplicate key」。两种形态都要在。
    let mut tx = app.begin().await.expect("开事务失败");
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .expect("设租户上下文失败");
    let err = sqlx::query(
        "INSERT INTO bot_console_updates (tenant_id, bot_id, update_id) VALUES ($1, $2, $3)",
    )
    .bind(a)
    .bind(ctl)
    .bind(UPD)
    .execute(&mut *tx)
    .await
    .expect_err("裸 INSERT 重投竟然成功 —— 主键不在那三列上");
    assert!(
        err.to_string().contains("duplicate key"),
        "重投被拒了，但不是唯一约束拒的：{err}"
    );
    drop(tx);

    // 另一个 Bot 的同号 update_id 必须能插进去。
    // 这半条是 bot_id 在键里的唯一证据：主键退化成 (tenant_id, update_id) 时，
    // 上面每一条断言都仍然会绿，只有这一条会红。
    assert!(
        handle(&app, a, data, UPD).await,
        "另一个 Bot 的同号 update_id 被当成重投 —— bot_id 不在幂等键里，\
         控制 Bot 的命令会被误判成数据 Bot 的重投（spec/02 §6.7）"
    );
}
