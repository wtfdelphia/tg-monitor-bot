#!/usr/bin/env bash
# eng/04 §四 卡口清单的本地等价执行器，供 AI 驱动循环验收用。不写条数 ——
# 清单数量会变（见 eng/04 的同款纪律），引用一律用「清单」。
#
# 逐条对照 .github/workflows/ci.yml 与 eng/04 §四 清单。最终裁决在 CI；
# 本脚本只是入场券 —— 本地全绿不等于可合并，但本地不绿一定不可合并。
#
# 用法：
#   scripts/ai-driver/ai-gates.sh            # 跑全部可本地跑的卡口
#   scripts/ai-driver/ai-gates.sh --no-db    # 只跑不连库的五条（快速反馈）
#   scripts/ai-driver/ai-gates.sh --from <name>  # 从某条卡口续跑（调试用）
#
# 退出码：任一卡口红即非 0。
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

# psql/sqlx 在非交互 shell 的 PATH 之外，同现有脚本的处置
if ! command -v psql >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  [ -f "$HOME/opt/pgdg17/env.sh" ] && source "$HOME/opt/pgdg17/env.sh"
fi

NO_DB=0
FROM=""
while [ $# -gt 0 ]; do
  case "$1" in
    --no-db) NO_DB=1; shift ;;
    --from) FROM="$2"; shift 2 ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done

export DATABASE_URL="${DATABASE_URL:-postgres://postgres:pw@127.0.0.1:55432/tgm}"
export TGM_DB_URL_OWNER="${TGM_DB_URL_OWNER:-postgres://postgres:pw@127.0.0.1:55432/tgm}"

# 版本漂移报警：ci.yml 钉了工具版本（本地装什么跑什么）。期望值从
# ci.yml 解析，不硬编码 —— 硬编码就是再造一份会漂的清单。
# ci.yml 钉了两个：cargo-deny 与 sqlx-cli（README 已知边界 7 点名两者）。
check_version_drift() {
  local tool="$1" local_ver="$2" ci_ver
  ci_ver="$(sed -n "s/.*cargo install $tool --locked --version \([0-9.]*\).*/\1/p" .github/workflows/ci.yml | head -1)"
  if [ -n "$ci_ver" ] && [ "$local_ver" != "$ci_ver" ]; then
    echo "⚠ $tool 版本漂移：本地 ${local_ver:-未安装}，ci.yml 钉 $ci_ver（见 README 已知边界 7）"
  fi
}
check_version_drift cargo-deny "$(cargo deny --version 2>/dev/null | sed 's/[^0-9.]//g')"
check_version_drift sqlx-cli "$(cargo sqlx --version 2>/dev/null | sed 's/[^0-9.]//g')"

declare -a ORDER=(fmt clippy offline-build deny newtype migrate sqlx-check migrate-idempotent test audit-rls double-pass)
declare -A NEED_DB=( [migrate]=1 [sqlx-check]=1 [migrate-idempotent]=1 [test]=1 [audit-rls]=1 [double-pass]=1 )

started=1
[ -n "$FROM" ] && started=0

# --from 拿 ORDER 校验：名字打错时 started 永不翻转，全部卡口被跳过、
# 末尾照样报「✓ 全部卡口通过」—— 卡口脚本自己制造假绿（实测 --from clipy）。
if [ -n "$FROM" ]; then
  from_ok=0
  for g in "${ORDER[@]}"; do
    [ "$g" = "$FROM" ] && { from_ok=1; break; }
  done
  if [ "$from_ok" = 0 ]; then
    echo "✗ --from 是未知卡口名：$FROM —— 合法值：${ORDER[*]}" >&2
    exit 2
  fi
fi

run() {
  local name="$1"; shift
  [ "$started" = 0 ] && { [ "$name" = "$FROM" ] && started=1 || { echo "⏭  跳过 $name（--from）"; return 0; }; }
  if [ "${NEED_DB[$name]:-0}" = 1 ] && [ "$NO_DB" = 1 ]; then
    echo "⏭  跳过 $name（--no-db）"; return 0
  fi
  echo "── 卡口 $name ────────────────────────────"
  if "$@"; then
    echo "✓  $name 通过"
  else
    echo "✗  $name 失败 —— 驱动循环在此停止，修复后重跑"
    exit 1
  fi
}

ensure_db() {
  # worktree 里起 compose 会另开一个项目撞端口：compose.yaml 没有 name:
  # 字段，项目名取目录名。驱动从 worktree 调用时导出 TGM_COMPOSE_DIR=主仓
  # （此前只有导出没有读取方，注释描述的防护没实现）；独立运行时未设，
  # 用当前目录（文件开头已 cd 到仓库根）。失败不吞：库起不来时后续连库
  # 卡口会以「代码问题」的形状报红 —— 正是 ci.yml 分两个 job 要避免的混淆。
  local dir="${TGM_COMPOSE_DIR:-.}"
  (cd "$dir" && docker compose up -d --wait) || {
    echo "✗ compose 起不来（目录：$dir；见 eng/02 §3.2：down -v 从零重建）" >&2
    exit 1
  }
}

# ── 不连库的五条（ci.yml static job）─────────────────────────────
run fmt            cargo fmt --all --check
run clippy         cargo clippy --all-targets --all-features -- -D warnings
run offline-build  env SQLX_OFFLINE=true cargo build --workspace --all-targets
run deny           cargo deny check
run newtype        scripts/check-newtype.sh

# ── 连库的六条（ci.yml database job）─────────────────────────────
if [ "$NO_DB" = 0 ]; then ensure_db; fi
run migrate            cargo run --quiet --bin tgm -- migrate
# sqlx prepare 对多余条目只 warning 且 rc=0 —— 照 ci.yml 用 grep 转失败。
# 用 bash -ec 而不是 bash -c：-e 让 prepare 自己的失败（如 .sqlx 缺条目）
# 不被末尾的 exit 0 吞掉 —— ci.yml 的同款结构是红的（GH Actions 默认 shell
# 带 -e），本地没 -e 就绿了（实测两种形状，一绿一红）。
run sqlx-check         bash -ec 'set -o pipefail; cargo sqlx prepare --workspace --check 2>&1 | tee /tmp/ai-gates-prepare.log; if grep -q "unused queries found" /tmp/ai-gates-prepare.log; then exit 1; fi'
run migrate-idempotent scripts/migrate-idempotent.sh
run test               cargo test --workspace
run audit-rls          cargo run --quiet --bin tgm -- audit-rls
run double-pass        scripts/audit-double-pass.sh

# ── 刻意不在本地跑的一条 ─────────────────────────────────────────
# tgm openapi --check：契约随 WP-5 落地，当前 exit 1（not_implemented）。
# 与 ci.yml 一致：现在跑它必红，红不携带信息。WP-5 落地后把下行删掉并把该步加回。
echo "── 跳过 openapi-check（同 ci.yml：WP-5 前该命令 exit 1）──"

echo "✓ 全部卡口通过"
