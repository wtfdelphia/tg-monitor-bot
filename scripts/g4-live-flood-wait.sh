#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
cd "$(git rev-parse --show-toplevel)"

ROOT="$(pwd -P)"
CONFIG="${TGM_G4_CONFIG_FILE:-$HOME/.config/tgm-g4/acceptance.env}"
CHECKER="$ROOT/scripts/check-g4-journal-redaction.py"
REPORT=""
JOURNAL_DIR=""
JOURNAL=""
UNIT=""
G4_DATABASE_URL='postgres://tgm_test:tpw@127.0.0.1:55439/tgm_test_control'

init_report() {
  REPORT="$(mktemp "${TMPDIR:-/tmp}/tgm-g4-flood-wait-report.XXXXXX")"
  printf '%s\n' \
    'source_message: not-run' \
    'flood_wait_observed: not-run' \
    'retry_at_persisted: not-run' \
    'identity_state: not-run' \
    'retry_suppressed: not-run' \
    'wait_expiry: not-run' \
    'single_delivery_after_recovery: not-run' \
    'duplicate_delivery: not-run' \
    'journal_redaction: not-run' \
    'production_credentials: not-used' \
    'production_services: untouched' \
    'cleanup: partial' > "$REPORT"
  chmod 600 "$REPORT"
}

run_test() {
  if [[ "${TGM_G4_FLOOD_WAIT_ALLOW:-}" != "1" ]]; then
    printf '%s\n' '拒绝执行：请确认仅使用隔离 Telegram 测试对象后设置 TGM_G4_FLOOD_WAIT_ALLOW=1' >&2
    return 2
  fi

  for name in TGM_PLATFORM_API_ID TGM_PLATFORM_API_HASH TGM_LIVE_BOT_TOKEN TGM_LIVE_BOT2_TOKEN TGM_LIVE_TARGET_CHAT TGM_G4_SENDER_SESSION_FILE TGM_G4_OBSERVER_SESSION_FILE; do
    if [[ -z "${!name:-}" ]]; then
      printf '缺少环境变量：%s\n' "$name" >&2
      return 2
    fi
  done

  if [[ -n "${DATABASE_URL:-}" && "$DATABASE_URL" != "$G4_DATABASE_URL" ]]; then
    printf '%s\n' '拒绝执行：G4 验收只允许本机 tgm_test_control 测试库' >&2
    return 2
  fi
  export DATABASE_URL="$G4_DATABASE_URL"
  export TGM_TEST_PG_PORT=55439
  export SQLX_OFFLINE=true
  export TGM_G4_REPORT_FILE="$REPORT"

  cargo test -p tgm-app --test delivery_live live_delivery_flood_wait_recovery \
    -- --ignored --exact --nocapture --test-threads=1
}

finalize_report() {
  local status="$1"
  if ! python3 "$CHECKER" --finalize --config "$CONFIG" --journal "$JOURNAL" --report "$REPORT"; then
    printf '%s\n' 'G4 日志脱敏检查失败；整体验收拒绝通过' >&2
    return 5
  fi

  printf 'G4 报告：%s\n' "$REPORT"
  if [[ "$status" != 0 ]]; then
    printf '%s\n' 'G4 真实限流退避闭环：partial/fail；检查脱敏报告中的缺项' >&2
    return 1
  fi
  if rg -q '^flood_wait_observed: not-run$' "$REPORT"; then
    printf '%s\n' 'G4 真实限流退避闭环：not-run（本次未自然观察到 FLOOD_WAIT，未加压重试）' >&2
    return 3
  fi

  for field in flood_wait_observed retry_at_persisted identity_state retry_suppressed wait_expiry single_delivery_after_recovery duplicate_delivery journal_redaction cleanup; do
    if ! rg -q "^${field}: pass$" "$REPORT"; then
      printf 'G4 未通过：%s\n' "$field" >&2
      return 1
    fi
  done
  printf '%s\n' 'G4 真实限流退避验收通过'
}

if [[ "${1:-}" == "--journal-child" ]]; then
  [[ "$#" == 2 ]] || exit 2
  REPORT="$2"
  [[ -f "$REPORT" && ! -L "$REPORT" ]] || exit 2
  [[ "$(stat -c '%a' "$REPORT")" == 600 ]] || exit 2
  [[ -f "$CONFIG" && ! -L "$CONFIG" ]] || exit 2
  python3 "$CHECKER" --preflight --config "$CONFIG"
  set -a
  source "$CONFIG"
  set +a
  run_test
  exit "$?"
fi

[[ "$#" == 0 ]] || {
  printf '用法：%s\n' "$0" >&2
  exit 2
}
[[ -f "$CONFIG" && ! -L "$CONFIG" ]] || {
  printf '%s\n' 'G4 配置必须是普通文件' >&2
  exit 2
}
python3 "$CHECKER" --preflight --config "$CONFIG"
init_report
JOURNAL_DIR="$(mktemp -d "${TMPDIR:-/tmp}/tgm-g4-journal.XXXXXX")"
JOURNAL="$JOURNAL_DIR/captured.log"
UNIT="tgm-g4-acceptance-$(date +%s)-$$.service"
CHILD_ENV=(
  "HOME=$HOME"
  "PATH=$PATH"
  "CARGO_TARGET_DIR=$ROOT/target/ai-gates-task"
  "DATABASE_URL=$G4_DATABASE_URL"
  "TGM_TEST_PG_PORT=55439"
  "TGM_G4_CONFIG_FILE=$CONFIG"
)
for name in CARGO_HOME RUSTUP_HOME RUSTUP_TOOLCHAIN; do
  if [[ -n "${!name:-}" ]]; then
    CHILD_ENV+=("$name=${!name}")
  fi
done

cleanup() {
  local status=$?
  if [[ -n "$UNIT" ]]; then
    systemctl --user stop "$UNIT" >/dev/null 2>&1 || true
    systemctl --user reset-failed "$UNIT" >/dev/null 2>&1 || true
  fi
  if [[ -n "$JOURNAL_DIR" ]]; then
    rm -f -- "$JOURNAL"
    rmdir -- "$JOURNAL_DIR" 2>/dev/null || true
  fi
  if [[ "$status" != 0 && -n "$REPORT" ]]; then
    printf '保留脱敏报告：%s\n' "$REPORT" >&2
  fi
  return "$status"
}
trap cleanup EXIT INT TERM

[[ "$(id -u)" != 0 ]] || {
  printf '%s\n' 'G4 真机验收不得以 root 运行' >&2
  exit 2
}
systemctl --user show-environment >/dev/null 2>&1 || {
  printf '%s\n' '当前会话没有可用的 systemd --user manager' >&2
  exit 2
}
command -v systemd-run >/dev/null
command -v journalctl >/dev/null
STARTED_AT="@$(date +%s)"

unit_status=0
systemd-run --user --quiet --wait --collect --unit="$UNIT" \
  --working-directory="$ROOT" --property=UMask=0077 \
  /usr/bin/env -i "${CHILD_ENV[@]}" bash "$ROOT/scripts/g4-live-flood-wait.sh" --journal-child "$REPORT" || unit_status=$?

capture_status=0
python3 "$CHECKER" --capture --config "$CONFIG" --since "$STARTED_AT" --unit "$UNIT" --journal "$JOURNAL" || capture_status=$?
if [[ "$capture_status" != 0 ]]; then
  printf '%s\n' '无法安全读取或脱敏本次 G4 user unit journal；拒绝验收' >&2
  exit 5
fi
finalize_report "$unit_status"
