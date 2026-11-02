#!/usr/bin/env bash
set -euo pipefail
umask 077

MODE=""
ARCHIVE=""
CHECKSUMS=""
BUNDLE=""
ROLLBACK=""
APP_SERVICES=0
ACCEPT_SAME_UID=0
CANDIDATE=0
ACCEPT_CANDIDATE=0
PROVENANCE=""
PROVENANCE_BUNDLE=""
RELEASE_BUILDINFO=""
RELEASE_SBOM=""
APP_ROLES=(control-plane bot-gateway mtproto-worker fanout rule delivery)
IDENTITY_REGEXP="${COSIGN_CERTIFICATE_IDENTITY_REGEXP:-https://github\\.com/wtfdelphia/tg-monitor-bot/.github/workflows/binary-release\\.yml@refs/(tags/v[0-9]+\\.[0-9]+\\.[0-9]+|heads/main)}"
OIDC_ISSUER="${COSIGN_CERTIFICATE_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"

usage() {
  cat <<'EOF'
用法：
  scripts/install-native.sh --root [--app-services] --archive <包.tar.gz> --checksums <SHA256SUMS> --bundle <SHA256SUMS.sigstore.json>
  scripts/install-native.sh --user [--app-services --accept-same-uid] --archive <包.tar.gz> --checksums <SHA256SUMS> --bundle <SHA256SUMS.sigstore.json>
  scripts/install-native.sh --user --candidate --accept-candidate --archive <包.tar.gz> --checksums <SHA256SUMS> --bundle <签名 bundle> --provenance <provenance.json> --provenance-bundle <provenance 签名 bundle> --buildinfo <BUILDINFO.json> --sbom <sbom.cdx.json>
  scripts/install-native.sh --root --rollback <版本>
  scripts/install-native.sh --user --rollback <版本>

候选包必须显式使用 --candidate --accept-candidate，并提供签名来源证明；候选包不代表稳定版。

安装器只做签名、校验、复制和帮助命令检查，不执行迁移、不启动服务、不启用 timer。
--app-services 只安装应用 unit，不安装 cleanup unit；user 模式须接受同 UID 下没有角色凭据隔离。
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
  check_directory_path "$parent"
  reject_non_symlink_target "$link"
  (
    temporary="$(mktemp -d "$parent/.tgm-link.XXXXXX")"
    trap 'rm -f -- "$temporary/tgm"; rmdir -- "$temporary"' EXIT
    ln -s "$target" "$temporary/tgm"
    mv -Tf "$temporary/tgm" "$link"
  )
}

check_directory_path() {
  local directory="$1" owner mode caller
  caller="$(id -u)"
  case "$directory" in /*) ;; *) fail '安装路径必须为绝对路径' ;; esac
  while :; do
    [ ! -L "$directory" ] || fail "安装父目录是符号链接：$directory"
    if [ -e "$directory" ]; then
      [ -d "$directory" ] || fail "安装父路径不是目录：$directory"
      owner="$(stat -c %u "$directory")"
      mode="$(stat -c %a "$directory")"
      [ "$owner" = 0 ] || [ "$owner" = "$caller" ] || fail "安装父目录属主不可信：$directory"
      [ "$((8#$mode & 0022))" = 0 ] || \
        { [ "$owner" = 0 ] && [ "$((8#$mode & 01000))" != 0 ]; } || \
        fail "安装父目录允许其他身份写入：$directory"
    fi
    [ "$directory" != / ] || break
    directory="$(dirname "$directory")"
  done
}

prepare_directory() {
  local directory="$1" mode="$2"
  check_directory_path "$directory"
  if [ ! -d "$directory" ]; then
    if [ "$MODE" = root ]; then
      install -d -o root -g root -m "$mode" "$directory"
    else
      install -d -m "$mode" "$directory"
    fi
  fi
}

reject_non_symlink_target() {
  local path="$1"
  [ ! -e "$path" ] || [ -L "$path" ] || fail "目标路径不是符号链接，拒绝覆盖：$path"
}

check_existing_file() {
  local source="$1"
  local destination="$2"
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    [ -f "$destination" ] && [ ! -L "$destination" ] || fail "目标路径不是普通文件，拒绝覆盖：$destination"
    local owner mode
    owner="$(stat -c %u "$destination")"
    mode="$(stat -c %a "$destination")"
    [ "$owner" = 0 ] || [ "$owner" = "$(id -u)" ] || fail "目标文件属主不可信：$destination"
    [ "$((8#$mode & 0022))" = 0 ] || fail "目标文件允许其他身份写入：$destination"
    cmp -s "$source" "$destination" || fail "目标文件内容不同，拒绝覆盖：$destination"
  fi
}

install_new_file() {
  local source="$1" destination="$2" mode="$3"
  check_existing_file "$source" "$destination"
  [ ! -e "$destination" ] || return 0
  (
    temporary="$(mktemp "$(dirname "$destination")/.tgm-file.XXXXXX")"
    trap 'rm -f -- "$temporary"' EXIT
    if [ "$MODE" = root ]; then
      install -o root -g root -m "$mode" "$source" "$temporary"
    else
      install -m "$mode" "$source" "$temporary"
    fi
    if ! ln -T -- "$temporary" "$destination" 2>/dev/null; then
      [ -e "$destination" ] || [ -L "$destination" ] || fail "工件无法落地：$destination"
      check_existing_file "$source" "$destination"
    fi
  )
  printf '已写入工件：%s\n' "$destination"
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
  if [ "$CANDIDATE" = 1 ]; then
    cosign verify-blob \
      --offline \
      --bundle "$bundle" \
      --certificate-identity 'https://github.com/wtfdelphia/tg-monitor-bot/.github/workflows/binary-candidate.yml@refs/heads/dev' \
      --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
      "$checksums" >/dev/null || fail "候选 SHA256SUMS 的 cosign 身份验证失败"
    return
  fi
  cosign verify-blob \
    --offline \
    --bundle "$bundle" \
    --certificate-identity-regexp "$IDENTITY_REGEXP" \
    --certificate-oidc-issuer "$OIDC_ISSUER" \
    "$checksums" >/dev/null || fail "SHA256SUMS 的 cosign keyless 签名验证失败"
}

validate_version() {
  local version="$1"
  if [ "$CANDIDATE" = 1 ]; then
    printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+-candidate\.[1-9][0-9]*\.[1-9][0-9]*\.[0-9a-f]{12}$' || \
      fail '候选 VERSION 格式无效'
  else
    printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || \
      fail 'VERSION 文件必须是 MAJOR.MINOR.PATCH；候选包需显式指定 --candidate'
  fi
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
  local arguments=(--binary "$binary" --unit-dir "$package_root/deploy/systemd")
  [ "$APP_SERVICES" = 0 ] || arguments+=(--app-services)
  "$preflight" "${arguments[@]}"
}

check_installed_app_compatibility() {
  local package_root="$1" installed_unit unit APP_SERVICES=1
  if [ "$MODE" = root ]; then
    installed_unit=/etc/systemd/system/tgm-app@.service
    unit="$package_root/deploy/systemd/app/tgm-app@.service"
  else
    installed_unit="$HOME/.config/systemd/user/tgm-app@.service"
    unit="$package_root/deploy/systemd/app/user/tgm-app@.service"
  fi
  if [ -e "$installed_unit" ] || [ -L "$installed_unit" ]; then
    validate_binary "$package_root/bin/tgm" "$package_root"
    check_existing_file "$unit" "$installed_unit"
  fi
}

check_app_account() {
  local account="$1" entry group_entry account_uid account_gid account_home account_shell group_gid group_members password other other_uid other_gid accounts groups
  accounts="$(getent passwd)" || fail '无法枚举本机账号，拒绝安装应用服务'
  groups="$(getent group)" || fail '无法枚举本机组，拒绝安装应用服务'
  if entry="$(getent passwd "$account")"; then
    IFS=: read -r _ _ account_uid account_gid _ account_home account_shell <<< "$entry"
    group_entry="$(getent group "$account")" || fail "服务用户缺少同名组：$account"
    IFS=: read -r _ _ group_gid group_members <<< "$group_entry"
    [ "$account_uid" -ne 0 ] && [ "$group_gid" -ne 0 ] && [ "$account_gid" = "$group_gid" ] || fail "服务账号身份不符合专用非 root 要求：$account"
    [ "$account_home" = "/nonexistent/$account" ] || fail "服务账号 HOME 不符合隔离要求：$account"
    [ ! -e "$account_home" ] && [ ! -L "$account_home" ] || fail "服务账号 HOME 必须不存在：$account"
    case "$account_shell" in /sbin/nologin|/usr/sbin/nologin) ;; *) fail "服务账号允许登录：$account" ;; esac
    [ "$(id -G "$account")" = "$account_gid" ] || fail "服务账号有额外组权限：$account"
    [ -z "$group_members" ] || [ "$group_members" = "$account" ] || fail "服务组包含其他成员：$account"
    entry="$(getent shadow "$account")" || fail "无法检查服务账号密码锁定状态：$account"
    IFS=: read -r _ password _ <<< "$entry"
    case "$password" in '!'*|'*'*) ;; *) fail "服务账号密码未锁定：$account" ;; esac
    while IFS=: read -r other _ other_uid other_gid _; do
      [ "$other" = "$account" ] || { [ "$other_uid" != "$account_uid" ] && [ "$other_gid" != "$account_gid" ]; } || \
        fail "服务账号 UID 或主组与其他身份共享：$account"
    done <<< "$accounts"
    while IFS=: read -r other _ other_gid _; do
      [ "$other" = "$account" ] || [ "$other_gid" != "$account_gid" ] || \
        fail "服务组 GID 与其他组共享：$account"
    done <<< "$groups"
  elif group_entry="$(getent group "$account")"; then
    IFS=: read -r _ _ group_gid group_members <<< "$group_entry"
    [ -z "$group_members" ] || fail "既有服务组包含其他成员：$account"
    [ "$group_gid" -ne 0 ] || fail "服务组不得使用 root 组：$account"
    while IFS=: read -r other _ _ other_gid _; do
      [ "$other_gid" != "$group_gid" ] || fail "既有服务组被其他账号用作主组：$account"
    done <<< "$accounts"
    while IFS=: read -r other _ other_gid _; do
      [ "$other" = "$account" ] || [ "$other_gid" != "$group_gid" ] || \
        fail "既有服务组 GID 与其他组共享：$account"
    done <<< "$groups"
  fi
}

check_app_services() {
  local extracted="$1" role account passwd_entry account_home
  [ -f "$extracted/deploy/systemd/app/tgm-app@.service" ] || fail '发布包缺少应用服务模板'
  [ -f "$extracted/deploy/systemd/app/user/tgm-app@.service" ] || fail '发布包缺少用户级应用服务模板'
  for role in "${APP_ROLES[@]}"; do
    [ -f "$extracted/deploy/systemd/app/$role.env.example" ] || fail "发布包缺少角色配置示例：$role"
  done
  if [ "$MODE" = root ]; then
    check_directory_path /etc/tgm/app
    check_directory_path /etc/systemd/system
    require_command getent
    require_command groupadd
    require_command useradd
    require_command nologin
    check_existing_file "$extracted/deploy/systemd/app/tgm-app@.service" /etc/systemd/system/tgm-app@.service
    for role in "${APP_ROLES[@]}"; do
      check_app_account "tgm-$role"
      check_existing_file "$extracted/deploy/systemd/app/$role.env.example" "/etc/tgm/app/$role.env.example"
    done
  else
    [ "$ACCEPT_SAME_UID" = 1 ] || fail 'user 应用服务不隔离角色凭据；须显式传入 --accept-same-uid'
    require_command systemctl
    require_command getent
    passwd_entry="$(getent passwd "$(id -u)")" || fail '无法确定用户级服务的 HOME'
    IFS=: read -r _ _ _ _ _ account_home _ <<< "$passwd_entry"
    [ "$HOME" = "$account_home" ] || fail '用户级 unit 的 %h 与 HOME 不一致，拒绝安装'
    check_directory_path "$HOME/.config/tgm/app"
    check_directory_path "$HOME/.config/systemd/user"
    systemctl --user show-environment >/dev/null 2>&1 || fail '用户级 systemd manager 不可用；可只安装二进制，或选 root 系统级安装'
    check_existing_file "$extracted/deploy/systemd/app/user/tgm-app@.service" "$HOME/.config/systemd/user/tgm-app@.service"
    for role in "${APP_ROLES[@]}"; do
      check_existing_file "$extracted/deploy/systemd/app/$role.env.example" "$HOME/.config/tgm/app/$role.env.example"
    done
  fi
}

install_app_services() {
  local release_root="$1" role account destination
  if [ "$MODE" = root ]; then
    for role in "${APP_ROLES[@]}"; do
      account="tgm-$role"
      if ! getent group "$account" >/dev/null; then
        groupadd --system "$account"
        printf '已创建服务组：%s\n' "$account"
      fi
      if ! getent passwd "$account" >/dev/null; then
        useradd --system --gid "$account" \
          --home-dir "/nonexistent/$account" --shell "$(command -v nologin)" --no-create-home "$account"
        printf '已创建服务账号：%s\n' "$account"
      fi
      check_app_account "$account"
    done
    prepare_directory /etc/tgm/app 0755
    destination=/etc/systemd/system/tgm-app@.service
    install_new_file "$release_root/deploy/systemd/app/tgm-app@.service" "$destination" 0644
    for role in "${APP_ROLES[@]}"; do
      destination="/etc/tgm/app/$role.env.example"
      install_new_file "$release_root/deploy/systemd/app/$role.env.example" "$destination" 0644
    done
  else
    prepare_directory "$HOME/.config/tgm/app" 0700
    prepare_directory "$HOME/.config/systemd/user" 0755
    destination="$HOME/.config/systemd/user/tgm-app@.service"
    install_new_file "$release_root/deploy/systemd/app/user/tgm-app@.service" "$destination" 0644
    for role in "${APP_ROLES[@]}"; do
      destination="$HOME/.config/tgm/app/$role.env.example"
      install_new_file "$release_root/deploy/systemd/app/$role.env.example" "$destination" 0600
    done
    printf '用户级服务同 UID 运行，不提供角色间凭据隔离；未设置 linger，退出登录后的持续运行须另行确认。\n'
  fi
  printf '已安装六角色应用模板和示例；未创建实际环境文件，未 daemon-reload，未启动或启用服务。\n'
}

install_root() {
  local extracted="$1"
  local version="$2"
  [ "$(id -u)" -eq 0 ] || fail "--root 必须由 root 执行"
  local release_root="/opt/tgm/releases/$version"
  check_directory_path /opt/tgm/releases
  check_directory_path /usr/local/bin
  check_directory_path /etc/systemd/system
  check_directory_path /etc/tgm
  [ ! -e "$release_root" ] && [ ! -L "$release_root" ] || fail "目标版本目录已存在：$release_root"
  reject_non_symlink_target /usr/local/bin/tgm
  [ "$APP_SERVICES" = 0 ] || check_app_services "$extracted"
  if [ "$APP_SERVICES" = 0 ]; then
    while IFS= read -r unit; do
      check_existing_file "$unit" "/etc/systemd/system/$(basename "$unit")"
    done < <(find "$extracted/deploy/systemd" -maxdepth 1 -type f \( -name '*.service' -o -name '*.timer' \) -print | sort)
    while IFS= read -r example; do
      destination="/etc/tgm/$(basename "$example")"
      [ ! -L "$destination" ] || fail "环境示例目标是符号链接，拒绝安装：$destination"
      [ ! -e "$destination" ] || [ -f "$destination" ] || \
        fail "环境示例目标不是普通文件，拒绝安装：$destination"
    done < <(find "$extracted/deploy/systemd" -maxdepth 1 -type f -name '*.env.example' -print | sort)
  fi
  prepare_directory /opt/tgm/releases 0755
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
    mv -T -n "$stage" "$release_root"
    [ ! -e "$stage" ] || fail "版本目录在安装期间被创建，拒绝覆盖：$release_root"
    trap - EXIT
  )
  printf '已创建版本目录：%s\n' "$release_root"

  prepare_directory /etc/systemd/system 0755
  prepare_directory /etc/tgm 0755
  prepare_directory /usr/local/bin 0755
  if [ "$APP_SERVICES" = 0 ]; then
    while IFS= read -r unit; do
      destination="/etc/systemd/system/$(basename "$unit")"
      install_new_file "$unit" "$destination" 0644
    done < <(find "$release_root/deploy/systemd" -maxdepth 1 -type f \( -name '*.service' -o -name '*.timer' \) -print)
    while IFS= read -r example; do
      destination="/etc/tgm/$(basename "$example")"
      install_new_file "$example" "$destination" 0644
    done < <(find "$release_root/deploy/systemd" -maxdepth 1 -type f -name '*.env.example' -print)
  fi
  [ "$APP_SERVICES" = 0 ] || install_app_services "$release_root"
  atomic_symlink "$release_root/tgm" /usr/local/bin/tgm
  printf 'root 安装完成：%s\n' "$release_root"
  printf '未执行迁移、未启动服务、未启用 timer。\n'
}

install_user() {
  local extracted="$1"
  local version="$2"
  [ "$(id -u)" -ne 0 ] || fail "--user 不接受 root 执行，请使用实际非 root 用户"
  local release_root="$HOME/.local/opt/tgm/releases/$version"
  check_directory_path "$HOME/.local/opt/tgm/releases"
  check_directory_path "$HOME/.local/bin"
  [ ! -e "$release_root" ] && [ ! -L "$release_root" ] || fail "目标版本目录已存在：$release_root"
  reject_non_symlink_target "$HOME/.local/bin/tgm"
  [ "$APP_SERVICES" = 0 ] || check_app_services "$extracted"
  prepare_directory "$HOME/.local/opt/tgm/releases" 0755
  (
    local stage="$HOME/.local/opt/tgm/releases/.${version}.stage.$$"
    trap 'rm -rf -- "$stage"' EXIT
    install -d -m 0755 "$stage"
    cp -a "$extracted/." "$stage/"
    chmod 0755 "$stage/bin/tgm" "$stage/scripts/install-native.sh" "$stage/scripts/native-release-preflight.sh"
    ln -s bin/tgm "$stage/tgm"
    mv -T -n "$stage" "$release_root"
    [ ! -e "$stage" ] || fail "版本目录在安装期间被创建，拒绝覆盖：$release_root"
    trap - EXIT
  )
  printf '已创建版本目录：%s\n' "$release_root"
  prepare_directory "$HOME/.local/bin" 0755
  [ "$APP_SERVICES" = 0 ] || install_app_services "$release_root"
  atomic_symlink "$release_root/tgm" "$HOME/.local/bin/tgm"
  printf 'user 安装完成：%s\n' "$release_root"
  printf '未改变系统服务状态。\n'
}

install_candidate() {
  local extracted="$1"
  local version="$2"
  [ "$(id -u)" -ne 0 ] || fail '候选包不接受 root 执行'
  local candidate_root="$HOME/.local/opt/tgm/candidates/$version"
  check_directory_path "$HOME/.local/opt/tgm/candidates"
  [ ! -e "$candidate_root" ] && [ ! -L "$candidate_root" ] || fail "候选目录已存在：$candidate_root"
  prepare_directory "$HOME/.local/opt/tgm/candidates" 0755
  (
    local stage
    stage="$(mktemp -d "$HOME/.local/opt/tgm/candidates/.${version}.stage.XXXXXX")"
    trap 'rm -rf -- "$stage"' EXIT
    cp -a "$extracted/." "$stage/"
    chmod 0755 "$stage/bin/tgm" "$stage/scripts/install-native.sh" \
      "$stage/scripts/native-release-preflight.sh" "$stage/scripts/verify-native-candidate.py"
    mv -T -n "$stage" "$candidate_root"
    [ ! -e "$stage" ] || fail "候选目录在安装期间被创建，拒绝覆盖：$candidate_root"
    trap - EXIT
  )
  printf '已隔离安装候选包：%s\n' "$candidate_root"
  printf '未切换 ~/.local/bin/tgm，未安装或改变 systemd unit。\n'
  printf '验收二进制：%s/bin/tgm\n' "$candidate_root"
}

rollback() {
  local version="$1"
  validate_version "$version"
  local release_root link
  if [ "$MODE" = root ]; then
    [ "$(id -u)" -eq 0 ] || fail "--root 必须由 root 执行"
    release_root="/opt/tgm/releases/$version"
    link=/usr/local/bin/tgm
  else
    [ "$(id -u)" -ne 0 ] || fail "--user 不接受 root 执行"
    release_root="$HOME/.local/opt/tgm/releases/$version"
    link="$HOME/.local/bin/tgm"
  fi
  check_directory_path "$release_root"
  [ -x "$release_root/tgm" ] || fail "版本不存在：$release_root"
  check_installed_app_compatibility "$release_root"
  atomic_symlink "$release_root/tgm" "$link"
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
    --provenance) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; PROVENANCE="$2"; shift 2 ;;
    --provenance-bundle) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; PROVENANCE_BUNDLE="$2"; shift 2 ;;
    --buildinfo) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; RELEASE_BUILDINFO="$2"; shift 2 ;;
    --sbom) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; RELEASE_SBOM="$2"; shift 2 ;;
    --rollback) [ "$#" -ge 2 ] || { usage >&2; exit 2; }; ROLLBACK="$2"; shift 2 ;;
    --candidate) CANDIDATE=1; shift ;;
    --accept-candidate) ACCEPT_CANDIDATE=1; shift ;;
    --app-services) APP_SERVICES=1; shift ;;
    --accept-same-uid) ACCEPT_SAME_UID=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

[ -n "$MODE" ] || { usage >&2; exit 2; }
[ "$ACCEPT_CANDIDATE" = 0 ] || [ "$CANDIDATE" = 1 ] || fail '--accept-candidate 需要 --candidate'
[ "$CANDIDATE" = 0 ] || [ "$ACCEPT_CANDIDATE" = 1 ] || fail '验收候选包必须显式指定 --accept-candidate'
if [ "$CANDIDATE" = 1 ]; then
  [ "$MODE" = user ] || fail '候选包只允许安装到非 root 用户目录'
  [ "$APP_SERVICES" = 0 ] || fail '候选包不安装 systemd unit'
fi
[ "$ACCEPT_SAME_UID" = 0 ] || { [ "$MODE" = user ] && [ "$APP_SERVICES" = 1 ]; } || fail '--accept-same-uid 仅用于 user 应用服务安装'
if [ -n "$ROLLBACK" ]; then
  [ "$APP_SERVICES" = 0 ] || fail '回滚只切换已安装的二进制，不安装应用 unit'
  [ -z "$ARCHIVE" ] && [ -z "$CHECKSUMS" ] && [ -z "$BUNDLE" ] && \
    [ -z "$PROVENANCE" ] && [ -z "$PROVENANCE_BUNDLE" ] && \
    [ -z "$RELEASE_BUILDINFO" ] && [ -z "$RELEASE_SBOM" ] || fail '回滚不能与发布包参数同时使用'
  [ "$CANDIDATE" = 0 ] && [ -z "$PROVENANCE" ] && [ -z "$PROVENANCE_BUNDLE" ] && \
    [ -z "$RELEASE_BUILDINFO" ] && [ -z "$RELEASE_SBOM" ] || fail '回滚不能使用候选验证参数'
  rollback "$ROLLBACK"
  exit 0
fi

[ -n "$ARCHIVE" ] && [ -n "$CHECKSUMS" ] && [ -n "$BUNDLE" ] || { usage >&2; exit 2; }
if [ "$CANDIDATE" = 1 ]; then
  [ -n "$PROVENANCE" ] && [ -n "$PROVENANCE_BUNDLE" ] && \
    [ -n "$RELEASE_BUILDINFO" ] && [ -n "$RELEASE_SBOM" ] || \
    fail '候选包必须提供 provenance、BUILDINFO、SBOM 与对应签名材料'
  [ "$MODE" = user ] || fail '候选包只允许安装到非 root 用户目录'
  [ "$APP_SERVICES" = 0 ] || fail '候选包不安装 systemd unit'
else
  [ -z "$PROVENANCE" ] && [ -z "$PROVENANCE_BUNDLE" ] && \
    [ -z "$RELEASE_BUILDINFO" ] && [ -z "$RELEASE_SBOM" ] || \
    fail '稳定包不能携带候选来源参数'
fi
require_command tar
require_command sha256sum
require_command cosign
[ "$CANDIDATE" = 0 ] || require_command python3
[ -f "$ARCHIVE" ] || fail "发布包不存在：$ARCHIVE"
[ -f "$CHECKSUMS" ] || fail "SHA256SUMS 不存在：$CHECKSUMS"
[ -f "$BUNDLE" ] || fail "签名 bundle 不存在：$BUNDLE"
[ "$CANDIDATE" = 0 ] || [ -f "$PROVENANCE" ] || fail "provenance 不存在：$PROVENANCE"
[ "$CANDIDATE" = 0 ] || [ -f "$PROVENANCE_BUNDLE" ] || fail "provenance 签名 bundle 不存在：$PROVENANCE_BUNDLE"
[ "$CANDIDATE" = 0 ] || [ -f "$RELEASE_BUILDINFO" ] || fail "候选 BUILDINFO 不存在：$RELEASE_BUILDINFO"
[ "$CANDIDATE" = 0 ] || [ -f "$RELEASE_SBOM" ] || fail "候选 SBOM 不存在：$RELEASE_SBOM"
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
if [ "$CANDIDATE" = 1 ]; then
  [ -f "$package_root/BUILDINFO.json" ] || fail '候选包缺少 BUILDINFO.json'
  python3 "$package_root/scripts/verify-native-candidate.py" \
    --archive "$ARCHIVE" --checksums "$CHECKSUMS" --checksum-bundle "$BUNDLE" \
    --provenance "$PROVENANCE" --provenance-bundle "$PROVENANCE_BUNDLE" \
    --buildinfo "$RELEASE_BUILDINFO" --sbom "$RELEASE_SBOM" \
    --package-buildinfo "$package_root/BUILDINFO.json" \
    --package-sbom "$package_root/sbom.cdx.json" --version "$version"
else
  [ -f "$package_root/BUILDINFO.json" ] || fail '发布包缺少 BUILDINFO.json'
fi
if [ "$CANDIDATE" = 1 ]; then
  validate_binary "$package_root/bin/tgm" "$package_root"
  install_candidate "$package_root" "$version"
  exit 0
fi
validate_binary "$package_root/bin/tgm" "$package_root"
check_installed_app_compatibility "$package_root"
if [ "$MODE" = root ]; then
  install_root "$package_root" "$version"
else
  install_user "$package_root" "$version"
fi
