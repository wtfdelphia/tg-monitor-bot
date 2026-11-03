#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

usage() {
  cat <<'EOF'
用法：
  scripts/native-release-preflight.sh --binary <tgm> --unit-dir <deploy/systemd> [--app-services]

只做本地/目标机预检，不启动服务、不执行迁移、不连接数据库。
EOF
}

fail() {
  printf '原生发布预检失败：%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令：$1"
}

verify_units() {
  local unit name status=0
  : >"$temporary/unit-names"
  for unit in "$@"; do
    name="$(basename "$unit")"
    printf '%s\n' "${name/@.service/@}" >>"$temporary/unit-names"
    if grep -Eq '^[[:space:]]*RestrictAddressFamilies[[:space:]]*=[[:space:]]*[^[:space:]#;]' "$unit"; then
      printf '%s\n' "$systemd_version" | grep -Eq '(^|[[:space:]])\+SECCOMP([[:space:]]|$)' || \
        fail 'unit 要求 RestrictAddressFamilies，但目标 systemd 未声明 +SECCOMP；拒绝忽略地址族限制'
    fi
  done
  systemd-analyze verify "$@" >"$temporary/verify.log" 2>&1 || status=$?
  cat "$temporary/verify.log"
  [ "$status" = 0 ] || fail 'systemd unit/timer 解析失败'
  if grep -F -f "$temporary/unit-names" "$temporary/verify.log" | \
      grep -Ei 'unknown|unrecognized|failed to (parse|set|resolve)|not supported|unsupported|ignor(ed|ing)|invalid|disabled at compile time|has no (value|timer)|lacks.*(value|setting)' >/dev/null; then
    fail 'systemd 忽略或不支持本任务 unit/timer 的配置；拒绝静默降级'
  fi
}

BINARY=""
UNIT_DIR=""
APP_SERVICES=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      BINARY="$2"
      shift 2
      ;;
    --unit-dir)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      UNIT_DIR="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --app-services)
      APP_SERVICES=1
      shift
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

[ -n "$BINARY" ] && [ -n "$UNIT_DIR" ] || { usage >&2; exit 2; }
[ -x "$BINARY" ] || fail "二进制不存在或不可执行：$BINARY"
[ -d "$UNIT_DIR" ] || fail "systemd 工件目录不存在：$UNIT_DIR"
BINARY="$(cd "$(dirname "$BINARY")" && pwd -P)/$(basename "$BINARY")"
UNIT_DIR="$(cd "$UNIT_DIR" && pwd -P)"
[ "$(uname -m)" = "x86_64" ] || fail "目标架构不是 x86_64：$(uname -m)"

require_command ldd
require_command openssl
require_command systemd-analyze
systemd_version="$(systemd-analyze --version)" || fail '无法读取 systemd 构建能力'
printf '%s\n' "$systemd_version"
if command -v rpm >/dev/null 2>&1; then
  if systemd_package="$(rpm -q systemd 2>/dev/null)"; then
    printf 'systemd RPM：%s\n' "$systemd_package"
  fi
fi

ldd_output="$(ldd "$BINARY" 2>&1)" || fail "ldd 无法解析二进制"
printf '%s\n' "$ldd_output"
printf '%s\n' "$ldd_output" | grep -q 'not found' && fail '二进制存在未满足的动态库依赖'
if printf '%s\n' "$ldd_output" | grep -Eq 'lib(ssl|crypto)\.so'; then
  fail '二进制依赖目标机 OpenSSL 动态库；首发工件必须使用 vendored OpenSSL'
fi

glibc_version="$(ldd --version 2>&1 | sed -n '1s/.* //p')"
[ -n "$glibc_version" ] || fail '无法读取目标机 glibc 版本'
if [ "$(printf '%s\n' '2.17' "$glibc_version" | sort -V | head -1)" != '2.17' ]; then
  fail "目标机 glibc 低于 2.17：$glibc_version"
fi
openssl_version="$(openssl version 2>&1)" || fail '无法读取目标机 OpenSSL 版本'
printf '目标机 OpenSSL：%s\n' "$openssl_version"

temporary="$(mktemp -d "${TMPDIR:-/tmp}/tgm-native-unit-check.XXXXXX")"
trap 'rm -rf -- "$temporary"' EXIT
if [ "$APP_SERVICES" = 1 ]; then
  for unit in "$UNIT_DIR/app/tgm-app@.service" "$UNIT_DIR/app/user/tgm-app@.service"; do
    [ -f "$unit" ] || fail "缺少应用服务模板：$unit"
    verify_units "$unit"
  done
  for role in control-plane bot-gateway mtproto-worker fanout rule delivery; do
    [ -f "$UNIT_DIR/app/$role.env.example" ] || fail "缺少角色环境示例：$role"
    help_output="$("$BINARY" serve --role "$role" --help)" || fail "角色 help 失败：$role"
    printf '%s\n' "$help_output" | grep -F -- '--env-file' >/dev/null || fail '二进制不支持原生环境文件'
    printf '%s\n' "$help_output" | grep -F -- '--check-config' >/dev/null || fail '二进制不支持离线配置检查'
  done
  printf '原生应用服务预检通过：架构=x86_64 glibc=%s unit=pass help=pass；尚未启动服务或验证沙箱。\n' "$glibc_version"
  exit 0
fi
unit_files=()
while IFS= read -r unit; do
  rendered="$temporary/$(basename "$unit")"
  awk -v binary="$BINARY" '
    BEGIN { prefix = "ExecStart=/usr/local/bin/tgm " }
    index($0, prefix) == 1 {
      print "ExecStart=" binary " " substr($0, length(prefix) + 1)
      next
    }
    { print }
  ' "$unit" >"$rendered"
  unit_files+=("$rendered")
done < <(find "$UNIT_DIR" -maxdepth 1 -type f \( -name '*.service' -o -name '*.timer' \) -print | sort)
[ "${#unit_files[@]}" -gt 0 ] || fail "没有找到 systemd service/timer 工件"

verify_units "${unit_files[@]}"
systemd-analyze --help >"$temporary/analyze-help" || fail '无法读取 systemd-analyze 命令能力'
has_calendar=0
if grep -Eq '^[[:space:]]+calendar([[:space:]]|$)' "$temporary/analyze-help"; then
  has_calendar=1
fi
timer_count=0
while IFS= read -r timer; do
  timer_count=$((timer_count + 1))
  mapfile -t calendars < <(awk '
    /^[[:space:]]*\[/ { in_timer = ($0 ~ /^[[:space:]]*\[Timer\][[:space:]]*$/) }
    in_timer && /^[[:space:]]*OnCalendar[[:space:]]*=/ { print substr($0, index($0, "=") + 1) }
  ' "$timer")
  [ "${#calendars[@]}" -gt 0 ] || fail "timer 缺少 OnCalendar：$(basename "$timer")"
  for calendar in "${calendars[@]}"; do
    [ -n "${calendar//[[:space:]]/}" ] || fail "timer 的 OnCalendar 为空：$(basename "$timer")"
    if [ "$has_calendar" = 1 ]; then
      systemd-analyze calendar "$calendar" >/dev/null || fail "timer 日历解析失败：$(basename "$timer")"
    fi
  done
done < <(find "$UNIT_DIR" -maxdepth 1 -type f -name '*.timer' -print | sort)
[ "$timer_count" -gt 0 ] || fail '没有找到 cleanup timer 工件'
if [ "$has_calendar" = 0 ]; then
  printf '目标 systemd-analyze 无 calendar 子命令；timer 已由本机 verify 解析并核对诊断。\n'
fi

"$BINARY" --help >/dev/null
"$BINARY" purge-deleted-tenants --help >/dev/null
printf '原生发布预检通过：架构=x86_64 glibc>=%s openssl=pass 动态依赖=pass systemd=pass help=pass\n' "$glibc_version"
