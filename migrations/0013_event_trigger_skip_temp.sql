-- 0012 的 ensure_rls 把 FORCE RLS 也打到了**临时表**上，本文件修掉。
--
-- 症状：任何 `CREATE TEMP TABLE` 之后对它的写入直接被拒，报
--   new row violates row-level security policy for table "<临时表名>"
-- 真因：临时表也是 object_type='table'，于是被 ENABLE + FORCE，
-- 而临时表不可能有策略（0010 的策略含表名，临时表是运行时才有的名字）——
-- FORCE + 零策略 = 拒绝一切写入。实测 relrowsecurity=t、relforcerowsecurity=t、
-- pg_policy 零行。
--
-- **为什么 12 个迁移全绿、审计 16 条全绿、59 条测试全绿都没抓到它：
-- 超级用户 bypass RLS，而迁移与 tgm audit-rls 都以 postgres 身份跑。**
-- 以 postgres 建临时表能正常写入，以 tgm_owner 或 app_user 就被拒 ——
-- 也就是说这个缺陷对现有的每一条验证路径都是隐形的。它是被一条
-- 无关的探针（R11 想用临时表存 id）撞出来的，不是查出来的。
--
-- 落地代价：目前没有任何代码用临时表（已 grep 确认），所以这是潜伏缺陷
-- 而非现行故障。修它是因为它的引爆点很远：下一个人写一句
-- `CREATE TEMP TABLE` 做批量导入或 COPY 中转，会拿到一条指向 RLS 的报错，
-- 而他的代码跟 RLS 毫无关系。
--
-- 为什么是新文件而不是改 0012：0012 已应用，sqlx 的校验和不区分注释与 DDL，
-- 改它只会炸已部署环境而空库照绿（eng/02 §3.3）。
SET ROLE tgm_owner;

-- 只加一个 relpersistence 判断，其余与 0012 逐字相同。
-- 'u'（UNLOGGED）不排除：它是持久表，只是不写 WAL，该受 RLS 管。
CREATE OR REPLACE FUNCTION rls_auto_enable() RETURNS event_trigger AS $$
DECLARE cmd record;
BEGIN
  FOR cmd IN SELECT * FROM pg_event_trigger_ddl_commands()
             WHERE object_type IN ('table','partitioned table')
  LOOP
    -- 临时表跳过。用 objid 查 relpersistence 而不是看 object_identity 的
    -- schema 名（pg_temp_N 的 N 随会话变），后者要靠字符串匹配，更脆。
    CONTINUE WHEN (SELECT relpersistence FROM pg_class WHERE oid = cmd.objid) = 't';
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', cmd.object_identity);
    EXECUTE format('ALTER TABLE %s FORCE  ROW LEVEL SECURITY', cmd.object_identity);
  END LOOP;
END $$ LANGUAGE plpgsql;

-- Event Trigger 本身不用重建：它指向函数名，函数体换了就生效。
-- 所以本文件不需要像 0012 那样 RESET ROLE 去拿超级用户权限。
RESET ROLE;
