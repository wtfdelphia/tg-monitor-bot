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

usage() {
  cat <<'EOF'
用法：
  bash scripts/tgm-native-target-acceptance.sh [auto|rocky|centos7] [材料目录]
  bash scripts/tgm-native-target-acceptance.sh --mode auto --tag <candidate-tag> \
    [--materials <目录>] [--helpers-dir <目录>] [--no-download] [--skip-install] [--skip-journal]

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

download_asset() {
  local name="$1" destination temporary
  destination="$MATERIALS/$name"
  if [[ "$NO_DOWNLOAD" == 1 ]]; then
    [[ -f "$destination" && ! -L "$destination" ]] || \
      fail "--no-download 时材料必须是普通文件：$destination"
    return 0
  fi
  [[ ! -e "$destination" && ! -L "$destination" ]] || \
    fail "材料已存在，拒绝覆盖：$destination；复用已有材料请使用 --no-download，或换用新的材料目录"
  temporary="$MATERIALS/.${name}.download.$$"
  [[ ! -e "$temporary" && ! -L "$temporary" ]] || fail "下载临时文件已存在：$temporary"
  if ! curl -fL --retry 3 --retry-delay 1 --proto '=https' --tlsv1.2 \
    -o "$temporary" \
    "https://github.com/$REPOSITORY/releases/download/$TAG/$name"; then
    rm -f -- "$temporary"
    fail "候选材料下载失败：$name"
  fi
  chmod 600 -- "$temporary"
  if ! ln -- "$temporary" "$destination" 2>/dev/null; then
    rm -f -- "$temporary"
    fail "材料落盘失败或目标已被并发创建：$destination"
  fi
  rm -f -- "$temporary"
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
fi

printf '验收模式：%s\n' "$MODE"
printf '材料目录：%s\n' "$MATERIALS"
printf '候选标签：%s\n' "$TAG"
printf 'Python 3：%s\n' "$PYTHON3"

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
