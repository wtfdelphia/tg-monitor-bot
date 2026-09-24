#!/usr/bin/env bash
# 无人值守循环：每次调用跑一个任务，跑完即退 —— 把节奏交给 cron / systemd timer，
# 而不是在一个进程里常驻。理由：驱动一次只动一个任务，串行天然安全；
# 每轮一个独立进程，崩了也只丢当轮，日志与状态都在文件系统里可查。
#
# 用法：
#   scripts/ai-driver/ai-loop.sh                  # 跑一个任务
#   scripts/ai-driver/ai-loop.sh --max 3          # 最多连续跑 3 个（0/不设 = 1）
#   scripts/ai-driver/ai-loop.sh --executor claude
#   scripts/ai-driver/ai-loop.sh --until "08:00"  # 当天到点停止
#   scripts/ai-driver/ai-loop.sh --from 22:00 --until 08:00  # 跨午夜时间窗
#
# cron 示例（每 2 小时试一次）：
#   0 */2 * * * /仓库绝对路径/scripts/ai-driver/ai-loop.sh --executor codex >> /tmp/ai-loop.log 2>&1
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

MAX=1
FROM=""
FROM_MINUTES=""
UNTIL=""
UNTIL_MINUTES=""
ARGS=()
usage_error() {
  echo "用法：$0 [--max 非负整数] [--from HH:MM --until HH:MM] [ai-run 参数 ...]" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --max)
      [ "$#" -ge 2 ] || usage_error
      MAX="$2"
      [[ "$MAX" =~ ^[0-9]+$ ]] || usage_error
      MAX=$((10#$MAX))
      [ "$MAX" -gt 0 ] || MAX=1
      shift 2
      ;;
    --from)
      [ "$#" -ge 2 ] || usage_error
      FROM="$2"
      [[ "$FROM" =~ ^([01][0-9]|2[0-3]):([0-5][0-9])$ ]] || usage_error
      FROM_MINUTES=$((10#${BASH_REMATCH[1]} * 60 + 10#${BASH_REMATCH[2]}))
      shift 2
      ;;
    --until)
      [ "$#" -ge 2 ] || usage_error
      UNTIL="$2"
      [[ "$UNTIL" =~ ^([01][0-9]|2[0-3]):([0-5][0-9])$ ]] || usage_error
      UNTIL_MINUTES=$((10#${BASH_REMATCH[1]} * 60 + 10#${BASH_REMATCH[2]}))
      shift 2
      ;;
    *) ARGS+=("$1"); shift ;;
  esac
done

if [ -n "$FROM" ] && [ -z "$UNTIL" ]; then
  echo "--from 必须和 --until 一起使用" >&2
  exit 2
fi
if [ -n "$FROM" ] && [ "$FROM_MINUTES" -eq "$UNTIL_MINUTES" ]; then
  echo "--from 和 --until 不能相同" >&2
  exit 2
fi

finished=0
loop_rc=0
while [ "$finished" -lt "$MAX" ]; do
  if [ -n "$UNTIL" ]; then
    now_hm="$(date +%H:%M)"
    now_minutes=$((10#${now_hm%:*} * 60 + 10#${now_hm#*:}))
    if [ -n "$FROM" ]; then
      if [ "$FROM_MINUTES" -lt "$UNTIL_MINUTES" ]; then
        in_window=$((now_minutes >= FROM_MINUTES && now_minutes < UNTIL_MINUTES))
      else
        in_window=$((now_minutes >= FROM_MINUTES || now_minutes < UNTIL_MINUTES))
      fi
      [ "$in_window" -eq 1 ] || { echo "[loop] 不在时间窗 $FROM-$UNTIL，停止"; break; }
    elif [ "$now_minutes" -ge "$UNTIL_MINUTES" ]; then
      echo "[loop] 到点 $UNTIL，停止"
      break
    fi
  fi
  echo "[loop] ===== 第 $((finished + 1)) 轮 $(date -Iseconds) ====="
  scripts/ai-driver/ai-run.sh "${ARGS[@]}"
  rc=$?
  case $rc in
    0) finished=$((finished + 1)) ;;
    *)
      echo "[loop] ai-run 退出码 $rc，本轮停止（避免在同一个坑里连摔）"
      loop_rc="$rc"
      break
      ;;
  esac
done

# 收尾状态：人看这一行就够
echo "[loop] backlog 状态一览（$(date -Iseconds)）："
COMMON_GIT_DIR="$(git rev-parse --git-common-dir)"
case "$COMMON_GIT_DIR" in
  /*) ;;
  *) COMMON_GIT_DIR="$(pwd)/$COMMON_GIT_DIR" ;;
esac
for f in docs/plan/backlog/T-*.md; do
  [ -f "$f" ] || continue
  task_id="$(basename "$f" .md)"
  state_file="$COMMON_GIT_DIR/ai-driver-state/$task_id.state"
  status="$(sed -n 's/^status: *//p' "$state_file" 2>/dev/null | head -1)"
  [ -n "$status" ] || status="$(sed -n 's/^status: *//p' "$f" | head -1)"
  printf "  %-12s %s\n" "$task_id" "$status"
done
exit "$loop_rc"
