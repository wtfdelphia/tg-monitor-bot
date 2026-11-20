-- ENABLE + FORCE，不含策略。
--
-- 与 0010 分成两个文件是安全要求（eng/02 §三）：
-- **策略能存在于一张 RLS 已关闭的表上，并被完全忽略**
-- （实测：DISABLE 后 relrowsecurity=f，但 pg_policy 仍有记录）。
-- 分开之后，审计能分别验证「开了」和「有策略」两件事。
--
-- FORCE 那一行是全套设计里最吃重的开关：没有它，owner 身份的一切路径
-- （迁移、SECURITY DEFINER 函数、普通视图）都绕过策略，且不报错。
-- CI 断言 A 主查询的三列里，forced 是唯一能暴露「开了 RLS 但忘了 FORCE」的那一列。
--
-- ALTER TABLE ... ENABLE/FORCE 是幂等的，重复执行无副作用 —— 本文件天然可重入。
SET ROLE tgm_owner;

-- 甲类 19 张（有 tenant_id 列，套 spec/03 §1.3 模板）。
-- 名单来自 spec/03 §1.3，判据是「表里有没有 tenant_id 列」，不是凭感觉划分。
ALTER TABLE tenant_members        ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_members        FORCE  ROW LEVEL SECURITY;
ALTER TABLE identities            ENABLE ROW LEVEL SECURITY;
ALTER TABLE identities            FORCE  ROW LEVEL SECURITY;
ALTER TABLE credential_keys       ENABLE ROW LEVEL SECURITY;
ALTER TABLE credential_keys       FORCE  ROW LEVEL SECURITY;
ALTER TABLE source_subscriptions  ENABLE ROW LEVEL SECURITY;
ALTER TABLE source_subscriptions  FORCE  ROW LEVEL SECURITY;
ALTER TABLE target_channels       ENABLE ROW LEVEL SECURITY;
ALTER TABLE target_channels       FORCE  ROW LEVEL SECURITY;
ALTER TABLE keywords              ENABLE ROW LEVEL SECURITY;
ALTER TABLE keywords              FORCE  ROW LEVEL SECURITY;
ALTER TABLE rules                 ENABLE ROW LEVEL SECURITY;
ALTER TABLE rules                 FORCE  ROW LEVEL SECURITY;
ALTER TABLE rule_versions         ENABLE ROW LEVEL SECURITY;
ALTER TABLE rule_versions         FORCE  ROW LEVEL SECURITY;
ALTER TABLE rule_replacements     ENABLE ROW LEVEL SECURITY;
ALTER TABLE rule_replacements     FORCE  ROW LEVEL SECURITY;
ALTER TABLE tenant_events         ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_events         FORCE  ROW LEVEL SECURITY;
ALTER TABLE notify_dedup          ENABLE ROW LEVEL SECURITY;
ALTER TABLE notify_dedup          FORCE  ROW LEVEL SECURITY;
ALTER TABLE delivery_tasks        ENABLE ROW LEVEL SECURITY;
ALTER TABLE delivery_tasks        FORCE  ROW LEVEL SECURITY;
ALTER TABLE audit_logs            ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_logs            FORCE  ROW LEVEL SECURITY;
ALTER TABLE login_sessions        ENABLE ROW LEVEL SECURITY;
ALTER TABLE login_sessions        FORCE  ROW LEVEL SECURITY;
ALTER TABLE system_logs           ENABLE ROW LEVEL SECURITY;
ALTER TABLE system_logs           FORCE  ROW LEVEL SECURITY;
ALTER TABLE bot_console_sessions  ENABLE ROW LEVEL SECURITY;
ALTER TABLE bot_console_sessions  FORCE  ROW LEVEL SECURITY;
ALTER TABLE bot_console_updates   ENABLE ROW LEVEL SECURITY;
ALTER TABLE bot_console_updates   FORCE  ROW LEVEL SECURITY;
ALTER TABLE web_sessions          ENABLE ROW LEVEL SECURITY;
ALTER TABLE web_sessions          FORCE  ROW LEVEL SECURITY;
ALTER TABLE tenant_consents       ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_consents       FORCE  ROW LEVEL SECURITY;

-- 乙类 2 张（无 tenant_id 列，靠 EXISTS 策略经父表兜）。
-- 它们不在 CI 主查询的覆盖内（主查询按有无 tenant_id 列筛表），
-- 由 04-CI门禁.md §三-1 的补充断言单独校验 —— 漏了这两行不会被主查询发现。
ALTER TABLE rule_conditions       ENABLE ROW LEVEL SECURITY;
ALTER TABLE rule_conditions       FORCE  ROW LEVEL SECURITY;
ALTER TABLE delivery_logs         ENABLE ROW LEVEL SECURITY;
ALTER TABLE delivery_logs         FORCE  ROW LEVEL SECURITY;

-- 丙类 6 张刻意不启用（spec/03 §1.3 的表逐条给了理由）：
--   tenants                   无 tenant_id 列，主键即租户本身，靠应用层授权
--   canonical_events          租户无关层，扇出后才有租户归属
--   media_archive             按 content_hash 全局复用，跨租户共享是设计意图
--   identity_secrets          经 identities 间接受约，且仅三个组件可解密
--   account_leases            运维层，Worker 抢租约时无租户上下文
--   console_login_challenges  登录前表，租户未知正是它要解决的问题；靠 GRANT 限 auth_lookup
-- 前三张的隔离完全依赖应用层，是本方案 RLS 覆盖面的已知边界，不是遗漏。

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
