#!/usr/bin/env bash
# eng/04 §四 卡口清单的本地等价执行器。
set -euo pipefail
umask 077
export GIT_NO_REPLACE_OBJECTS=1
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR \
  GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
  GIT_ALTERNATE_OBJECT_DIRECTORIES_RELATIVE GIT_REPLACE_REF_BASE

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "✗ 无法解析卡口执行根目录" >&2
  exit 1
}
ROOT="$(realpath -e -- "$ROOT")" || {
  echo "✗ 卡口执行根目录不可解析" >&2
  exit 1
}

if [ -n "${TGM_GATES_ROOT:-}" ]; then
  requested_root="$(realpath -e -- "$TGM_GATES_ROOT")" || {
    echo "✗ TGM_GATES_ROOT 不可解析" >&2
    exit 1
  }
  [ "$requested_root" = "$ROOT" ] || {
    echo "✗ TGM_GATES_ROOT 不能改变卡口执行根目录" >&2
    exit 1
  }
fi

if git for-each-ref --format='%(refname)' refs/replace | grep -q .; then
  echo "✗ 检测到 refs/replace/*，拒绝在可替换 Git 对象上运行" >&2
  exit 1
fi

cd -- "$ROOT"

NO_DB=0
MODE="all"
FROM=""
OWNER_DB_URL="${TGM_TRUSTED_OWNER_DB_URL:-${TGM_DB_URL_OWNER:-}}"
TASK_DB_URL="${TGM_DB_URL_TEST:-${TGM_TEST_DATABASE_URL:-${DATABASE_URL:-}}}"
TASK_SCHEMA_DB_URL="${TGM_DB_URL_TASK_SCHEMA:-${TGM_TASK_SCHEMA_DATABASE_URL:-postgres://app_user:apw@127.0.0.1:55432/tgm_test_schema}}"
TASK_SCHEMA_OWNER_DB_URL="${TGM_DB_URL_TASK_SCHEMA_OWNER:-${TGM_TASK_SCHEMA_OWNER_DATABASE_URL:-postgres://postgres:pw@127.0.0.1:55432/tgm_test_schema}}"
TRUSTED_ROOT="${TGM_TRUSTED_ROOT:-$ROOT}"
TRUSTED_TARGET_DIR="${TGM_TRUSTED_TARGET_DIR:-$ROOT/target/ai-gates-trusted}"
TASK_TARGET_DIR="${TGM_TASK_TARGET_DIR:-${CARGO_TARGET_DIR:-$ROOT/target/ai-gates-task}}"
COMPOSE_ROOT="${TGM_COMPOSE_DIR:-$ROOT}"

if [ -n "${TGM_TRUSTED_ROOT:-}" ] && [ -z "${TGM_TRUSTED_ROOT_LOCK:-}" ]; then
  echo "✗ 外部可信源码根目录没有驱动锁定值，拒绝运行可信卡口" >&2
  exit 1
fi

if [ -n "${TGM_TRUSTED_ROOT_LOCK:-}" ]; then
  locked_root="$(realpath -e -- "$TGM_TRUSTED_ROOT_LOCK")" || {
    echo "✗ 可信源码锁定根目录不可解析" >&2
    exit 1
  }
  trusted_root_check="$(realpath -e -- "$TRUSTED_ROOT")" || {
    echo "✗ 可信源码根目录不可解析" >&2
    exit 1
  }
  [ "$trusted_root_check" = "$locked_root" ] || {
    echo "✗ 可信源码根目录不是驱动锁定目录" >&2
    exit 1
  }
fi
TRUSTED_ROOT="$(realpath -e -- "$TRUSTED_ROOT")" || {
  echo "✗ 可信源码根目录不可解析" >&2
  exit 1
}
[ -d "$TRUSTED_ROOT" ] && [ ! -L "$TRUSTED_ROOT" ] || {
  echo "✗ 可信源码根目录不是普通目录" >&2
  exit 1
}

if [ -n "${TGM_TRUSTED_TARGET_DIR_LOCK:-}" ]; then
  [ "$TRUSTED_TARGET_DIR" = "$TGM_TRUSTED_TARGET_DIR_LOCK" ] || {
    echo "✗ 可信 target 目录不是驱动锁定目录" >&2
    exit 1
  }
fi
if [ -n "${TGM_TASK_TARGET_DIR_LOCK:-}" ]; then
  [ "$TASK_TARGET_DIR" = "$TGM_TASK_TARGET_DIR_LOCK" ] || {
    echo "✗ 任务 target 目录不是驱动锁定目录" >&2
    exit 1
  }
fi

compose_root_check="$(realpath -e -- "$COMPOSE_ROOT")" || {
  echo "✗ Compose 根目录不可解析" >&2
  exit 1
}
if [ -n "${TGM_COMPOSE_ROOT_LOCK:-}" ]; then
  [ "$compose_root_check" = "$TGM_COMPOSE_ROOT_LOCK" ] || {
    echo "✗ Compose 根目录不是驱动锁定目录" >&2
    exit 1
  }
else
  [ "$compose_root_check" = "$ROOT" ] || {
    echo "✗ 独立运行时只能使用当前仓库的 Compose 根目录" >&2
    exit 1
  }
fi
COMPOSE_ROOT="$compose_root_check"

while [ $# -gt 0 ]; do
  case "$1" in
    --no-db) NO_DB=1; shift ;;
    --static-only) MODE=static; shift ;;
    --task-only) MODE=task; shift ;;
    --db-only|--trusted-db-only) MODE=trusted; shift ;;
    --from)
      [ "$#" -ge 2 ] || { echo "--from 缺少参数" >&2; exit 2; }
      FROM="$2"; shift 2 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

case "$MODE" in
  static)
    ;;
  task)
    [ "$NO_DB" = 0 ] || { echo "✗ --task-only 不能与 --no-db 同时使用" >&2; exit 2; }
    [ -n "$TASK_DB_URL" ] || { echo "✗ 缺少任务测试连接串" >&2; exit 2; }
    ;;
  trusted)
    [ "$NO_DB" = 0 ] || { echo "✗ --trusted-db-only 不能与 --no-db 同时使用" >&2; exit 2; }
    [ -n "$OWNER_DB_URL" ] || { echo "✗ 缺少 owner 连接串" >&2; exit 2; }
    ;;
  all)
    if [ "$NO_DB" = 1 ]; then
      MODE=static
    else
      [ -n "$TASK_DB_URL" ] || { echo "✗ 缺少任务测试连接串" >&2; exit 2; }
      [ -n "$OWNER_DB_URL" ] || { echo "✗ 缺少 owner 连接串" >&2; exit 2; }
    fi
    ;;
esac

case "$MODE" in
  trusted|all)
    if [ -n "${TGM_TRUSTED_ROOT:-}" ]; then
      [ -n "${TGM_TRUSTED_ROOT_LOCK:-}" ] \
        || { echo "✗ 外部可信源码根目录必须由驱动提供锁定根目录" >&2; exit 1; }
    else
      [ "$TRUSTED_ROOT" = "$ROOT" ] \
        || { echo "✗ 独立卡口运行只能使用当前仓库作为可信源码根目录" >&2; exit 1; }
    fi
    ;;
esac

mkdir -p -- "$TRUSTED_TARGET_DIR" "$TASK_TARGET_DIR"
[ ! -L "$TRUSTED_TARGET_DIR" ] && [ -d "$TRUSTED_TARGET_DIR" ] || {
  echo "✗ 可信 target 目录不是普通目录" >&2
  exit 1
}
[ ! -L "$TASK_TARGET_DIR" ] && [ -d "$TASK_TARGET_DIR" ] || {
  echo "✗ 任务 target 目录不是普通目录" >&2
  exit 1
}

if ! command -v psql >/dev/null 2>&1 && [ -f "${HOME:-}/opt/pgdg17/env.sh" ]; then
  # shellcheck disable=SC1091
  source "${HOME}/opt/pgdg17/env.sh"
fi

check_version_drift() {
  local tool="$1" local_ver="$2" ci_ver
  ci_ver="$(sed -n "s/.*cargo install $tool --locked --version \([0-9.]*\).*/\1/p" .github/workflows/ci.yml | head -1)"
  if [ -n "$ci_ver" ] && [ "$local_ver" != "$ci_ver" ]; then
    echo "⚠ $tool 版本漂移：本地 ${local_ver:-未安装}，ci.yml 钉 $ci_ver"
  fi
}
check_version_drift cargo-deny "$(cargo deny --version 2>/dev/null | sed 's/[^0-9.]//g')"
check_version_drift sqlx-cli "$(cargo sqlx --version 2>/dev/null | sed 's/[^0-9.]//g')"

declare -a ORDER=(fmt clippy offline-build deny newtype driver-regressions migrate sqlx-check migrate-idempotent test audit-rls double-pass)
declare -A CLASS=(
  [fmt]=static [clippy]=static [offline-build]=static [deny]=static [newtype]=static
  [driver-regressions]=static
  [migrate]=trusted [sqlx-check]=task [migrate-idempotent]=trusted [test]=task
  [audit-rls]=trusted [double-pass]=trusted
)

if [ -n "$FROM" ]; then
  valid_from=0
  for gate in "${ORDER[@]}"; do
    [ "$gate" = "$FROM" ] && { valid_from=1; break; }
  done
  [ "$valid_from" = 1 ] || { echo "✗ --from 是未知卡口名：$FROM" >&2; exit 2; }
  case "$MODE" in
    static) [ "${CLASS[$FROM]}" = static ] || { echo "✗ --static-only 不能从 $FROM 开始" >&2; exit 2; } ;;
    task) [ "${CLASS[$FROM]}" = static ] || [ "${CLASS[$FROM]}" = task ] || { echo "✗ --task-only 不能从 $FROM 开始" >&2; exit 2; } ;;
    trusted) [ "${CLASS[$FROM]}" = trusted ] || { echo "✗ --trusted-db-only 不能从 $FROM 开始" >&2; exit 2; } ;;
  esac
fi

started=1
[ -n "$FROM" ] && started=0

run_static() {
  local cmd="$1"; shift
  (
    unset DATABASE_URL TGM_DB_URL_OWNER TGM_TRUSTED_OWNER_DB_URL TGM_TEST_DATABASE_URL \
      TGM_DB_URL_TEST TGM_DB_URL_TASK_SCHEMA TGM_TASK_SCHEMA_DATABASE_URL \
      TGM_TASK_SCHEMA_OWNER_DATABASE_URL TGM_DB_URL_TASK_SCHEMA_OWNER TGM_TRUSTED_ROOT \
      TGM_TRUSTED_TARGET_DIR TGM_TASK_TARGET_DIR TGM_OWNER_DB_FD \
      TGM_DRIVER_OWNER_DB_URL TGM_COMPOSE_DIR
    export CARGO_TARGET_DIR="$TASK_TARGET_DIR"
    if [ "$cmd" = trusted_helper ]; then
      trusted_helper "$@"
    else
      command "$cmd" "$@"
    fi
  )
}

run_task() {
  local gate="$1"; shift
  local prepare_log
  (
    unset DATABASE_URL TGM_DB_URL_OWNER TGM_TRUSTED_OWNER_DB_URL TGM_OWNER_DB_FD \
      TGM_DB_URL_TEST TGM_TEST_DATABASE_URL TGM_DB_URL_TASK_SCHEMA \
      TGM_TASK_SCHEMA_DATABASE_URL TGM_TASK_SCHEMA_OWNER_DATABASE_URL \
      TGM_DB_URL_TASK_SCHEMA_OWNER TGM_TRUSTED_ROOT TGM_TRUSTED_TARGET_DIR \
      TGM_DRIVER_OWNER_DB_URL
    case "$gate" in
      sqlx-check) export DATABASE_URL="$TASK_SCHEMA_DB_URL" ;;
      test)
        export DATABASE_URL="$TASK_DB_URL"
        export SQLX_OFFLINE=true
        ;;
      *) echo "✗ 未知任务卡口：$gate" >&2; exit 2 ;;
    esac
    export CARGO_TARGET_DIR="$TASK_TARGET_DIR"
    if [ "$gate" = sqlx-check ]; then
      prepare_log="$(mktemp "${TMPDIR:-/tmp}/ai-gates-prepare.XXXXXX")" || exit 1
      trap 'rm -f -- "$prepare_log"' EXIT
      set -o pipefail
      cargo sqlx prepare --workspace --check 2>&1 | tee "$prepare_log"
      ! grep -q "unused queries found" "$prepare_log"
    else
      command "$@"
    fi
  )
}

run_trusted_url() {
  local db_url="$1"
  shift
  (
    cd -- "$TRUSTED_ROOT"
    unset TGM_TEST_DATABASE_URL TGM_DB_URL_TEST TGM_TASK_SCHEMA_DATABASE_URL \
      TGM_DB_URL_TASK_SCHEMA TGM_TASK_SCHEMA_OWNER_DATABASE_URL TGM_DB_URL_TASK_SCHEMA_OWNER
    export DATABASE_URL="$db_url"
    export TGM_DB_URL_OWNER="$db_url"
    export CARGO_TARGET_DIR="$TRUSTED_TARGET_DIR"
    if [ "${1:-}" = trusted_helper ]; then
      export TGM_HELPER_FROM_TRUSTED=1
      export MIG_SOURCE="$TRUSTED_ROOT/migrations"
      trusted_helper "${@:2}"
    else
      command "$@"
    fi
  )
}

run_trusted() { run_trusted_url "$OWNER_DB_URL" "$@"; }

trusted_helper() {
  local helper="$1"; shift
  local base="${TGM_GATES_BASE_REF:-}" expected_file helper_path mode
  case "$helper" in
    scripts/check-newtype.sh|scripts/migrate-idempotent.sh|scripts/audit-double-pass.sh) ;;
    *) echo "✗ helper 路径不在固定白名单：$helper" >&2; return 1 ;;
  esac
  if [ "${TGM_HELPER_FROM_TRUSTED:-0}" = 1 ]; then
    helper_path="$TRUSTED_ROOT/$helper"
  else
    helper_path="$ROOT/$helper"
  fi
  [ -f "$helper_path" ] && [ ! -L "$helper_path" ] || {
    echo "✗ helper 不是普通文件：$helper_path" >&2
    return 1
  }
  if [ -z "$base" ]; then
    "$helper_path" "$@"
    return $?
  fi
  mode="$(git -C "$ROOT" ls-tree "$base" -- "$helper" | awk 'NR == 1 {print $1}')" || return 1
  [ "$mode" = 100755 ] || {
    echo "✗ helper 基线不是可执行普通文件：$base:$helper" >&2
    return 1
  }
  expected_file="$(mktemp)"
  if ! git -C "$ROOT" show "$base:$helper" >"$expected_file"; then
    rm -f -- "$expected_file"
    echo "✗ helper 基线读取失败：$base:$helper" >&2
    return 1
  fi
  if ! cmp -s "$expected_file" "$helper_path"; then
    rm -f -- "$expected_file"
    echo "✗ helper 与固定基线原始字节不一致：$helper" >&2
    return 1
  fi
  rm -f -- "$expected_file"
  "$helper_path" "$@"
}

run_gate() {
  local class="$1" name="$2"; shift 2
  case "$MODE:$class" in
    static:static) ;;
    task:static|task:task) ;;
    trusted:trusted) ;;
    all:*) ;;
    *) echo "⏭  跳过 $name（模式 $MODE）"; return 0 ;;
  esac
  if [ "$started" = 0 ]; then
    if [ "$name" = "$FROM" ]; then started=1
    else echo "⏭  跳过 $name（--from）"; return 0
    fi
  fi
  echo "── 卡口 $name ────────────────────────────"
  case "$class" in
    static) run_static "$@" ;;
    task) run_task "$name" "$@" ;;
    trusted) run_trusted "$@" ;;
  esac
  echo "✓  $name 通过"
}

ensure_db() {
  local need_owner=0 need_task=0 need_schema=0 need_schema_owner=0 db_ok=1
  command -v psql >/dev/null 2>&1 || { echo "✗ 找不到 psql" >&2; exit 1; }
  case "$MODE" in
    task) need_task=1; need_schema=1; need_schema_owner=1 ;;
    trusted) need_owner=1 ;;
    all) need_owner=1; need_task=1; need_schema=1; need_schema_owner=1 ;;
    *) echo "✗ static 模式不应调用数据库就绪检查" >&2; exit 2 ;;
  esac
  if [ "$need_owner" = 1 ] && ! psql "$OWNER_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1; then db_ok=0; fi
  if [ "$need_task" = 1 ] && ! psql "$TASK_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1; then db_ok=0; fi
  if [ "$need_schema" = 1 ] && ! psql "$TASK_SCHEMA_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1; then db_ok=0; fi
  if [ "$need_schema_owner" = 1 ] && ! psql "$TASK_SCHEMA_OWNER_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1; then db_ok=0; fi
  if [ "$db_ok" = 0 ]; then
    [ -f "$COMPOSE_ROOT/compose.yaml" ] || { echo "✗ 缺少固定 compose.yaml" >&2; exit 1; }
    env -u COMPOSE_FILE -u COMPOSE_PROFILES -u COMPOSE_PROJECT_NAME \
      -u DOCKER_HOST -u DOCKER_CONTEXT -u DOCKER_CONFIG \
      docker compose --project-name "$(basename -- "$COMPOSE_ROOT")" --project-directory "$COMPOSE_ROOT" \
        --env-file /dev/null -f "$COMPOSE_ROOT/compose.yaml" up -d --wait
  fi
  if [ "$need_owner" = 1 ]; then
    psql "$OWNER_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1 \
      || { echo "✗ owner 数据库不可用" >&2; exit 1; }
  fi
  if [ "$need_task" = 1 ]; then
    psql "$TASK_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1 \
      || { echo "✗ 任务控制库不可用，请按 eng/02 §3.2 执行 docker compose down -v 后重建" >&2; exit 1; }
  fi
  if [ "$need_schema" = 1 ]; then
    psql "$TASK_SCHEMA_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1 \
      || { echo "✗ 任务 schema 库不可用，请按 eng/02 §3.2 执行 docker compose down -v 后重建" >&2; exit 1; }
  fi
  if [ "$need_schema_owner" = 1 ]; then
    psql "$TASK_SCHEMA_OWNER_DB_URL" -Atqc 'SELECT 1' >/dev/null 2>&1 \
      || { echo "✗ 任务 schema owner 连接不可用，请按 eng/02 §3.2 执行 docker compose down -v 后重建" >&2; exit 1; }
    if [ "${TGM_TASK_SCHEMA_READY:-0}" != 1 ]; then
      run_trusted_url "$TASK_SCHEMA_OWNER_DB_URL" cargo run --quiet --bin tgm -- migrate
      TGM_TASK_SCHEMA_READY=1
    fi
  fi
}

run_gate static fmt cargo fmt --all --check
run_gate static clippy cargo clippy --all-targets --all-features -- -D warnings
run_gate static offline-build env SQLX_OFFLINE=true cargo build --workspace --all-targets
run_gate static deny cargo deny check
run_gate static newtype trusted_helper scripts/check-newtype.sh
run_gate static driver-regressions python3 scripts/ai-driver/check-regressions.py
if [ "$MODE" = trusted ] || [ "$MODE" = all ]; then
  ensure_db
fi
run_gate trusted migrate cargo run --quiet --bin tgm -- migrate

if [ "$MODE" = task ] || [ "$MODE" = all ]; then
  ensure_db
fi
run_gate task sqlx-check
run_gate trusted migrate-idempotent trusted_helper scripts/migrate-idempotent.sh
run_gate task test cargo test --workspace
run_gate trusted audit-rls cargo run --quiet --bin tgm -- audit-rls
run_gate trusted double-pass trusted_helper scripts/audit-double-pass.sh

echo "── 跳过 openapi-check（PRE-004 在实现入分支后接入）──"
case "$MODE" in
  static) echo "✓ 已执行的静态卡口通过；数据库卡口未运行" ;;
  task) echo "✓ 已执行的任务代码卡口通过；可信数据库卡口未运行" ;;
  trusted) echo "✓ 已执行的可信数据库卡口通过" ;;
  all)
    if [ -n "$FROM" ]; then
      echo "✓ 已执行从 $FROM 开始的本地卡口（此前卡口按 --from 跳过）"
    else
      echo "✓ 已执行的全部本地卡口通过"
    fi
    ;;
esac
