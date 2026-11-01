#!/usr/bin/env bash
set -euo pipefail
umask 077

PACKAGE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE_REF="$(cat "$PACKAGE_ROOT/image.ref")"
EXPECTED_IMAGE_ID="$(cat "$PACKAGE_ROOT/image.id")"
EXPECTED_IMAGE_FINGERPRINT="$(cat "$PACKAGE_ROOT/image.fingerprint")"
SOURCE_COMMIT="$(cat "$PACKAGE_ROOT/source.commit")"
PROJECT="tgm-wp8-release-preflight-${UID}-$$"

required_files=(
  image.tar
  image.id
  image.fingerprint
  image.ref
  source.commit
  package-files.sha256
  README.md
  deploy/docker/wp8-acceptance.compose.yaml
  scripts/init/01-roles.sql
  scripts/wp8-docker-acceptance.sh
  scripts/wp8-docker-release-preflight.sh
)

for required_file in "${required_files[@]}"; do
  [ -f "$PACKAGE_ROOT/$required_file" ] || {
    printf '拒绝执行：发布包缺少 %s。\n' "$required_file" >&2
    exit 1
  }
done

case "$IMAGE_REF" in
  ""|*[!A-Za-z0-9._:/@-]*)
    printf '拒绝执行：发布包中的镜像引用无效。\n' >&2
    exit 1
    ;;
esac

command -v docker >/dev/null
command -v sha256sum >/dev/null

image_fingerprint() {
  docker image inspect --format \
    '{{.Architecture}}\n{{.Os}}\n{{.Config.User}}\n{{json .Config.Env}}\n{{json .Config.Entrypoint}}\n{{json .Config.Cmd}}\n{{json .Config.WorkingDir}}\n{{json .Config.ExposedPorts}}\n{{json .Config.Volumes}}\n{{json .Config.Labels}}\n{{json .RootFS.Layers}}' \
    "$1" | sha256sum | cut -d ' ' -f 1
}

(
  cd "$PACKAGE_ROOT"
  sha256sum --check package-files.sha256
)

docker load --input "$PACKAGE_ROOT/image.tar"
ACTUAL_IMAGE_ID="$(docker image inspect --format '{{.Id}}' "$IMAGE_REF")"
ACTUAL_IMAGE_FINGERPRINT="$(image_fingerprint "$IMAGE_REF")"
[ "$ACTUAL_IMAGE_FINGERPRINT" = "$EXPECTED_IMAGE_FINGERPRINT" ] || {
  printf '拒绝执行：加载后的镜像内容指纹与发布包不一致。\n' >&2
  printf '预期指纹：%s\n实际指纹：%s\n' "$EXPECTED_IMAGE_FINGERPRINT" "$ACTUAL_IMAGE_FINGERPRINT" >&2
  exit 1
}

docker image inspect --format '{{.Config.User}}' "$IMAGE_REF" | grep -qx '10001:10001'
if docker image inspect --format '{{json .Config.Env}}' "$IMAGE_REF" | grep -q 'SQLX_OFFLINE'; then
  printf '拒绝执行：运行时镜像意外包含构建期 SQLX_OFFLINE。\n' >&2
  exit 1
fi

docker run --rm --network none --entrypoint /bin/sh "$IMAGE_REF" -ec \
  'test "$(id -u)" = 10001 && test ! -e /src && test ! -e /root/.cargo && test ! -e /.env'

printf '来源提交：%s\n' "$SOURCE_COMMIT"
printf '加载镜像：%s\n' "$IMAGE_REF"
printf '镜像 ID（本机诊断值）：%s\n' "$ACTUAL_IMAGE_ID"
printf '镜像内容指纹：%s\n' "$ACTUAL_IMAGE_FINGERPRINT"
printf '运行时身份与内容检查：pass\n'

TGM_ACCEPTANCE_ROOT="$PACKAGE_ROOT" \
  "$PACKAGE_ROOT/scripts/wp8-docker-acceptance.sh" --image "$IMAGE_REF" \
  --project-name "$PROJECT"

printf '目标机 Docker-only 发布包预检通过。\n'
printf '保留已加载镜像；未启用生产调度，未连接生产服务，未删除真实数据。\n'
