#!/usr/bin/env bash
set -euo pipefail

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
systemd-analyze --version

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
    if ! systemd-analyze verify "$unit" >"$temporary/verify.log" 2>&1; then
      cat "$temporary/verify.log" >&2
      fail '应用服务 unit 解析失败'
    fi
    cat "$temporary/verify.log"
    if grep -F 'tgm-app@' "$temporary/verify.log" | grep -Ei 'unknown (lvalue|key|section)|failed to parse|not supported|unsupported' >/dev/null; then
      fail 'systemd 不支持应用 unit 中的配置；拒绝静默降级'
    fi
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

systemd-analyze verify "${unit_files[@]}"
while IFS= read -r timer; do
  calendar="$(awk -F= '$1 == "OnCalendar" { print substr($0, index($0, "=") + 1); exit }' "$timer")"
  [ -n "$calendar" ] || fail "timer 缺少 OnCalendar：$(basename "$timer")"
  systemd-analyze calendar "$calendar" >/dev/null
done < <(find "$UNIT_DIR" -maxdepth 1 -type f -name '*.timer' -print | sort)

"$BINARY" --help >/dev/null
"$BINARY" purge-deleted-tenants --help >/dev/null
printf '原生发布预检通过：架构=x86_64 glibc>=%s openssl=pass 动态依赖=pass systemd=pass help=pass\n' "$glibc_version"
