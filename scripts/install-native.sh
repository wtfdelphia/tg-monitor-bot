#!/usr/bin/env bash
set -euo pipefail
umask 077

MODE=""
ARCHIVE=""
CHECKSUMS=""
BUNDLE=""
ROLLBACK=""
IDENTITY_REGEXP="${COSIGN_CERTIFICATE_IDENTITY_REGEXP:-https://github\\.com/wtfdelphia/tg-monitor-bot/.github/workflows/binary-release\\.yml@refs/(tags/v[0-9]+\\.[0-9]+\\.[0-9]+|heads/main)}"
OIDC_ISSUER="${COSIGN_CERTIFICATE_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"

usage() {
  cat <<'EOF'
用法：
  scripts/install-native.sh --root --archive <包.tar.gz> --checksums <SHA256SUMS> --bundle <SHA256SUMS.sigstore.json>
  scripts/install-native.sh --user --archive <包.tar.gz> --checksums <SHA256SUMS> --bundle <SHA256SUMS.sigstore.json>
  scripts/install-native.sh --root --rollback <版本>
  scripts/install-native.sh --user --rollback <版本>

安装器只做签名、校验、复制和帮助命令检查，不执行迁移、不启动服务、不启用 timer。
EOF
}

fail() {
  printf '安装失败：%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令：$1"
}

atomic_symlink() {
  local target="$1"
  local link="$2"
  local parent
  parent="$(dirname "$link")"
  mkdir -p "$parent"
  local temporary="$parent/.tgm-link.$$"
  rm -f "$temporary"
  ln -s "$target" "$temporary"
  mv -Tf "$temporary" "$link"
}

reject_non_symlink_target() {
  local path="$1"
  [ ! -e "$path" ] || [ -L "$path" ] || fail "目标路径不是符号链接，拒绝覆盖：$path"
}

check_existing_file() {
  local source="$1"
  local destination="$2"
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    [ -f "$destination" ] || fail "目标路径不是普通文件，拒绝覆盖：$destination"
    cmp -s "$source" "$destination" || fail "目标文件内容不同，拒绝覆盖：$destination"
  fi
}

verify_archive_members() {
  local archive="$1"
  while IFS= read -r member; do
    member="${member%/}"
    [ -n "$member" ] || continue
    case "$member" in
      /*)
        fail "压缩包包含危险路径：$member"
        ;;
    esac
    local segment
    local segments=()
    IFS='/' read -r -a segments <<< "$member"
    for segment in "${segments[@]}"; do
      [ "$segment" != ".." ] || fail "压缩包包含危险路径：$member"
    done
  done < <(tar -tzf "$archive")
  while IFS= read -r entry; do
    case "${entry:0:1}" in
      -|d) ;;
      *) fail "压缩包包含不允许的链接或特殊文件：$entry" ;;
    esac
  done < <(tar -tvzf "$archive")
}

verify_outer_checksum() {
  local archive="$1"
  local checksums="$2"
  local archive_name
  archive_name="$(basename "$archive")"
  local expected
  expected="$(awk -v name="$archive_name" '$2 == name || $2 == "*" name || $2 == "./" name { print $1; exit }' "$checksums")"
  [ -n "$expected" ] || fail "外层校验文件没有记录 $archive_name"
  local actual
  actual="$(sha256sum "$archive" | awk '{print $1}')"
  [ "$actual" = "$expected" ] || fail "发布包 SHA-256 不匹配"
}

verify_signature() {
  local checksums="$1"
  local bundle="$2"
  cosign verify-blob \
    --offline \
    --bundle "$bundle" \
    --certificate-identity-regexp "$IDENTITY_REGEXP" \
    --certificate-oidc-issuer "$OIDC_ISSUER" \
    "$checksums" >/dev/null || fail "SHA256SUMS 的 cosign keyless 签名验证失败"
}

validate_version() {
  local version="$1"
  printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || \
    fail "VERSION 文件必须是 MAJOR.MINOR.PATCH"
}

read_version() {
  local version_file="$1"
  local version
  version="$(tr -d '[:space:]' < "$version_file")"
  [ -n "$version" ] || fail "VERSION 文件为空"
  validate_version "$version"
  printf '%s\n' "$version"
}

validate_binary() {
  local binary="$1"
  local package_root="$2"
  [ -x "$binary" ] || fail "发布包中的 tgm 不可执行"
  local preflight="$package_root/scripts/native-release-preflight.sh"
  [ -x "$preflight" ] || fail '发布包缺少 native-release-preflight.sh'
  "$preflight" --binary "$binary" --unit-dir "$package_root/deploy/systemd"
}

install_root() {
  local extracted="$1"
  local version="$2"
  [ "$(id -u)" -eq 0 ] || fail "--root 必须由 root 执行"
  local release_root="/opt/tgm/releases/$version"
  [ ! -e "$release_root" ] || fail "目标版本目录已存在：$release_root"
  reject_non_symlink_target /usr/local/bin/tgm
  while IFS= read -r unit; do
    check_existing_file "$unit" "/etc/systemd/system/$(basename "$unit")"
  done < <(find "$extracted/deploy/systemd" -maxdepth 1 -type f \( -name '*.service' -o -name '*.timer' \) -print | sort)
  while IFS= read -r example; do
    destination="/etc/tgm/$(basename "$example")"
    [ ! -L "$destination" ] || fail "环境示例目标是符号链接，拒绝安装：$destination"
    [ ! -e "$destination" ] || [ -f "$destination" ] || \
      fail "环境示例目标不是普通文件，拒绝安装：$destination"
  done < <(find "$extracted/deploy/systemd" -maxdepth 1 -type f -name '*.env.example' -print | sort)
  install -d -o root -g root -m 0755 /opt/tgm/releases
  (
    local stage="/opt/tgm/releases/.${version}.stage.$$"
    trap 'rm -rf -- "$stage"' EXIT
    install -d -o root -g root -m 0755 "$stage"
    cp -a "$extracted/." "$stage/"
    chown -R root:root "$stage"
    chmod 0755 "$stage/bin/tgm" "$stage/scripts/install-native.sh" "$stage/scripts/native-release-preflight.sh"
    find "$stage" -type f ! -path "$stage/bin/tgm" ! -path "$stage/scripts/install-native.sh" \
      ! -path "$stage/scripts/native-release-preflight.sh" -exec chmod 0644 {} +
    find "$stage" -type d -exec chmod 0755 {} +
    ln -s bin/tgm "$stage/tgm"
    mv "$stage" "$release_root"
    trap - EXIT
  )

  install -d -o root -g root -m 0755 /etc/systemd/system /etc/tgm /usr/local/bin
  find "$release_root/deploy/systemd" -maxdepth 1 -type f \( -name '*.service' -o -name '*.timer' \) \
    | while IFS= read -r unit; do
        destination="/etc/systemd/system/$(basename "$unit")"
        [ -e "$destination" ] || install -o root -g root -m 0644 "$unit" "$destination"
      done
  find "$release_root/deploy/systemd" -maxdepth 1 -type f -name '*.env.example' \
    | while IFS= read -r example; do
        destination="/etc/tgm/$(basename "$example")"
        [ -e "$destination" ] || install -o root -g root -m 0644 "$example" "$destination"
      done
  atomic_symlink "$release_root/tgm" /usr/local/bin/tgm
  printf 'root 安装完成：%s\n' "$release_root"
  printf '未执行迁移、未启动服务、未启用 timer。\n'
}

install_user() {
  local extracted="$1"
  local version="$2"
  [ "$(id -u)" -ne 0 ] || fail "--user 不接受 root 执行，请使用实际非 root 用户"
  local release_root="$HOME/.local/opt/tgm/releases/$version"
  [ ! -e "$release_root" ] || fail "目标版本目录已存在：$release_root"
  reject_non_symlink_target "$HOME/.local/bin/tgm"
  mkdir -p "$HOME/.local/opt/tgm/releases"
  (
    local stage="$HOME/.local/opt/tgm/releases/.${version}.stage.$$"
    trap 'rm -rf -- "$stage"' EXIT
    install -d -m 0755 "$stage"
    cp -a "$extracted/." "$stage/"
    chmod 0755 "$stage/bin/tgm" "$stage/scripts/install-native.sh" "$stage/scripts/native-release-preflight.sh"
    ln -s bin/tgm "$stage/tgm"
    mv "$stage" "$release_root"
    trap - EXIT
  )
  mkdir -p "$HOME/.local/bin"
  atomic_symlink "$release_root/tgm" "$HOME/.local/bin/tgm"
  printf 'user 安装完成：%s\n' "$release_root"
  printf '未安装 systemd unit，未改变系统服务状态。\n'
}

rollback() {
  local version="$1"
  validate_version "$version"
  if [ "$MODE" = root ]; then
    [ "$(id -u)" -eq 0 ] || fail "--root 必须由 root 执行"
    local release="/opt/tgm/releases/$version/tgm"
    [ -x "$release" ] || fail "root 版本不存在：$release"
    atomic_symlink "$release" /usr/local/bin/tgm
  else
    [ "$(id -u)" -ne 0 ] || fail "--user 不接受 root 执行"
    local release="$HOME/.local/opt/tgm/releases/$version/tgm"
    [ -x "$release" ] || fail "user 版本不存在：$release"
    atomic_symlink "$release" "$HOME/.local/bin/tgm"
  fi
  printf '已切换到版本：%s\n' "$version"
  printf '数据库迁移不会被回滚。\n'
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --root) [ -z "$MODE" ] || fail '--root 与 --user 不能同时使用'; MODE=root; shift ;;
    --user) [ -z "$MODE" ] || fail '--root 与 --user 不能同时使用'; MODE=user; shift ;;
    --archive) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; ARCHIVE="$2"; shift 2 ;;
    --checksums) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; CHECKSUMS="$2"; shift 2 ;;
    --bundle) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; BUNDLE="$2"; shift 2 ;;
    --rollback) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; ROLLBACK="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

[ -n "$MODE" ] || { usage >&2; exit 2; }
if [ -n "$ROLLBACK" ]; then
  [ -z "$ARCHIVE" ] && [ -z "$CHECKSUMS" ] && [ -z "$BUNDLE" ] || fail '回滚不能与发布包参数同时使用'
  rollback "$ROLLBACK"
  exit 0
fi

[ -n "$ARCHIVE" ] && [ -n "$CHECKSUMS" ] && [ -n "$BUNDLE" ] || { usage >&2; exit 2; }
require_command tar
require_command sha256sum
require_command cosign
[ -f "$ARCHIVE" ] || fail "发布包不存在：$ARCHIVE"
[ -f "$CHECKSUMS" ] || fail "SHA256SUMS 不存在：$CHECKSUMS"
[ -f "$BUNDLE" ] || fail "签名 bundle 不存在：$BUNDLE"
verify_outer_checksum "$ARCHIVE" "$CHECKSUMS"
verify_signature "$CHECKSUMS" "$BUNDLE"
verify_archive_members "$ARCHIVE"

temporary="$(mktemp -d "${TMPDIR:-/tmp}/tgm-native-install.XXXXXX")"
trap 'rm -rf "$temporary"' EXIT
tar -xzf "$ARCHIVE" --no-same-owner --no-overwrite-dir -C "$temporary"
package_root="$temporary"
[ -f "$package_root/SHA256SUMS" ] || fail '发布包缺少内部 SHA256SUMS'
(cd "$package_root" && sha256sum -c SHA256SUMS) >/dev/null || fail '包内文件校验失败'
[ -f "$package_root/VERSION" ] || fail '发布包缺少 VERSION'
version="$(read_version "$package_root/VERSION")"
archive_name="$(basename "$ARCHIVE")"
[ "$archive_name" = "tgm-${version}-linux-x86_64-gnu.tar.gz" ] || \
  fail "归档文件名与 VERSION 不一致：$archive_name / $version"
validate_binary "$package_root/bin/tgm" "$package_root"
if [ "$MODE" = root ]; then
  install_root "$package_root" "$version"
else
  install_user "$package_root" "$version"
fi
