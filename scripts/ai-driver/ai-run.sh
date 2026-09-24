#!/usr/bin/env bash
# AI 自主驱动循环：一次跑一个任务。流程与用法见本目录 README.md。
#
#   任务文件 → 干净 worktree → 起库 → 执行器实现 → 保护路径检查 →
#   卡口验证（不信 agent 自述）→ 推分支开 PR → 回写任务状态
#
# 人只保留合并决策。命中 AGENTS.md §五 升级条件的任务由 agent 写提案后停止。
# 任务可在文件头声明 `allow-paths:`（逗号分隔）放行保护路径，
# 格式见 docs/plan/backlog/README.md，放行责任在写任务的人。
set -uo pipefail
set -o pipefail
export GIT_NO_REPLACE_OBJECTS=1
umask 077

# 不允许调用环境把驱动带到另一棵仓库、索引或对象库里。
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR \
  GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
  GIT_ALTERNATE_OBJECT_DIRECTORIES_RELATIVE

for forbidden_git_env in GIT_REPLACE_REF_BASE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_ALTERNATE_OBJECT_DIRECTORIES_RELATIVE; do
  [ -z "${!forbidden_git_env:-}" ] || { echo "[driver] ✗ 拒绝异常 Git 对象环境：$forbidden_git_env" >&2; exit 1; }
done

HOST_GIT_CONFIG_GLOBAL="${GIT_CONFIG_GLOBAL:-${HOME:-}/.gitconfig}"
HOST_GIT_CONFIG_SYSTEM="${GIT_CONFIG_SYSTEM:-/etc/gitconfig}"
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_SYSTEM=/dev/null
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_COUNT=0

PRIMARY="$(git rev-parse --show-toplevel)"
cd "$PRIMARY"
BACKLOG_DIR="$PRIMARY/docs/plan/backlog"
WT_ROOT="$(dirname "$PRIMARY")/ai-worktrees"
LOG_DIR=""

EXECUTOR="codex"
TASK_ID=""
BASE="dev"
REMOTE="cloud"
DRY=0
NO_PR=0
MAX_RETRIES=2
TIMEOUT=3600
ALLOW_PATHS=""
BASE_COMMIT=""
EXECUTOR_DATABASE_URL="${TGM_DB_URL_EXECUTOR:-}"
TASK_DATABASE_URL="${TGM_DB_URL_TEST:-${TGM_TEST_DATABASE_URL:-postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control}}"
TASK_SCHEMA_DATABASE_URL="${TGM_DB_URL_TASK_SCHEMA:-${TGM_TASK_SCHEMA_DATABASE_URL:-postgres://app_user:apw@127.0.0.1:55432/tgm_test_schema}}"
TASK_SCHEMA_OWNER_DATABASE_URL="${TGM_DB_URL_TASK_SCHEMA_OWNER:-${TGM_TASK_SCHEMA_OWNER_DATABASE_URL:-postgres://postgres:pw@127.0.0.1:55432/tgm_test_schema}}"
TASK_EXPLICIT=0

DRIVER_OWNER_DB_URL="${TGM_DB_URL_OWNER:-postgres://postgres:pw@127.0.0.1:55432/tgm}"
OWNER_TASK_SCHEMA_URL="${TGM_DB_URL_OWNER_TASK_SCHEMA:-}"
unset DATABASE_URL TGM_DB_URL_OWNER TGM_DB_URL_EXECUTOR TGM_DB_URL_TEST

while [ $# -gt 0 ]; do
  case "$1" in
    --task) [ "$#" -ge 2 ] || { echo "--task 缺少参数" >&2; exit 2; }; TASK_ID="$2"; TASK_EXPLICIT=1; shift 2 ;;
    --executor) [ "$#" -ge 2 ] || { echo "--executor 缺少参数" >&2; exit 2; }; EXECUTOR="$2"; shift 2 ;;
    --base) [ "$#" -ge 2 ] || { echo "--base 缺少参数" >&2; exit 2; }; BASE="$2"; shift 2 ;;
    --remote) [ "$#" -ge 2 ] || { echo "--remote 缺少参数" >&2; exit 2; }; REMOTE="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --no-pr) NO_PR=1; shift ;;
    --max-retries) [ "$#" -ge 2 ] || { echo "--max-retries 缺少参数" >&2; exit 2; }; MAX_RETRIES="$2"; shift 2 ;;
    --timeout) [ "$#" -ge 2 ] || { echo "--timeout 缺少参数" >&2; exit 2; }; TIMEOUT="$2"; shift 2 ;;
    --commit-status)
      echo "--commit-status 已废弃：状态只写入 Git common dir 下的本地账本" >&2
      exit 2
      ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done

[[ "$MAX_RETRIES" =~ ^[0-9]+$ ]] || { echo "--max-retries 必须是非负整数" >&2; exit 2; }
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ ]] || { echo "--timeout 必须是正整数秒数" >&2; exit 2; }
[[ -n "$REMOTE" && "$REMOTE" != -* ]] || { echo "--remote 必须是非空且不以 - 开头的远端名" >&2; exit 2; }
[[ -n "$BASE" && "$BASE" != -* ]] || { echo "--base 必须是非空且不以 - 开头的分支名" >&2; exit 2; }

log() { echo "[driver] $*"; }
die() { echo "[driver] ✗ $*" >&2; exit 1; }

private_dir() {
  local dir="$1" label="$2" mode uid gid resolved
  if [ -L "$dir" ] || { [ -e "$dir" ] && [ ! -d "$dir" ]; }; then
    die "$label 不是普通目录：$dir"
  fi
  mkdir -p -- "$dir" || die "$label 创建失败：$dir"
  resolved="$(realpath -e -- "$dir")" || die "$label 真实路径解析失败：$dir"
  [ "$resolved" = "$dir" ] || die "$label 真实路径不一致：$dir"
  mode="$(stat -c '%a' -- "$dir")" || die "$label 权限读取失败：$dir"
  uid="$(stat -c '%u' -- "$dir")" || die "$label 属主读取失败：$dir"
  gid="$(stat -c '%g' -- "$dir")" || die "$label 组读取失败：$dir"
  [ -n "$mode" ] && [ "$uid" = "$(id -u)" ] && [ "$gid" = "$(id -g)" ] \
    || die "$label 属主或元数据不符合预期：$dir"
  # bash 算术优先级 == 高于 &：不加括号时 `a & b == 0` 是 `a & (b == 0)`，
  # 恒为 0、恒触发 die —— 实测 700 目录也被拒，驱动启动即死。
  (( (8#$mode & 022) == 0 )) || die "$label 对组/其他用户可写：$dir"
  [ -x "$dir" ] || die "$label 不可遍历：$dir"
}

safe_log_file() {
  local file="$1"
  [ ! -e "$file" ] && [ ! -L "$file" ] || die "日志目标已存在或是符号链接：$file"
  (set -o noclobber; : > "$file") || die "日志文件创建失败：$file"
  chmod 600 -- "$file" || die "日志文件权限设置失败：$file"
}

safe_write_log() {
  local file="$1"
  shift
  [ ! -e "$file" ] && [ ! -L "$file" ] || die "日志目标已存在或是符号链接：$file"
  (set -o noclobber; cat > "$file") || die "日志文件写入失败：$file"
  chmod 600 -- "$file" || die "日志文件权限设置失败：$file"
}

run_compose() {
  env -u COMPOSE_FILE -u COMPOSE_PROFILES -u COMPOSE_PROJECT_NAME \
    -u DOCKER_HOST -u DOCKER_CONTEXT -u DOCKER_CONFIG \
    docker compose --project-name "$(basename -- "$PRIMARY")" \
      --project-directory "$PRIMARY" \
      --env-file /dev/null -f "$PRIMARY/compose.yaml" "$@"
}

# 同一仓库只允许一个驱动实例：重叠的 cron / systemd 轮次会互相删除
# worktree、覆盖任务状态，甚至让两个执行器同时写同一个分支。
COMMON_GIT_DIR="$(git rev-parse --git-common-dir)" || die "无法解析 Git common dir"
case "$COMMON_GIT_DIR" in
  /*) ;;
  *) COMMON_GIT_DIR="$PRIMARY/$COMMON_GIT_DIR" ;;
esac
STATE_DIR="$COMMON_GIT_DIR/ai-driver-state"
LOG_DIR="$(dirname "$COMMON_GIT_DIR")/ai-driver-logs"
[ -d "$COMMON_GIT_DIR" ] && [ ! -L "$COMMON_GIT_DIR" ] \
  || die "Git common dir 不是普通目录：$COMMON_GIT_DIR"
if ! git for-each-ref --format='%(refname)' refs/replace >"$COMMON_GIT_DIR/ai-driver.replace-refs"; then
  die "无法检查 refs/replace/*，拒绝运行"
fi
if grep -q . "$COMMON_GIT_DIR/ai-driver.replace-refs"; then
  rm -f -- "$COMMON_GIT_DIR/ai-driver.replace-refs"
  die "检测到 refs/replace/*，拒绝在可替换 Git 对象上运行"
fi
rm -f -- "$COMMON_GIT_DIR/ai-driver.replace-refs"
command -v flock >/dev/null 2>&1 || die "找不到 flock，拒绝在无互斥锁下运行"
exec 9>"$COMMON_GIT_DIR/ai-driver.lock" || die "无法创建驱动锁：$COMMON_GIT_DIR/ai-driver.lock"
flock -n 9 || die "已有另一个 ai-run 实例运行中，退出以避免并发破坏 worktree"
if [ -L "$LOG_DIR" ] || { [ -e "$LOG_DIR" ] && [ ! -d "$LOG_DIR" ]; }; then
  die "日志目录不是普通目录：$LOG_DIR"
fi
mkdir -p -- "$LOG_DIR" || die "日志目录创建失败：$LOG_DIR"
[ -d "$LOG_DIR" ] && [ ! -L "$LOG_DIR" ] || die "日志目录不是普通目录：$LOG_DIR"
private_dir "$WT_ROOT" "worktree 根目录"
private_dir "$LOG_DIR" "日志目录"

# psql 不在非交互 shell（cron）的 PATH 里 —— 与 ai-gates.sh 同款兜底。
# 教训出处：eng/00 §零 的实测记录；漏掉它会让库完全正常时也报「PG 不可达」。
if ! command -v psql >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  [ -f "$HOME/opt/pgdg17/env.sh" ] && source "$HOME/opt/pgdg17/env.sh"
fi
command -v psql >/dev/null 2>&1 || die "找不到 psql（source ~/opt/pgdg17/env.sh 或把 pgdg17 的 bin 加进 PATH）"
command -v timeout >/dev/null 2>&1 || die "找不到 timeout，拒绝在无界配置读取下运行"
command -v systemd-run >/dev/null 2>&1 || die "找不到 systemd-run，拒绝在无 cgroup 终止边界下运行"
command -v systemctl >/dev/null 2>&1 || die "找不到 systemctl，拒绝在无法清空 cgroup 时运行"

require_systemd_scope() {
  local unit="ai-driver-probe-$$.scope"
  systemctl --user is-system-running >/dev/null 2>&1 \
    || die "systemd 用户会话不可用，拒绝回退到可脱离的进程组 watchdog"
  systemd-run --user --scope --unit="$unit" --collect --quiet /bin/true \
    || die "systemd scope 不可用，拒绝回退到可脱离的进程组 watchdog"
  wait_scope_empty "$unit" || die "systemd probe scope 未清空，拒绝继续"
}

wait_scope_empty() {
  local unit="$1" i
  for i in $(seq 1 100); do
    if ! systemctl --user is-active --quiet "$unit"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

validate_task_database() {
  local row role is_super bypass_rls can_create_role can_create_db owner_inherit owner_set prod_connect
  [ -n "$TASK_DATABASE_URL" ] || die "缺少 TGM_DB_URL_TEST；任务卡口拒绝使用 owner 连接串"
  row="$(PGCONNECT_TIMEOUT=5 psql "$TASK_DATABASE_URL" -Atqc \
    "SELECT current_user || '|' || r.rolsuper || '|' || r.rolbypassrls || '|' || r.rolcreaterole || '|' || r.rolcreatedb || '|' || COALESCE((SELECT m.inherit_option FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid WHERE m.member = r.oid AND g.rolname = 'tgm_owner'), false) || '|' || COALESCE((SELECT m.set_option FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid WHERE m.member = r.oid AND g.rolname = 'tgm_owner'), false) || '|' || has_database_privilege(current_user, 'tgm', 'CONNECT') FROM pg_roles r WHERE r.rolname = current_user")" \
    || die "任务测试连接不可用，拒绝运行任务卡口"
  IFS='|' read -r role is_super bypass_rls can_create_role can_create_db owner_inherit owner_set prod_connect <<< "$row"
  [ -n "$role" ] || die "任务测试连接未返回当前角色"
  [ "$is_super" = f ] || die "任务测试角色不得是超级用户"
  [ "$bypass_rls" = f ] || die "任务测试角色不得带 BYPASSRLS"
  [ "$can_create_role" = f ] || die "任务测试角色不得带 CREATEROLE"
  [ "$can_create_db" = t ] || die "任务测试角色必须具备 CREATEDB 以供 sqlx::test 建临时库"
  [ "$owner_inherit" = f ] && [ "$owner_set" = t ] \
    || die "任务测试角色必须仅以 SET ROLE 方式使用 tgm_owner"
  [ "$prod_connect" = f ] || die "任务测试角色不得连接生产数据库 tgm"
  [ "${role}" = "tgm_test" ] || die "任务卡口必须使用固定测试角色 tgm_test"
}

# 保护路径：裁决机制与安全面的所在。AGENTS.md §五 硬边界的**机器判据** ——
# 提示词拦不住的，这里拦。agent 的 diff 命中即强制升级，**不跑卡口**
# （卡口脚本本身在 scripts/ 下，被改过之后跑出来的绿不携带信息）。
# 任务确需触碰（如加依赖要改根 Cargo.toml）时，在任务文件声明
# `allow-paths:`（逗号分隔），由写任务的人负责这个放行。
#
# AGENTS.md / CLAUDE.md / backlog 在清单里的理由与卡口脚本同源：
# 下一轮的提示词由 AGENTS.md 与任务文件拼成，agent 改一行 §五 升级条件、
# 或把自己任务的 escalation 改成 no，就是自我减刑，而这份改动会经 PR 进基线
# （§七 信任边界 1 的机器化）。backlog 进清单还顺手消掉状态双写：
# 驱动不再改任务文件，agent 也不能改，合并就不会撞。
#
# 刻意不在清单里：docs/plan/proposals/（升级路径的产物）、
# docs/plan/ponytail-debt.md（§4.4 要求 agent 登记）—— 提示词点名要 agent 写,
# 圈进来会让每个任务都误判 protected，把 escalated 出口废掉。
#
# `.cargo/` 在清单里：rustc-wrapper、alias 等配置等于换掉编译器本身，
# 拦截的是裁决工具而不只是被测代码。build.rs 刻意不在 —— 它是被测物
# （编译期运行、进 PR diff 可见、CI 冷环境重跑），圈进来会让每个加
# crate 的任务都误升级（覆盖分层的完整说明见 README 已知边界 11）。
PROTECTED=(
  scripts/
  migrations/
  .github/
  crates/db/src/audit/
  .cargo/
  AGENTS.md
  CLAUDE.md
  docs/plan/backlog/
  migrations-test/
  Cargo.toml
  Cargo.lock
  compose.yaml
  .env
  compose.override.yaml
  docker-compose.override.yml
  deny.toml
  clippy.toml
  rustfmt.toml
  rust-toolchain.toml
  .gitignore
)

allowed_path() {
  local hit="$1" a
  [ -z "$ALLOW_PATHS" ] && return 1
  # 不用 $(echo|tr) 的无引号分词：ALLOW_PATHS 来自任务文件，而任务文件正是
  # §七 信任边界 1 点名的输入 —— 分词会顺带做 glob 展开。
  local -a list=()
  IFS=',' read -ra list <<< "$ALLOW_PATHS"
  for a in "${list[@]}"; do
    a="${a#"${a%%[![:space:]]*}"}"; a="${a%"${a##*[![:space:]]}"}"
    [ -z "$a" ] && continue
    a="${a%/}"   # 容忍尾斜杠：PROTECTED 清单是「目录/」风格，照抄不该失效
    case "$hit" in "$a" | "$a"/*) return 0 ;; esac
  done
  return 1
}

# 索引隐藏标志扫描：skip-worktree（S）与 assume-unchanged（小写字母）会让
# git status 与一切工作区 diff 对磁盘上的真实内容失明 —— check_protected 的
# Git 视图与卡口后的脏树判据全部被骗成空（实测：篡改保护文件后设标志，
# Git 视图与 status 同时输出空，卡口照被篡改的内容跑出「绿」）。
# 新鲜 worktree 里这两个标志没有合法来源：驱动从不设置它们，本仓不启用
# sparse-checkout（它也会设 skip-worktree）。命中即强制升级、不跑卡口。
#
# ls-files -v 的 tag：H/M/R/C/K = 正常工作状态（正常/未合并/暂存删除/暂存改
# 动/待杀）；S = skip-worktree；小写 = assume-unchanged（s = 两标志同时）。
check_index_flags() {
  local bad
  bad="$(git -C "$wt" ls-files -v | awk 'substr($0, 1, 1) !~ /^[HMRCK]$/')" \
    || die "索引标志检查失败（git ls-files -v）—— 不放行，先修环境"
  printf '%s' "$bad"
}

protected_tree_mismatches() {
  local tree_root="$1" ref="$2"
  local p root entry relative record prefix mode type baseline_path working_path
  local scan_dir work_entries base_entries record_file expected_file mismatch_file
  local link_target_file
  scan_dir="$(mktemp -d "$COMMON_GIT_DIR/ai-driver-protected.XXXXXX")" || return 1
  work_entries="$scan_dir/work"
  base_entries="$scan_dir/base"
  record_file="$scan_dir/record"
  expected_file="$scan_dir/expected"
  mismatch_file="$scan_dir/mismatch"
  link_target_file="$scan_dir/link-target"
  : >"$mismatch_file" || { rm -rf -- "$scan_dir"; return 1; }
  if ! git -C "$tree_root" rev-parse --verify "$ref^{commit}" >/dev/null 2>&1; then
    rm -rf -- "$scan_dir"
    echo "[driver] ✗ 保护路径基线不可解析：$ref" >&2
    return 1
  fi
  for p in "${PROTECTED[@]}"; do
    root="$tree_root/${p%/}"
    if [ -e "$root" ] || [ -L "$root" ]; then
      if ! find -P "$root" -print0 >"$work_entries"; then
        rm -rf -- "$scan_dir"
        echo "[driver] ✗ 保护路径 find 失败：$root" >&2
        return 1
      fi
      while IFS= read -r -d '' entry; do
        relative="${entry#"$tree_root/"}"
        : >"$record_file" || { rm -rf -- "$scan_dir"; return 1; }
        if ! git -C "$tree_root" ls-tree -z "$ref" -- "$relative" >"$record_file"; then
          rm -rf -- "$scan_dir"
          echo "[driver] ✗ 保护路径基线读取失败：$relative" >&2
          return 1
        fi
        record=""
        if [ -s "$record_file" ]; then
          IFS= read -r -d '' record <"$record_file" || {
            rm -rf -- "$scan_dir"
            echo "[driver] ✗ 保护路径基线记录损坏：$relative" >&2
            return 1
          }
        fi
        prefix="${record%%$'\t'*}"
        mode="${prefix%% *}"
        type="${prefix#* }"
        type="${type%% *}"
        if [ -L "$entry" ]; then
          if [ "$type" != blob ] || [ "$mode" != 120000 ]; then
            printf '%s\n' "$relative" >>"$mismatch_file"
          else
            if ! git -C "$tree_root" show "$ref:$relative" >"$expected_file"; then
              rm -rf -- "$scan_dir"
              echo "[driver] ✗ 保护路径基线内容读取失败：$relative" >&2
              return 1
            fi
            if ! readlink -n -- "$entry" >"$link_target_file"; then
              rm -rf -- "$scan_dir"
              echo "[driver] ✗ 保护路径链接目标读取失败：$relative" >&2
              return 1
            fi
            if ! cmp -s "$link_target_file" "$expected_file"; then
              printf '%s\n' "$relative" >>"$mismatch_file"
            fi
          fi
        elif [ -d "$entry" ]; then
          [ "$type" = tree ] || printf '%s\n' "$relative" >>"$mismatch_file"
        elif [ -f "$entry" ]; then
          case "$mode" in
            100644|100755)
              if ! git -C "$tree_root" show "$ref:$relative" >"$expected_file"; then
                rm -rf -- "$scan_dir"
                echo "[driver] ✗ 保护路径基线内容读取失败：$relative" >&2
                return 1
              fi
              cmp -s "$expected_file" "$entry" || printf '%s\n' "$relative" >>"$mismatch_file"
              if [ "$mode" = 100755 ]; then
                [ -x "$entry" ] || printf '%s\n' "$relative" >>"$mismatch_file"
              else
                [ ! -x "$entry" ] || printf '%s\n' "$relative" >>"$mismatch_file"
              fi
              ;;
            *)
              printf '%s\n' "$relative" >>"$mismatch_file"
              ;;
          esac
        else
          printf '%s\n' "$relative" >>"$mismatch_file"
        fi
      done < "$work_entries"
    fi

    if ! git -C "$tree_root" ls-tree -r -z --name-only "$ref" -- "$p" >"$base_entries"; then
      rm -rf -- "$scan_dir"
      echo "[driver] ✗ 保护路径基线枚举失败：$p" >&2
      return 1
    fi
    while IFS= read -r -d '' baseline_path; do
      [ -z "$baseline_path" ] && continue
      working_path="$tree_root/$baseline_path"
      if [ ! -e "$working_path" ] && [ ! -L "$working_path" ]; then
        printf '%s\n' "$baseline_path" >>"$mismatch_file"
      fi
    done < "$base_entries"
  done
  if ! LC_ALL=C sort -u "$mismatch_file"; then
    rm -rf -- "$scan_dir"
    echo "[driver] ✗ 保护路径差异排序失败" >&2
    return 1
  fi
  rm -rf -- "$scan_dir"
  return 0
}

check_protected() {
  local hits="" p f tree_hit diff_file tree_file hit_file
  # 保护路径必须绕过 Git 的工作区视图：clean/smudge filter、info/attributes
  # 和 .gitignore 都能让 git diff / status 对磁盘内容失明。这里直接把基线
  # 提交对象的原始字节与 worktree 磁盘内容逐个比较，再单独检查提交树新增路径
  # 和磁盘新增文件；同时检查目录与符号链接类型，避免用同内容的链接替换目录。
  tree_file="$(mktemp)" || die "保护路径原始差异临时文件创建失败"
  if ! protected_tree_mismatches "$wt" "$BASE_COMMIT" >"$tree_file"; then
    rm -f -- "$tree_file"
    die "保护路径原始差异检查失败 —— 不放行，先修环境"
  fi
  while IFS= read -r tree_hit; do
    [ -z "$tree_hit" ] || { allowed_path "$tree_hit" || hits="$hits$tree_hit"$'\n'; }
  done < "$tree_file"
  rm -f -- "$tree_file"

  for p in "${PROTECTED[@]}"; do
    diff_file="$(mktemp)" || die "保护路径差异临时文件创建失败"
    if ! git -C "$wt" diff-tree --no-commit-id --name-only -r -z "$BASE_COMMIT" HEAD -- "$p" > "$diff_file"; then
      rm -f -- "$diff_file"
      die "保护路径检查失败（git diff-tree $BASE_COMMIT HEAD -- $p）—— 不放行，先修环境"
    fi
    while IFS= read -r -d '' f; do
      [ -z "$f" ] || { allowed_path "$f" || hits="$hits$f"$'\n'; }
    done < "$diff_file"
    rm -f -- "$diff_file"
  done
  hit_file="$(mktemp)" || die "保护路径命中临时文件创建失败"
  printf '%s' "$hits" >"$hit_file" || { rm -f -- "$hit_file"; die "保护路径命中临时文件写入失败"; }
  if ! LC_ALL=C sort -u "$hit_file"; then
    rm -f -- "$hit_file"
    die "保护路径命中排序失败 —— 不放行，先修环境"
  fi
  LC_ALL=C sort -u "$hit_file"
  rm -f -- "$hit_file"
}

state_fingerprint() {
  check_state_dir
  fingerprint_path_tree "$STATE_DIR" "state"
}

fingerprint_path_tree() {
  local root="$1" label="$2" entry kind metadata target hash
  local entries sorted output
  if [ ! -e "$root" ] && [ ! -L "$root" ]; then
    printf 'missing\t%s\n' "$(printf '%q' "$root")"
    return 0
  fi
  if [ -d "$root" ] && [ ! -L "$root" ] && [ ! -x "$root" ]; then
    echo "[driver] ✗ $label 根目录不可遍历：$root" >&2
    return 1
  fi
  entries="$(mktemp "$COMMON_GIT_DIR/ai-driver-scan.XXXXXX")" || return 1
  sorted="${entries}.sorted"
  if ! find -P "$root" -print0 >"$entries"; then
    rm -f -- "$entries" "$sorted"
    echo "[driver] ✗ $label find 失败：$root" >&2
    return 1
  fi
  if ! LC_ALL=C sort -z "$entries" >"$sorted"; then
    rm -f -- "$entries" "$sorted"
    echo "[driver] ✗ $label 排序失败：$root" >&2
    return 1
  fi
  output="${entries}.output"
  : > "$output" || { rm -f -- "$entries" "$sorted"; return 1; }
  while IFS= read -r -d '' entry; do
    metadata="$(stat -c '%F %a %u %g' -- "$entry")" || { rm -f -- "$entries" "$sorted" "$output"; return 1; }
    if [ -L "$entry" ]; then
      kind=symlink
      target="$(readlink -n -- "$entry" && printf '.')" || { rm -f -- "$entries" "$sorted" "$output"; return 1; }
      target="${target%.}"
      hash=""
      if [ -f "$entry" ]; then
        hash="$(sha256sum -- "$entry" | awk '{print $1}')" || { rm -f -- "$entries" "$sorted" "$output"; return 1; }
      fi
      printf '%s\t%s\t%s\t%s\t%s\n' "$kind" "$metadata" "$(printf '%q' "$entry")" "$(printf '%q' "$target")" "$hash" >>"$output"
    elif [ -f "$entry" ]; then
      hash="$(sha256sum -- "$entry" | awk '{print $1}')" || { rm -f -- "$entries" "$sorted" "$output"; return 1; }
      printf 'file\t%s\t%s\t%s\n' "$metadata" "$(printf '%q' "$entry")" "$hash" >>"$output"
    elif [ -d "$entry" ]; then
      printf 'directory\t%s\t%s\n' "$metadata" "$(printf '%q' "$entry")" >>"$output"
    else
      printf 'other\t%s\t%s\n' "$metadata" "$(printf '%q' "$entry")" >>"$output"
    fi
  done < "$sorted"
  rm -f -- "$entries" "$sorted"
  if ! LC_ALL=C sort "$output"; then
    rm -f -- "$output"
    echo "[driver] ✗ $label 指纹排序失败：$root" >&2
    return 1
  fi
  rm -f -- "$output"
}

check_state_guard() {
  local current
  check_state_dir
  current="$(state_fingerprint)" || die "驱动状态账本检查失败 —— 不放行，先修环境"
  [ "$current" = "$STATE_GUARD_SNAPSHOT" ]
}

check_state_dir() {
  local resolved mode uid gid
  if [ -L "$STATE_DIR" ] || { [ -e "$STATE_DIR" ] && [ ! -d "$STATE_DIR" ]; }; then
    die "状态目录不是普通目录：$STATE_DIR"
  fi
  [ -d "$STATE_DIR" ] || mkdir -p -- "$STATE_DIR" || die "状态目录创建失败：$STATE_DIR"
  [ ! -L "$STATE_DIR" ] || die "状态目录不能是符号链接：$STATE_DIR"
  resolved="$(realpath -e -- "$STATE_DIR")" || die "状态目录真实路径解析失败：$STATE_DIR"
  [ "$resolved" = "$STATE_DIR" ] || die "状态目录不是预期目录：$STATE_DIR"
  mode="$(stat -c '%a' -- "$STATE_DIR")" || die "状态目录权限读取失败：$STATE_DIR"
  uid="$(stat -c '%u' -- "$STATE_DIR")" || die "状态目录属主读取失败：$STATE_DIR"
  gid="$(stat -c '%g' -- "$STATE_DIR")" || die "状态目录组读取失败：$STATE_DIR"
  [ -n "$mode" ] && [ "$uid" = "$(id -u)" ] && [ "$gid" = "$(id -g)" ] \
    || die "状态目录属主或元数据不符合预期：$STATE_DIR"
  # 同 private_dir：== 优先级高于 &，不加括号判据恒假。
  (( (8#$mode & 077) == 0 )) || die "状态目录对其他用户可读写：$STATE_DIR"
  [ -x "$STATE_DIR" ] || die "状态目录不可遍历：$STATE_DIR"
}

# 执行器位于 worktree，但仍以全权限运行，理论上可以用绝对路径改写主仓。
# 保护路径检查只看 wt，所以额外保存主仓保护路径的**原始文件指纹**；日志目录是
# 驱动自己的预期写入，明确排除。只比较 status 码不够：同一文件继续保持 M
# 时内容仍可能被换掉，必须直接摘要磁盘字节。
primary_guard_fingerprint() {
  local p root snapshot output
  output="$(mktemp "$COMMON_GIT_DIR/ai-driver-primary.XXXXXX")" || return 1
  for p in "${PROTECTED[@]}"; do
    root="$PRIMARY/$p"
    if [ ! -e "$root" ] && [ ! -L "$root" ]; then
      printf 'missing\t%s\n' "$(printf '%q' "$root")" >>"$output"
      continue
    fi
    snapshot="$(fingerprint_path_tree "$root" "保护路径 $p")" || return 1
    printf '%s\t%s\n' "protected-root" "$(printf '%q' "$p")" >>"$output"
    printf '%s\n' "$snapshot" >>"$output"
  done
  if ! LC_ALL=C sort "$output"; then
    rm -f -- "$output"
    echo "[driver] ✗ 主仓保护路径指纹排序失败" >&2
    return 1
  fi
  rm -f -- "$output"
}

check_trusted_source_snapshot() {
  local current
  [ -d "$TRUSTED_ROOT" ] && [ ! -L "$TRUSTED_ROOT" ] \
    || die "可信源码根目录不是普通目录 —— 不放行，先修环境"
  current="$(fingerprint_path_tree "$TRUSTED_ROOT" "可信源码")" \
    || die "可信源码快照检查失败 —— 不放行，先修环境"
  [ "$current" = "$TRUSTED_SOURCE_SNAPSHOT" ]
}

primary_guard_index() {
  git -C "$PRIMARY" ls-files -v -- "${PROTECTED[@]}"
}

check_primary_guard() {
  local current head index
  current="$(primary_guard_fingerprint)" \
    || die "主仓保护路径检查失败 —— 不放行，先修环境"
  head="$(git -C "$PRIMARY" rev-parse HEAD 2>/dev/null)" \
    || die "主仓 HEAD 检查失败 —— 不放行，先修环境"
  index="$(primary_guard_index)" \
    || die "主仓索引检查失败 —— 不放行，先修环境"
  [ "$current" = "$PRIMARY_GUARD_SNAPSHOT" ] \
    && [ "$head" = "$PRIMARY_HEAD_SNAPSHOT" ] \
    && [ "$index" = "$PRIMARY_INDEX_SNAPSHOT" ]
}

check_primary_start_clean() {
  local dirty hidden raw primary_head
  primary_head="$(git -C "$PRIMARY" rev-parse --verify HEAD^{commit})" \
    || die "主仓 HEAD 解析失败 —— 不放行，先修环境"
  raw="$(protected_tree_mismatches "$PRIMARY" "$primary_head")" \
    || die "主仓保护路径原始内容检查失败 —— 不放行，先修环境"
  [ -z "$raw" ] || die "主仓保护路径已有磁盘改动，拒绝启动驱动：$(printf '%s' "$raw" | tr '\n' ' ')"
  dirty="$(git -C "$PRIMARY" status --porcelain=v1 --untracked-files=all -- "${PROTECTED[@]}")"
  [ -z "$dirty" ] || die "主仓保护路径已有未提交改动，拒绝启动驱动：$(printf '%s' "$dirty" | tr '\n' ' ')"
  hidden="$(primary_guard_index | awk 'substr($0, 1, 1) !~ /^[HMRCK]$/')"
  [ -z "$hidden" ] \
    || die "主仓保护路径存在索引隐藏标志，拒绝启动驱动：$(printf '%s' "$hidden" | tr '\n' ' ')"
}

check_base_unchanged() {
  local current
  current="$(git ls-remote --exit-code "$REMOTE" "refs/heads/$BASE" 2>/dev/null)" \
    || return 1
  [ "${current%%$'\t'*}" = "$BASE_COMMIT" ]
}

validate_task_id() {
  [ -n "$TASK_ID" ] || die "任务 id 不能为空"
  [[ "$TASK_ID" =~ ^T-[0-9]+-.+$ ]] || die "任务 id 格式非法：$TASK_ID"
  [[ "$TASK_ID" != */* && "$TASK_ID" != *"\\"* ]] \
    || die "任务 id 含路径分隔符：$TASK_ID"
  case "$TASK_ID" in *..*) die "任务 id 含非法路径片段：$TASK_ID" ;; esac
  [[ "$TASK_ID" != *[[:cntrl:]]* ]] || die "任务 id 含控制字符"
}

# ── 任务选取 ─────────────────────────────────────────────────────
task_status() {
  local file="$1"
  sed -n 's/^status: *//p' "$file" | head -1
}

check_task_status() {
  local base_st ledger_st
  base_st="$(task_status "$TASK_FILE")"
  [ -n "$base_st" ] || die "任务文件缺少 status：$TASK_FILE"
  case "$base_st" in pending|active|done|escalated|blocked) ;; *) die "任务 status 非法：$base_st" ;; esac
  ledger_st="$(task_status "$(task_state_file)" 2>/dev/null || true)"
  if [ "$TASK_EXPLICIT" = 1 ]; then
    [ "$base_st" = pending ] || die "显式任务不是 pending，拒绝重复执行：$TASK_ID（status=$base_st）"
    [ -z "$ledger_st" ] || [ "$ledger_st" = pending ] || die "显式任务已有本地运行状态，拒绝重复执行：$TASK_ID（status=$ledger_st）"
  fi
}

# ── 任务选取 ─────────────────────────────────────────────────────
pick_task() {
  local f id base_st st state_file
  for f in "$BACKLOG_DIR"/T-*.md; do
    [ -f "$f" ] || continue
    id="$(basename "$f" .md)"
    state_file="$STATE_DIR/$id.state"
    if [ -L "$state_file" ] || { [ -e "$state_file" ] && [ ! -f "$state_file" ]; }; then
      die "任务状态账本条目不是普通文件：$state_file"
    fi
    base_st="$(sed -n 's/^status: *//p' "$f" | head -1)"
    st="$(sed -n 's/^status: *//p' "$state_file" 2>/dev/null | head -1)"
    [ -n "$st" ] || st="$base_st"
    if [ "$st" = "active" ]; then
      # 已经拿到驱动锁，能走到这里就说明没有另一个 ai-run 持有该任务。
      # active 只可能是上次异常退出留下的残局，恢复为 backlog 的静态状态，
      # 不依赖可能被 PID 重用的 kill -0。
      st="$base_st"
    fi
    if [ "$st" = "pending" ]; then
      printf '%s\n' "$id"
      return 0
    fi
  done
  return 1
}

check_state_dir
[ -n "$TASK_ID" ] || TASK_ID="$(pick_task)" || die "backlog 里没有 pending 任务（$BACKLOG_DIR）"
validate_task_id
TASK_FILE="$BACKLOG_DIR/$TASK_ID.md"
[ -f "$TASK_FILE" ] || die "任务文件不存在：$TASK_FILE"
TASK_FILE_REAL="$(realpath -e -- "$TASK_FILE" 2>/dev/null || true)"
case "$TASK_FILE_REAL" in
  "$BACKLOG_DIR"/*) TASK_FILE="$TASK_FILE_REAL" ;;
  *) die "任务文件不在 backlog 根目录：$TASK_FILE" ;;
esac

task_field() { sed -n "s/^$1: *//p" "$TASK_FILE" | head -1; }
ESCALATION="$(task_field escalation)"
# allow-paths：任务确需触碰保护路径时的放行清单（逗号分隔），由写任务的人负责。
ALLOW_PATHS="$(task_field allow-paths)"
# 标题取 H1（`# T-0000：<标题>` 里冒号后那截），backlog/README.md 的格式里
# 没有 `title:` 字段 —— 取字段会恒为空，PR 标题退化成裸任务 id。
task_title() {
  sed -n '1s/^# *//p' "$TASK_FILE" | sed 's/^T-[0-9]*[：:] *//'
}

# 状态账本在 Git common dir 下，不改 backlog。backlog 是静态任务定义，避免驱动
# 自己制造保护路径脏状态，也避免 --commit-status 把状态直接提交到 dev。
task_state_file() { printf '%s/%s.state\n' "$STATE_DIR" "$TASK_ID"; }

check_task_status

set_status() {
  local st="$1" note="${2:-}"
  local state_file tmp
  check_state_dir
  state_file="$(task_state_file)"
  if [ -L "$state_file" ] || { [ -e "$state_file" ] && [ ! -f "$state_file" ]; }; then
    die "任务状态账本条目不是普通文件：$state_file"
  fi
  tmp="$(mktemp "$STATE_DIR/.${TASK_ID}.state.XXXXXX")" \
    || die "任务状态临时文件创建失败：$STATE_DIR"
  {
    printf 'status: %s\n' "$st"
    printf 'pid: %s\n' "$$"
    printf 'last-run: %s %s %s\n' "$(date -Iseconds)" "$st" "$note"
  } > "$tmp" || { rm -f -- "$tmp"; die "任务状态写入失败：$state_file"; }
  mv -f -- "$tmp" "$state_file" || { rm -f -- "$tmp"; die "任务状态替换失败：$state_file"; }
  log "任务状态 → $st $note"
}

# ── worktree 与分支 ──────────────────────────────────────────────
git check-ref-format --branch "$BASE" >/dev/null 2>&1 \
  || die "基线分支名非法：$BASE"
git fetch -- "$REMOTE" "$BASE" || die "fetch $REMOTE/$BASE 失败"
BASE_COMMIT="$(git rev-parse --verify "$REMOTE/$BASE^{commit}")" \
  || die "基线引用解析失败：$REMOTE/$BASE"
branch="codex/$(echo "$TASK_ID" | tr '[:upper:]' '[:lower:]')"
git check-ref-format --branch "$branch" >/dev/null 2>&1 \
  || die "任务 id 生成的分支名非法：$branch"
n=2
while git ls-remote --exit-code --heads "$REMOTE" "$branch" >/dev/null 2>&1; do
  branch="codex/$(echo "$TASK_ID" | tr '[:upper:]' '[:lower:]')-r$n"
  n=$((n + 1))
done
# 本地同名分支有未推送提交时拒绝重建 —— `-B` 会静默重置指针，
# 把上一轮没推出去的提交直接抹掉（数据丢失，无警告）。
if git show-ref --verify --quiet "refs/heads/$branch"; then
  local_branch_head="$(git rev-parse --verify "$branch^{commit}" 2>/dev/null || true)"
  [ "$local_branch_head" = "$BASE_COMMIT" ] \
    || die "本地分支 $branch 已存在且不等于固定基线 $BASE_COMMIT —— 先人工处理，不静默覆盖"
fi

wt="$WT_ROOT/$TASK_ID"
case "$wt" in
  "$WT_ROOT"/*) ;;
  *) die "worktree 路径逃逸预期根目录：$wt" ;;
esac
if [ -e "$wt" ] || [ -L "$wt" ]; then
  [ -f "$wt/.git" ] && [ ! -L "$wt/.git" ] \
    || die "已有路径不是可安全复用的任务 worktree：$wt"
  [ -z "$(git -C "$wt" status --porcelain=v1 --untracked-files=all 2>/dev/null)" ] \
    || die "已有任务 worktree 含未提交残局，拒绝删除：$wt"
  git worktree remove "$wt" 2>/dev/null \
    || die "旧任务 worktree 清理失败，保留现场：$wt"
fi
git worktree add -B "$branch" "$wt" "$BASE_COMMIT" >/dev/null || die "worktree 创建失败"
log "worktree：$wt（分支 $branch，基于固定基线 $BASE_COMMIT）"

# worktree 的 `.git` 文件与 common dir 元数据决定后续 `git -C "$wt"`
# 实际读哪一个仓库。执行器可以改磁盘上的这个指针；一旦被切到另一棵仓库，
# 保护路径检查、HEAD 计数和卡口工作区都会失去共同信任根。驱动只允许使用
# `git worktree add` 生成的固定元数据，执行器期间发生任何变化都强制升级。
WORKTREE_GIT_DIR="$(git -C "$wt" rev-parse --git-dir 2>/dev/null)" \
  || die "无法解析任务 worktree 的 git dir"
case "$WORKTREE_GIT_DIR" in
  /*) ;;
  *) WORKTREE_GIT_DIR="$wt/$WORKTREE_GIT_DIR" ;;
esac
WORKTREE_GIT_DIR="$(realpath -e -- "$WORKTREE_GIT_DIR" 2>/dev/null || true)"
[ -n "$WORKTREE_GIT_DIR" ] || die "任务 worktree 的 git dir 不存在"
WORKTREE_GITDIR_FILE="$WORKTREE_GIT_DIR/gitdir"
WORKTREE_COMMONDIR_FILE="$WORKTREE_GIT_DIR/commondir"

worktree_git_metadata_fingerprint() {
  local git_file="$wt/.git" path
  [ -f "$git_file" ] && [ ! -L "$git_file" ] \
    || return 1
  printf 'worktree-git-file\t%s\t%s\n' \
    "$(stat -c '%a %u %g' -- "$git_file" 2>/dev/null)" \
    "$(sha256sum -- "$git_file" 2>/dev/null | awk '{print $1}')"
  for path in "$WORKTREE_GITDIR_FILE" "$WORKTREE_COMMONDIR_FILE"; do
    [ -f "$path" ] && [ ! -L "$path" ] || return 1
    printf 'worktree-meta\t%s\t%s\t%s\n' \
      "$(printf '%q' "$path")" \
      "$(stat -c '%a %u %g' -- "$path" 2>/dev/null)" \
      "$(sha256sum -- "$path" 2>/dev/null | awk '{print $1}')"
  done
  git -C "$wt" rev-parse --show-toplevel >/dev/null 2>&1 || return 1
  git -C "$wt" rev-parse --git-common-dir >/dev/null 2>&1 || return 1
}

check_worktree_git_metadata() {
  local current
  current="$(worktree_git_metadata_fingerprint)" \
    || die "任务 worktree 的 Git 元数据不可验证 —— 不放行，先修环境"
  [ "$current" = "$WORKTREE_GIT_METADATA_SNAPSHOT" ]
}

WORKTREE_GIT_METADATA_SNAPSHOT="$(worktree_git_metadata_fingerprint)" \
  || die "无法建立任务 worktree 的 Git 元数据快照"

# ── 环境：库、隔离 target、卡口用的 compose 目录 ─────────────────
# worktree 里起 compose 会另开项目名撞端口 —— 一律用主仓的服务
export TGM_COMPOSE_DIR="$PRIMARY"
# 卡口 helper 的完整性基准（ai-gates.sh 的 trusted_helper 用）：三个 helper
# 从 worktree 跑（要读任务源码），执行前与这个 ref 里的版本逐字节比对。
# 现在就解析成 SHA —— 中途基线移动不该让基准换人。
export TGM_GATES_BASE_REF="$BASE_COMMIT"
# 卡口执行器自身优先取任务基线的原始 blob，不取主仓工作区副本：执行器可写
# worktree，不能让它通过绝对路径改写主仓的 ai-gates.sh 后再验收。首次把驱动
# 本身引入基线时，base 还没有该脚本，允许从当前驱动提交 bootstrap 一次；
# 脚本随后立即读入内存，执行器不能再改写它。
GATES_REF="$TGM_GATES_BASE_REF"
git cat-file -e "$GATES_REF:scripts/ai-driver/ai-gates.sh" \
  || die "任务基线没有可信卡口脚本；先合入驱动基线，再运行任务"
GATES_SCRIPT="$(git show "$GATES_REF:scripts/ai-driver/ai-gates.sh")" \
  || die "读取可信卡口脚本失败：$GATES_REF"
[ -n "$GATES_SCRIPT" ] || die "可信卡口脚本为空：$GATES_REF"
TARGET_ROOT="$COMMON_GIT_DIR/ai-driver-target/$TASK_ID"
EXECUTOR_TARGET_DIR="$TARGET_ROOT/executor"
GATES_TARGET_DIR=""
TRUSTED_ROOT="$TARGET_ROOT/trusted-source"
TRUSTED_TARGET_DIR="$TARGET_ROOT/trusted-target"
mkdir -p -- "$EXECUTOR_TARGET_DIR" "$TRUSTED_ROOT" "$TRUSTED_TARGET_DIR" \
  || die "隔离 target 目录创建失败：$TARGET_ROOT"
git archive --format=tar "$BASE_COMMIT" | tar -xf - -C "$TRUSTED_ROOT" \
  || die "可信源码快照创建失败：$BASE_COMMIT"
private_dir "$TRUSTED_ROOT" "可信源码根目录"
private_dir "$TRUSTED_TARGET_DIR" "可信 target 目录"
TRUSTED_SOURCE_SNAPSHOT="$(fingerprint_path_tree "$TRUSTED_ROOT" "可信源码")" \
  || die "无法建立可信源码快照"
require_systemd_scope
export CARGO_TARGET_DIR="$EXECUTOR_TARGET_DIR"
EXECUTOR_GIT_USER_NAME="ai-driver"
EXECUTOR_GIT_USER_EMAIL="ai-driver@invalid"

# dry-run 不起库：它只生成提示词，不需要环境 ——
# 起库失败不该挡在「看一眼提示词」的路上。
if [ "$DRY" = 0 ]; then
  run_compose up -d --wait || die "compose 起不来（见 eng/02 §3.2：down -v 重建）"
  psql "$DRIVER_OWNER_DB_URL" -c 'SELECT 1' >/dev/null 2>&1 || die "PG 不可达"
  validate_task_database
  # compose 已在执行器启动前完成；卡口阶段不要再次读取可被执行器改写的
  # 主仓 compose.yaml。若服务后来被执行器停掉，数据库卡口会直接报连接失败。
fi

# ── 提示词 ───────────────────────────────────────────────────────
TS="$(date +%Y%m%d-%H%M%S)"
PROMPT_FILE="$LOG_DIR/$TASK_ID.$TS.prompt.md"
RUN_LOG="$LOG_DIR/$TASK_ID.$TS.log"

build_prompt() {
  local gate_feedback="${1:-}"
  {
    cat <<EOF
你是本仓库的自主执行者。仓库根的 AGENTS.md 是你的入口约定（实现纪律、卡口、
升级条件都在那里），先读它再动手。

当前任务（任务文件全文如下）：

---
$(cat "$TASK_FILE")
---

执行流程：
1. 按 AGENTS.md §一 的顺序读必要文档，确认任务引用的判据原文。
2. 在仓库内实现。遵守 AGENTS.md §四 的实现纪律。
   写或改任何中文散文（docs/、提案、commit body）时，先读
   scripts/ai-driver/refs/humanizer-zh.md 再动笔 —— 那是本仓的中文写作事实源。
3. 自查用 scripts/ai-driver/ai-gates.sh --static-only（静态卡口：fmt / clippy /
   离线构建 / deny / newtype）。数据库卡口（迁移 / sqlx / 测试 / 审计 / 双遍）
   需要受限与 owner 连接串，你的环境里没有 —— 那部分由驱动在提交后亲自重跑并裁决。
   **不得放宽任何卡口**；若你认为某条卡口与任务冲突，走升级流程，不要改卡口。
4. 提交前对实现 diff 做一遍 AGENTS.md §4.4 的 ponytail-review；
   按 §六（caveman-commit）风格提交，一任务一提交。
5. 留下的 \`ponytail:\` 注释登记进 docs/plan/ponytail-debt.md 台账（§4.4）。
6. 输出 AGENTS.md §八 的 [report] 块作为最后一段输出。
EOF
    if [ -n "$gate_feedback" ]; then
      cat <<EOF

这是修复回合：上一轮结束后驱动脚本亲自跑了卡口，仍然失败。失败输出尾部如下：

\`\`\`
$gate_feedback
\`\`\`

定位原因、修复，重跑 scripts/ai-driver/ai-gates.sh --static-only 自查静态卡口
（数据库卡口由驱动重跑裁决，你的环境里没有连接串），追加提交、输出 [report]。
EOF
    fi
    if [ "$ESCALATION" = "yes" ]; then
      cat <<EOF

注意：本任务在 backlog 里标记为 escalation: yes —— 它预期会命中
AGENTS.md §五 的升级条件（如需要真机、触碰安全面或基线文档）。
预期路径是：完成你能安全完成的部分（如设计、骨架、测试规格），
把剩余部分写成提案 docs/plan/proposals/$TASK_ID.md 并提交，
[report] 里 path: escalated。不要为凑 path: ok 而越界。
EOF
    else
      cat <<EOF

硬边界（命中其一立即走升级流程，见 AGENTS.md §五）：新建或修改表结构与迁移、
改动安全面（RLS/GRANT/审计断言/卡口/ci.yml）、需要写新 ADR、需要 Telegram 真机。
升级时把提案写到 docs/plan/proposals/$TASK_ID.md，提交后 [report] 里 path: escalated。
EOF
    fi
  } | safe_write_log "$PROMPT_FILE"
}

executor_env() {
  local unit="$1"
  shift
  local -a env_args service_args
  local runtime_home runtime_config runtime_cargo runtime_rustup rust_bin node_bin fixed_path
  local pgdg_lib pgdg_bin executor_codex executor_claude f
  runtime_home="$TARGET_ROOT/executor-home"
  runtime_config="$runtime_home/config"
  runtime_cargo="$runtime_home/cargo"
  runtime_rustup="${RUSTUP_HOME:-$HOME/.rustup}"
  mkdir -p -- "$runtime_config" "$runtime_cargo" || die "执行器临时 HOME 创建失败"
  rust_bin="$(dirname "$(rustup which rustc)")" || die "无法解析 Rust 工具链目录"
  node_bin="$(dirname "$(command -v node)")" || die "无法解析 Node 工具目录"
  # pgdg 路径从驱动的真实 HOME 推导（不是执行器的临时 HOME）：
  # env -i 会丢掉 LD_LIBRARY_PATH，没有它 psql 加载不了 libpq.so.5
  # （实测 rc=127）—— 执行器按提示词自跑 ai-gates.sh 时会死在这里。
  # 只放 lib 目录的真实二进制：usr/bin 里是 perl 包装（需要集群配置）。
  pgdg_lib="${TGM_PGDG_LIB_DIR:-$HOME/opt/pgdg17/usr/lib/x86_64-linux-gnu}"
  pgdg_bin="${TGM_PGDG_BIN_DIR:-$HOME/opt/pgdg17/usr/lib/postgresql/17/bin}"
  # 宿主 ~/.cargo/bin 放的是 cargo install 装的子命令（cargo-deny、
  # cargo-sqlx）—— 执行器的 CARGO_HOME 是临时目录，不带这些插件，
  # 静态卡口的 deny 会 command not found（实测）。工具链本体的
  # fmt/clippy 在 $rust_bin 里，不走这里。
  fixed_path="/usr/bin:/bin:$rust_bin:$pgdg_bin:$node_bin:$HOME/.cargo/bin"
  # 执行器认证凭据：最小复制。codex 只需 auth.json + config.toml（实测
  # env -i 下认证成功）；不挂整个 CODEX_HOME —— 那是会话与历史的 1G+，
  # 全量暴露违反最小凭据原则。缺失时不 die：--dry-run 与无凭据环境
  # 不该挡在提示词生成的路上，认证失败会在执行器退出码里暴露。
  executor_codex="$runtime_home/.codex"
  if [ -d "$HOME/.codex" ]; then
    mkdir -p -- "$executor_codex" || die "执行器 .codex 目录创建失败"
    for f in auth.json config.toml codex-models.json; do
      if [ -f "$HOME/.codex/$f" ]; then
        cp -- "$HOME/.codex/$f" "$executor_codex/$f" || die "执行器凭据复制失败：$f"
      fi
    done
  fi
  executor_claude="$runtime_home/.claude"
  if [ -d "$HOME/.claude" ]; then
    mkdir -p -- "$executor_claude" || die "执行器 .claude 目录创建失败"
    for f in .claude.json settings.json; do
      if [ -f "$HOME/.claude/$f" ]; then
        cp -- "$HOME/.claude/$f" "$executor_claude/$f" || die "执行器凭据复制失败：$f"
      fi
    done
  fi
  env_args=(
    "HOME=$runtime_home"
    "XDG_CONFIG_HOME=$runtime_config"
    "CARGO_HOME=$runtime_cargo"
    "RUSTUP_HOME=$runtime_rustup"
    "PATH=$fixed_path"
    "LD_LIBRARY_PATH=$pgdg_lib"
    "USER=ai-driver"
    "LOGNAME=ai-driver"
    "SHELL=/bin/bash"
    "TERM=dumb"
    "LANG=C.UTF-8"
    "LC_ALL=C.UTF-8"
    "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-}"
    "DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-}"
    "CARGO_TARGET_DIR=$EXECUTOR_TARGET_DIR"
    "TGM_COMPOSE_DIR=$PRIMARY"
    "GH_CONFIG_DIR=$runtime_config/gh"
    "GIT_TERMINAL_PROMPT=0"
    "GIT_SSH_COMMAND=/bin/false"
    "SSH_AUTH_SOCK="
    "GIT_CONFIG_NOSYSTEM=1"
    "GIT_CONFIG_GLOBAL=/dev/null"
    "GIT_CONFIG_SYSTEM=/dev/null"
    "GIT_CONFIG_COUNT=2"
    "GIT_CONFIG_KEY_0=user.name"
    "GIT_CONFIG_VALUE_0=$EXECUTOR_GIT_USER_NAME"
    "GIT_CONFIG_KEY_1=user.email"
    "GIT_CONFIG_VALUE_1=$EXECUTOR_GIT_USER_EMAIL"
    "GIT_NO_REPLACE_OBJECTS=1"
  )
  [ -z "$EXECUTOR_DATABASE_URL" ] || env_args+=("DATABASE_URL=$EXECUTOR_DATABASE_URL")
  service_args=(/usr/bin/env -i "${env_args[@]}" /bin/bash -c \
    'exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-; exec "$@"' _ "$@")
  systemd-run --user --unit="$unit" --collect --wait --pipe \
    --property=KillMode=control-group --working-directory="$wt" \
    "${service_args[@]//\$/\$\$}"
}

run_executor() {
  local unit runner_pid watchdog_pid rc timeout_marker
  local -a command
  case "$EXECUTOR" in
    codex)
      command=("$(command -v codex)" exec --dangerously-bypass-approvals-and-sandbox -C "$wt" "$(cat "$PROMPT_FILE")")
      ;;
    claude)
      command=("$(command -v claude)" -p --dangerously-skip-permissions)
      ;;
    *) die "未知执行器：$EXECUTOR（codex | claude）" ;;
  esac
  unit="ai-driver-$$-$RANDOM.service"
  timeout_marker="$RUN_LOG.timeout"
  safe_log_file "$RUN_LOG"
  rm -f -- "$timeout_marker"
  # 不用 --property=ReadOnlyPaths：systemd 249 用户会话实测不生效
  # （--scope 形态报 Unknown assignment；--unit 形态属性解析为空、
  # 写入照样成功）。假防线比没有防线更危险 —— 它让审计以为保护在场。
  # 可信源码的完整性由执行器退出后的 check_trusted_source_snapshot
  # 逐字节指纹复查保证（主循环与卡口前后各复查一次），篡改即强制升级。
  executor_env "$unit" "${command[@]}" \
    < "$PROMPT_FILE" > "$RUN_LOG" 2>&1 &
  runner_pid="$!"
  (
    exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-
    sleep "$TIMEOUT"
    if kill -0 "$runner_pid" 2>/dev/null; then
      printf '%s\n' timeout >"$timeout_marker"
      printf '[driver] 执行器超时，终止 systemd service：%s\n' "$unit" >>"$RUN_LOG"
      systemctl --user stop "$unit" >/dev/null 2>&1 || true
      systemctl --user kill --kill-who=all --signal=KILL "$unit" >/dev/null 2>&1 || true
    fi
  ) &
  watchdog_pid="$!"
  wait "$runner_pid"; rc="$?"
  kill -TERM "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  wait_scope_empty "$unit" || die "执行器结束后 cgroup 未清空：$unit"
  if [ -f "$timeout_marker" ]; then
    rm -f -- "$timeout_marker"
    cat "$RUN_LOG"
    return 124
  fi
  cat "$RUN_LOG"
  return "$rc"
}

# agent 可能留下未提交的残局 —— 留一份可查的快照，但**不动工作区**：
# `stash create -u` 只造提交对象，不改工作区（与 `stash push` 的区别）。
# 记进本地 ref（refs/ai-leftovers/），不推远端；也不像 `git add -A` 那样
# 把临时文件（.env、dump、日志）塞进 PR。
#
# 实测教训（这里曾经跟着一对 `checkout -- .` + `clean -fd`）：
#   1 agent 写了实现却没 commit 时，那两条把实现删回基线 —— 卡口于是跑在
#     干净的 $REMOTE/$BASE 上必绿，配合 agent 自述 path: ok 就成了
#     「零提交的 done」（实测 ahead=0 也能走到 set_status done）；
#   2 未跟踪文件被 clean 掉后，check_protected 的「未跟踪」那条来源恒为空 ——
#     往 scripts/ 里塞新脚本的形状检查不到。
# 现在残局留在工作区：卡口照它跑，保护路径检查看得见它。
record_leftovers() {
  local sha
  [ -n "$(git -C "$wt" status --porcelain)" ] || return 0
  # 残局只有未跟踪文件时 `stash create -u` 输出空（实测 git 2.34）—— 此时没有快照，
  # 而下一轮取件会 `worktree remove --force` 把残局连 worktree 一起抹掉，
  # 「残局留在 worktree 里」并不构成兜底。兜底：全部入暂存区后不带 -u 再造一次
  # （实测 2.34：快照完整含未跟踪文件）。暂存区被动没有副作用：卡口与检查
  # 都在本函数之后跑，读的是工作区与提交树。
  sha="$(git -C "$wt" stash create -u 2>/dev/null)"
  if [ -z "$sha" ]; then
    git -C "$wt" add -A
    sha="$(git -C "$wt" stash create 2>/dev/null)"
    # 拍完快照把索引还原：add -A 会把未跟踪文件吞进索引，
    # check_protected 的未跟踪来源（ls-files --others）就看不见它们了（实测），
    # 而本函数跑在保护路径检查之前。reset 不动工作区，残局原样留着。
    git -C "$wt" reset -q
  fi
  [ -n "$sha" ] || { log "✗ 残局快照失败（stash create 输出为空）—— 残局仍在 $wt，人工检查"; return 0; }
  git -C "$wt" update-ref "refs/ai-leftovers/$TASK_ID-$TS" "$sha"
  log "未提交残局已快照到 refs/ai-leftovers/$TASK_ID-$TS（查看：git -C $wt stash show -p $sha）"
}

# 卡口的最终裁决由驱动亲自跑，不信 agent 自述
run_gates() {
  local unit runner_pid watchdog_pid rc timeout_marker
  local -a env_args service_args
  unit="ai-driver-gates-$$-$RANDOM.service"
  timeout_marker="$LOG_DIR/$TASK_ID.$TS.gates.timeout"
  rm -f -- "$timeout_marker"
  # 通过 stdin 流式执行已锁定提交里的原始脚本，既不执行主仓工作区副本，
  # 也不把一份可被执行器改写的临时脚本留在 worktree。脚本自身仍在 wt
  # 目录运行，因此 cargo、迁移与 helper 读取的是待验收的任务树。
  (
    cd "$wt" || exit 1
    [ -n "$GATES_TARGET_DIR" ] || exit 1
    [ -n "$TASK_DATABASE_URL" ] || { echo "[driver] ✗ 缺少 TGM_DB_URL_TEST，拒绝向任务卡口注入 owner 连接串" >&2; exit 1; }
    env_args=(
      "HOME=$HOME"
      "PATH=$PATH"
      "LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}"
      "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}"
      "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}"
      "RUSTFLAGS=${RUSTFLAGS:-}"
      "LANG=C.UTF-8"
      "LC_ALL=C.UTF-8"
      "GIT_CONFIG_NOSYSTEM=1"
      "GIT_CONFIG_GLOBAL=/dev/null"
      "GIT_CONFIG_SYSTEM=/dev/null"
      "GIT_CONFIG_COUNT=0"
      "GIT_NO_REPLACE_OBJECTS=1"
      "GIT_TERMINAL_PROMPT=0"
      "TGM_TRUSTED_ROOT=$TRUSTED_ROOT"
      "TGM_TRUSTED_ROOT_LOCK=$TRUSTED_ROOT"
      "TGM_TRUSTED_TARGET_DIR=$TRUSTED_TARGET_DIR"
      "TGM_TRUSTED_TARGET_DIR_LOCK=$TRUSTED_TARGET_DIR"
      "TGM_TASK_TARGET_DIR=$GATES_TARGET_DIR/task"
      "TGM_TASK_TARGET_DIR_LOCK=$GATES_TARGET_DIR/task"
      "TGM_COMPOSE_DIR=$PRIMARY"
      "TGM_COMPOSE_ROOT_LOCK=$PRIMARY"
      "TGM_GATES_ROOT=$wt"
      "TGM_GATES_BASE_REF=$BASE_COMMIT"
      "TGM_TRUSTED_OWNER_DB_URL=$DRIVER_OWNER_DB_URL"
      "TGM_TEST_DATABASE_URL=$TASK_DATABASE_URL"
      "TGM_TASK_SCHEMA_DATABASE_URL=$TASK_SCHEMA_DATABASE_URL"
      "TGM_TASK_SCHEMA_OWNER_DATABASE_URL=$TASK_SCHEMA_OWNER_DATABASE_URL"
    )
    mkdir -p -- "$GATES_TARGET_DIR/task" || exit 1
    exec 9>&-
    service_args=(/usr/bin/env -i "${env_args[@]}" /bin/bash -c \
      'exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-; exec /bin/bash -s')
    systemd-run --user --unit="$unit" --collect --wait --pipe \
      --property=KillMode=control-group --working-directory="$wt" \
      "${service_args[@]//\$/\$\$}" \
      <<< "$GATES_SCRIPT"
  ) 2>&1 &
  runner_pid="$!"
  (
    exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-
    sleep "$TIMEOUT"
    if kill -0 "$runner_pid" 2>/dev/null; then
      printf '%s\n' timeout >"$timeout_marker"
      printf '[driver] 卡口超时，终止 systemd service：%s\n' "$unit" >&2
      systemctl --user stop "$unit" >/dev/null 2>&1 || true
      systemctl --user kill --kill-who=all --signal=KILL "$unit" >/dev/null 2>&1 || true
    fi
  ) &
  watchdog_pid="$!"
  wait "$runner_pid"; rc="$?"
  kill -TERM "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  wait_scope_empty "$unit" || die "卡口结束后 cgroup 未清空：$unit"
  if [ -f "$timeout_marker" ]; then
    rm -f -- "$timeout_marker"
    return 124
  fi
  return "$rc"
}

prepare_gates_target() {
  GATES_TARGET_DIR="$(mktemp -d "$COMMON_GIT_DIR/ai-driver-gates.XXXXXX")" \
    || die "卡口 target 目录创建失败"
}

discard_gates_target() {
  local target="$GATES_TARGET_DIR"
  GATES_TARGET_DIR=""
  [ -n "$target" ] || return 0
  rm -rf -- "$target" || die "卡口 target 目录清理失败：$target"
}

# 取最后一个 [report] 块，不用行数窗口（-A 6 会被多写的一行截断）。
# 块在空行或 ``` 处结束。
# 块头用前缀匹配而不是全等：实测 `[report]` 后多一个空格，全等就取不到 path，
# 于是落到 blocked —— 执行器多打一个空格不该改变任务结局。
report_field() {
  awk -v key="$1" '
    { lines[NR]=$0; if (index($0, "[report]") == 1) last=NR }
    END {
      if (!last) exit
      for (j=last+1; j<=NR; j++) {
        l=lines[j]
        if (l ~ /^```/ || l ~ /^[[:space:]]*$/) break
        if (index(l, key ": ") == 1) { print substr(l, length(key)+3); exit }
      }
    }' "$RUN_LOG"
}

if [ "$DRY" = 1 ]; then
  build_prompt "" || die "dry-run 提示词生成失败"
  log "dry-run：提示词已生成 → $PROMPT_FILE（未执行）"
  exit 0
fi

# ── git 配置层指纹：执行器跑之前快照，每轮卡口前比对 ─────────────
# 执行器能让 git「说谎」的通道全在配置层，而这些配置在任何树视图之外
# （check_protected 的提交树与工作区视图看不见）：
#   1 clean/smudge filter：hash-object 与工作区 diff 都会应用它，磁盘上的
#     恶意内容能被转成良性字节（实测：吐出基线内容的 filter 骗过
#     trusted_helper 旧版）；
#   2 core.hooksPath 重定向：hooks 生效目录被指到别处；
#   3 .git/hooks 本身：执行器写进去就是后续一切 git 操作的持久化后门
#     （实测 worktree 与主仓共享 hooks 目录）。
# 共同点：1 和 2 都必须写配置才成立，3 要动 hooks 目录。处置：执行器跑之前
# 对三者取内容指纹，每轮卡口前比对，变了即强制升级 —— 不逐通道打补丁，
# 守住「让 git 说谎」这一类。新鲜 worktree 没有合法理由动配置。
#
# hooksPath 每轮重新解析（--git-path hooks 会跟着配置走），并记录生效目录的
# 类型、权限、链接目标和文件内容，避免「旧目录没变、生效目录已被换掉」或
# 「内容没变、权限/链接变了」的形状漏掉。
# 属性文件（.git/info/attributes）决定 filter 是否生效，必须独立纳入指纹。
# 系统/全局配置也纳入指纹；执行器会被置为干净的配置环境，驱动自身的 Git
# 操作仍不能把宿主配置留在验收视图之外。
config_file_signature() {
  local path="$1" label="$2" target resolved mode uid gid hash
  if [ ! -e "$path" ] && [ ! -L "$path" ]; then
    printf '%s-missing\t%s\n' "$label" "$(printf '%q' "$path")"
    return 0
  fi
  [ -f "$path" ] || {
    echo "[driver] ✗ $label 不是普通文件：$path" >&2
    return 1
  }
  mode="$(stat -c '%a' -- "$path")" || return 1
  uid="$(stat -c '%u' -- "$path")" || return 1
  gid="$(stat -c '%g' -- "$path")" || return 1
  hash="$(sha256sum -- "$path" | awk '{print $1}')" || return 1
  target=""
  if [ -L "$path" ]; then
    target="$(readlink -- "$path")" || return 1
    resolved="$(realpath -e -- "$path")" || return 1
    [ -f "$resolved" ] || return 1
  fi
  printf '%s\t%s %s %s\t%s\t%s\t%s\n' \
    "$label" "$mode" "$uid" "$gid" "$(printf '%q' "$path")" \
    "$(printf '%q' "$target")" "$hash"
}

check_config_include_files() {
  local config="$1" base_dir include_lines line key value include_path resolved rc
  [ -f "$config" ] || return 0
  base_dir="$(dirname -- "$config")" || return 1
  if include_lines="$(git config --file "$config" --no-includes --get-regexp '^include' 2>/dev/null)"; then
    :
  else
    rc=$?
    [ "$rc" -eq 1 ] || return 1
    include_lines=""
  fi
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    key="${line%%[[:space:]]*}"
    value="${line#*[[:space:]]}"
    [[ "$key" == *\.path ]] || continue
    include_path="$value"
    case "$include_path" in
      ~/*) resolved="$HOME/${include_path#~/}" ;;
      /*) resolved="$include_path" ;;
      *) resolved="$base_dir/$include_path" ;;
    esac
    config_file_signature "$resolved" "included-config" >/dev/null || return 1
  done <<< "$include_lines"
}

read_config_values() {
  local config="$1" label="$2" values
  config_file_signature "$config" "$label-file" || return 1
  [ -f "$config" ] || {
    printf '%s\tmissing\n' "$label"
    return 0
  }
  check_config_include_files "$config" || return 1
  values="$(timeout --preserve-status 5s git config --file "$config" --includes --list --show-origin)" \
    || { echo "[driver] ✗ $label 读取超时或失败：$config" >&2; return 1; }
  printf '%s\t%s\n' "$label" "$(printf '%s' "$values" | sha256sum | awk '{print $1}')"
}

git_guard_fingerprint() {
  local config_path attr_path hooks_path wt_values primary_values
  wt_values="$(timeout --preserve-status 5s git -C "$wt" config --local --includes --list)" \
    || { echo "[driver] ✗ worktree Git 配置读取超时或失败" >&2; return 1; }
  primary_values="$(timeout --preserve-status 5s git -C "$PRIMARY" config --local --includes --list)" \
    || { echo "[driver] ✗ 主仓 Git 配置读取超时或失败" >&2; return 1; }
  printf 'worktree-local-config\t%s\n' "$(printf '%s' "$wt_values" | LC_ALL=C sort | sha256sum | awk '{print $1}')"
  printf 'primary-local-config\t%s\n' "$(printf '%s' "$primary_values" | LC_ALL=C sort | sha256sum | awk '{print $1}')"

  read_config_values "$HOST_GIT_CONFIG_GLOBAL" global-config || return 1
  read_config_values "$HOST_GIT_CONFIG_SYSTEM" system-config || return 1

  for config_path in \
    "$(git -C "$wt" rev-parse --git-path config)" \
    "$(git -C "$PRIMARY" rev-parse --git-path config)"; do
    case "$config_path" in /*) ;; *) config_path="$PRIMARY/$config_path" ;; esac
    config_file_signature "$config_path" local-config || return 1
    check_config_include_files "$config_path" || return 1
  done

  for attr_path in \
    "$(git -C "$wt" rev-parse --git-path info/attributes)" \
    "$(git -C "$PRIMARY" rev-parse --git-path info/attributes)"; do
    case "$attr_path" in /*) ;; *) attr_path="$PRIMARY/$attr_path" ;; esac
    config_file_signature "$attr_path" attributes-file || return 1
  done

  hooks_path="$(git -C "$wt" rev-parse --git-path hooks)" || return 1
  case "$hooks_path" in /*) ;; *) hooks_path="$wt/$hooks_path" ;; esac
  [ ! -L "$hooks_path" ] && { [ ! -e "$hooks_path" ] || [ -d "$hooks_path" ]; } \
    || return 1
  printf 'hooks-path\t%s\n' "$(printf '%q' "$hooks_path")"
  fingerprint_path_tree "$hooks_path" "生效 hooks 目录" || return 1
}

GIT_GUARD_SNAPSHOT="$(git_guard_fingerprint)"

# ── 主循环：首轮 + 至多 MAX_RETRIES 次卡口修复回合 ───────────────
check_primary_start_clean
set_status active "(driver)"
STATE_GUARD_SNAPSHOT="$(state_fingerprint)" \
  || die "无法建立驱动状态账本快照 —— 不放行，先修环境"
build_prompt "" || die "首轮提示词生成失败，拒绝启动执行器"
path=""
protected_hit=""
VALIDATED_HEAD=""
PRIMARY_GUARD_SNAPSHOT="$(primary_guard_fingerprint)" \
  || die "无法建立主仓保护路径快照 —— 不放行，先修环境"
PRIMARY_HEAD_SNAPSHOT="$(git -C "$PRIMARY" rev-parse HEAD 2>/dev/null)" \
  || die "无法建立主仓 HEAD 快照 —— 不放行，先修环境"
PRIMARY_INDEX_SNAPSHOT="$(primary_guard_index)" \
  || die "无法建立主仓索引快照 —— 不放行，先修环境"
log "执行器：$EXECUTOR，超时 ${TIMEOUT}s，日志：$RUN_LOG"
if run_executor; then
  executor_ok=1
else
  executor_ok=0
  log "执行器退出码非 0，产出只能进入 blocked，仍继续做完整检查"
fi
if ! check_trusted_source_snapshot; then
  log "可信源码快照在执行器期间被改动 —— 强制升级（不跑卡口）"
  path="protected"
  protected_hit="可信源码快照被执行器改动"
fi
if ! check_worktree_git_metadata; then
  log "任务 worktree 的 Git 元数据在执行器期间被改动 —— 强制升级（不跑卡口）"
  path="protected"
  protected_hit="任务 worktree 的 Git 元数据被执行器改动"
fi
git_guard_now="$(git_guard_fingerprint)" \
  || die "执行器后 Git 配置层检查失败 —— 不放行，先修环境"
if [ "$git_guard_now" != "$GIT_GUARD_SNAPSHOT" ]; then
  log "执行器期间 Git 配置层被改动 —— 强制升级（不推送为完成）"
  path="protected"
  protected_hit="git 配置层被执行器改动"
fi
if ! check_state_guard; then
  log "执行器期间驱动状态账本被改动 —— 强制升级（不跑卡口）"
  path="protected"
  protected_hit="驱动状态账本被执行器改动"
fi
if ! check_primary_guard; then
  log "主仓保护路径在执行器期间被改动 —— 强制升级（不跑卡口）"
  path="protected"
  protected_hit="主仓保护路径被执行器改动"
fi
record_leftovers

attempt=0
gates_ok=0
# executor_ok 不在这里清零：它记录执行器最近一次的成功与否，首轮成功后
# 的快乐路径靠它放行；清零会让「首轮成功 + 首轮卡口绿」也判
# executor-failed（实测仿真复现）。循环计数器只有 attempt 与 gates_ok。
# path 必须在循环前初始化：它只在保护路径分支里赋值，而 `set -u` 下
# 循环后那句 `[ "$path" != "protected" ]` 读未定义变量会直接杀掉脚本 ——
# 卡口绿也走不到推分支，任务永久停在 active（实测 rc=1，队列卡死）。
while :; do
  if [ "$path" = "protected" ]; then
    break
  fi
  if ! check_trusted_source_snapshot; then
    log "可信源码快照已漂移 —— 强制升级（不跑卡口）"
    path="protected"
    protected_hit="可信源码快照漂移"
    break
  fi
  if ! check_worktree_git_metadata; then
    log "任务 worktree 的 Git 元数据已漂移 —— 强制升级（不跑卡口）"
    path="protected"
    protected_hit="任务 worktree 的 Git 元数据被改动"
    break
  fi
  if ! check_base_unchanged; then
    log "远端基线 $REMOTE/$BASE 已漂移 —— 强制升级（不跑卡口）"
    path="protected"
    protected_hit="远端基线已从 $BASE_COMMIT 漂移"
    break
  fi
  if ! check_state_guard; then
    log "驱动状态账本被改动 —— 强制升级（不跑卡口）"
    path="protected"
    protected_hit="驱动状态账本被改动"
    break
  fi
  # 配置层指纹比对先于一切检查：它在任何树视图之外，只有内容指纹看得见。
  # 变了即升级 —— 变化可能无害（比如人工排障），但机器判据一律保守，
  # 由人看了再放。
  git_guard_now="$(git_guard_fingerprint)"
  if [ "$git_guard_now" != "$GIT_GUARD_SNAPSHOT" ]; then
    log "git 配置层在执行期间被改动（配置 / hooks 生效目录）—— 强制升级（不跑卡口）"
    log "  （clean filter、core.hooksPath、hooks 后门都从这里进来）"
    path="protected"
    protected_hit="git 配置层被改动（执行前指纹与当前不一致）"
    break
  fi
  # 索引隐藏标志先于一切：标志在场时，后面的保护路径检查与卡口后的
  # 状态判据全部失明 —— 先扫标志，再谈检查。
  index_flags="$(check_index_flags)" || die "索引标志检查失败 —— 不放行，先修环境"
  if [ -n "$index_flags" ]; then
    log "发现索引隐藏标志（skip-worktree/assume-unchanged）—— git 视图被篡改，强制升级（不跑卡口）："
    printf '%s' "$index_flags" | sed 's/^/    /'
    log "（被标志隐藏的文件在 git status 里是干净的；查看实际内容：git -C $wt diff / cat 对应文件）"
    path="protected"
    protected_hit="索引隐藏标志: $(printf '%s' "$index_flags" | tr '\n' ' ')"
    break
  fi
  # 保护路径检查先于卡口：命中即升级，不跑卡口。
  # 理由：卡口脚本与审计 SQL 本身在 scripts/、crates/db/src/audit/ 下，
  # 被改过之后跑出来的绿不携带信息（AGENTS.md §五 的机器判据）。
  # die 在 check_protected 里只会杀掉命令替换的子壳 —— 父脚本拿到空字符串，
  # 恰好等于「没有命中」，检查静默放行（实测）。这里接住子壳的退出码：
  # 检查失败就中止，不放行。
  protected_hit="$(check_protected)" || die "保护路径检查失败 —— 不放行，先修环境"
  if [ -n "$protected_hit" ]; then
    log "保护路径被改动，强制升级（不跑卡口）："
    printf '%s' "$protected_hit" | sed 's/^/    /'
    path="protected"
    break
  fi
  GATE_LOG="$LOG_DIR/$TASK_ID.$TS.gates-$attempt.log"
  log "驱动亲自跑卡口（第 $((attempt + 1)) 遍）→ $GATE_LOG"
  prepare_gates_target
  safe_log_file "$GATE_LOG"
  if run_gates > "$GATE_LOG" 2>&1; then
    discard_gates_target
    gates_ok=1
    if ! check_trusted_source_snapshot; then
      log "可信源码快照在卡口期间被改动 —— 强制升级（不推送为完成）"
      path="protected"
      protected_hit="卡口期间可信源码快照被改动"
      break
    fi
    if ! check_worktree_git_metadata; then
      log "卡口期间任务 worktree 的 Git 元数据被改动 —— 强制升级（不推送为完成）"
      path="protected"
      protected_hit="卡口期间任务 worktree 的 Git 元数据被改动"
      break
    fi
    git_guard_now="$(git_guard_fingerprint)" \
      || die "卡口后 Git 配置层检查失败 —— 不放行，先修环境"
    if [ "$git_guard_now" != "$GIT_GUARD_SNAPSHOT" ]; then
      log "卡口期间 Git 配置层被改动 —— 强制升级（不推送为完成）"
      path="protected"
      protected_hit="卡口期间 Git 配置层被改动"
      break
    fi
    if ! check_primary_guard; then
      log "卡口期间主仓保护路径被改动 —— 强制升级（不推送为完成）"
      path="protected"
      protected_hit="卡口期间主仓保护路径被改动"
      break
    fi
    index_flags="$(check_index_flags)" \
      || die "卡口后索引标志检查失败 —— 不放行，先修环境"
    if [ -n "$index_flags" ]; then
      log "卡口期间出现索引隐藏标志 —— 强制升级（不推送为完成）"
      path="protected"
      protected_hit="卡口期间索引隐藏标志: $(printf '%s' "$index_flags" | tr '\n' ' ')"
      break
    fi
    protected_hit="$(check_protected)" \
      || die "卡口后保护路径检查失败 —— 不放行，先修环境"
    if [ -n "$protected_hit" ]; then
      log "卡口期间保护路径被改动 —— 强制升级（不推送为完成）："
      printf '%s' "$protected_hit" | sed 's/^/    /'
      path="protected"
      break
    fi
    VALIDATED_HEAD="$(git -C "$wt" rev-parse HEAD 2>/dev/null)" \
      || die "无法记录卡口验收 HEAD —— 不放行，先修环境"
    break
  fi
  if ! check_trusted_source_snapshot; then
    discard_gates_target
    log "可信源码快照在失败卡口期间被改动 —— 强制升级"
    path="protected"
    protected_hit="失败卡口期间可信源码快照被改动"
    break
  fi
  discard_gates_target
  if [ "$attempt" -ge "$MAX_RETRIES" ]; then
    break
  fi
  log "卡口红，进入修复回合 $((attempt + 1))/$MAX_RETRIES"
  PROMPT_FILE="$LOG_DIR/$TASK_ID.$TS.retry$attempt.prompt.md"
  build_prompt "$(tail -200 "$GATE_LOG")" \
    || die "修复回合提示词生成失败，拒绝启动执行器"
  RUN_LOG="$LOG_DIR/$TASK_ID.$TS.retry$attempt.log"
  if run_executor; then
    executor_ok=1
  else
    executor_ok=0
    log "修复回合执行器退出码非 0，最终产出不能判 done"
  fi
  if ! check_worktree_git_metadata; then
    log "任务 worktree 的 Git 元数据在修复回合期间被改动 —— 强制升级（不跑卡口）"
    path="protected"
    protected_hit="修复回合改动任务 worktree 的 Git 元数据"
    break
  fi
  if ! check_primary_guard; then
    log "主仓保护路径在修复回合期间被改动 —— 强制升级（不跑卡口）"
    path="protected"
    protected_hit="主仓保护路径被执行器改动"
    break
  fi
  if ! check_base_unchanged; then
    log "卡口期间远端基线已漂移 —— 强制升级（不推送为完成）"
    path="protected"
    protected_hit="卡口期间远端基线漂移"
    break
  fi
  if ! check_state_guard; then
    log "卡口期间驱动状态账本被改动 —— 强制升级（不推送为完成）"
    path="protected"
    protected_hit="卡口期间驱动状态账本被改动"
    break
  fi
  record_leftovers
  attempt=$((attempt + 1))
done

[ "$path" != "protected" ] && path="$(report_field path)"
if [ "$path" = "ok" ] && [ "$executor_ok" != 1 ]; then
  log "✗ 执行器最终退出码非 0，不能按 path: ok 完成 —— 判 blocked"
  path="executor-failed"
fi
proposal_rel="docs/plan/proposals/$TASK_ID.md"
proposal="$wt/$proposal_rel"
proposal_committed=0
proposal_mode="$(git -C "$wt" ls-tree HEAD -- "$proposal_rel" | awk 'NR == 1 { print $1 }')"
if [ "$proposal_mode" = 100644 ] && [ -f "$proposal" ] && [ ! -L "$proposal" ]; then
  proposal_committed=1
fi

# escalation: yes 是机器出口约束，不是提示词建议：任务必须带入已提交的提案，
# 否则不能进入 done。未提交的 worktree 文件不能算提案，因为 push 不会携带它。
if [ "$ESCALATION" = "yes" ]; then
  case "$path" in
    ok|""|escalated)
      if [ "$proposal_committed" = 1 ]; then
        path="escalated"
      else
        log "✗ 任务声明 escalation: yes，但没有已提交提案：$proposal_rel —— 判 blocked"
        path="blocked"
      fi
      ;;
  esac
fi
[ -z "$path" ] && [ "$proposal_committed" = 1 ] && path="escalated"

# 零提交不算完成：卡口跑在基线上必绿（基线本来就是绿的），配合 agent 自述
# path: ok 就会推一个空分支、开 PR 失败只打一行日志、状态照样落 done。
# 判据是「分支相对基线有没有提交」—— 机器可核，不依赖 agent 自述。
commits_ahead="$(git -C "$wt" rev-list --count "$BASE_COMMIT..HEAD" 2>/dev/null || echo 0)"
base_tree="$(git -C "$wt" rev-parse "$BASE_COMMIT^{tree}" 2>/dev/null || true)"
head_tree="$(git -C "$wt" rev-parse "HEAD^{tree}" 2>/dev/null || true)"
if [ "$path" = "ok" ] && { [ "$commits_ahead" = 0 ] || [ -z "$base_tree" ] || [ -z "$head_tree" ] || [ "$base_tree" = "$head_tree" ]; }; then
  log "✗ 自述 path: ok 但提交树相对固定基线没有实际变化 —— 判 blocked，不按 ok 处理"
  path="no-commit"
fi

if [ "$path" = "ok" ] && [ "$gates_ok" = 1 ]; then
  current_head="$(git -C "$wt" rev-parse HEAD 2>/dev/null)" \
    || die "无法读取推送 HEAD —— 不放行，先修环境"
  if [ "$current_head" != "$VALIDATED_HEAD" ]; then
    log "✗ 卡口通过后 HEAD 已变化 —— 判 blocked，不推送为完成"
    path="validated-head-changed"
  fi
fi

# 判据跑在工作区、推送的是提交树：执行器提交了坏代码、只在工作区里修好时，
# 卡口绿验证的是工作区，PR 携带的却是坏的 HEAD 树 —— 绿与交付物不是同一棵树。
# path: ok 要求两棵树一致（工作区干净）；其余出口本来就推分支保现场，不受此限。
# 实测：工作区比对与提交树比对在「已提交又还原」形状下输出一空一非空。
dirty_block=0
if [ "$path" = "ok" ] && [ -n "$(git -C "$wt" status --porcelain)" ]; then
  log "✗ 自述 path: ok 但工作区不干净 —— 卡口验证的不是被推送的树，判 blocked"
  dirty_block=1
  path="dirty-tree"
fi

# ── 推分支与 PR ─────────────────────────────────────────────────
exit_rc=1
push_and_pr() {
  local title="$1" label="$2" pr_url pr_base_name pr_base_oid pr_head_oid
  local push_head="$VALIDATED_HEAD"
  [ -n "$push_head" ] || push_head="$(git -C "$wt" rev-parse HEAD 2>/dev/null)"
  git_guard_now="$(git_guard_fingerprint)" \
    || { log "推送前 Git 配置层检查失败，拒绝推送"; return 1; }
  [ "$git_guard_now" = "$GIT_GUARD_SNAPSHOT" ] \
    || { log "推送前 Git 配置层已漂移，拒绝推送"; return 1; }
  check_base_unchanged || { log "推送前远端基线已漂移，拒绝推送"; return 1; }
  git -C "$wt" -c core.hooksPath=/dev/null push --atomic \
    --force-with-lease="refs/heads/$branch:" \
    "$REMOTE" \
    "$push_head:refs/heads/$branch" || { log "推送失败"; return 1; }
  check_base_unchanged || { log "推送后远端基线已漂移，拒绝创建 PR"; return 1; }
  if [ "$NO_PR" = 1 ]; then log "--no-pr：分支已推但未创建 PR，任务只能判 blocked"; return 2; fi
  local body_file="$LOG_DIR/$TASK_ID.$TS.pr-body.md"
  {
    echo "任务：**$TASK_ID**（$label）"
    echo
    echo "验收判据出处：见任务文件 \`docs/plan/backlog/$TASK_ID.md\` 的 criteria 字段。"
    echo
    if [ "$path" = "protected" ]; then
      # 这条路径刻意没跑卡口，没有 GATE_LOG 可指 —— 别拿「未全绿」的话术
      # 去描述一次没发生的失败。
      echo "卡口：**刻意未跑**。改动命中保护路径，卡口脚本本身在清单里，"
      echo "改过之后跑出来的绿不携带信息（AGENTS.md §五 的机器判据）。"
    elif [ "$gates_ok" = 1 ]; then
      echo "卡口：驱动脚本本地全绿（scripts/ai-driver/ai-gates.sh），以本 PR 的 CI 为准。"
      if [ "$dirty_block" = 1 ]; then
        echo "注意：工作区不干净 —— 卡口验证的树与分支携带的树不一致，请勿合并。"
      fi
    else
      echo "卡口：**本地未全绿**（见 ../ai-driver-logs/$TASK_ID.$TS.gates-$attempt.log），请勿合并，先看失败原因。"
    fi
    echo
    echo "本 PR 由 AI 驱动循环生成（scripts/ai-driver/ai-run.sh，执行器：$EXECUTOR）。"
  } > "$body_file"
  pr_url="$(cd "$wt" && gh pr create --base "$BASE" --head "$branch" --title "$title" --body-file "$body_file")" || {
    log "gh pr create 失败 —— 分支 $branch 已推，请手工开 PR"
    return 1
  }
  pr_url="$(printf '%s\n' "$pr_url" | tail -1)"
  [ -n "$pr_url" ] || { log "gh pr create 未返回 PR URL，拒绝写完成状态"; return 1; }
  pr_base_name="$(gh pr view "$pr_url" --json baseRefName --jq .baseRefName)" || return 1
  pr_base_oid="$(gh pr view "$pr_url" --json baseRefOid --jq .baseRefOid)" || return 1
  pr_head_oid="$(gh pr view "$pr_url" --json headRefOid --jq .headRefOid)" || return 1
  if [ "$pr_base_name" != "$BASE" ] || [ "$pr_base_oid" != "$BASE_COMMIT" ] || [ "$pr_head_oid" != "$push_head" ]; then
    log "PR 实际引用与固定提交不一致：base=$pr_base_name@$pr_base_oid head=$pr_head_oid"
    return 1
  fi
  log "PR 已创建并核验 base=$BASE@$BASE_COMMIT"
}

# push_and_pr 返回码的记账：0 = 分支已推且 PR 已开；2 = 分支已推但按
# --no-pr 未开 PR（此前 2 落进 else 被记成「推送失败」，归因错误 ——
# --no-pr 明明推送成功了）；其余 = 推送或开 PR 失败。
push_outcome_note() {
  local rc="$1" br="$2"
  case "$rc" in
    2) printf '(--no-pr：分支已推、未开 PR, branch: %s)' "$br" ;;
    *) printf '(推送失败, branch %s 只在本地)' "$br" ;;
  esac
}

# 推送结果必须进状态记账：推送失败而状态落 done，等于状态说完成、
# 远端没有分支也没有 PR —— 人看状态会以为交付了。
case "$path" in
  ok)
    if [ "$gates_ok" = 1 ]; then
      title="$(task_title)"
      # 带任务 id 前缀与另外三条出口同形：PR 列表里一眼能对回 backlog。
      push_rc=0
      push_and_pr "$TASK_ID：${title:-实现完成}" "实现" || push_rc=$?
      if [ "$push_rc" = 0 ]; then
        set_status done "(branch: $branch)"
        exit_rc=0
      else
        set_status blocked "$(push_outcome_note "$push_rc" "$branch")"
      fi
    else
      push_rc=0
      push_and_pr "$TASK_ID：卡口未绿，待人工处理" "失败" || push_rc=$?
      [ "$push_rc" = 0 ] || log "$(push_outcome_note "$push_rc" "$branch")"
      set_status blocked "(卡口未绿, branch: $branch)"
    fi
    ;;
  escalated)
    push_rc=0
    push_and_pr "$TASK_ID：升级提案" "升级提案" || push_rc=$?
    if [ "$push_rc" = 0 ]; then
      set_status escalated "(branch: $branch)"
      exit_rc=0
    else
      set_status blocked "$(push_outcome_note "$push_rc" "$branch")"
    fi
    ;;
  protected)
    push_rc=0
    push_and_pr "$TASK_ID：改动保护路径，强制升级（未跑卡口）" "保护路径" || push_rc=$?
    if [ "$push_rc" = 0 ]; then
      set_status escalated "(branch: $branch, 保护路径: $(printf '%s' "$protected_hit" | tr '\n' ' '))"
      exit_rc=0
    else
      set_status blocked "$(push_outcome_note "$push_rc" "$branch")"
    fi
    ;;
  no-commit)
    log "零提交，不推分支（没有东西可推）"
    set_status blocked "(自述 ok 但零提交)"
    ;;
  dirty-tree)
    push_rc=0
    push_and_pr "$TASK_ID：工作区与提交树不一致，待人工处理" "受阻" || push_rc=$?
    [ "$push_rc" = 0 ] || log "$(push_outcome_note "$push_rc" "$branch")"
    set_status blocked "(自述 ok 但工作区不干净, branch: $branch)"
    ;;
  *)
    push_rc=0
    push_and_pr "$TASK_ID：未完成，待人工处理" "受阻" || push_rc=$?
    [ "$push_rc" = 0 ] || log "$(push_outcome_note "$push_rc" "$branch")"
    set_status blocked "(branch: $branch)"
    ;;
esac

log "完成。worktree 保留在 $wt 供检查；清理：git worktree remove --force $wt"
exit "$exit_rc"
