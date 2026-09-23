#!/usr/bin/env bash
# 无人值守循环：每次调用跑一个任务，跑完即退 —— 把节奏交给 cron / systemd timer，
# 而不是在一个进程里常驻。理由：驱动一次只动一个任务，串行天然安全；
# 每轮一个独立进程，崩了也只丢当轮，日志与状态都在文件系统里可查。
#
# 用法：
#   scripts/ai-driver/ai-loop.sh                  # 跑一个任务
#   scripts/ai-driver/ai-loop.sh --max 3          # 最多连续跑 3 个（0/不设 = 1）
#   scripts/ai-driver/ai-loop.sh --executor claude
#   scripts/ai-driver/ai-loop.sh --until "08:00"  # 到点停止（防止白天与人抢资源）
#
# cron 示例（每 2 小时试一次）：
#   0 */2 * * * /仓库绝对路径/scripts/ai-driver/ai-loop.sh --executor codex >> /tmp/ai-loop.log 2>&1
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

MAX=1
UNTIL=""
ARGS=()
# 变量名不能用 done：它是 shell 关键字，虽在赋值位置能跑，
# 但 `[ "$done" -lt ... ]` 这类用法脆弱且误导读者。
while [ $# -gt 0 ]; do
  case "$1" in
    --max) MAX="$2"; shift 2 ;;
    --until) UNTIL="$2"; shift 2 ;;
    *) ARGS+=("$1"); shift ;;
  esac
done

finished=0
while [ "$finished" -lt "$MAX" ]; do
  if [ -n "$UNTIL" ] && [ "$(date +%H%M)" -ge "${UNTIL//:/}" ]; then
    echo "[loop] 到点 $UNTIL，停止"
    break
  fi
  echo "[loop] ===== 第 $((finished + 1)) 轮 $(date -Iseconds) ====="
  scripts/ai-driver/ai-run.sh "${ARGS[@]}"
  rc=$?
  case $rc in
    0) finished=$((finished + 1)) ;;
    *) echo "[loop] ai-run 退出码 $rc，本轮停止（避免在同一个坑里连摔）"; break ;;
  esac
done

# 收尾状态：人看这一行就够
echo "[loop] backlog 状态一览（$(date -Iseconds)）："
for f in docs/plan/backlog/T-*.md; do
  [ -f "$f" ] || continue
  printf "  %-12s %s\n" "$(basename "$f" .md)" "$(sed -n 's/^status: *//p' "$f" | head -1)"
done
