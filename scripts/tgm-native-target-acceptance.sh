#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export LC_ALL=C

REPOSITORY="wtfdelphia/tg-monitor-bot"
TAG="${TGM_CANDIDATE_TAG:-candidate-42399939250-a1-eb973d62f24d}"
IDENTITY="https://github.com/wtfdelphia/tg-monitor-bot/.github/workflows/binary-candidate.yml@refs/heads/dev"
ISSUER="https://token.actions.githubusercontent.com"
MODE="auto"
MATERIALS=""
NO_DOWNLOAD=0
SKIP_INSTALL=0
SKIP_JOURNAL=0
KEEP_WORK=0
HELPERS_DIR=""
EXPECTED_SOURCE_COMMIT="${TGM_EXPECTED_SOURCE_COMMIT:-}"
WORK=""

# 下载相关（可用环境变量覆盖）
DOWNLOAD_SOURCE="${TGM_DOWNLOAD_SOURCE:-auto}"          # auto | direct | api | gh
USE_GH_TOKEN="${TGM_USE_GH_TOKEN:-1}"                  # 1=无环境变量 token 时尝试 gh auth token
GH_READY=0
TOKEN_ORIGIN="none"
DOWNLOAD_ATTEMPTS="${TGM_DOWNLOAD_ATTEMPTS:-4}"         # 每个文件的整体轮次
DOWNLOAD_BACKOFF="${TGM_DOWNLOAD_BACKOFF:-10}"          # 轮次间等待基数（秒，线性递增）
CONNECT_TIMEOUT="${TGM_CONNECT_TIMEOUT:-20}"
GITHUB_WEB="${TGM_GITHUB_WEB:-https://github.com}"
GITHUB_API="${TGM_GITHUB_API:-https://api.github.com}"
REUSE_MATERIALS=0
CURL_BASE=()
CURL_AUTH_CONFIG=""
RELEASE_JSON=""
DIRECT_COUNT=0
API_COUNT=0
GH_COUNT=0
REUSED_COUNT=0

usage() {
  cat <<'EOF'
用法：
  bash scripts/tgm-native-target-acceptance.sh [auto|rocky|centos7] [材料目录]
  bash scripts/tgm-native-target-acceptance.sh --mode auto --tag <candidate-tag> \
    [--materials <目录>] [--helpers-dir <目录>] [--no-download] [--reuse-materials] \
    [--download-source auto|direct|api|gh] [--no-gh-token] [--skip-install] [--skip-journal] [--keep-work]

脚本自动完成：候选 Release 下载、签名与摘要验证、provenance 来源验证、候选隔离安装、
原生服务预检，以及 Rocky 目标机的六角色非生产运行时验收。

模式：
  auto       从 /etc/os-release 自动识别 Rocky 或 CentOS 7（默认）
  rocky      强制 Rocky 运行时验收
  centos7    强制 CentOS 7 负向原生服务验收

安全边界：
  - 仅使用非生产候选包；不连接生产服务，不启用生产服务或 cleanup timer。
  - Rocky 六角色验收会在本机临时 PostgreSQL、Redis、MinIO 中执行正式迁移和审计，数据仅限本次临时环境。
  - CentOS 7 的 systemd -SECCOMP 会按策略拒绝应用原生服务预检，不允许绕过。
  - Rocky 默认要求 sudo -n 只读 journalctl；--skip-journal 仅用于明确的未完成补测。

下载：
  --download-source auto   依次尝试 Release 直链 → GitHub API 资产端点 → gh release download
                           （gh 已安装并登录时才会加入）（默认）
  --download-source direct 只走 Release 直链（releases/download）
  --download-source api    只走 API 资产端点（releases/assets/<id>）
  --download-source gh     只用 gh release download（需 gh 已登录）
  --no-gh-token            不从 gh 读取 token（等同 TGM_USE_GH_TOKEN=0）
  --reuse-materials        材料目录中已有的普通文件直接复用，只补下缺失文件；
                           复用的文件同样要经过 cosign、SHA-256、provenance 校验
  --no-download            完全离线，材料必须事先齐全
  环境变量：TGM_DOWNLOAD_ATTEMPTS（默认 4）、TGM_DOWNLOAD_BACKOFF（默认 10 秒）、
           TGM_CONNECT_TIMEOUT（默认 20 秒）。
  token 优先级：TGM_GITHUB_TOKEN > GITHUB_TOKEN > GH_TOKEN > `gh auth token`。
           token 只发往 API 主机，不随跳转外发，不回显，也不出现在进程参数中。
  代理沿用 shell 的 https_proxy / HTTPS_PROXY；Rocky 模式下 Docker 守护进程的代理需另行
  配置（脚本会检测并告警）。

默认候选可用 --tag 或 TGM_CANDIDATE_TAG 覆盖。材料目录默认新建在
$HOME/tgm-candidate.XXXXXX，并在结束后保留为验收证据。
EOF
}

fail() {
  printf '验收失败：%s\n' "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令：$1"
}

find_python3() {
  local candidate
  for candidate in python3 python3.13 python3.12 python3.11 python3.10 python3.9 python3.8 python3.7 python3.6; do
    if command -v "$candidate" >/dev/null 2>&1; then
      command -v "$candidate"
      return 0
    fi
  done
  return 1
}

cleanup() {
  local status=$?
  if [[ -n "$CURL_AUTH_CONFIG" ]]; then
    rm -f -- "$CURL_AUTH_CONFIG"
  fi
  if [[ "$status" -ne 0 || "$KEEP_WORK" == 1 ]]; then
    [[ -n "$WORK" ]] && printf '保留临时诊断目录：%s\n' "$WORK" >&2
  elif [[ -n "$WORK" ]]; then
    rm -rf -- "$WORK"
  fi
  return "$status"
}

fail_with_output() {
  local message="$1"
  local output="$2"
  printf '%s\n' "$output" >&2
  fail "$message"
}

detect_mode() {
  local os_id="" version_id="" major_version=""
  if [[ -r /etc/os-release ]]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    os_id="${ID:-}"
    version_id="${VERSION_ID:-}"
  fi
  major_version="${version_id%%.*}"
  case "$os_id" in
    rocky)
      printf 'rocky\n'
      ;;
    centos)
      [[ "$major_version" == 7 ]] || fail "检测到 CentOS $version_id；本脚本只支持 CentOS 7 负向验收"
      printf 'centos7\n'
      ;;
    *)
      if [[ -r /etc/rocky-release ]]; then
        printf 'rocky\n'
      elif [[ -r /etc/centos-release ]] && grep -q 'release 7' /etc/centos-release; then
        printf 'centos7\n'
      else
        fail "无法自动识别受支持的系统；请显式传入 rocky 或 centos7"
      fi
      ;;
  esac
}

prepare_materials() {
  if [[ -z "$MATERIALS" ]]; then
    MATERIALS="$(mktemp -d "$HOME/tgm-candidate.XXXXXX")"
  elif [[ ! -e "$MATERIALS" ]]; then
    mkdir -m 700 -- "$MATERIALS"
  fi
  [[ -d "$MATERIALS" && ! -L "$MATERIALS" ]] || fail "材料目录必须是普通目录：$MATERIALS"
  MATERIALS="$(cd "$MATERIALS" && pwd -P)"
  local owner mode
  owner="$(stat -c '%u' "$MATERIALS")"
  mode="$(stat -c '%a' "$MATERIALS")"
  [[ "$owner" == "$(id -u)" ]] || fail "材料目录不属于当前用户：$MATERIALS"
  [[ "$((8#$mode & 0077))" == 0 ]] || fail "材料目录权限过宽，要求 0700 或更严格：$MATERIALS"
}

validate_material_file() {
  local path="$1" owner mode
  [[ -f "$path" && ! -L "$path" ]] || fail "材料必须是普通文件：$path"
  owner="$(stat -c '%u' "$path")"
  mode="$(stat -c '%a' "$path")"
  [[ "$owner" == "$(id -u)" ]] || fail "材料文件不属于当前用户：$path"
  [[ "$((8#$mode & 0077))" == 0 ]] || fail "材料文件权限过宽，要求 0600 或更严格：$path"
}

mask_proxy() {
  printf '%s' "$1" | sed -E 's#//[^/@]*@#//***@#'
}

init_curl() {
  CURL_BASE=(-fsS --proto '=https' --tlsv1.2 --connect-timeout "$CONNECT_TIMEOUT"
    --speed-limit 1024 --speed-time 60 --retry 2 --retry-delay 2)
  # --retry-all-errors 需 curl >= 7.71（Rocky 9 有，CentOS 7 的 7.29 没有）
  if curl --help all 2>/dev/null | grep -F -- '--retry-all-errors' >/dev/null; then
    CURL_BASE+=(--retry-all-errors)
  fi
  local token=""
  if [[ -n "${TGM_GITHUB_TOKEN:-}" ]]; then
    token="$TGM_GITHUB_TOKEN"; TOKEN_ORIGIN="TGM_GITHUB_TOKEN"
  elif [[ -n "${GITHUB_TOKEN:-}" ]]; then
    token="$GITHUB_TOKEN"; TOKEN_ORIGIN="GITHUB_TOKEN"
  elif [[ -n "${GH_TOKEN:-}" ]]; then
    token="$GH_TOKEN"; TOKEN_ORIGIN="GH_TOKEN"
  fi
  if command -v gh >/dev/null 2>&1 && gh auth status --hostname github.com >/dev/null 2>&1; then
    GH_READY=1
    if [[ -z "$token" && "$USE_GH_TOKEN" == 1 ]]; then
      # gh >= 2.17 才有 auth token 子命令；失败则保持匿名
      token="$(gh auth token --hostname github.com 2>/dev/null || true)"
      [[ -z "$token" ]] || TOKEN_ORIGIN="gh auth token"
    fi
  fi
  if [[ -n "$token" ]]; then
    [[ "$token" =~ ^[A-Za-z0-9_]+$ ]] || fail 'GitHub token 含非法字符'
    CURL_AUTH_CONFIG="$WORK/curl-auth.conf"
    ( umask 077; printf 'header = "Authorization: Bearer %s"\n' "$token" >"$CURL_AUTH_CONFIG" )
  fi
  local proxy="${https_proxy:-${HTTPS_PROXY:-${all_proxy:-${ALL_PROXY:-}}}}"
  if [[ -n "$proxy" ]]; then
    printf 'HTTPS 代理：%s\n' "$(mask_proxy "$proxy")"
  else
    printf 'HTTPS 代理：未设置（直连）\n'
  fi
  printf '下载来源：%s（每文件最多 %s 轮）\n' "$DOWNLOAD_SOURCE" "$DOWNLOAD_ATTEMPTS"
  printf 'GitHub API 认证：%s\n' "$([[ -n "$CURL_AUTH_CONFIG" ]] && printf '%s（不回显）' "$TOKEN_ORIGIN" || printf '匿名')"
  printf 'gh CLI：%s\n' "$([[ "$GH_READY" == 1 ]] && printf '已登录' || printf '不可用或未登录')"
  if [[ "$DOWNLOAD_SOURCE" == gh && "$GH_READY" != 1 ]]; then
    fail '--download-source gh 需要已安装并登录的 gh（gh auth login）'
  fi
}

curl_api() {
  # 仅用于 API 主机；绝不带 -L，避免认证头随跳转外发
  if [[ -n "$CURL_AUTH_CONFIG" ]]; then
    curl "${CURL_BASE[@]}" -K "$CURL_AUTH_CONFIG" -H 'X-GitHub-Api-Version: 2022-11-28' "$@"
  else
    curl "${CURL_BASE[@]}" -H 'X-GitHub-Api-Version: 2022-11-28' "$@"
  fi
}

fetch_release_json() {
  [[ -z "$RELEASE_JSON" ]] || return 0
  local target="$WORK/release.json"
  if ! curl_api -H 'Accept: application/vnd.github+json' -o "$target" \
    "$GITHUB_API/repos/$REPOSITORY/releases/tags/$TAG"; then
    rm -f -- "$target"
    printf '  API 获取 Release 元数据失败（可能被限流；可 gh auth login 或设置 GITHUB_TOKEN 后重试）\n' >&2
    return 1
  fi
  RELEASE_JSON="$target"
}

# 输出：<asset_id>\t<size>
lookup_asset() {
  "$PYTHON3" - "$RELEASE_JSON" "$TAG" "$1" <<'PY'
import json
import sys

path, tag, name = sys.argv[1:4]
try:
    with open(path, encoding="utf-8") as stream:
        release = json.load(stream)
except (OSError, ValueError) as error:
    raise SystemExit("Release 元数据无法解析：%s" % error)
if release.get("tag_name") != tag:
    raise SystemExit("Release 元数据 tag 不匹配：%r" % release.get("tag_name"))
matches = [a for a in release.get("assets", []) if a.get("name") == name]
if len(matches) != 1:
    raise SystemExit("Release 中找不到唯一资产：%s" % name)
asset = matches[0]
if asset.get("state") not in (None, "uploaded"):
    raise SystemExit("资产状态异常：%s %r" % (name, asset.get("state")))
asset_id, size = asset.get("id"), asset.get("size")
if not isinstance(asset_id, int) or not isinstance(size, int) or size < 0:
    raise SystemExit("资产元数据无效：%s" % name)
print("%d\t%d" % (asset_id, size))
PY
}

fetch_direct() {
  local name="$1" output="$2"
  curl "${CURL_BASE[@]}" -L -o "$output" \
    "$GITHUB_WEB/$REPOSITORY/releases/download/$TAG/$name"
}

fetch_api() {
  local name="$1" output="$2" info asset_id size response_code location actual
  fetch_release_json || return 1
  info="$(lookup_asset "$name")" || return 1
  IFS=$'\t' read -r asset_id size <<<"$info"
  # 第一步：向 API 取一次性下载地址（不跟随跳转，不落盘正文以外的东西）
  response_code="$(curl_api -H 'Accept: application/octet-stream' \
    -o /dev/null -w '%{http_code}\t%{redirect_url}' \
    "$GITHUB_API/repos/$REPOSITORY/releases/assets/$asset_id")" || return 1
  IFS=$'\t' read -r response_code location <<<"$response_code"
  [[ "$response_code" =~ ^3[0-9][0-9]$ ]] || {
    printf '  API 资产没有返回下载跳转：HTTP %s\n' "$response_code" >&2
    return 1
  }
  if [[ -n "$location" ]]; then
    [[ "$location" == https://* ]] || { printf '  API 返回了非 HTTPS 跳转\n' >&2; return 1; }
    # 第二步：匿名下载签名地址（不带认证头）
    curl "${CURL_BASE[@]}" -L -o "$output" "$location" || return 1
  fi
  [[ -f "$output" ]] || {
    printf '  API 资产下载地址为空：%s\n' "$name" >&2
    return 1
  }
  actual="$(stat -c '%s' "$output")"
  if [[ "$actual" != "$size" ]]; then
    printf '  大小不符：%s 实得 %s，期望 %s\n' "$name" "$actual" "$size" >&2
    return 1
  fi
}

fetch_gh() {
  local name="$1" output="$2" directory
  [[ "$GH_READY" == 1 ]] || return 1
  directory="$(mktemp -d "$WORK/gh-download.XXXXXX")"
  # gh 自行处理认证与跳转；--pattern 是 glob，资产名不含通配符
  if ! gh release download "$TAG" --repo "$REPOSITORY" --pattern "$name" \
    --dir "$directory" >/dev/null; then
    rm -rf -- "$directory"
    return 1
  fi
  if [[ ! -f "$directory/$name" || -L "$directory/$name" ]]; then
    rm -rf -- "$directory"
    return 1
  fi
  mv -f -- "$directory/$name" "$output"
  rm -rf -- "$directory"
}

download_asset() {
  local name="$1" destination temporary attempt source used=""
  destination="$MATERIALS/$name"
  if [[ "$NO_DOWNLOAD" == 1 ]]; then
    validate_material_file "$destination"
    return 0
  fi
  if [[ -e "$destination" || -L "$destination" ]]; then
    if [[ "$REUSE_MATERIALS" == 1 && -f "$destination" && ! -L "$destination" && -s "$destination" ]]; then
      validate_material_file "$destination"
      printf '复用已有材料：%s\n' "$name"
      REUSED_COUNT=$((REUSED_COUNT + 1))
      return 0
    fi
    fail "材料已存在，拒绝覆盖：$destination；补下缺失文件请用 --reuse-materials，完全离线用 --no-download，或换用新的材料目录"
  fi
  temporary="$MATERIALS/.${name}.download.$$"
  [[ ! -e "$temporary" && ! -L "$temporary" ]] || fail "下载临时文件已存在：$temporary"

  local sources=()
  case "$DOWNLOAD_SOURCE" in
    auto)
      sources=(direct api)
      [[ "$GH_READY" != 1 ]] || sources+=(gh)
      ;;
    direct) sources=(direct) ;;
    api) sources=(api) ;;
    gh) sources=(gh) ;;
  esac

  for ((attempt = 1; attempt <= DOWNLOAD_ATTEMPTS; attempt++)); do
    for source in "${sources[@]}"; do
      rm -f -- "$temporary"
      printf '下载 %s（第 %d 轮，%s）\n' "$name" "$attempt" "$source"
      if "fetch_$source" "$name" "$temporary" && [[ -f "$temporary" ]]; then
        used="$source"
        break 2
      fi
      printf '  %s 失败\n' "$source" >&2
    done
    if ((attempt < DOWNLOAD_ATTEMPTS)); then
      sleep $((DOWNLOAD_BACKOFF * attempt))
    fi
  done
  if [[ -z "$used" ]]; then
    rm -f -- "$temporary"
    fail "候选材料下载失败：$name（已尝试 ${DOWNLOAD_ATTEMPTS} 轮：${sources[*]}）"
  fi
  case "$used" in
    direct) DIRECT_COUNT=$((DIRECT_COUNT + 1)) ;;
    api) API_COUNT=$((API_COUNT + 1)) ;;
    gh) GH_COUNT=$((GH_COUNT + 1)) ;;
  esac
  chmod 600 -- "$temporary"
  if ! ln -- "$temporary" "$destination" 2>/dev/null; then
    rm -f -- "$temporary"
    fail "材料落盘失败或目标已被并发创建：$destination"
  fi
  rm -f -- "$temporary"
}

download_summary() {
  if [[ "$NO_DOWNLOAD" == 1 ]]; then
    printf 'offline'
  else
    printf 'direct=%d api=%d gh=%d reused=%d auth=%s' "$DIRECT_COUNT" "$API_COUNT" "$GH_COUNT" \
      "$REUSED_COUNT" "$TOKEN_ORIGIN"
  fi
}

check_docker_proxy() {
  local shell_proxy="${https_proxy:-${HTTPS_PROXY:-}}" daemon_proxy
  [[ -n "$shell_proxy" ]] || return 0
  daemon_proxy="$(docker info --format '{{.HTTPSProxy}}' 2>/dev/null || true)"
  if [[ -z "$daemon_proxy" ]]; then
    printf '警告：shell 设置了 HTTPS 代理，但 Docker 守护进程未配置代理；\n' >&2
    printf '      六角色验收拉取 PostgreSQL / Redis / MinIO 镜像可能失败。\n' >&2
    printf '      可在 /etc/systemd/system/docker.service.d/proxy.conf 配置后 restart docker。\n' >&2
  else
    printf 'Docker 守护进程代理：%s\n' "$(mask_proxy "$daemon_proxy")"
  fi
}

read_buildinfo() {
  "$PYTHON3" - "$MATERIALS/candidate-BUILDINFO.json" "$TAG" <<'PY'
import json
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
tag = sys.argv[2]
try:
    value = json.loads(path.read_text(encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"无法读取 candidate-BUILDINFO.json：{error}")
if not isinstance(value, dict):
    raise SystemExit("candidate-BUILDINFO.json 根节点必须是对象")
if value.get("channel") != "candidate" or value.get("stable") is not False:
    raise SystemExit("候选 BUILDINFO 未标记为非稳定候选")
source_commit = value.get("source_commit")
version = value.get("version")
artifact = value.get("artifact")
release_tag = value.get("release_tag")
if not isinstance(source_commit, str) or not re.fullmatch(r"[0-9a-f]{40}", source_commit):
    raise SystemExit("候选 BUILDINFO 的 source_commit 无效")
if not isinstance(version, str) or not re.fullmatch(
    r"[0-9]+\.[0-9]+\.[0-9]+-candidate\.[1-9][0-9]*\.[1-9][0-9]*\.[0-9a-f]{12}",
    version,
):
    raise SystemExit("候选 BUILDINFO 的 version 无效")
if artifact != f"tgm-{version}-linux-x86_64-gnu.tar.gz":
    raise SystemExit("候选 BUILDINFO 的 artifact 与 version 不匹配")
if release_tag != tag:
    raise SystemExit(f"候选 tag 不匹配：BUILDINFO={release_tag!r} 参数={tag!r}")
if release_tag != f"candidate-{value.get('candidate_run_id')}-a{value.get('candidate_attempt')}-{source_commit[:12]}":
    raise SystemExit("候选 release_tag 与运行标识或 source_commit 不匹配")
print("\t".join((version, artifact, source_commit, release_tag)))
PY
}

extract_archive() {
  "$PYTHON3" - "$1" "$2" <<'PY'
import posixpath
import sys
import tarfile
from pathlib import Path


archive = Path(sys.argv[1])
destination = Path(sys.argv[2])
seen = set()
try:
    with tarfile.open(str(archive), mode="r:gz") as stream:
        members = stream.getmembers()
        for member in members:
            name = member.name
            normalized = posixpath.normpath(name)
            if not name or "\x00" in name or name.startswith("/"):
                raise SystemExit("归档包含绝对或空路径：%r" % name)
            if normalized == ".." or normalized.startswith("../"):
                raise SystemExit("归档包含路径穿越：%s" % name)
            if normalized in seen:
                raise SystemExit("归档包含重复路径：%s" % name)
            seen.add(normalized)
            if member.issym() or member.islnk():
                raise SystemExit("归档包含链接：%s" % name)
            if not (member.isfile() or member.isdir()):
                raise SystemExit("归档包含特殊文件：%s" % name)
        stream.extractall(str(destination), members=members)
except (OSError, tarfile.TarError) as error:
    raise SystemExit("归档无法安全读取：%s" % error)
PY
}

verify_materials() {
  local archive="$MATERIALS/$ARCHIVE"
  local package_buildinfo="$WORK/package/BUILDINFO.json"
  local package_sbom="$WORK/package/sbom.cdx.json"
  local actual_version
  for file in "$archive" "$MATERIALS/candidate-SHA256SUMS" \
    "$MATERIALS/candidate-SHA256SUMS.sigstore.json" "$MATERIALS/candidate-provenance.json" \
    "$MATERIALS/candidate-provenance.sigstore.json" "$MATERIALS/candidate-BUILDINFO.json" \
    "$MATERIALS/candidate-sbom.cdx.json"; do
    [[ -f "$file" ]] || fail "缺少候选材料：$file"
  done
  find "$MATERIALS" -maxdepth 1 -type f -exec chmod 600 -- {} +

  printf '\n== 候选签名与摘要验证 ==\n'
  cosign verify-blob --offline \
    --bundle "$MATERIALS/candidate-SHA256SUMS.sigstore.json" \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$ISSUER" \
    "$MATERIALS/candidate-SHA256SUMS"
  cosign verify-blob --offline \
    --bundle "$MATERIALS/candidate-provenance.sigstore.json" \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$ISSUER" \
    "$MATERIALS/candidate-provenance.json"
  (cd "$MATERIALS" && sha256sum -c candidate-SHA256SUMS)

  mkdir -p -- "$WORK/package"
  extract_archive "$archive" "$WORK/package"
  actual_version="$(cat "$WORK/package/VERSION")"
  [[ "$actual_version" == "$VERSION" ]] || fail "包内 VERSION 不匹配：$actual_version / $VERSION"
  [[ -x "$WORK/package/bin/tgm" ]] || fail '包内 bin/tgm 不存在或不可执行'
  [[ -f "$package_buildinfo" && -f "$package_sbom" ]] || fail '包内 BUILDINFO 或 SBOM 缺失'

  printf '\n== 候选来源、provenance、SBOM 验证 ==\n'
  "$PYTHON3" "$WORK/package/scripts/verify-native-candidate.py" \
    --archive "$archive" \
    --checksums "$MATERIALS/candidate-SHA256SUMS" \
    --checksum-bundle "$MATERIALS/candidate-SHA256SUMS.sigstore.json" \
    --provenance "$MATERIALS/candidate-provenance.json" \
    --provenance-bundle "$MATERIALS/candidate-provenance.sigstore.json" \
    --buildinfo "$MATERIALS/candidate-BUILDINFO.json" \
    --sbom "$MATERIALS/candidate-sbom.cdx.json" \
    --package-buildinfo "$package_buildinfo" \
    --package-sbom "$package_sbom" \
    --version "$VERSION"
  [[ -z "$EXPECTED_SOURCE_COMMIT" || "$SOURCE_COMMIT" == "$EXPECTED_SOURCE_COMMIT" ]] || \
    fail "候选来源提交不匹配：$SOURCE_COMMIT / $EXPECTED_SOURCE_COMMIT"

  printf '\n== 二进制静态信息 ==\n'
  file "$WORK/package/bin/tgm" || true
  ldd "$WORK/package/bin/tgm"
  if ldd "$WORK/package/bin/tgm" 2>&1 | grep -F 'not found' >/dev/null; then
    fail '二进制存在未满足的动态库依赖'
  fi
  "$WORK/package/bin/tgm" --help >/dev/null
  "$WORK/package/bin/tgm" purge-deleted-tenants --help >/dev/null
  printf '二进制帮助命令：pass\n'
}

install_candidate() {
  CANDIDATE_ROOT="$HOME/.local/opt/tgm/candidates/$VERSION"
  if [[ "$SKIP_INSTALL" == 1 ]]; then
    [[ -x "$CANDIDATE_ROOT/bin/tgm" ]] || fail "--skip-install 时候选目录不存在：$CANDIDATE_ROOT"
    cmp -s "$MATERIALS/candidate-BUILDINFO.json" "$CANDIDATE_ROOT/BUILDINFO.json" || \
      fail "已安装候选 BUILDINFO 与 Release 不一致：$CANDIDATE_ROOT"
    cmp -s "$WORK/package/bin/tgm" "$CANDIDATE_ROOT/bin/tgm" || \
      fail "已安装候选二进制与已验证归档不一致：$CANDIDATE_ROOT/bin/tgm"
    printf '复用已安装候选：%s\n' "$CANDIDATE_ROOT"
    return 0
  fi
  [[ "$(id -u)" -ne 0 ]] || fail '候选安装必须以普通用户执行，不要使用 root'
  bash "$WORK/package/scripts/install-native.sh" \
    --user --candidate --accept-candidate \
    --archive "$MATERIALS/$ARCHIVE" \
    --checksums "$MATERIALS/candidate-SHA256SUMS" \
    --bundle "$MATERIALS/candidate-SHA256SUMS.sigstore.json" \
    --provenance "$MATERIALS/candidate-provenance.json" \
    --provenance-bundle "$MATERIALS/candidate-provenance.sigstore.json" \
    --buildinfo "$MATERIALS/candidate-BUILDINFO.json" \
    --sbom "$MATERIALS/candidate-sbom.cdx.json"
  [[ -x "$CANDIDATE_ROOT/bin/tgm" ]] || fail "候选安装后缺少二进制：$CANDIDATE_ROOT/bin/tgm"
}

run_native_preflight() {
  local output status
  printf '\n== 应用 unit / 配置模板预检（%s） ==\n' "$MODE"
  set +e
  output="$(bash "$WORK/package/scripts/native-release-preflight.sh" \
    --binary "$WORK/package/bin/tgm" \
    --unit-dir "$WORK/package/deploy/systemd" \
    --app-services 2>&1)"
  status=$?
  set -e
  printf '%s\n' "$output"
  if [[ "$MODE" == rocky ]]; then
    [[ "$status" -eq 0 ]] || fail_with_output 'Rocky 原生应用服务预检失败' "$output"
    NATIVE_PREFLIGHT_RESULT=pass
    return 0
  fi
  [[ "$status" -ne 0 ]] || fail 'CentOS 7 预检意外通过；安全能力不能被静默忽略'
  printf '%s\n' "$output" | grep -F '目标 systemd 未声明 +SECCOMP' >/dev/null || \
    fail_with_output 'CentOS 7 失败原因不是预期的 +SECCOMP 能力缺失' "$output"
  NATIVE_PREFLIGHT_RESULT=expected-fail
}

find_rocky_helper() {
  local candidates=()
  if [[ -n "$HELPERS_DIR" ]]; then
    candidates+=("$HELPERS_DIR/scripts/wp8-rocky-app-acceptance.py")
  fi
  candidates+=(
    "$SCRIPT_DIR/wp8-rocky-app-acceptance.py"
    "$SCRIPT_DIR/../scripts/wp8-rocky-app-acceptance.py"
    "$MATERIALS/scripts/wp8-rocky-app-acceptance.py"
  )
  local candidate
  for candidate in "${candidates[@]}"; do
    if [[ -f "$candidate" ]]; then
      printf '%s\n' "$(cd "$(dirname "$candidate")" && pwd -P)/$(basename "$candidate")"
      return 0
    fi
  done
  return 1
}

run_rocky_runtime_acceptance() {
  local helper
  helper="$(find_rocky_helper)" || fail '缺少 Rocky 六角色验收辅助脚本：scripts/wp8-rocky-app-acceptance.py'
  local helper_root
  helper_root="$(cd "$(dirname "$helper")/.." && pwd -P)"
  [[ -f "$helper_root/scripts/init/01-roles.sql" ]] || \
    fail "缺少 Rocky 验收辅助目录中的 scripts/init/01-roles.sql：$helper_root"
  printf '\n== Rocky 六角色非生产运行时验收 ==\n'
  local arguments=(
    "$PYTHON3" "$helper"
    --source-commit "$SOURCE_COMMIT"
    --binary "$CANDIDATE_ROOT/bin/tgm"
    --buildinfo "$CANDIDATE_ROOT/BUILDINFO.json"
  )
  [[ "$SKIP_JOURNAL" == 1 ]] || arguments+=(--system-journal)
  "${arguments[@]}"
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    auto|rocky|centos7)
      [[ "$MODE" == auto ]] || fail '验收模式重复指定'
      MODE="$1"
      shift
      ;;
    --mode)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      MODE="$2"
      shift 2
      ;;
    --tag)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      TAG="$2"
      shift 2
      ;;
    --materials)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      MATERIALS="$2"
      shift 2
      ;;
    --helpers-dir)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      HELPERS_DIR="$2"
      shift 2
      ;;
    --expected-source-commit)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      EXPECTED_SOURCE_COMMIT="$2"
      shift 2
      ;;
    --no-download)
      NO_DOWNLOAD=1
      shift
      ;;
    --reuse-materials)
      REUSE_MATERIALS=1
      shift
      ;;
    --no-gh-token)
      USE_GH_TOKEN=0
      shift
      ;;
    --download-source)
      [[ "$#" -ge 2 ]] || { usage >&2; exit 2; }
      DOWNLOAD_SOURCE="$2"
      shift 2
      ;;
    --skip-install)
      SKIP_INSTALL=1
      shift
      ;;
    --skip-journal)
      SKIP_JOURNAL=1
      shift
      ;;
    --keep-work)
      KEEP_WORK=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      usage >&2
      exit 2
      ;;
    *)
      [[ -z "$MATERIALS" ]] || fail "多余位置参数：$1"
      MATERIALS="$1"
      shift
      ;;
  esac
done
[[ "$#" -eq 0 ]] || fail "多余参数：$*"
[[ "$MODE" =~ ^(auto|rocky|centos7)$ ]] || fail "不支持的验收模式：$MODE"
[[ "$DOWNLOAD_SOURCE" =~ ^(auto|direct|api|gh)$ ]] || fail "不支持的下载来源：$DOWNLOAD_SOURCE"
[[ "$DOWNLOAD_ATTEMPTS" =~ ^[1-9][0-9]*$ ]] || fail "TGM_DOWNLOAD_ATTEMPTS 必须是正整数"
[[ "$DOWNLOAD_BACKOFF" =~ ^[0-9]+$ ]] || fail "TGM_DOWNLOAD_BACKOFF 必须是非负整数"
[[ "$CONNECT_TIMEOUT" =~ ^[1-9][0-9]*$ ]] || fail "TGM_CONNECT_TIMEOUT 必须是正整数"
[[ "$USE_GH_TOKEN" =~ ^[01]$ ]] || fail "TGM_USE_GH_TOKEN 只能是 0 或 1"
[[ "$NO_DOWNLOAD" == 0 || "$REUSE_MATERIALS" == 0 ]] || fail '--no-download 与 --reuse-materials 不能同时使用'
[[ "$GITHUB_WEB" == 'https://github.com' ]] || fail 'GITHUB_WEB 只能使用 https://github.com'
[[ "$GITHUB_API" == 'https://api.github.com' ]] || fail 'GITHUB_API 只能使用 https://api.github.com'
[[ "$TAG" =~ ^candidate-[1-9][0-9]*-a[1-9][0-9]*-[0-9a-f]{12}$ ]] || fail "候选 tag 格式无效：$TAG"
[[ -z "$EXPECTED_SOURCE_COMMIT" || "$EXPECTED_SOURCE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || \
  fail "expected source commit 必须是完整小写 SHA"
if [[ -n "$HELPERS_DIR" ]]; then
  [[ -d "$HELPERS_DIR" && ! -L "$HELPERS_DIR" ]] || fail "辅助目录必须是普通目录：$HELPERS_DIR"
  HELPERS_DIR="$(cd "$HELPERS_DIR" && pwd -P)"
fi

PYTHON3="$(find_python3)" || fail '缺少 Python 3：需要 python3 或 python3.6 等 Python 3 命令'
for command_name in sha256sum tar ldd openssl systemd-analyze; do
  require_command "$command_name"
done
[[ "$NO_DOWNLOAD" == 1 ]] || require_command curl
require_command cosign

prepare_materials
WORK="$(mktemp -d "${TMPDIR:-/tmp}/tgm-native-target-acceptance.XXXXXX")"
trap cleanup EXIT

if [[ "$MODE" == auto ]]; then
  MODE="$(detect_mode)"
fi
if [[ "$MODE" == rocky ]]; then
  require_command docker
  require_command systemctl
  require_command journalctl
  check_docker_proxy
fi

printf '验收模式：%s\n' "$MODE"
printf '材料目录：%s\n' "$MATERIALS"
printf '候选标签：%s\n' "$TAG"
printf 'Python 3：%s\n' "$PYTHON3"
[[ "$NO_DOWNLOAD" == 1 ]] || init_curl

if [[ "$NO_DOWNLOAD" == 0 ]]; then
  printf '\n== 下载候选 BUILDINFO ==\n'
fi
download_asset candidate-BUILDINFO.json
METADATA="$(read_buildinfo)"
IFS=$'\t' read -r VERSION ARCHIVE SOURCE_COMMIT RELEASE_TAG <<<"$METADATA"
printf '候选版本：%s\n' "$VERSION"
printf '候选来源：%s\n' "$SOURCE_COMMIT"
printf '候选归档：%s\n' "$ARCHIVE"
[[ "$RELEASE_TAG" == "$TAG" ]] || fail '候选 release tag 与参数不一致'

for asset in "$ARCHIVE" candidate-SHA256SUMS candidate-SHA256SUMS.sigstore.json \
  candidate-provenance.json candidate-provenance.sigstore.json candidate-sbom.cdx.json; do
  download_asset "$asset"
done

printf '\n== 主机信息 ==\n'
if [[ -r /etc/rocky-release ]]; then cat /etc/rocky-release; fi
if [[ -r /etc/centos-release ]]; then cat /etc/centos-release; fi
uname -a
uname -m
getconf GNU_LIBC_VERSION || true
ldd --version | sed -n '1p'
openssl version -a | sed -n '1,8p'
systemd-analyze --version | sed -n '1,2p'
systemd-analyze --version | grep -o '[+-]SECCOMP' || true
if command -v rpm >/dev/null 2>&1; then
  rpm -q systemd libseccomp || true
fi
"$PYTHON3" --version
cosign version | sed -n '1,8p'

verify_materials
run_native_preflight

if [[ "$MODE" == centos7 ]]; then
  printf 'CentOS 7 负向验收：预检按策略拒绝，未安装或启用生产服务。\n'
  printf '\n[report]\n'
  printf 'mode: centos7\n'
  printf 'download: %s\n' "$(download_summary)"
  printf 'signature: pass\nprovenance: pass\nbinary: pass\n'
  printf 'native_preflight: expected-fail\n'
  printf 'reason: systemd lacks +SECCOMP for RestrictAddressFamilies\n'
  printf 'candidate_install: not-run\nproduction_services: untouched\n'
  printf '材料目录保留：%s\n' "$MATERIALS"
  exit 0
fi

install_candidate

if [[ "$MODE" == rocky ]]; then
  run_rocky_runtime_acceptance
  printf '\n[report]\n'
  printf 'mode: rocky\n'
  printf 'download: %s\n' "$(download_summary)"
  printf 'signature: pass\nprovenance: pass\nbinary: pass\n'
  printf 'native_preflight: %s\n' "$NATIVE_PREFLIGHT_RESULT"
  printf 'candidate_install: pass\n'
  if [[ "$SKIP_JOURNAL" == 1 ]]; then
    printf 'journal_redaction: not-run\n'
  else
    printf 'journal_redaction: pass\n'
  fi
  printf 'production_services: untouched\ndatabase_migrations: non-production acceptance only\n'
fi
printf '材料目录保留：%s\n' "$MATERIALS"
