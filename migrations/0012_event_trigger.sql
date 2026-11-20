-- 分区用的 RLS 兜底 Event Trigger。抄自 docs/design/spec/04-数据生命周期与容量.md §3.1。
--
-- 为什么现在就建，尽管分区已推迟（spec/04 §3.1 结论：不提前优化）：
-- 它不只覆盖 pg_partman。任何 CREATE TABLE 都会经过它 ——
-- 包括「改列类型要重建表」这种日常操作，而那个操作会静默摘掉一张表的 RLS
-- （eng/02 §3.1 陷阱 1：CREATE TABLE ... LIKE INCLUDING ALL 不含 RLS，
-- relrowsecurity=f、零策略，且不报错）。
--
-- 它只能补 ENABLE / FORCE，**策略本身仍需单独创建** —— 策略含表名，无法泛化。
-- 所以它是兜底，不是替代 0010。
--
-- 可重入：CREATE OR REPLACE FUNCTION 幂等；
-- Event Trigger 没有 IF NOT EXISTS，故 DROP IF EXISTS 后重建。
SET ROLE tgm_owner;

CREATE OR REPLACE FUNCTION rls_auto_enable() RETURNS event_trigger AS $$
DECLARE cmd record;
BEGIN
  FOR cmd IN SELECT * FROM pg_event_trigger_ddl_commands()
             WHERE object_type IN ('table','partitioned table')
  LOOP
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', cmd.object_identity);
    EXECUTE format('ALTER TABLE %s FORCE  ROW LEVEL SECURITY', cmd.object_identity);
  END LOOP;
END $$ LANGUAGE plpgsql;

-- Event Trigger 必须由超级用户创建（PG 的硬限制，tgm_owner 不行），
-- 故此处退回迁移执行者身份。这是本文件与其余迁移不同的一处，
-- 偏离 spec/04 §3.1 —— 该处给的片段没说执行身份。
RESET ROLE;

DROP EVENT TRIGGER IF EXISTS ensure_rls;
CREATE EVENT TRIGGER ensure_rls ON ddl_command_end
  WHEN TAG IN ('CREATE TABLE','CREATE TABLE AS') EXECUTE FUNCTION rls_auto_enable();

-- 注意本触发器与 0002~0007 的执行顺序：它排在建表之后，
-- 所以 27 张表的 ENABLE/FORCE 不靠它，靠 0009 显式写出。
-- 若将来有人把本文件的序号提前，0009 会变成冗余而丙类 6 张表会被误开 RLS ——
-- **本文件必须留在最后**。丙类被误开的后果不是报错，是静默 0 行。
