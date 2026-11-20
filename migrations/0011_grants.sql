-- 权限矩阵。定义处是 docs/design/eng/02-本地环境与迁移.md §二 的那张表。
--
-- 逐权限列举，**不写 GRANT ALL**。这不是风格：GRANT ALL 会带上 TRUNCATE，
-- 而 TRUNCATE 不受 RLS 约束（spec/05 §3.5）—— 有这个权限就能清空他租户的数据，
-- 策略写得再对也拦不住。CI 断言 A1 查的就是 app_user 有没有 TRUNCATE。
--
-- 位置在 0009/0010 之后：权限必须晚于 RLS 启用（eng/02 §三）。
-- 反过来（先授权后开 RLS）会有一个窗口期，窗口内 app_user 可无策略访问全表。
--
-- GRANT / REVOKE 都是幂等的 —— 本文件天然可重入。
SET ROLE tgm_owner;

-- ── app_user：业务运行时 ─────────────────────────────────────────
-- 甲类 19 张里的 15 张给全量 DML。剩下四张在下面单独处理：
--   identities           UPDATE 收窄到列级，不含 tenant_id（CI 断言 §三-2）
--   tenant_consents      只 SELECT, INSERT（凭证不可改）
--   bot_console_sessions / web_sessions   REVOKE ALL（补偿唯一约束侧信道）
GRANT SELECT, INSERT, UPDATE, DELETE ON
  tenant_members, credential_keys, source_subscriptions,
  target_channels, keywords, rules, rule_versions, rule_replacements,
  tenant_events, notify_dedup, delivery_tasks, audit_logs, login_sessions,
  system_logs, bot_console_updates
  TO app_user;

-- identities 的 UPDATE 收窄到列级，不给 tenant_id。
-- eng/02 §二 的矩阵这一格写的是「甲类 19 张：SELECT/INSERT/UPDATE/DELETE」，
-- 按字面照写会让 eng/04 §三-2 的补充断言直接报红 —— 那条断言查的正是
-- has_column_privilege(..., 'identities', 'tenant_id', 'UPDATE')。
-- 依据是 spec/06 §2.5「不提供把身份转给别的租户的端点」。
--
-- 写法上有个实测过的陷阱：**列级 REVOKE 减不掉表级 GRANT**。
--   GRANT UPDATE ON identities → has_column_privilege 对每一列都为 t
--   再 REVOKE UPDATE (tenant_id) ON identities → 仍然是 t（实测）
-- 所以必须先 REVOKE 表级再逐列 GRANT，顺序反了不报错、权限照旧。
-- 两条都幂等，本段可重入。
REVOKE UPDATE ON identities FROM app_user;
GRANT SELECT, INSERT, DELETE ON identities TO app_user;
-- 逐列列举而非「除 tenant_id 之外全部」—— PG 没有后者的语法，
-- 且逐列列举会在将来加列时暴露出来（新列默认不可写），这是想要的方向。
-- id 是 GENERATED ALWAYS，本就不可写，故不在列表里。
GRANT UPDATE (kind, tg_user_id, display_name, status, proxy_url, updated_at)
  ON identities TO app_user;

-- 乙类 2 张。它们没有 tenant_id 列，隔离靠 0010 的 EXISTS 策略
GRANT SELECT, INSERT, UPDATE, DELETE ON rule_conditions, delivery_logs TO app_user;

-- 丙类里业务确实要碰的三张。它们不启用 RLS，隔离完全依赖应用层 ——
-- 这是本方案 RLS 覆盖面的已知边界（spec/03 §1.3 末）。
-- control-plane 若要给租户暴露原始事件查询，必须经 tenant_events 连接，
-- 不得直查 canonical_events。
GRANT SELECT, INSERT, UPDATE, DELETE ON canonical_events, media_archive TO app_user;
-- tenants：租户自身的行。无 tenant_id 列故无 RLS，按 id 访问、靠应用层授权。
-- 不给 DELETE —— 注销租户是平台动作，不是业务运行时动作
GRANT SELECT, INSERT, UPDATE ON tenants TO app_user;

-- identity_secrets：凭据密文。仅三个组件可解密（spec/05 §七），
-- 但读取路径仍在业务进程内，故给 DML。真正的保护是信封加密，不是 GRANT
GRANT SELECT, INSERT, UPDATE, DELETE ON identity_secrets TO app_user;

-- account_leases：Worker 抢租约。无租户上下文（丙类），Worker 跑在业务角色下
GRANT SELECT, INSERT, UPDATE, DELETE ON account_leases TO app_user;

-- tenant_consents：只 SELECT, INSERT（spec/03 §7.4）。
-- 凭证一旦被改就不是凭证了。撤回同意不是删行，是再插一条反向记录 ——
-- 审计链要能看出「同意过又撤回了」，而不是「从来没同意过」
GRANT SELECT, INSERT ON tenant_consents TO app_user;

-- 三张表对 app_user 零权限（spec/03 §7.1 / §7.3、spec/07 §1.4）。
-- 这是 A4 白名单的补偿项，由 A7 独立校验；补偿一撤销，A4 放过的侧信道立刻回来。
-- REVOKE 在此其实是空操作（上面没给过），显式写出来是为了让意图可读、
-- 且将来有人在上面的 GRANT 列表里加了这三张表时，本文件仍然正确
REVOKE ALL ON bot_console_sessions     FROM app_user;
REVOKE ALL ON web_sessions             FROM app_user;
REVOKE ALL ON console_login_challenges FROM app_user;

-- 序列权限。16 张表用 GENERATED ALWAYS AS IDENTITY，其隐含序列需要 USAGE，
-- 否则 INSERT 报 "permission denied for sequence"。
-- eng/02 §二 的矩阵没有这一行 —— 它按表列举，漏了序列这一层。
-- 逐个列举与上面的表清单一致，不用 ALL SEQUENCES IN SCHEMA：
-- 后者会把 bot_console_sessions 之外不该碰的序列一并带上（当前恰好无害，
-- 但它是「将来新增表自动获权」的形状，与逐权限列举的原则相反）
GRANT USAGE ON SEQUENCE
  tenants_id_seq, identities_id_seq, source_subscriptions_id_seq,
  target_channels_id_seq, keywords_id_seq, rules_id_seq,
  canonical_events_id_seq, tenant_events_id_seq, media_archive_id_seq,
  delivery_tasks_id_seq, delivery_logs_id_seq, rule_conditions_id_seq,
  audit_logs_id_seq, login_sessions_id_seq, system_logs_id_seq,
  tenant_consents_id_seq
  TO app_user;

-- ── auth_lookup：鉴权反查专用 ────────────────────────────────────
-- 权限面比名字大 —— 它对三张会话表有全量 DML，因为那三张 app_user 完全碰不到。
-- 实际是「鉴权域的专属角色」。命名不改（改名会让已写好的 SQL 与文档全部漂移），
-- 但不要因为名字里有 lookup 就往它身上加更多只读需求 ——
-- 需要跨租户只读的场合用 platform_ops（spec/05 §六）
GRANT SELECT ON tenant_members TO auth_lookup;     -- 只 SELECT，不给写
GRANT SELECT, INSERT, UPDATE, DELETE ON
  bot_console_sessions, web_sessions, console_login_challenges TO auth_lookup;

-- ── platform_ops：平台日志只读 ───────────────────────────────────
-- 策略限 tenant_id IS NULL（0010 的 platform_log_read），所以它看不到租户日志行。
-- 两层都要：GRANT 管动作，RLS 管行
GRANT SELECT ON system_logs, audit_logs TO platform_ops;

-- ── 不授予的，逐条记明 ───────────────────────────────────────────
-- TRUNCATE          三个运行时角色一律不给（eng/02 §二 那一行是安全要求）。
--                   清表交给迁移角色。上面全程没出现 TRUNCATE 字样即是落实
-- pg_dump / 备份    只有 tgm_owner 能做（ops/01 §备份：pg_dump -U tgm_owner）。
--                   以 app_user 跑备份实测必然失败 —— FORCE + 策略下导不出全量
-- 建表 / 建策略     只有 tgm_owner。app_user 不是任何表的 owner，由 A2 校验

-- SET ROLE 持续到事务末尾，而 sqlx 记版本号那条 INSERT 也在同一事务里 ——
-- 不复位则报 "permission denied for table _sqlx_migrations"（已实测）。
RESET ROLE;
