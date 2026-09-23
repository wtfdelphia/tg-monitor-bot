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

PRIMARY="$(git rev-parse --show-toplevel)"
cd "$PRIMARY"
BACKLOG_DIR="$PRIMARY/docs/plan/backlog"
WT_ROOT="$(dirname "$PRIMARY")/ai-worktrees"
LOG_DIR="$PRIMARY/scripts/ai-driver/logs"
mkdir -p "$LOG_DIR"

EXECUTOR="codex"
TASK_ID=""
BASE="dev"
REMOTE="cloud"
DRY=0
NO_PR=0
MAX_RETRIES=2
TIMEOUT=3600
COMMIT_STATUS=0
ALLOW_PATHS=""

while [ $# -gt 0 ]; do
  case "$1" in
    --task) TASK_ID="$2"; shift 2 ;;
    --executor) EXECUTOR="$2"; shift 2 ;;
    --base) BASE="$2"; shift 2 ;;
    --remote) REMOTE="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --no-pr) NO_PR=1; shift ;;
    --max-retries) MAX_RETRIES="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --commit-status) COMMIT_STATUS=1; shift ;;
    *) echo "未知参数：$1"; exit 2 ;;
  esac
done

log() { echo "[driver] $*"; }
die() { echo "[driver] ✗ $*" >&2; exit 1; }

# psql 不在非交互 shell（cron）的 PATH 里 —— 与 ai-gates.sh 同款兜底。
# 教训出处：eng/00 §零 的实测记录；漏掉它会让库完全正常时也报「PG 不可达」。
if ! command -v psql >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  [ -f "$HOME/opt/pgdg17/env.sh" ] && source "$HOME/opt/pgdg17/env.sh"
fi
command -v psql >/dev/null 2>&1 || die "找不到 psql（source ~/opt/pgdg17/env.sh 或把 pgdg17 的 bin 加进 PATH）"

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
# 驱动在主仓改 status，agent 不碰任务文件，合并就不会撞。
#
# 刻意不在清单里：docs/plan/proposals/（升级路径的产物）、
# docs/plan/ponytail-debt.md（§4.4 要求 agent 登记）—— 提示词点名要 agent 写,
# 圈进来会让每个任务都误判 protected，把 escalated 出口废掉。
PROTECTED=(
  scripts/
  migrations/
  .github/
  crates/db/src/audit/
  AGENTS.md
  CLAUDE.md
  docs/plan/backlog/
  Cargo.toml
  Cargo.lock
  compose.yaml
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

check_protected() {
  local hits="" p f tracked untracked
  # 两条来源：对基线的已跟踪改动 + 未跟踪新文件（git diff 看不见后者，
  # 而往 scripts/ 里塞一个新脚本同样是改卡口）。
  #
  # 不吞 stderr：ref 解析不了时 git 把 fatal 写到 stderr、stdout 为空，
  # 吞掉就等于「检查静默放行」—— 本仓记过多次「验证手段坏掉时表现为通过」。
  for p in "${PROTECTED[@]}"; do
    tracked="$(git -C "$wt" diff --name-only "$REMOTE/$BASE" -- "$p")" \
      || die "保护路径检查失败（git diff $REMOTE/$BASE -- $p）—— 不放行，先修环境"
    # 不带 --exclude-standard：该开关会尊重 .gitignore —— 往 .gitignore 加一行
    # 就能把要藏的文件从检查里抹掉（实测）。.gitignore 也在清单里挡「改规则」
    # 这一层，这里挡「藏文件」那一层；宁可把新增的 ignored 文件也报出来让人看，
    # 方向保守。
    untracked="$(git -C "$wt" ls-files --others -- "$p")" \
      || die "保护路径检查失败（git ls-files -- $p）—— 不放行，先修环境"
    while IFS= read -r f; do
      [ -z "$f" ] && continue
      allowed_path "$f" || hits="$hits$f"$'\n'
    done <<< "$tracked"$'\n'"$untracked"
  done
  printf '%s' "$hits" | sort -u
}

# ── 任务选取 ─────────────────────────────────────────────────────
pick_task() {
  local f
  for f in "$BACKLOG_DIR"/T-*.md; do
    [ -f "$f" ] || continue
    local st
    st="$(sed -n 's/^status: *//p' "$f" | head -1)"
    if [ "$st" = "pending" ]; then
      basename "$f" .md
      return 0
    fi
  done
  return 1
}

[ -n "$TASK_ID" ] || TASK_ID="$(pick_task)" || die "backlog 里没有 pending 任务（$BACKLOG_DIR）"
TASK_FILE="$BACKLOG_DIR/$TASK_ID.md"
[ -f "$TASK_FILE" ] || die "任务文件不存在：$TASK_FILE"

task_field() { sed -n "s/^$1: *//p" "$TASK_FILE" | head -1; }
ESCALATION="$(task_field escalation)"
# allow-paths：任务确需触碰保护路径时的放行清单（逗号分隔），由写任务的人负责。
ALLOW_PATHS="$(task_field allow-paths)"
# 标题取 H1（`# T-0000：<标题>` 里冒号后那截），backlog/README.md 的格式里
# 没有 `title:` 字段 —— 取字段会恒为空，PR 标题退化成裸任务 id。
task_title() {
  sed -n '1s/^# *//p' "$TASK_FILE" | sed 's/^T-[0-9]*[：:] *//'
}

# 回写状态：替换 status 行，追加一行 last-run（幂等替换旧行）。
# 记账只在主仓的 $BASE 上做，任务分支不碰任务文件（backlog/ 在 PROTECTED 里）——
# 两边同时改同一个文件会在合并时冲突，而状态写在未合并的分支上则主仓看不见，
# pick_task 会把同一个任务反复取件（两种都实测过）。
set_status() {
  local st="$1" note="${2:-}"
  sed -i "s/^status: .*/status: $st/" "$TASK_FILE"
  sed -i '/^last-run: /d' "$TASK_FILE"
  echo "last-run: $(date -Iseconds) $st $note" >> "$TASK_FILE"
  log "任务状态 → $st $note"
  if [ "$COMMIT_STATUS" = 1 ]; then
    # `-- "$TASK_FILE"` 限定路径：不带它会把人手里无关的暂存改动一并吞进
    # 这个 chore 提交（实测）。
    git commit -m "chore: backlog 状态 $TASK_ID → $st" -- "$TASK_FILE" >/dev/null || true
  fi
}

# ── worktree 与分支 ──────────────────────────────────────────────
git fetch "$REMOTE" "$BASE" || die "fetch $REMOTE/$BASE 失败"
branch="codex/$(echo "$TASK_ID" | tr '[:upper:]' '[:lower:]')"
n=2
while git ls-remote --exit-code --heads "$REMOTE" "$branch" >/dev/null 2>&1; do
  branch="codex/$(echo "$TASK_ID" | tr '[:upper:]' '[:lower:]')-r$n"
  n=$((n + 1))
done
# 本地同名分支有未推送提交时拒绝重建 —— `-B` 会静默重置指针，
# 把上一轮没推出去的提交直接抹掉（数据丢失，无警告）。
if git show-ref --verify --quiet "refs/heads/$branch"; then
  ahead="$(git rev-list --count "$REMOTE/$BASE..$branch" 2>/dev/null || echo 0)"
  if [ "$ahead" -gt 0 ]; then
    die "本地分支 $branch 领先 $REMOTE/$BASE 共 $ahead 个提交 —— 先推送或删除该分支再重跑，不静默覆盖"
  fi
fi

wt="$WT_ROOT/$TASK_ID"
git worktree remove --force "$wt" 2>/dev/null || true
rm -rf "$wt"
git worktree add -B "$branch" "$wt" "$REMOTE/$BASE" >/dev/null || die "worktree 创建失败"
log "worktree：$wt（分支 $branch，基于 $REMOTE/$BASE）"

# ── 环境：库、共享 target、卡口用的 compose 目录 ─────────────────
export DATABASE_URL="${DATABASE_URL:-postgres://postgres:pw@127.0.0.1:55432/tgm}"
export TGM_DB_URL_OWNER="${TGM_DB_URL_OWNER:-postgres://postgres:pw@127.0.0.1:55432/tgm}"
# worktree 里起 compose 会另开项目名撞端口 —— 一律用主仓的服务
export TGM_COMPOSE_DIR="$PRIMARY"
# 共享编译缓存：驱动一次只跑一个任务，省掉每次全量重编
export CARGO_TARGET_DIR="$PRIMARY/target"

# dry-run 不起库：它只生成提示词，不需要环境 ——
# 起库失败不该挡在「看一眼提示词」的路上。
if [ "$DRY" = 0 ]; then
  (cd "$PRIMARY" && docker compose up -d --wait) || die "compose 起不来（见 eng/02 §3.2：down -v 重建）"
  psql "$DATABASE_URL" -c 'SELECT 1' >/dev/null 2>&1 || die "PG 不可达：$DATABASE_URL"
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
3. 跑 scripts/ai-driver/ai-gates.sh 直到全绿。**不得放宽任何卡口**；
   若你认为某条卡口与任务冲突，走升级流程，不要改卡口。
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

定位原因、修复、重跑 scripts/ai-driver/ai-gates.sh 直到全绿、追加提交、输出 [report]。
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
  } > "$PROMPT_FILE"
}

run_executor() {
  case "$EXECUTOR" in
    codex)
      timeout "$TIMEOUT" codex exec --dangerously-bypass-approvals-and-sandbox \
        -C "$wt" "$(cat "$PROMPT_FILE")" 2>&1 | tee "$RUN_LOG"
      ;;
    claude)
      (cd "$wt" && timeout "$TIMEOUT" claude -p --dangerously-skip-permissions \
        < "$PROMPT_FILE" 2>&1) | tee "$RUN_LOG"
      ;;
    *) die "未知执行器：$EXECUTOR（codex | claude）" ;;
  esac
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
  # 残局只有未跟踪文件时 stash create 输出空（实测 git 2.34）—— 此时没有快照，
  # 但残局本来就留在 worktree 里，worktree 也不被清理，所以不补机制。
  sha="$(git -C "$wt" stash create -u 2>/dev/null)"
  [ -n "$sha" ] || return 0
  git -C "$wt" update-ref "refs/ai-leftovers/$TASK_ID-$TS" "$sha"
  log "未提交残局已快照到 refs/ai-leftovers/$TASK_ID-$TS（查看：git -C $wt stash show -p $sha）"
}

# 卡口的最终裁决由驱动亲自跑，不信 agent 自述
run_gates() {
  (cd "$wt" && "$PRIMARY/scripts/ai-driver/ai-gates.sh") 2>&1
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
  build_prompt ""
  log "dry-run：提示词已生成 → $PROMPT_FILE（未执行）"
  exit 0
fi

# ── 主循环：首轮 + 至多 MAX_RETRIES 次卡口修复回合 ───────────────
set_status active "(driver)"
build_prompt ""
log "执行器：$EXECUTOR，超时 ${TIMEOUT}s，日志：$RUN_LOG"
run_executor || log "执行器退出码非 0，继续按产出判定"
record_leftovers

attempt=0
gates_ok=0
protected_hit=""
# path 必须在循环前初始化：它只在保护路径分支里赋值，而 `set -u` 下
# 循环后那句 `[ "$path" != "protected" ]` 读未定义变量会直接杀掉脚本 ——
# 卡口绿也走不到推分支，任务永久停在 active（实测 rc=1，队列卡死）。
path=""
while :; do
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
  if run_gates > "$GATE_LOG" 2>&1; then
    gates_ok=1
    break
  fi
  if [ "$attempt" -ge "$MAX_RETRIES" ]; then
    break
  fi
  log "卡口红，进入修复回合 $((attempt + 1))/$MAX_RETRIES"
  build_prompt "$(tail -200 "$GATE_LOG")"
  RUN_LOG="$LOG_DIR/$TASK_ID.$TS.retry$attempt.log"
  run_executor || log "执行器退出码非 0，继续按产出判定"
  record_leftovers
  attempt=$((attempt + 1))
done

[ "$path" != "protected" ] && path="$(report_field path)"
proposal="$wt/docs/plan/proposals/$TASK_ID.md"
[ -z "$path" ] && [ -f "$proposal" ] && path="escalated"

# 零提交不算完成：卡口跑在基线上必绿（基线本来就是绿的），配合 agent 自述
# path: ok 就会推一个空分支、开 PR 失败只打一行日志、状态照样落 done。
# 判据是「分支相对基线有没有提交」—— 机器可核，不依赖 agent 自述。
commits_ahead="$(git -C "$wt" rev-list --count "$REMOTE/$BASE..HEAD" 2>/dev/null || echo 0)"
if [ "$path" = "ok" ] && [ "$commits_ahead" = 0 ]; then
  log "✗ 自述 path: ok 但分支相对 $REMOTE/$BASE 零提交 —— 判 blocked，不按 ok 处理"
  path="no-commit"
fi

# ── 推分支与 PR ─────────────────────────────────────────────────
push_and_pr() {
  local title="$1" label="$2"
  git -C "$wt" push -u "$REMOTE" "$branch" || { log "推送失败"; return 1; }
  if [ "$NO_PR" = 1 ]; then log "--no-pr：跳过开 PR（分支 $branch 已推）"; return 0; fi
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
    else
      echo "卡口：**本地未全绿**（见 logs/$TASK_ID.$TS.gates-$attempt.log），请勿合并，先看失败原因。"
    fi
    echo
    echo "本 PR 由 AI 驱动循环生成（scripts/ai-driver/ai-run.sh，执行器：$EXECUTOR）。"
  } > "$body_file"
  if (cd "$wt" && gh pr create --base "$BASE" --head "$branch" --title "$title" --body-file "$body_file"); then
    log "PR 已创建"
  else
    log "gh pr create 失败 —— 分支 $branch 已推，请手工开 PR"
  fi
}

# 推送结果必须进状态记账：推送失败而状态落 done，等于状态说完成、
# 远端没有分支也没有 PR —— 人看状态会以为交付了。
case "$path" in
  ok)
    if [ "$gates_ok" = 1 ]; then
      title="$(task_title)"
      # 带任务 id 前缀与另外三条出口同形：PR 列表里一眼能对回 backlog。
      if push_and_pr "$TASK_ID：${title:-实现完成}" "实现"; then
        set_status done "(branch: $branch)"
      else
        set_status blocked "(推送失败, branch $branch 只在本地)"
      fi
    else
      push_and_pr "$TASK_ID：卡口未绿，待人工处理" "失败" || log "推送失败 —— 分支 $branch 只在本地"
      set_status blocked "(卡口未绿, branch: $branch)"
    fi
    ;;
  escalated)
    if push_and_pr "$TASK_ID：升级提案" "升级提案"; then
      set_status escalated "(branch: $branch)"
    else
      set_status blocked "(推送失败, branch $branch 只在本地)"
    fi
    ;;
  protected)
    if push_and_pr "$TASK_ID：改动保护路径，强制升级（未跑卡口）" "保护路径"; then
      set_status escalated "(branch: $branch, 保护路径: $(printf '%s' "$protected_hit" | tr '\n' ' '))"
    else
      set_status blocked "(推送失败, branch $branch 只在本地)"
    fi
    ;;
  no-commit)
    log "零提交，不推分支（没有东西可推）"
    set_status blocked "(自述 ok 但零提交)"
    ;;
  *)
    push_and_pr "$TASK_ID：未完成，待人工处理" "受阻" || log "推送失败 —— 分支 $branch 只在本地"
    set_status blocked "(branch: $branch)"
    ;;
esac

log "完成。worktree 保留在 $wt 供检查；清理：git worktree remove --force $wt"
