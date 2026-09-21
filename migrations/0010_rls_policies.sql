-- RLS 策略。与 0009 分文件的理由见 0009 头部。
--
-- 可重入：PG 没有 CREATE POLICY IF NOT EXISTS，故用 DROP IF EXISTS + CREATE。
-- 这个写法只在事务里安全（中间有一瞬间无策略），sqlx 默认每个迁移文件一个事务，
-- 本文件不得加 `-- no-transaction`。
--
-- 偏离 spec/03 §1.3 的模板一处：模板没有 TO 子句，本文件给 19 张甲类
-- 全部加上 `TO app_user`。三条理由，第三条是主要的：
--   1 六张表另有附加策略（见下方「附加策略」段）。PERMISSIVE 策略之间是 OR，
--     一条未限定角色的策略（polroles='{0}' 即 PUBLIC）会直接作废租户策略 ——
--     CI 断言 A6 查的就是这个，这六张表的租户策略必须限定角色。
--   2 spec/05 §3.7 给 tenant_members 的落地形态本来就写的是 TO app_user。
--   3 统一加，是为了让「将来给某张表加第二条策略」这个动作不会顺手制造 A6 违规。
--     只给需要的六张加，等于把一个陷阱留在剩下十三张表上。
-- 代价：platform_ops / auth_lookup 在未授策略的表上匹配不到任何策略 → 0 行，
-- 不是报错。fail-loud 由 GRANT 层承担（permission denied），见 0011。
SET ROLE tgm_owner;

-- ── 甲类 19 张：spec/03 §1.3 模板逐表落地 ──────────────────────────
DROP POLICY IF EXISTS tenant_isolation ON tenant_members;
CREATE POLICY tenant_isolation ON tenant_members TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON identities;
CREATE POLICY tenant_isolation ON identities TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON credential_keys;
CREATE POLICY tenant_isolation ON credential_keys TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON source_subscriptions;
CREATE POLICY tenant_isolation ON source_subscriptions TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON target_channels;
CREATE POLICY tenant_isolation ON target_channels TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON keywords;
CREATE POLICY tenant_isolation ON keywords TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON rules;
CREATE POLICY tenant_isolation ON rules TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON rule_versions;
CREATE POLICY tenant_isolation ON rule_versions TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON rule_replacements;
CREATE POLICY tenant_isolation ON rule_replacements TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON tenant_events;
CREATE POLICY tenant_isolation ON tenant_events TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON notify_dedup;
CREATE POLICY tenant_isolation ON notify_dedup TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON delivery_tasks;
CREATE POLICY tenant_isolation ON delivery_tasks TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON bot_console_sessions;
CREATE POLICY tenant_isolation ON bot_console_sessions TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON bot_console_updates;
CREATE POLICY tenant_isolation ON bot_console_updates TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON web_sessions;
CREATE POLICY tenant_isolation ON web_sessions TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

-- tenant_consents 的差别在 GRANT 层（只给 SELECT, INSERT），策略照套模板 ——
-- GRANT 管「能做什么动作」，RLS 管「能碰哪些行」，两层都要（spec/03 §7.4）
DROP POLICY IF EXISTS tenant_isolation ON tenant_consents;
CREATE POLICY tenant_isolation ON tenant_consents TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

-- audit_logs / login_sessions / system_logs：tenant_id 可空（铁律 5 的破例）。
-- 模板里的 `tenant_id = ...` 对 NULL 行求值为 NULL，即不可见 ——
-- 正好是想要的：平台级记录（tenant_id IS NULL）对租户不可见，
-- 由下方 platform_ops 的附加策略单独承载。
DROP POLICY IF EXISTS tenant_isolation ON audit_logs;
CREATE POLICY tenant_isolation ON audit_logs TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON login_sessions;
CREATE POLICY tenant_isolation ON login_sessions TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_isolation ON system_logs;
CREATE POLICY tenant_isolation ON system_logs TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

-- ── 乙类 2 张：无 tenant_id 列，经父表 EXISTS ────────────────────────
-- 不能套模板，也不能因此跳过 —— 否则拿到 rule_id / task_id
-- 就能读任意租户的规则条件与投递日志（spec/03 §1.3 已实测 0 行）。
-- 性能实测不成问题：PG 17 把子查询优化成 hashed SubPlan，整个查询只执行一次
-- （5000 规则 / 20 万子表记录 0.327 ms），前提是父表 tenant_id 上有索引。
DROP POLICY IF EXISTS tenant_isolation ON rule_conditions;
CREATE POLICY tenant_isolation ON rule_conditions TO app_user
  USING (EXISTS (
    SELECT 1 FROM rules r
    WHERE r.id = rule_conditions.rule_id
      AND r.tenant_id = current_setting('app.tenant_id')::bigint
  ));

DROP POLICY IF EXISTS tenant_isolation ON delivery_logs;
CREATE POLICY tenant_isolation ON delivery_logs TO app_user
  USING (EXISTS (
    SELECT 1 FROM delivery_tasks t
    WHERE t.id = delivery_logs.task_id
      AND t.tenant_id = current_setting('app.tenant_id')::bigint
  ));

-- ── 附加策略：六张表的差别（spec/03 §1.3 的差别表）────────────────
-- 纪律（spec/05 §3.7 末、CI 断言 A6）：**每一条都必须限定 TO <角色>**。
-- PERMISSIVE 策略之间是 OR，未限定角色的策略（polroles='{0}'）会作废租户策略。
-- 实测后果：一条不限角色的自查策略让业务连接返回 2 行、横跨租户 1 和 2。

-- 1) 鉴权反查（spec/05 §3.7）。反查发生在知道 tenant_id 之前，
--    此时走租户策略必然 fail-loud 报错 —— 那是设计正确但路径不对。
--    解法是按角色分离策略，不是放宽租户策略。
DROP POLICY IF EXISTS member_self_lookup ON tenant_members;
CREATE POLICY member_self_lookup ON tenant_members FOR SELECT TO auth_lookup
  USING (user_id = current_setting('app.auth_user_id')::bigint);

-- web_sessions 的同形状策略，出处是 spec/07 §1.4（按 cookie 查会话时还不知租户）。
-- 复用 auth_lookup，不为它开新角色。
DROP POLICY IF EXISTS session_self_lookup ON web_sessions;
CREATE POLICY session_self_lookup ON web_sessions FOR SELECT TO auth_lookup
  USING (id = current_setting('app.session_id')::bytea);

-- bot_console_sessions 的同一类策略。spec/03 §1.3 的差别表列了它要一条
-- `TO auth_lookup` 的 SELECT 策略，但全套文档没有给出 SQL —— 下面是新写的，
-- 不是转录。选 tg_user_id 作谓词，因为它就是这张表唯一的鉴权输入
-- （主键破铁律 1 的全部理由，见 §7.1），与 member_self_lookup 同一个会话变量。
DROP POLICY IF EXISTS console_self_lookup ON bot_console_sessions;
CREATE POLICY console_self_lookup ON bot_console_sessions FOR SELECT TO auth_lookup
  USING (tg_user_id = current_setting('app.auth_user_id')::bigint);

-- 注意上面三条都是 FOR SELECT，而 eng/02 §二 的权限矩阵给 auth_lookup
-- 在 bot_console_sessions / web_sessions 上的是**全量 DML**。
-- 两者不矛盾但需要知道后果：INSERT/UPDATE/DELETE 找不到可匹配的策略 →
-- 写入被 RLS 拒（不是 permission denied，是策略不匹配）。
-- 这三张表的写入路径因此必须走 tgm_owner 或另加策略 —— 取何种形态属于实现期决定，
-- 此处不替它做：spec 明写的是 SELECT-only 策略 + 全量 GRANT，照写。

-- 2) identities 的共享身份策略（spec/03 §1.3 差别表第二行：
--    「另加一条 TO app_user 的 FOR SELECT 共享身份策略」，理由是租户要能解析演示身份）。
--    文档只给了这一句描述，没有 SQL —— 下面是新写的。
--    谓词取 tenant_id IS NULL，因为「平台共享演示身份」的定义就是它
--    （spec/02 §4.1 与 §三 的列注释均以此为判据）。
--    只读、只加 SELECT：租户对演示源只有订阅/取消订阅，不得修改该身份。
--    它开的洞由应用层补：源订阅端点必须拒绝租户指定 tenant_id IS NULL 的身份，
--    否则租户可把任意频道挂到平台演示身份上采集（spec/02 §4.1 边界 4，
--    反向测试 R19）。这是权限机制，超级用户绕得过 —— 记在这里以免将来误读成够用了。
DROP POLICY IF EXISTS shared_identity_read ON identities;
CREATE POLICY shared_identity_read ON identities FOR SELECT TO app_user
  USING (tenant_id IS NULL);

-- 3) platform_ops 读平台级日志（spec/05 §六：「运维查 tenant_id IS NULL 的日志行」）。
--    同样只有这一句描述，没有 SQL —— 下面是新写的。
--    只 FOR SELECT，与权限矩阵的「SELECT only」一致；
--    谓词严格限 IS NULL，所以运维看不到任何租户归属的日志行。
DROP POLICY IF EXISTS platform_log_read ON system_logs;
CREATE POLICY platform_log_read ON system_logs FOR SELECT TO platform_ops
  USING (tenant_id IS NULL);

DROP POLICY IF EXISTS platform_log_read ON audit_logs;
CREATE POLICY platform_log_read ON audit_logs FOR SELECT TO platform_ops
  USING (tenant_id IS NULL);

-- 写入平台级日志（tenant_id IS NULL）的路径也需要一条策略：app_user 的模板策略
-- WITH CHECK 对 NULL 求值为 NULL 即拒绝，所以 app_user 写不进平台级行。
-- 这是有意的 —— 平台级日志由以 tgm_owner 身份运行的路径写。
-- 但 tgm_owner 受 FORCE 约束且无匹配策略，同样写不进去。
-- 这一处缺口留给实现期定（候选：给两张日志表加 TO tgm_owner 的策略，
-- 或平台级日志不走 PG）。此处不擅自补策略 —— 加 TO tgm_owner 的策略
-- 等于给 owner 开一条通道，那是需要单独论证的决定，不是转录。


-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
