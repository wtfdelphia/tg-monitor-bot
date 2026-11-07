#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export LC_ALL=C

usage() {
  cat <<'EOF'
用法：
  bash scripts/tgm-native-user-unit-check.sh \
    --binary <tgm> --role <role> --env-file <development.env> [--config-only] [--kill-test]

脚本只使用当前会话已有的 systemd --user manager，并通过 systemctl --user 控制临时 unit。
不安装系统 unit，不启用 timer，不修改 ~/.local/bin/tgm。
EOF
}

fail() {
  printf '临时 user unit 验收失败：%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令：$1"
}

show_failure() {
  systemctl --user show "$UNIT" \
    --property=ActiveState,SubState,Result,MainPID,NRestarts,ExecMainCode,ExecMainStatus,ExecMainStartTimestamp,ExecMainExitTimestamp \
    --no-pager || true
  if [[ -f "$LOG_FILE" ]]; then
    printf '%s\n' '--- 应用日志 ---'
    cat "$LOG_FILE"
  fi
  journalctl --user -u "$UNIT" --no-pager --output=cat -n 80 || true
}

wait_for_state() {
  local expected_active="$1"
  local expected_sub="$2"
  local attempts="${3:-60}"
  local active sub

  for _ in $(seq 1 "$attempts"); do
    active="$(systemctl --user show "$UNIT" --property=ActiveState --value 2>/dev/null || true)"
    sub="$(systemctl --user show "$UNIT" --property=SubState --value 2>/dev/null || true)"
    if [[ "$active" == "$expected_active" && "$sub" == "$expected_sub" ]]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_until_stable() {
  local seconds="${1:-35}"
  for _ in $(seq 1 "$seconds"); do
    wait_for_state active running 1 || return 1
    sleep 1
  done
}

BINARY=""
ROLE=""
ENV_FILE=""
CONFIG_ONLY=0
KILL_TEST=0

while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --binary)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      BINARY="$2"
      shift 2
      ;;
    --role)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      ROLE="$2"
      shift 2
      ;;
    --env-file)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      ENV_FILE="$2"
      shift 2
      ;;
    --config-only)
      CONFIG_ONLY=1
      shift
      ;;
    --kill-test)
      KILL_TEST=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

[[ "$(id -u)" != 0 ]] || fail '不要以 root 运行 user unit 验收'
[[ "$ROLE" =~ ^(control-plane|bot-gateway|mtproto-worker|fanout|rule|delivery)$ ]] || \
  fail "不支持的角色：$ROLE"
[[ "$BINARY" = /* && -x "$BINARY" && ! -L "$BINARY" ]] || \
  fail '二进制必须是绝对路径、普通可执行文件'
[[ "$ENV_FILE" = /* && -f "$ENV_FILE" && ! -L "$ENV_FILE" ]] || \
  fail '环境文件必须是绝对路径、普通文件'
[[ "$BINARY" != *[[:space:]]* && "$ENV_FILE" != *[[:space:]]* ]] || \
  fail '二进制和环境文件路径不能包含空格'

for command_name in systemctl systemd-analyze journalctl stat awk grep; do
  require_command "$command_name"
done

[[ -n "${XDG_RUNTIME_DIR:-}" && -d "$XDG_RUNTIME_DIR" ]] || \
  fail 'XDG_RUNTIME_DIR 不可用'
systemctl --user show-environment >/dev/null 2>&1 || \
  fail '当前会话没有可用的 systemd --user manager'

owner="$(stat -c '%u' "$ENV_FILE")"
mode="$(stat -c '%a' "$ENV_FILE")"
[[ "$owner" == "$(id -u)" && "$mode" == 600 ]] || \
  fail "环境文件必须由当前用户拥有且权限为 0600，当前为 uid=$owner mode=$mode"
grep -qx 'TGM_PROFILE=development' "$ENV_FILE" || \
  fail '临时验收环境必须使用 TGM_PROFILE=development'

"$BINARY" serve --role "$ROLE" --env-file "$ENV_FILE" --check-config

if [[ "$CONFIG_ONLY" == 1 ]]; then
  printf '配置预检通过；未启动临时 unit。\n'
  exit 0
fi

USER_UNIT_DIR="$XDG_RUNTIME_DIR/systemd/user"
mkdir -p -- "$USER_UNIT_DIR"
chmod 700 -- "$USER_UNIT_DIR"
UNIT="tgm-manual-${ROLE}-$$.service"
UNIT_PATH="$USER_UNIT_DIR/$UNIT"
LOG_FILE="$XDG_RUNTIME_DIR/tgm-native-user-${ROLE}-$$.log"

cleanup() {
  local status=$?
  systemctl --user stop "$UNIT" >/dev/null 2>&1 || true
  systemctl --user reset-failed "$UNIT" >/dev/null 2>&1 || true
  rm -f -- "$UNIT_PATH"
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  if [[ "$status" == 0 && "${KEEP_LOG:-0}" != 1 ]]; then
    rm -f -- "$LOG_FILE"
  else
    printf '保留诊断日志：%s\n' "$LOG_FILE" >&2
  fi
  return "$status"
}
trap cleanup EXIT

cat >"$UNIT_PATH" <<EOF
[Unit]
Description=TGM temporary user acceptance ($ROLE)
StartLimitIntervalSec=120s
StartLimitBurst=3

[Service]
Type=simple
WorkingDirectory=/
ExecStartPre=/usr/bin/env -i $BINARY serve --role $ROLE --env-file $ENV_FILE --check-config
ExecStart=/usr/bin/env -i $BINARY serve --role $ROLE --env-file $ENV_FILE
Restart=on-failure
RestartSec=10s
KillSignal=SIGTERM
KillMode=control-group
TimeoutStopSec=30s
UMask=0077
StandardOutput=append:$LOG_FILE
StandardError=append:$LOG_FILE
SyslogIdentifier=tgm-manual-$ROLE
NoNewPrivileges=true
EOF
chmod 644 -- "$UNIT_PATH"

systemctl --user daemon-reload
systemd-analyze --user verify "$UNIT_PATH"
printf '临时 unit：%s\n' "$UNIT"
printf 'systemctl --user 控制器：available\n'

systemctl --user start "$UNIT" || { show_failure; exit 1; }
if ! wait_for_state active running; then
  show_failure
  exit 1
fi
if ! wait_until_stable "${TGM_USER_UNIT_STARTUP_GRACE_SECONDS:-35}"; then
  show_failure
  fail '进程未稳定运行；配置预检通过但应用启动阶段失败，未判定为可优雅停止'
fi

pid="$(systemctl --user show "$UNIT" --property=MainPID --value)"
[[ "$pid" =~ ^[1-9][0-9]*$ ]] || { show_failure; fail '无法取得主进程 PID'; }
[[ "$(stat -c '%u' "/proc/$pid")" == "$(id -u)" ]] || {
  show_failure
  fail '进程没有以当前用户运行'
}
printf '启动：pass pid=%s\n' "$pid"

systemctl --user stop "$UNIT"
if ! wait_for_state inactive dead; then
  show_failure
  fail 'SIGTERM 停止后 unit 未进入 inactive/dead'
fi
printf 'SIGTERM 停止：pass\n'

if [[ "$KILL_TEST" == 1 ]]; then
  systemctl --user reset-failed "$UNIT" >/dev/null 2>&1 || true
  systemctl --user start "$UNIT" || { show_failure; exit 1; }
  wait_for_state active running || { show_failure; exit 1; }
  old_pid="$(systemctl --user show "$UNIT" --property=MainPID --value)"
  kill -KILL "$old_pid"

  restarted=0
  for _ in $(seq 1 60); do
    new_pid="$(systemctl --user show "$UNIT" --property=MainPID --value 2>/dev/null || true)"
    restarts="$(systemctl --user show "$UNIT" --property=NRestarts --value 2>/dev/null || true)"
    if [[ "$new_pid" =~ ^[1-9][0-9]*$ && "$new_pid" != "$old_pid" && "$restarts" -ge 1 ]]; then
      restarted=1
      break
    fi
    sleep 1
  done
  [[ "$restarted" == 1 ]] || { show_failure; fail 'SIGKILL 后未观察到自动恢复'; }
  printf 'SIGKILL 自动恢复：pass old_pid=%s new_pid=%s\n' "$old_pid" "$new_pid"
  systemctl --user stop "$UNIT"
  wait_for_state inactive dead || { show_failure; fail '恢复测试后的停止失败'; }
fi

printf '临时 user unit 验收完成；退出时只清理本脚本创建的 unit。\n'
