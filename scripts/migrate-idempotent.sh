#!/usr/bin/env bash
# 迁移可重入（plan/00 §4.3 的 WP-1 出口判据第一条）。
#
# 判据不是「第二遍不报错」—— sqlx 按文件名记版本，跑第二遍时它压根不会
# 重新执行任何文件，于是「第二遍绿」这件事**不携带信息**。真正要证的是：
# 每个 DDL 文件内部是幂等的，因为 ADR-0021 那条理由成立 ——
# 一个文件里有多条语句时，部分成功的状态是真实存在的（第 5 条建表成功、
# 第 6 条失败），那种状态下重跑该文件必须能走完。
#
# 所以这个脚本做三件事：
#   ① 记下第一遍后的库状态快照与版本表
#   ② 把版本记录删掉，强制 sqlx 重新执行全部文件（模拟「重跑同一个文件」）
#   ③ 比对快照必须逐字节相同，且没有一条语句报错
#
# 与 audit-double-pass.sh 同一个模式：一个只会绿的检查等于没有检查。
set -euo pipefail

# psql 不在非交互 shell 的 PATH 里（.bashrc 早退在 source env.sh 之前），
# 所以这里显式 source —— eng/00 §零 那句「✓ 已装」对脚本和 CI 都是假的。
if ! command -v psql >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  [ -f "$HOME/opt/pgdg17/env.sh" ] && source "$HOME/opt/pgdg17/env.sh"
fi
command -v psql >/dev/null 2>&1 || { echo "✗ 找不到 psql"; exit 1; }

: "${DATABASE_URL:?需要 DATABASE_URL，例如 postgres://postgres:pw@127.0.0.1:55432/tgm}"

MIG="${MIG_SOURCE:-migrations}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# 快照取的是「迁移应当产出的东西」：表、列、约束、索引、策略、RLS 两个开关、
# 表的 owner。owner 必须在里面 —— 它决定 FORCE 那一层是真的还是装饰，
# 而一个漏了 SET ROLE 的重跑恰好会把 owner 改成 postgres（见 0002 开头的注释）。
snapshot() {
  psql "$DATABASE_URL" -Atq <<'SQL'
SELECT 'table|'||c.relname||'|'||pg_get_userbyid(c.relowner)
       ||'|'||c.relrowsecurity||'|'||c.relforcerowsecurity
  FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
 WHERE n.nspname='public' AND c.relkind IN ('r','p') ORDER BY 1;
SELECT 'column|'||table_name||'|'||column_name||'|'||data_type||'|'||is_nullable
  FROM information_schema.columns WHERE table_schema='public' ORDER BY 1;
SELECT 'constraint|'||conrelid::regclass||'|'||conname||'|'||pg_get_constraintdef(oid)
  FROM pg_constraint WHERE connamespace='public'::regnamespace ORDER BY 1;
SELECT 'index|'||indexrelid::regclass||'|'||indrelid::regclass
  FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid
  JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' ORDER BY 1;
SELECT 'policy|'||schemaname||'.'||tablename||'|'||policyname||'|'||cmd
       ||'|'||coalesce(qual,'-')||'|'||coalesce(with_check,'-')
  FROM pg_policies WHERE schemaname='public' ORDER BY 1;
SELECT 'grant|'||grantee||'|'||table_name||'|'||privilege_type
  FROM information_schema.role_table_grants
 WHERE table_schema='public' ORDER BY 1;
SQL
}

echo "══ 第一遍：跑到最新 ══"
sqlx migrate run --source "$MIG" >/dev/null
snapshot > "$tmp/pass1"
before_versions="$(psql "$DATABASE_URL" -Atqc \
  'SELECT count(*) FROM _sqlx_migrations WHERE success')"
echo "✓ $before_versions 个版本已应用，快照 $(wc -l < "$tmp/pass1") 行"

# 反证这个脚本自己不是空转的：快照必须真的能反映差异。
# 造一处改动 → 快照必须变。不做这一步的话，一个返回空的 snapshot()
# 会让下面的「逐字节相同」永远成立。
echo "══ 自检：快照必须能看见差异 ══"
psql "$DATABASE_URL" -q -c 'CREATE TABLE _idem_probe(id int)' >/dev/null
if diff -q "$tmp/pass1" <(snapshot) >/dev/null; then
  echo "✗ 自检失败：加了一张表而快照没变 —— snapshot() 是空转的"
  psql "$DATABASE_URL" -q -c 'DROP TABLE _idem_probe' >/dev/null
  exit 1
fi
psql "$DATABASE_URL" -q -c 'DROP TABLE _idem_probe' >/dev/null
diff -q "$tmp/pass1" <(snapshot) >/dev/null \
  || { echo "✗ 自检的探针表没清干净"; exit 1; }
echo "✓ 快照对差异敏感，且探针已清"

echo "══ 第二遍：清空版本记录，强制重跑全部文件 ══"
# 这才是判据。只跑 `sqlx migrate run` 第二次的话它会一条都不执行。
psql "$DATABASE_URL" -q -c 'DELETE FROM _sqlx_migrations' >/dev/null
if ! sqlx migrate run --source "$MIG" > "$tmp/pass2.log" 2>&1; then
  echo "✗ 重跑失败 —— 有 DDL 文件不幂等："
  grep -iE 'error|already exists' "$tmp/pass2.log" | head -20
  exit 1
fi
after_versions="$(psql "$DATABASE_URL" -Atqc \
  'SELECT count(*) FROM _sqlx_migrations WHERE success')"
echo "✓ 全部 $after_versions 个文件重跑无报错"

echo "══ 比对 ══"
snapshot > "$tmp/pass3"
if ! diff -u "$tmp/pass1" "$tmp/pass3"; then
  echo "✗ 重跑改变了库状态 —— 上面是差异（- 第一遍 / + 重跑后）"
  exit 1
fi
[ "$before_versions" = "$after_versions" ] \
  || { echo "✗ 版本数变了：$before_versions → $after_versions"; exit 1; }

echo
echo "迁移可重入：$after_versions 个文件全部重跑，库状态逐字节相同。"
echo "这不证明迁移内容正确（那是 audit-rls 与反向测试的事），"
echo "只证明部分成功后重跑不会把库带进另一种状态。"
