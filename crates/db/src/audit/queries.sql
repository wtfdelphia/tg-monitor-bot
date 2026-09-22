-- RLS 静态审计的断言全文。定义处是 docs/design/eng/04-CI门禁.md，
-- 本文件是它的可执行副本 —— 不是重写。每条断言的编号、注释里的实测依据，
-- 都与那份文档一一对应；改这里必须同步改那里。
--
-- 约定：**每条断言返回任何行即失败**，返回的行就是违规清单。
-- 所以断言都写成「查违规项」而不是「查合规项」—— 前者的空集是好消息，
-- 后者的空集是断言写坏了（eng/04 §二 在 A0 与 A8 下各记了一处「静默全绿」实测）。
--
-- 每条以 `-- name: <ID> <一句话>` 起头，由 crates/db/src/audit/mod.rs 切分。
-- 结果列统一转成一列文本（`SELECT q::text FROM (...) q`），因为调用方
-- 只需要把违规行原样打出来，不需要按类型解读 —— 不同断言列形状不同，
-- 统一成文本换来的是 Rust 侧只有一条取值路径。

-- 「运行时角色集合」这个派生在 A0/A1/A2a/A2b/C2 里各写一遍（A2a 是内联在
-- WHERE 里，另四处是 CTE），没有抽成视图或函数。
-- 两个理由：审计只读目录，建视图需要 owner 权限且 A8 正在禁视图；
-- 每条断言自带 CTE 才能单独粘进 psql 复现 —— 排查时这一点比省几行重要。
-- 代价是五处要同步改。派生逻辑本身由两条断言兜住：A0 管「算成了空集」，
-- C5 管「不为空却缺人」里可实测的那一种（权限经组角色继承）。
--
-- 派生查三处而不是两处：relacl（表级 GRANT）、**attacl（列级 GRANT）**、pg_policy。
-- 漏掉 attacl 的后果实测过：`CREATE ROLE probe_col LOGIN` +
-- `GRANT UPDATE (tenant_id) ON identities TO probe_col` 之后，relacl 命中 0、
-- pg_policy 命中 0、attacl 命中 1 —— 整个角色从派生里落选，A1/A2a/A2b/C2 一条
-- 都不查它，而它 has_column_privilege(identities, tenant_id, UPDATE) = true，
-- 正是 C2 要拦的那件事。A0 也拦不住这种失效：派生「不为空却缺人」时它照样绿。

-- name: A0 runtime_role 派生必须非空
-- 这条不在 eng/04 的编号里，是本次实现新增的**元断言**。
-- A1/A2/C2 都建立在「运行时角色集合」这个派生上（见下方 runtime_role CTE）。
-- 派生若因为 GRANT 写法变化而算出空集，那三条断言会全部静默通过 ——
-- 这是「静默全绿」那类坑：断言没报错，只是不再检查任何东西。
-- 所以先断言派生本身有结果。当前实测得到 app_user / auth_lookup / platform_ops 三个。
--
-- 它只管「空集」这一种失效，管不了「少算了某个角色」。后者靠派生本身查全
-- relacl/attacl/pg_policy 三处来保证 —— 但那三处认的都是**直授**，
-- 权限经组角色继承而来时被授的是组的 oid，登录角色本人查不到。
-- 这个缺口由 C5 从另一侧补：禁掉登录角色持有组成员身份，让「权限都是直授的」
-- 这个前提由断言维持，而不是靠假设。为什么不直接改派生去追认继承，见 C5 的注释。
SELECT q::text FROM (
  SELECT '运行时角色派生为空集：A1/A2/C2 将全部空转，不是通过' AS why
  WHERE NOT EXISTS (
    SELECT 1 FROM pg_roles r
    WHERE r.rolcanlogin
      AND (EXISTS (SELECT 1 FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(c.relacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_attribute att
                   JOIN pg_class c ON c.oid = att.attrelid
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(att.attacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND att.attnum > 0 AND NOT att.attisdropped
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_policy p WHERE r.oid = ANY (p.polroles)))
  )
) q;

-- name: MAIN RLS 三件套缺一即失败
-- eng/04 §一 的主查询，原样照抄。判据是「表里有没有 tenant_id 列」，
-- 不是人工维护的表清单 —— 新建表自动纳入，这是它最重要的性质。
-- 乙类 rule_conditions / delivery_logs 无 tenant_id 列，不在覆盖内，见 C1。
SELECT q::text FROM (
  SELECT c.relname,
         c.relrowsecurity      AS rls_on,
         c.relforcerowsecurity AS forced,
         (SELECT count(*) FROM pg_policy p WHERE p.polrelid = c.oid) AS policies
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'public'
    AND c.relkind IN ('r','p')            -- 含分区父表
    AND EXISTS (SELECT 1 FROM pg_attribute a
                WHERE a.attrelid = c.oid AND a.attname = 'tenant_id'
                  AND a.attnum > 0 AND NOT a.attisdropped)
    AND NOT (c.relrowsecurity                 -- 已 ENABLE
             AND c.relforcerowsecurity        -- 已 FORCE（漏这个，owner 全见）
             AND (SELECT count(*) FROM pg_policy p WHERE p.polrelid = c.oid) > 0)
  ORDER BY 1
) q;

-- name: A1 运行时角色不得持有 TRUNCATE
-- TRUNCATE 不受 RLS 约束 —— 有这个权限就能清空他租户的数据，策略写得再对也拦不住
-- （spec/05 §六）。eng/04 原文只查 app_user，本实现按 A0 的派生查全部运行时角色：
-- 硬编码角色名本身就是 eng/04 §六 自己记下的弱点。
SELECT q::text FROM (
  WITH runtime_role AS (
    SELECT r.oid, r.rolname FROM pg_roles r
    WHERE r.rolcanlogin
      AND (EXISTS (SELECT 1 FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(c.relacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_attribute att
                   JOIN pg_class c ON c.oid = att.attrelid
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(att.attacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND att.attnum > 0 AND NOT att.attisdropped
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_policy p WHERE r.oid = ANY (p.polroles)))
  )
  SELECT rr.rolname AS role, c.relname AS tbl
  FROM runtime_role rr
  CROSS JOIN pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'public' AND c.relkind = 'r'
    AND has_table_privilege(rr.oid, c.oid, 'TRUNCATE')
  ORDER BY 1, 2
) q;

-- name: A2a 运行时角色不得是超级用户或 BYPASSRLS
-- 任一成立则整个 RLS 机制被旁路且不报错。
-- eng/01 §4.3 在启动期查同一件事（只查 current_user），两处都要：
-- CI 管迁移产物，启动期管实际连上的库。
SELECT q::text FROM (
  SELECT r.rolname, r.rolsuper, r.rolbypassrls FROM pg_roles r
  WHERE r.rolcanlogin
    AND (EXISTS (SELECT 1 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 CROSS JOIN LATERAL aclexplode(c.relacl) a
                 WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                   AND a.grantee = r.oid)
      OR EXISTS (SELECT 1 FROM pg_attribute att
                 JOIN pg_class c ON c.oid = att.attrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 CROSS JOIN LATERAL aclexplode(att.attacl) a
                 WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                   AND att.attnum > 0 AND NOT att.attisdropped
                   AND a.grantee = r.oid)
      OR EXISTS (SELECT 1 FROM pg_policy p WHERE r.oid = ANY (p.polroles)))
    AND (r.rolsuper OR r.rolbypassrls)
  ORDER BY 1
) q;

-- name: A2b 运行时角色不得是任何表的 owner
-- owner 身份在 FORCE 之前完全绕过策略；FORCE 之后则是另一种坏法
-- （无策略匹配 owner → 静默 0 行）。两种都不该出现在运行时角色上。
SELECT q::text FROM (
  WITH runtime_role AS (
    SELECT r.oid, r.rolname FROM pg_roles r
    WHERE r.rolcanlogin
      AND (EXISTS (SELECT 1 FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(c.relacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_attribute att
                   JOIN pg_class c ON c.oid = att.attrelid
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(att.attacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND att.attnum > 0 AND NOT att.attisdropped
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_policy p WHERE r.oid = ANY (p.polroles)))
  )
  SELECT rr.rolname AS role, c.relname AS owned
  FROM runtime_role rr
  JOIN pg_class c ON c.relowner = rr.oid
  ORDER BY 1, 2
) q;

-- name: A3 不得存在引用 current_setting 的 CHECK 约束
-- 这条最值得自动化：CHECK (tenant_id = current_setting(...)) 建表时不报错、
-- 日常也正常，只在恢复备份时才炸 —— pg_restore 的会话里没有 app.tenant_id，
-- 每一行都过不了约束。「建表时接受、恢复时失败」的定时炸弹，人工评审拦不住。
SELECT q::text FROM (
  SELECT conrelid::regclass AS tbl, conname FROM pg_constraint
  WHERE contype = 'c' AND pg_get_constraintdef(oid) LIKE '%current_setting%'
  ORDER BY 1, 2
) q;

-- name: A4 有 tenant_id 的表上唯一约束必须含 tenant_id
-- 漏了会开一条 RLS 补不了的侧信道：插入撞唯一约束时返回 duplicate key，
-- 攻击者据此判断他租户是否存在某个值 —— 行读不到，存在性泄漏了。
-- 本项目已在 rule_versions 上实测命中过一次，并改了 PK（见 migrations/0004）。
SELECT q::text FROM (
  SELECT c.relname AS tbl, i.relname AS idx
  FROM pg_index x
  JOIN pg_class c ON c.oid = x.indrelid
  JOIN pg_class i ON i.oid = x.indexrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'public' AND x.indisunique
    -- 排除单列代理键唯一索引（GENERATED ALWAYS，attidentity='a'）：
    -- 值由库分配，攻击者填不进去，duplicate key 探测不出东西。
    -- 判据是「攻击者能不能构造这个值」，不是「是不是主键」——
    -- 所以 bot_console_sessions 的自然键主键仍会被命中（实测，且命中是对的）
    -- 用 indnkeyatts 不是 indnatts：前者是参与唯一性的键列数，后者含 INCLUDE 列
    AND NOT (x.indnkeyatts = 1 AND EXISTS (
          SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
          AND a.attnum = x.indkey[0] AND a.attidentity = 'a'))
    -- 白名单两项，都是已论证 + 已由 GRANT 层补偿的破例：
    --   bot_console_sessions_pkey  鉴权前不知 tenant_id（spec/03 §7.1）
    --   uq_web_session_id          随机 32 字节 id 需全局唯一（spec/03 §7.3）
    -- eng/04 原文这里是三项，第三项 uq_task_idem 已在本步骤实测确认为真实漏洞
    -- 并改掉（见 A7 的注释与 migrations/0006），故白名单少一项。
    AND i.relname NOT IN ('bot_console_sessions_pkey',
                          'uq_web_session_id')
    AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
                AND a.attname = 'tenant_id' AND a.attnum > 0 AND NOT a.attisdropped)
    -- 成员测试只看前 indnkeyatts 个键列，**不是整个 indkey**。
    -- INCLUDE 列也在 indkey 里，但不参与唯一性判定 —— 拿整个 indkey 匹配
    -- 会把 tenant_id 塞进 INCLUDE 的索引当成合规放过。实测：
    --   CREATE UNIQUE INDEX ... ON keywords (word) INCLUDE (tenant_id)
    --   → indkey = "3 2"、indnatts = 2、indnkeyatts = 1
    --   换租户插同一个 word 照样报 duplicate key，侧信道原样开着。
    -- int2vector 转数组后下界是 0（实测 [0:1]={3,2}），所以切片写 [0:indnkeyatts-1]。
    --
    -- 已知的误报（可接受，方向保守）：表达式索引的 indkey 里是 0，
    -- 而没有任何列的 attnum 是 0，所以含 tenant_id 的表达式唯一索引
    -- （如 ((tenant_id::text || word))）会被报红。实测确认过。
    AND NOT EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
                    AND a.attname = 'tenant_id'
                    AND a.attnum = ANY((x.indkey::smallint[])[0:x.indnkeyatts-1]))
  ORDER BY 1, 2
) q;

-- name: A5 tenant_id 必须 NOT NULL
-- 可空会让复合外键被 NULL 绕过（NULL 参与的 FK 不做检查）。
-- 三张排除表是铁律 5 的有意破例（spec/03 §1.2）。
--
-- 本步骤订正过这里的一句错话。原注释写的是「它们都不做复合 FK 的父表，
-- 破例不会传导成跨租户边 —— 这一点由 A9 独立校验」，两个分句都不成立：
--   identities 是七张子表的 FK 父表（identity_secrets / source_subscriptions /
--   target_channels / canonical_events / delivery_tasks / account_leases /
--   bot_console_updates），且七条 FK 全是单列的；
--   而 A9 的父表侧要求 tenant_id NOT NULL，identities 的 tenant_id 恰好可空，
--   于是这七条全部自动落选 —— 「由 A9 独立校验」是空的。
--
-- 这段注释自己也订正过一次：先前写「那是共享身份池的设计后果，池里的身份
-- 本就跨租户可见」，也是错的。实测那条边指向的是**他租户的私有身份**，
-- 不是池里 tenant_id IS NULL 的共享身份 —— 攻击者甚至看不见那一行：
--   tenant 21 上下文下 SELECT ... WHERE id=900 → 0 行（RLS 挡住）
--   INSERT target_channels(21, -1, 900)        → INSERT 0 1（边建成了）
-- 「读不到却能引用」，因为 FK 检查不过 RLS。
-- 另外两个后果也实测了：
--   填存在的他租户 id → INSERT 0 1；填不存在的 → FK violation，两者可区分
--     = 一个跨租户 id 枚举 oracle
--   建边之后 tenant 20 删自己的身份 900 → ERROR: still referenced
--     = 攻击者可单方面钉住他租户的数据
-- 这三条与 A9 在复合 FK 下关掉的后果同形，只是这里没有断言可依。
-- 缺口只能靠应用层的身份归属校验兜（spec/06 §2.5 第 1 条），不靠库层的 FK 形状
-- —— 这里不该声称有一道自动校验。R19 也不覆盖这个形状（它测共享身份，
-- 上面实测的是他租户的私有身份），登记在 pre-do/03 十九。
SELECT q::text FROM (
  SELECT c.relname FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
    AND c.relname NOT IN ('identities', 'system_logs', 'audit_logs')
    AND a.attname = 'tenant_id' AND a.attnum > 0 AND NOT a.attisdropped
    AND NOT a.attnotnull
  ORDER BY 1
) q;

-- name: A6 多策略表上不得有恒真或未限定角色的 PERMISSIVE 策略
-- PERMISSIVE 之间是 OR —— 一条恒真策略就作废了旁边的租户策略。
-- 实测：polroles = '{0}' 即 PUBLIC，{16385} 之类是具体角色 OID。
-- 单策略表（绝大多数）不进这个查询，下方的 count(*) > 1 子查询已排除。
--
-- 本步骤订正过判据。原判据只查 polroles = '{0}'，漏掉了角色限定的恒真策略：
--   CREATE POLICY ... ON tenant_members FOR SELECT TO app_user USING (true)
--   的 polroles 是 {16386} 而不是 {0}，旧断言一声不响。
--   实测危害与 PUBLIC 等同：attacker 身份读 tenant_members 从 0 行变成 1 行。
-- 所以判据是「PUBLIC 策略 **或** 表达式恒真」，两者都要查。
--
-- 覆盖边界（PG 不规范化恒真表达式，实测）：
--   USING (true)                              → polqual 存成 'true'，抓得到
--   USING (1=1)                                → 存成 '(1 = 1)'，抓不到
--   USING (x IS NULL OR x IS NOT NULL)         → 原样保留，抓不到
-- 字面量匹配只拦最直白的那种。绕开它需要刻意写成等价的复杂式子，
-- 那已不是「顺手写错」而是「刻意规避审计」—— 后者靠评审，不靠这条断言。
--
-- 报的是策略名而不是表名（原实现按表聚合）：多策略表上报出具体哪条策略恒真，
-- 修的时候能直接定位，不必再自己去 pg_policy 里翻。
SELECT q::text FROM (
  SELECT c.relname AS tbl, p.polname, p.polroles::text AS roles,
         CASE WHEN p.polroles = '{0}' THEN 'PUBLIC 策略'
              ELSE 'qual 恒真' END AS why
  FROM pg_policy p
  JOIN pg_class c ON c.oid = p.polrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'public' AND p.polpermissive
    AND (SELECT count(*) FROM pg_policy p2
         WHERE p2.polrelid = p.polrelid AND p2.polpermissive) > 1
    AND (p.polroles = '{0}'
         OR pg_get_expr(p.polqual, p.polrelid) = 'true'
         OR pg_get_expr(p.polwithcheck, p.polrelid) = 'true')
  ORDER BY 1, 2
) q;

-- name: A7 A4 白名单表的 GRANT 补偿必须在位
-- 白名单是靠 GRANT 换 RLS 的：app_user 完全碰不到那些表，只有 auth_lookup
-- 能读写（eng/02 §二 的权限矩阵）。补偿一撤销，A4 放过的侧信道立刻回来。
-- **A4 与 A7 必须成对存在。**
--
-- eng/04 原文只查 bot_console_sessions 一张表，且 §六 自己记着「另两项没有任何
-- 断言」。本实现把白名单写成 VALUES 列表、由索引名反查所属表，
-- 这样 A4 的白名单与 A7 的补偿清单不可能各自漂移 —— 表名是从索引名算出来的。
--
-- 第三项 uq_task_idem 不在这里，因为它的补偿**根本不存在**：
-- app_user 对 delivery_tasks 有全量 DML（那是业务主表，不可能收回）。
-- 本步骤实测确认这是一条真实可用的通道，不是理论风险：
--   idem_key = sha256("tenant|rule_id|rule_version|event_id|target")，
--   target 是公开频道 id（攻击者可自行订阅同一频道而得知），其余四项是小整数。
--   以 ON CONFLICT (idem_key) DO NOTHING 批量插入 5000 个候选，一条语句 0.58 秒，
--   返回的「插入成功行数」就是 oracle —— 5000 个候选里命中 1 个时返回 4999。
--   不需要逐个试探、不触发任何报错、也不留下行（事务回滚即可）。
-- 修法是把索引改成 (tenant_id, idem_key)（见 migrations/0006）。
-- 改后同一批探测返回 5000（无信息），而同租户内重复 idem_key 仍报 duplicate key
-- —— 幂等这项正业没有削弱，因为 tenant_id 本来就在哈希输入里，
-- 加进索引键不改变任何一对 (tenant, key) 的唯一性判定。
-- 所以 uq_task_idem 从 A4 白名单里移除了，它现在由 A4 本身正常覆盖。
SELECT q::text FROM (
  WITH whitelist(idx) AS (
    VALUES ('bot_console_sessions_pkey'), ('uq_web_session_id')
  ),
  -- 由索引名反查表名。找不到索引也要报红：索引被改名而白名单没跟上，
  -- 等于 A4 的白名单指向了一个不存在的东西，而 A4 那边只会静默少排除一项
  -- 反查必须限定 nspname='public' 且 relkind='i'，且往下传 oid 而不是 relname。
  -- 三处都是同一个失效的不同侧面：别的 schema 里出现同名对象时 ——
  --   不限定 relkind：同名的表/视图让 indrelid 子查询返回 NULL，
  --     这一条被当成「索引不存在」报红（误报，方向安全但指错地方）；
  --   不限定 nspname：另一个 schema 里的同名**索引**会被反查到，
  --     拿到它的基表 relname 再喂给 has_*_privilege，
  --     而该表不在搜索路径里 → ERROR: relation "t" does not exist。
  --     A7 整条崩掉，排在它后面的 A8/A9/C1/C4/C2 也一起不执行（mod.rs 的 `?`）；
  --   传 relname 而不是 oid：即使限定了 schema，裸名字仍要走 search_path 解析。
  -- 本库目前只有 public 一个 schema，所以这是「以后会炸」而不是「现在是坏的」。
  wl_tbl AS (
    SELECT w.idx, c.oid AS tbl_oid, c.relname AS tbl
    FROM whitelist w
    LEFT JOIN pg_class i ON i.relname = w.idx AND i.relkind = 'i'
                       AND i.relnamespace = 'public'::regnamespace
    LEFT JOIN pg_class c ON c.oid = (SELECT x.indrelid FROM pg_index x WHERE x.indexrelid = i.oid)
  )
  SELECT idx, coalesce(tbl, '<索引不存在>') AS tbl,
         '白名单索引查不到所属表' AS why
  FROM wl_tbl WHERE tbl IS NULL
  UNION ALL
  -- 四项有列级形式的权限用 has_any_column_privilege。
  -- 原实现全用 has_table_privilege，实测漏报：
  --   GRANT SELECT (id) ON web_sessions TO app_user
  --   → has_table_privilege = false、has_any_column_privilege = true
  --   而 app_user 的 `SELECT id FROM web_sessions` 真的执行成功（不是权限拒绝）。
  -- 白名单是靠 GRANT 换 RLS 的，补偿被列级授权掏空了旧断言察觉不到。
  -- 表级授权同样会让 has_any_column_privilege 为真，所以这一支覆盖两种写法。
  -- 传 tbl_oid（regclass 重载）而不是 w.tbl，理由见上面 wl_tbl 的注释
  SELECT w.idx, w.tbl, 'app_user 仍持有 ' || pv || '（含列级）' AS why
  FROM wl_tbl w
  CROSS JOIN unnest(ARRAY['SELECT','INSERT','UPDATE','REFERENCES']) pv
  WHERE w.tbl_oid IS NOT NULL AND has_any_column_privilege('app_user', w.tbl_oid, pv)
  UNION ALL
  -- DELETE / TRUNCATE 没有列级形式，只能问表级。
  -- 实测传给 has_any_column_privilege 会直接报错：
  --   ERROR: unrecognized privilege type: "DELETE"
  -- 所以两支必须分开，不能图省事合成一支
  SELECT w.idx, w.tbl, 'app_user 仍持有 ' || pv AS why
  FROM wl_tbl w
  CROSS JOIN unnest(ARRAY['DELETE','TRUNCATE']) pv
  WHERE w.tbl_oid IS NOT NULL AND has_table_privilege('app_user', w.tbl_oid, pv)
  ORDER BY 1, 3
) q;

-- name: A8 tenant_id 表上不得有普通视图与 SECURITY DEFINER 函数
-- 这条拦的不是泄漏，是功能坏掉：FORCE 后这两条路径以 owner 身份执行，
-- 无策略匹配 owner → 静默 0 行。在功能测试里表现为「查不到数据」，
-- 极易误诊成数据没写进去。所以它是静态断言而不是动态测试。
SELECT q::text FROM (
  WITH tenant_tbl AS (
    SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
      AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
                  AND a.attname = 'tenant_id' AND a.attnum > 0 AND NOT a.attisdropped)
  )
  -- 视图侧与函数侧**不限定 schema**，只有上面 tenant_tbl（基表侧）限定 public。
  -- 别的 schema 里的视图照样能读 public 的表，SECURITY DEFINER 函数更是
  -- 与所在 schema 无关。原实现三处都写死 public，等于「换个 schema 就绕过」。
  -- 排除 pg_catalog / information_schema：那两处的 SECURITY DEFINER 函数
  -- 是 PG 自带的，不是本项目能改的东西，报出来只是噪声。
  -- 用 nspname 前缀过滤而不是 oid 白名单，因为 pg_toast / pg_temp_N 也要排除。
  SELECT n.nspname || '.' || v.relname AS obj, 'view' AS kind FROM pg_class v
  JOIN pg_namespace n ON n.oid = v.relnamespace
  WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    AND v.relkind = 'v'
    -- 用 pg_options_to_table 取值再转 bool，不要拿 reloptions 做数组包含判断：
    -- 实测 security_invoker = on 会原样存成 {security_invoker=on}，
    -- 而 = true 与裸写都存成 =true，按数组匹配会把合规视图当违规报出来
    AND NOT coalesce((SELECT o.option_value::bool
                      FROM pg_options_to_table(v.reloptions) o
                      WHERE o.option_name = 'security_invoker'), false)
    -- 视图对基表的依赖挂在 pg_rewrite 上，不是视图 oid 上。实测直接用
    -- pg_depend.objid = v.oid 查不到任何行，断言会静默全绿
    AND EXISTS (SELECT 1 FROM pg_rewrite rw
                JOIN pg_depend d ON d.classid = 'pg_rewrite'::regclass AND d.objid = rw.oid
                WHERE rw.ev_class = v.oid AND d.refclassid = 'pg_class'::regclass
                  AND d.refobjid IN (SELECT oid FROM tenant_tbl))
  UNION ALL
  -- 函数不按表过滤：pg_depend 不记录函数体引用的表（实测 0 行），
  -- 所以无法判断一个 SECURITY DEFINER 函数碰不碰带 tenant_id 的表。
  -- 定案是「全面禁用」，按全库报出反而是正确形状
  SELECT n.nspname || '.' || p.proname, 'security_definer_function' FROM pg_proc p
  JOIN pg_namespace n ON n.oid = p.pronamespace
  WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    AND p.prosecdef
) q;

-- name: A9 双方都有 tenant_id NOT NULL 时外键必须含 tenant_id
-- 单列 FK 会让跨租户边在库层可表达：RLS 不拦建边、规则静默失效、
-- FK 报错成为 id 枚举侧信道、且可 DoS 他租户的 DELETE。四项均已实测。
-- 断言形状承载了铁律 4 的例外：父表侧要求 tenant_id NOT NULL，
-- identities 的 tenant_id 有意可空 → 自动落选，不需要硬编码表名。
-- 与 A4 互补不重复：A4 管唯一约束（探测存在性），A9 管外键（建跨租户边）。
SELECT q::text FROM (
  SELECT ct.conrelid::regclass AS child, ct.confrelid::regclass AS parent, ct.conname
  FROM pg_constraint ct
  JOIN pg_class c ON c.oid = ct.conrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE ct.contype = 'f' AND n.nspname = 'public'
    AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = ct.conrelid
                AND a.attname = 'tenant_id' AND a.attnum > 0 AND NOT a.attisdropped)
    AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = ct.confrelid
                AND a.attname = 'tenant_id' AND a.attnum > 0 AND NOT a.attisdropped
                AND a.attnotnull)
    -- 判据是「列序对齐」，不是「conkey 里含 tenant_id」。
    -- 后者只要子表 tenant_id 出现在键里就放行，不管父表侧对到的是哪一列 ——
    -- FOREIGN KEY (tenant_id, keyword_id) REFERENCES keywords (id, tenant_id)
    -- 这种错位写法形式合规而实质仍可建跨租户边。
    -- conkey[i] 与 confkey[i] 是按下标一一对应的，所以要同一个 i 上两边都是 tenant_id。
    -- 实测现有 10 条复合 FK 在这个判据下全部对齐（新旧判据在当前库同为绿），
    -- 所以这是补覆盖缺口，不是修现存违规。
    AND NOT EXISTS (
          SELECT 1 FROM generate_subscripts(ct.conkey, 1) i
          WHERE ct.conkey[i] = (SELECT a.attnum FROM pg_attribute a
                                WHERE a.attrelid = ct.conrelid AND a.attname = 'tenant_id')
            AND ct.confkey[i] = (SELECT a.attnum FROM pg_attribute a
                                 WHERE a.attrelid = ct.confrelid AND a.attname = 'tenant_id'))
  ORDER BY 1, 3
) q;

-- ── eng/04 §三 的补充断言。不在 A1~A9 编号里，各自只管一处 ──────────────

-- name: C1 乙类两张子表的 RLS 白名单断言
-- rule_conditions / delivery_logs 没有 tenant_id 列，主查询覆盖不到。
-- 这是唯一一处用白名单表名的断言，因为判据不可能从列结构推出来 ——
-- 这两张表恰恰是以「没有 tenant_id」为特征的。
--
-- 原实现只查 pg_class 的两个布尔标志，三处漏报：
--   表被删或改名 —— `WHERE relname IN (...)` 匹配不到就是零行，零行即通过。
--     白名单断言必须从白名单那一侧出发（VALUES LEFT JOIN），不能从目录出发，
--     否则「要查的东西不存在」和「查过了没问题」在结果里长得一样。
--   策略数为 0 —— MAIN 查策略数，但它按「有 tenant_id 列」筛表，
--     而这两张表的特征恰恰是没有 tenant_id，所以谁都没在看它们的策略。
--     FORCE + 零策略是默认拒绝：业务一行都读不到。这是故障不是泄漏，
--     但同样该红，而且比泄漏更容易被误诊成「数据没写进去」（同 A8 的理由）。
--   没有 nspname 过滤 —— 别的 schema 里的同名表会混进来，
--     与 A7 反查那处是同一个形状。
SELECT q::text FROM (
  SELECT w.tbl,
         CASE WHEN c.oid IS NULL THEN '表不存在于 public —— 被删或被改名'
              WHEN NOT c.relrowsecurity THEN 'RLS 未 ENABLE'
              WHEN NOT c.relforcerowsecurity THEN 'RLS 未 FORCE'
              ELSE '零条策略 —— FORCE 下等于默认拒绝，业务读不到任何行'
         END AS why
  FROM (VALUES ('rule_conditions'), ('delivery_logs')) w(tbl)
  LEFT JOIN pg_class c ON c.relname = w.tbl AND c.relkind IN ('r','p')
                      AND c.relnamespace = 'public'::regnamespace
  WHERE c.oid IS NULL
     OR NOT (c.relrowsecurity AND c.relforcerowsecurity
             AND (SELECT count(*) FROM pg_policy p WHERE p.polrelid = c.oid) > 0)
  ORDER BY 1
) q;

-- name: C4 不得存在物化视图
-- 本步骤新增。它与 A8 的差别在于判据的性质：A8 禁的是「配置不对的普通视图」
-- （加 security_invoker 就合规），这条禁的是「这类对象本身」——
-- 物化视图**根本不支持 RLS**，实测：
--   ALTER TABLE probe_mv ENABLE ROW LEVEL SECURITY
--   → ERROR: ALTER action ENABLE ROW SECURITY cannot be performed on relation
--     DETAIL: This operation is not supported for materialized views.
-- 所以没有「配齐三件套」这条出路，只能禁建。
--
-- 危害实测：victim 租户写入 keywords 一行，以 owner 建物化视图固化其内容，
-- attacker 租户从基表 keywords 读到 0 行，从物化视图读到 1 行 'victim-secret'。
-- 原实现漏了它是两处 relkind 都不含 'm'：MAIN 查 ('r','p')、A8 查 'v'。
--
-- 不引用带 tenant_id 的表也报。理由是判据不该依赖 pg_depend 的准确性：
-- 物化视图可以后续 REFRESH、可以被 ALTER，而「当前定义没碰租户表」
-- 不保证下一版不碰。一律禁掉，需要破例时走评审加白名单。
SELECT q::text FROM (
  WITH tenant_tbl AS (
    SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
      AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
                  AND a.attname = 'tenant_id' AND a.attnum > 0 AND NOT a.attisdropped)
  )
  -- 物化视图侧不限定 schema，同 A8：换个 schema 建照样能固化别人的行。
  -- 只有 tenant_tbl（基表侧）限定 public
  SELECT n.nspname || '.' || m.relname AS matview,
         CASE WHEN EXISTS (SELECT 1 FROM pg_rewrite rw
                           JOIN pg_depend d ON d.classid = 'pg_rewrite'::regclass
                                           AND d.objid = rw.oid
                           WHERE rw.ev_class = m.oid
                             AND d.refclassid = 'pg_class'::regclass
                             AND d.refobjid IN (SELECT oid FROM tenant_tbl))
              THEN '读了带 tenant_id 的表' ELSE '未引用 tenant_id 表，仍禁止' END AS why
  FROM pg_class m JOIN pg_namespace n ON n.oid = m.relnamespace
  WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    AND m.relkind = 'm'
  ORDER BY 1
) q;

-- name: C2 identities.tenant_id 写入即不可变
-- spec/06 §2.5 要求「不提供把身份转给别的租户的端点」。库层没有「列不可 UPDATE」
-- 这种约束，所以断言只能做到检查运行时角色对 identities 的 UPDATE 权限
-- 是否被收窄到不含 tenant_id 列。
--
-- 这条断言弱于它想保证的性质：列级 GRANT 能拦住改这一列，
-- 但拦不住「先删后建」这种等效操作。真正的保证靠 §2.5 的「写入路径只有一条」。
--
-- eng/04 原文只查 app_user，本实现按 A0 的派生查全部运行时角色。
-- 实测过一处写法陷阱：**列级 REVOKE 减不掉表级 GRANT**。
--   GRANT UPDATE ON identities → 表级，has_column_privilege 对每一列都为 t
--   再 REVOKE UPDATE (tenant_id) → 仍然是 t
-- 必须先 REVOKE 表级 UPDATE，再逐列 GRANT（见 migrations/0011）。
-- 这个陷阱正是本断言存在的理由：改法写反了不报错，权限照旧。
SELECT q::text FROM (
  WITH runtime_role AS (
    SELECT r.oid, r.rolname FROM pg_roles r
    WHERE r.rolcanlogin
      AND (EXISTS (SELECT 1 FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(c.relacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_attribute att
                   JOIN pg_class c ON c.oid = att.attrelid
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                   CROSS JOIN LATERAL aclexplode(att.attacl) a
                   WHERE n.nspname = 'public' AND c.relkind IN ('r','p')
                     AND att.attnum > 0 AND NOT att.attisdropped
                     AND a.grantee = r.oid)
        OR EXISTS (SELECT 1 FROM pg_policy p WHERE r.oid = ANY (p.polroles)))
  )
  SELECT rr.rolname AS role, 'identities.tenant_id UPDATE' AS what
  FROM runtime_role rr
  WHERE has_column_privilege(rr.oid, 'identities'::regclass, 'tenant_id', 'UPDATE')
  ORDER BY 1
) q;

-- name: C5 登录角色不得持有组成员身份
-- 本步骤新增，补的是 A0 那条派生的一个盲区，不在 eng/04 原文里。
--
-- 派生（A0/A1/A2a/A2b/C2 共五处同一份 CTE）认的是 `a.grantee = r.oid`，
-- 即**直接授给该角色 oid** 的 ACL 条目。权限若经由一个组角色继承而来，
-- 被授的是组的 oid，登录角色本人在 relacl/attacl/polroles 三处都查不到 ——
-- 整个角色从派生里落选，那四条断言一条都不查它。而 A0 照样绿：
-- 派生非空，只是缺人。这正是 A0 注释里承认的那条覆盖边界。
--
-- 为什么不去修派生。把 `a.grantee = r.oid` 换成
-- `pg_has_role(r.oid, a.grantee, 'USAGE')` 是直觉修法，实测会把 `postgres`
-- 拉进派生集（它经 `GRANT tgm_owner TO postgres` 继承 owner 的权限），
-- 而 `postgres` 是 rolsuper + rolbypassrls → A2a 在健康库上当场报红，是误报。
-- 那再加一句 `AND NOT r.rolsuper` 排掉？方向更坏：A2a 的全部职责就是
-- 「运行时角色被提成超级用户时报红」，而把超级用户从派生里排掉，
-- 这条断言就永远不可能被触发 —— 从「误报」换成「绿得毫无意义」。
-- 所以这里换个方向：不去追认继承来的权限，而是禁掉「登录角色持有组成员身份」
-- 这件事本身。派生的前提（权限都是直授的）从此由断言来维持，而不是靠假设。
--
-- 豁免只有 postgres → tgm_owner 这一对，而不是豁免 postgres 整个角色。
-- 它是 scripts/init/01-roles.sql:7 故意建的：tgm_owner 是 NOLOGIN，
-- 审计与迁移都靠 postgres 继承它的 owner 身份才能跑。写成「一对」而不是
-- 「一个角色」，是为了让别人以后给 postgres 加第二个组时这条断言会红。
--
-- 列名按 PG16+ 的 pg_auth_members（inherit_option / set_option），实测 PG17.11 有这两列。
-- 一并打出来是因为两者差别很大：inherit 是自动生效，set 要 SET ROLE 才生效，
-- 但都能拿到权限 —— 所以判据不看这两个标志，只看关系存在不存在。
SELECT q::text FROM (
  SELECT m.rolname AS login_role, g.rolname AS grp,
         CASE WHEN g.rolsuper THEN '该组是超级用户 —— 等于给了登录角色超级用户'
              WHEN g.rolbypassrls THEN '该组持有 BYPASSRLS —— RLS 对该登录角色失效'
              ELSE '经该组继承的权限对 runtime_role 派生不可见，A1/A2/C2 不会查它'
         END AS why,
         'inherit=' || am.inherit_option || ' set=' || am.set_option AS how
  FROM pg_auth_members am
  JOIN pg_roles m ON m.oid = am.member
  JOIN pg_roles g ON g.oid = am.roleid
  WHERE m.rolcanlogin
    AND NOT (m.rolname = 'postgres' AND g.rolname = 'tgm_owner')
  ORDER BY 1, 2
) q;

-- C3（openapi.json 与代码一致）不在本文件里：它比对的是文件而不是数据库状态，
-- 归 `tgm openapi --check` 这个独立子命令。见 eng/04 §三-5。
