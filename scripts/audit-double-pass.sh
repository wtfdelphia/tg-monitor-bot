#!/usr/bin/env bash
# ADR-0022 的双遍执行器。用法：TGM_DB_URL_OWNER=... scripts/audit-double-pass.sh
#
# 为什么要有这个脚本，而不是在 CI 里写两行 `tgm audit-rls`：
# 判据不是「跑两次」，是**第二遍必须红**。一个「期望失败」的步骤在 CI 的 YAML 里
# 很难表达（退出码非 0 就中断整个 job），写成脚本才能把期望编码进去。
#
# 更要紧的理由：`USING (true)` 能通过 A1~A9 全部断言（eng/04 §五 实测）。
# 所以「第一遍全绿」本身不携带信息 —— 携带信息的是差值：破坏一处配置，
# 对应那条断言必须红。一条永远绿的断言与一条不存在的断言，在 CI 日志里长得一样。
#
# 第二条判据是「别的断言不该跟着红」。一处破坏让五条断言一起红，退出码仍然对，
# 但报告就失去了「指出改哪里」的能力 —— 而那才是这套断言替代第二人复核的前提。
# 少数耦合是配置本身的性质而非断言的缺陷，登记在 ALSO 里，见那里的逐条说明。
#
# 每处破坏都立即撤销，跑完库回到原状（末尾会重跑一遍第一遍来证明这一点）。
set -uo pipefail

: "${TGM_DB_URL_OWNER:?需要 TGM_DB_URL_OWNER，例如 postgres://postgres:pw@127.0.0.1:55432/tgm}"
cd "$(dirname "$0")/.."

# 破坏用例。选择标准是「只碰一处」—— 碰得越少，第 2 条判据才越有意义。
# 大部分落在 keywords 上：它是最普通的甲类表，没有任何破例，改它不会牵动别的断言。
declare -A BREAK=(
  [MAIN]="ALTER TABLE rules DISABLE ROW LEVEL SECURITY;"
  [A1]="GRANT TRUNCATE ON keywords TO app_user;"
  [A2a]="ALTER ROLE app_user BYPASSRLS;"
  [A2b]="ALTER TABLE keywords OWNER TO app_user;"
  # NOT VALID：不加的话新约束要校验现有行，而会话里没有 app.tenant_id，
  # ADD CONSTRAINT 自己就会先报错 —— 那样测的是约束建不起来，不是断言查得到
  [A3]="ALTER TABLE keywords ADD CONSTRAINT ck_probe_bomb CHECK (tenant_id = current_setting('app.tenant_id')::bigint) NOT VALID;"
  # 用 INCLUDE 形状，不用「两列且不含 tenant_id」那种直白形状。
  # 理由见文件头「破坏形状」那段：tenant_id 在 INCLUDE 位不参与唯一性，
  # 但它在 indkey 里 —— 旧断言拿整个 indkey 做成员测试，这条就被放过了（实测）。
  # 直白形状仍被覆盖：它是本形状的子集，A4 的切片对两者都报。
  [A4]="CREATE UNIQUE INDEX uq_probe_inc ON keywords (word) INCLUDE (tenant_id);"
  [A5]="ALTER TABLE keywords ALTER COLUMN tenant_id DROP NOT NULL;"
  # tenant_members 现有 2 条策略，加第三条才进得了 A6 的「多策略」前提。
  # 用 TO app_user 而不是 PUBLIC：polroles 是 {16386} 不是 {0}，
  # 旧断言只查 {0} 所以漏报，而危害等同（实测 attacker 读到的行数 0 → 1）
  [A6]="CREATE POLICY probe_role_true ON tenant_members FOR SELECT TO app_user USING (true);"
  # 列级而非表级 GRANT：has_table_privilege 看不见它（实测 false），
  # 而 app_user 的 SELECT id FROM web_sessions 真的执行成功
  [A7]="GRANT SELECT (id) ON web_sessions TO app_user;"
  # 建在 public 之外的 schema 里。原断言三处（视图/函数/物化视图）都写死
  # nspname='public'，等于「换个 schema 就绕过」，而 SECURITY DEFINER
  # 与所在 schema 无关。public 里那种直白形状仍被覆盖，判据对两者都报
  [A8]="CREATE SCHEMA probe_ns; CREATE FUNCTION probe_ns.probe_secdef() RETURNS int LANGUAGE sql SECURITY DEFINER AS \$\$ SELECT 1 \$\$;"
  # 用列序错位的复合 FK，不用单列 FK。
  # 错位形状：子表 (tenant_id, keyword_id) 对到父表 (id, tenant_id) ——
  # conkey={2,3}、confkey={1,2}，子表 tenant_id 在位置 1，而该位置父表侧是 id。
  # 旧断言只问「conkey 里含不含 tenant_id」，含就放行，于是这条形式合规、
  # 实质仍可建跨租户边。单列形状仍被覆盖：它连 tenant_id 都不在 conkey 里。
  # 被引用列 (id, tenant_id) 复用现有的 uq_keywords_tenant_id (tenant_id, id) ——
  # PG 匹配被引用列按集合不按顺序，所以不需要另建索引
  [A9]="ALTER TABLE rules ADD CONSTRAINT fk_probe_mis FOREIGN KEY (tenant_id, keyword_id) REFERENCES keywords (id, tenant_id);"
  # 删策略而不是 NO FORCE。旧断言只查两个布尔标志，删掉策略后标志还在 → 漏报，
  # 而 FORCE + 零策略是默认拒绝：业务一行都读不到。
  # 这两张表没有 tenant_id 列，MAIN 按「有 tenant_id」筛表，所以谁都没在看它们的策略数。
  # NO FORCE 那种直白形状仍被覆盖，新判据里是同一个 CASE 的另一支
  [C1]="DROP POLICY tenant_isolation ON delivery_logs;"
  [C4]="CREATE MATERIALIZED VIEW probe_mv AS SELECT tenant_id, word FROM keywords;"
  [C2]="GRANT UPDATE ON identities TO app_user;"
  # 权限只授给组，登录角色本人在 relacl/attacl/polroles 三处都查不到 ——
  # 这正是它能绕过 runtime_role 派生的原因。组带 BYPASSRLS，
  # 于是 probe_worker 实际上 RLS 全免，而 A2a 因为派生里没有它而保持绿。
  # 用 IN ROLE 一句建完，不额外发 GRANT：这两种写法在 pg_auth_members 里等价
  [C5]="CREATE ROLE probe_g2 NOLOGIN BYPASSRLS; GRANT SELECT ON keywords TO probe_g2; CREATE ROLE probe_worker LOGIN PASSWORD 'wpw' IN ROLE probe_g2;"
)

declare -A UNDO=(
  [MAIN]="ALTER TABLE rules ENABLE ROW LEVEL SECURITY;"
  [A1]="REVOKE TRUNCATE ON keywords FROM app_user;"
  [A2a]="ALTER ROLE app_user NOBYPASSRLS;"
  # 改回 owner **不还原受赠权限**。实测：ALTER TABLE keywords OWNER TO app_user
  # 之后 relacl 里 app_user 那条受赠条目消失了（被并进 owner 条目），
  # 再 OWNER TO tgm_owner 得到的是 tgm_owner 一条 —— app_user 的四项权限没了。
  # 所以撤销必须显式补 GRANT，权限值照 migrations/0011 的甲类那一行。
  # 这个损耗第一版脚本漏了，而审计照样全绿：没有任何断言查「该有的权限还在不在」。
  [A2b]="ALTER TABLE keywords OWNER TO tgm_owner; GRANT SELECT, INSERT, UPDATE, DELETE ON keywords TO app_user;"
  [A3]="ALTER TABLE keywords DROP CONSTRAINT ck_probe_bomb;"
  [A4]="DROP INDEX uq_probe_inc;"
  [A5]="ALTER TABLE keywords ALTER COLUMN tenant_id SET NOT NULL;"
  [A6]="DROP POLICY probe_role_true ON tenant_members;"
  # 列级授权用列级 REVOKE 收回。这里方向对得上（加的是列级、收的是列级），
  # 与 C2 那条「列级 REVOKE 减不掉表级 GRANT」的陷阱不是同一件事
  [A7]="REVOKE SELECT (id) ON web_sessions FROM app_user;"
  [A8]="DROP SCHEMA probe_ns CASCADE;"
  [A9]="ALTER TABLE rules DROP CONSTRAINT fk_probe_mis;"
  # 重建策略要与 migrations/0010:135 逐字一致，否则快照的 POL 段 diff 会红。
  # 这是好事：策略文本的任何漂移都会被收尾比对抓到
  [C1]="CREATE POLICY tenant_isolation ON delivery_logs TO app_user USING (EXISTS (SELECT 1 FROM delivery_tasks t WHERE t.id = delivery_logs.task_id AND t.tenant_id = current_setting('app.tenant_id')::bigint));"
  [C4]="DROP MATERIALIZED VIEW probe_mv;"
  # 撤销要恢复列级形态，顺序同 migrations/0011：先收表级再逐列给。
  # 反了不报错、权限照旧 —— 这个陷阱正是 C2 存在的理由
  [C2]="REVOKE UPDATE ON identities FROM app_user; GRANT UPDATE (kind, tg_user_id, display_name, status, proxy_url, updated_at) ON identities TO app_user;"
  # DROP ROLE 要求该角色不再持有任何权限，所以先 REVOKE 表级授权 ——
  # 漏了就是 ERROR: role "probe_g2" cannot be dropped because some objects depend on it。
  # 成员关系随 DROP ROLE 自动消失，不用单独 REVOKE；MBR 段就是用来证实这一点的
  [C5]="DROP ROLE probe_worker; REVOKE SELECT ON keywords FROM probe_g2; DROP ROLE probe_g2;"
)

# 需要超级用户而不是 tgm_owner 的三例：
#   A2a  ALTER ROLE ... BYPASSRLS 本身要超级用户
#   A2b  改 owner 要求当前角色是新 owner 的成员，tgm_owner 不是 app_user 的成员
#   A8   建函数要 public schema 的 CREATE，不绕 SET ROLE 更省事
#   C5   建带 BYPASSRLS 的角色要超级用户（CREATEROLE 也不够）
declare -A AS_SUPER=([A2a]=1 [A2b]=1 [A8]=1 [C5]=1)

# 预期的连带报红。这些不是断言的缺陷，是被破坏的那处配置真的同时违反了两条：
#   A2b → A1  owner 隐含全部权限，含 TRUNCATE。所以「运行时角色成了 owner」
#             必然同时触发 A1。两条断言从不同角度抓同一个事实，是冗余而非耦合
declare -A ALSO=([A2b]="A1")

ORDER=(MAIN A1 A2a A2b A3 A4 A5 A6 A7 A8 A9 C1 C4 C2 C5)

psql_do() {  # $1=sql  $2=1 表示以超级用户身份
  if [ "${2:-}" = "1" ]; then
    psql "$TGM_DB_URL_OWNER" -At -q -v ON_ERROR_STOP=1 -c "$1" >/dev/null
  else
    psql "$TGM_DB_URL_OWNER" -At -q -v ON_ERROR_STOP=1 \
      -c "SET ROLE tgm_owner; $1 RESET ROLE;" >/dev/null
  fi
}

audit() { cargo run -q -p tgm -- audit-rls 2>&1; }

# 目录快照，用于证明破坏真的撤销干净了。
# 不能用「审计重回全绿」代替：断言只查「不该有的东西」，不查「该有的还在不在」。
# 一个 app_user 什么权限都没有的库在这套断言下完全合规 —— 而 A2b 的撤销恰好
# 会造成那种损耗（见 UNDO[A2b] 的注释）。所以收尾必须比对状态而非比对退出码。
#
# 每段都显式限定表别名（co.oid 而不是 oid）并对 "char" 列做 ::text：
# 第一版把两者都漏了，`text || "char"` 报「operator is not unique」，
# 而报错落进了快照文件本身 —— 前后两份带着同样的报错，diff 干净，
# 于是「快照逐字节相同」这句话里根本不含策略和约束。
# 下面的「缺段检查」就是为这一类失败设的。
snapshot() {
  local out rc
  out="$(psql "$TGM_DB_URL_OWNER" -At -q -v ON_ERROR_STOP=1 <<'SQL'
SELECT 'TBL|'||c.relname||'|'||c.relrowsecurity||'|'||c.relforcerowsecurity||'|'||pg_get_userbyid(c.relowner)
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname='public' AND c.relkind IN ('r','p') ORDER BY 1;
SELECT 'ACL|'||c.relname||'|'||a.grantee::regrole::text||'|'||a.privilege_type
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
CROSS JOIN LATERAL aclexplode(c.relacl) a
WHERE n.nspname='public' AND c.relkind IN ('r','p') ORDER BY 1;
-- 列级授权在 pg_attribute.attacl，**不在 relacl 里**，要单独查一段。
-- 漏了它的后果是具体的：C2 的撤销改的正是 identities 的列级 GRANT
-- （六列 UPDATE 全在 attacl），而 C2 只断言 tenant_id 那一列，
-- 所以别的列漂移了既不会被断言抓到、也不会被快照抓到。
-- 实测：GRANT UPDATE (created_at) ON identities 之后，只查 relacl 的快照 diff 干净
SELECT 'COLACL|'||c.relname||'|'||att.attname||'|'||a.grantee::regrole::text||'|'||a.privilege_type
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
JOIN pg_attribute att ON att.attrelid=c.oid AND att.attnum>0 AND NOT att.attisdropped
CROSS JOIN LATERAL aclexplode(att.attacl) a
WHERE n.nspname='public' ORDER BY 1;
SELECT 'IDX|'||i.relname||'|'||pg_get_indexdef(x.indexrelid)
FROM pg_index x JOIN pg_class i ON i.oid=x.indexrelid
JOIN pg_class c ON c.oid=x.indrelid JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname='public' ORDER BY 1;
-- 策略表达式里的换行要压掉：乙类两条 EXISTS 子查询的 pg_get_expr 是多行的，
-- 留着会让「一行一条记录」不再成立，行数下限那道检查就跟着失真
SELECT 'POL|'||c.relname||'|'||p.polname||'|'||p.polcmd::text||'|'||p.polpermissive
       ||'|'||regexp_replace(coalesce(pg_get_expr(p.polqual,p.polrelid),'-'),'\s+',' ','g')
       ||'|'||regexp_replace(coalesce(pg_get_expr(p.polwithcheck,p.polrelid),'-'),'\s+',' ','g')
FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid ORDER BY 1;
SELECT 'CON|'||co.conrelid::regclass||'|'||co.conname||'|'||pg_get_constraintdef(co.oid)
FROM pg_constraint co JOIN pg_class c ON c.oid=co.conrelid
JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' ORDER BY 1;
-- 筛法从「rolcanlogin OR tgm%」放宽成「非 pg_ 开头」。旧写法漏掉的正是
-- C5 破坏用例里那个组角色：NOLOGIN 且不叫 tgm* → 残留在库里快照看不见。
-- 实测放宽前后在干净库上是同样 5 个名字（app_user/auth_lookup/platform_ops/
-- postgres/tgm_owner），所以这是纯覆盖增益，基线 diff 不动
SELECT 'ROL|'||rolname||'|'||rolsuper||'|'||rolbypassrls FROM pg_roles
WHERE rolname NOT LIKE 'pg\_%' ORDER BY 1;
-- 成员关系要单独一段，它不在 pg_roles 里。漏了它，C5 的破坏用例
-- （GRANT 组身份给登录角色）撤销时只 DROP 了角色而漏掉 REVOKE 的话，
-- 快照 diff 照样干净 —— 与 COLACL/MV 缺段是同一类失效。
-- 第一行是计数哨兵：干净库上这一段只有 postgres→tgm_owner 一条（实测 count=1），
-- 将来那条 GRANT 若被改掉，本段会变成 0 行而触发下面的缺段检查（误报）
SELECT 'MBR|count|'||count(*) FROM pg_auth_members am
JOIN pg_roles m ON m.oid=am.member JOIN pg_roles g ON g.oid=am.roleid
WHERE m.rolname NOT LIKE 'pg\_%' AND g.rolname NOT LIKE 'pg\_%';
SELECT 'MBR|'||m.rolname||'|'||g.rolname||'|'||am.inherit_option||'|'||am.set_option
       ||'|'||am.admin_option
FROM pg_auth_members am
JOIN pg_roles m ON m.oid=am.member JOIN pg_roles g ON g.oid=am.roleid
WHERE m.rolname NOT LIKE 'pg\_%' AND g.rolname NOT LIKE 'pg\_%' ORDER BY 1;
-- FUN 与 MV 两段都不限定 public，与断言侧的范围保持一致。
-- 限定了的话，A8 的破坏用例（建在 probe_ns 里的 SECURITY DEFINER 函数）
-- 撤销漏掉时快照 diff 照样干净 —— 又一次「快照看不见的地方就验不了撤销」。
-- 前缀里带 nspname，否则同名不同 schema 的对象在快照里无法区分
SELECT 'FUN|'||n.nspname||'.'||p.proname||'|'||p.prosecdef FROM pg_proc p
JOIN pg_namespace n ON n.oid=p.pronamespace
WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname<>'information_schema' ORDER BY 1;
-- schema 自身也要有一段。probe_ns 建了没删时，若只看 FUN 段，
-- 一个空 schema 残留在库里是看不见的
SELECT 'NSP|count|'||count(*) FROM pg_namespace
WHERE nspname NOT LIKE 'pg\_%' AND nspname<>'information_schema';
SELECT 'NSP|'||nspname||'|'||pg_get_userbyid(nspowner) FROM pg_namespace
WHERE nspname NOT LIKE 'pg\_%' AND nspname<>'information_schema' ORDER BY 1;
SELECT 'ETR|'||evtname||'|'||evtevent||'|'||evtenabled::text FROM pg_event_trigger ORDER BY 1;
-- 物化视图要单独一段。它不在上面任何一段的视野里：TBL 的 relkind 只收 ('r','p')，
-- IDX/POL/CON 三段都从基表出发。所以 C4 的破坏用例（CREATE MATERIALIZED VIEW）
-- 撤销漏掉时，快照 diff 照样干净 —— 「撤销干净」这个结论会是空的。
-- 与早先 COLACL 缺段坑住 C2 撤销是同一类失效，只是对象类型换了。
--
-- 第一行是计数哨兵，不能省。干净库上物化视图是 0 个（实测 count=0），
-- 而「某段返回 0 行」正是下面那道缺段检查判定快照不可信的条件 ——
-- 只列明细的话，这一段在正常库上就会让整个快照自我否决
SELECT 'MV|count|'||count(*) FROM pg_class m JOIN pg_namespace n ON n.oid=m.relnamespace
WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname<>'information_schema' AND m.relkind='m';
SELECT 'MV|'||n.nspname||'.'||m.relname||'|'||m.relispopulated||'|'||pg_get_userbyid(m.relowner)
       ||'|'||regexp_replace(pg_get_viewdef(m.oid,true),'\s+',' ','g')
FROM pg_class m JOIN pg_namespace n ON n.oid=m.relnamespace
WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname<>'information_schema'
  AND m.relkind='m' ORDER BY 1;
SQL
)"
  rc=$?
  # psql 对单条语句报错不会置非零退出码（ON_ERROR_STOP 只管脚本级），
  # 所以既查退出码也查输出里有没有 ERROR，再查行数下限 —— 三道都为「段落静默消失」设
  if [ $rc -ne 0 ] || grep -q 'ERROR' <<<"$out"; then
    echo "快照 SQL 报错，快照不可信：" >&2
    grep 'ERROR' <<<"$out" >&2
    return 1
  fi
  # 逐段查在不在，而不是查总行数。总数下限拦不住「A 段整段消失而 B 段恰好变多」，
  # 而「整段消失」正是要防的那个失效 —— 第一版 POL/CON/ETR 三段同时消失时，
  # 剩下的 ACL 段有 323 行，任何一个合理的总数下限都会放它过去
  local missing=""
  for sec in TBL ACL COLACL IDX POL CON ROL MBR FUN ETR MV NSP; do
    grep -q "^$sec|" <<<"$out" || missing="$missing $sec"
  done
  if [ -n "$missing" ]; then
    echo "快照缺段：$missing —— 那几段的 SQL 没返回任何行，快照不可信" >&2
    return 1
  fi
  printf '%s\n' "$out"
}

fail=0

echo "══ 第一遍：未破坏的库 ══"
first="$(audit)"; first_rc=$?
echo "$first" | grep -E '^(FAIL|[0-9]+ 条断言|       )'
if [ $first_rc -ne 0 ]; then
  echo "✗ 第一遍就是红的 —— 先修库，双遍对比无从谈起"
  exit 1
fi
echo "✓ 第一遍 exit=0"
# 必须查 snapshot 的返回码。它失败时返回空输出 ——
# 而两个空文件 diff 是干净的，失败会被后面的比对静默吞掉。
# 第一版漏了这一句，实测在 psql 起不来时（libpq.so.5 找不到）
# 每一步都报「快照不可信」到 stderr，而脚本照样一路打 ✓
if ! snapshot > /tmp/tgm-audit-snap-before.txt; then
  echo "✗ 基线快照拿不到 —— 见上面 stderr。没有基线就无从比对，停"
  exit 1
fi

# A0 没有破坏用例，单独说明为什么。
# 要让它红，得让「运行时角色」这个派生算出空集 —— 那要撤掉全部 GRANT 并删掉全部
# 策略，破坏面远大于一处，回滚也不可靠。它的价值在第一遍而不在第二遍：
# 派生若为空，A1/A2a/A2b/C2 会静默全部通过，A0 是那一情形下唯一的信号。
#
# 它仍然验过，只是换了个库验：在一个空库（无 GRANT、无策略）上跑 A0 的 SQL，
# 返回「运行时角色派生为空集」；同一段 SQL 在本库返回 0 行。
# 复现：CREATE DATABASE 一个空库，把 queries.sql 里 A0 那段贴进去跑。
echo
echo "A0 无破坏用例（元断言）—— 已在空库上单独验过会报红，见脚本内注释"

echo
echo "══ 第二遍：逐条造针对性破坏 ══"
for id in "${ORDER[@]}"; do
  super="${AS_SUPER[$id]:-}"

  if ! psql_do "${BREAK[$id]}" "$super"; then
    echo "✗ $id 破坏 SQL 执行失败 —— 用例写坏了，这一条没测到"
    fail=1
    continue
  fi

  out="$(audit)"; rc=$?
  reds="$(echo "$out" | grep '^FAIL ' | awk '{print $2}' | sort | tr '\n' ' ' | sed 's/ *$//')"

  if ! psql_do "${UNDO[$id]}" "$super"; then
    echo "✗ $id 撤销失败 —— 库已被污染，后面的结果都不可信，停"
    exit 1
  fi

  # 每条撤销后立刻比对快照，而不是只在末尾比一次 —— 末尾比只能告诉你「有残留」，
  # 逐条比能直接指出是哪个用例的撤销不完整（A2b 的权限损耗就是这样找出来的）。
  # 先取值再比，不写成 `snapshot | diff`：管道会丢掉 snapshot 自己的返回码，
  # 于是「快照拿不到」会被报成「撤销有残留」，把人指向错误的方向
  if ! now="$(snapshot)"; then
    echo "✗ $id 撤销后快照拿不到 —— 见上面 stderr。不能判断库是否干净，停"
    exit 1
  fi
  if ! diff -q /tmp/tgm-audit-snap-before.txt <(printf '%s\n' "$now") >/dev/null; then
    echo "✗ $id 撤销有残留 —— 目录快照与第一遍不一致："
    diff /tmp/tgm-audit-snap-before.txt <(printf '%s\n' "$now") | sed 's/^/       /'
    echo "       后面的结果都建立在被污染的库上，停"
    exit 1
  fi

  expect="$(printf '%s\n' "$id" ${ALSO[$id]:-} | sort | tr '\n' ' ' | sed 's/ *$//')"

  if [ $rc -eq 0 ]; then
    echo "✗ $id 破坏后仍然 exit=0 —— 这条断言是坏的，它查不到任何东西"
    fail=1
  elif [ "$reds" = "$expect" ]; then
    if [ -n "${ALSO[$id]:-}" ]; then
      echo "✓ $id 报红「$reds」—— 连带的 ${ALSO[$id]} 是已登记的冗余"
    else
      echo "✓ $id 破坏后恰好这一条红"
    fi
  else
    echo "△ $id 期望红「$expect」，实际红「$reds」—— 退出码对，但报告指不准位置"
    fail=1
  fi
done

echo
echo "══ 收尾：确认库已回到原状 ══"
if ! final="$(snapshot)"; then
  echo "✗ 收尾快照拿不到 —— 见上面 stderr"
  fail=1
elif diff -q /tmp/tgm-audit-snap-before.txt <(printf '%s\n' "$final") >/dev/null; then
  echo "✓ 目录快照与第一遍逐字节相同（$(wc -l < /tmp/tgm-audit-snap-before.txt) 行）"
  rm -f /tmp/tgm-audit-snap-before.txt
else
  echo "✗ 快照有差异，库没回到原状："
  diff /tmp/tgm-audit-snap-before.txt <(printf '%s\n' "$final") | sed 's/^/       /'
  fail=1
fi

echo
if [ $fail -eq 0 ]; then
  echo "双遍通过：每条断言都能被它对应的那处破坏单独打红。"
  echo "这不证明隔离正确（eng/04 §五），只证明断言本身不是空转的。"
else
  echo "双遍失败：带 ✗ / △ 的条目要改断言，不是改库。"
fi
exit $fail
