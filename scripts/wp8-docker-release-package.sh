#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT="$(git rev-parse --show-toplevel)"
IMAGE_REF=""
OUTPUT=""
SOURCE_COMMIT="$(git rev-parse HEAD)"

usage() {
  cat <<'EOF'
用法：
  scripts/wp8-docker-release-package.sh --image <镜像引用> --output <发布目录>

发布目录只包含已导出的 Docker 镜像、校验文件、隔离 Compose 工件和目标机预检脚本。
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --image)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      IMAGE_REF="$2"
      shift 2
      ;;
    --output)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      OUTPUT="$2"
      shift 2
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

[ -n "$IMAGE_REF" ] || { usage >&2; exit 2; }
[ -n "$OUTPUT" ] || { usage >&2; exit 2; }
case "$IMAGE_REF" in
  *[!A-Za-z0-9._:/@-]*)
    printf '拒绝执行：镜像引用包含不允许的字符。\n' >&2
    exit 2
    ;;
esac

git diff --quiet HEAD --
git diff --cached --quiet
if [ -n "$(git status --porcelain)" ]; then
  printf '拒绝执行：工作树不是干净的，不能把未提交内容标记为发布提交。\n' >&2
  exit 1
fi

command -v docker >/dev/null
command -v sha256sum >/dev/null
docker image inspect "$IMAGE_REF" >/dev/null 2>&1 || {
  printf '拒绝执行：镜像 %s 不存在。\n' "$IMAGE_REF" >&2
  exit 1
}

image_fingerprint() {
  docker image inspect --format \
    '{{.Architecture}}\n{{.Os}}\n{{.Config.User}}\n{{json .Config.Env}}\n{{json .Config.Entrypoint}}\n{{json .Config.Cmd}}\n{{json .Config.WorkingDir}}\n{{json .Config.ExposedPorts}}\n{{json .Config.Volumes}}\n{{json .Config.Labels}}\n{{json .RootFS.Layers}}' \
    "$1" | sha256sum | cut -d ' ' -f 1
}

if [ -e "$OUTPUT" ]; then
  if [ "$(find "$OUTPUT" -mindepth 1 -print -quit 2>/dev/null)" != "" ]; then
    printf '拒绝执行：发布目录 %s 非空。\n' "$OUTPUT" >&2
    exit 1
  fi
else
  mkdir -p "$OUTPUT"
fi

mkdir -p "$OUTPUT/deploy/docker" "$OUTPUT/scripts/init"
docker save "$IMAGE_REF" -o "$OUTPUT/image.tar"
docker image inspect --format '{{.Id}}' "$IMAGE_REF" >"$OUTPUT/image.id"
image_fingerprint "$IMAGE_REF" >"$OUTPUT/image.fingerprint"
printf '%s\n' "$IMAGE_REF" >"$OUTPUT/image.ref"
printf '%s\n' "$SOURCE_COMMIT" >"$OUTPUT/source.commit"
cp "$ROOT/deploy/docker/README.md" "$OUTPUT/README.md"
cp "$ROOT/deploy/docker/wp8-acceptance.compose.yaml" "$OUTPUT/deploy/docker/"
cp "$ROOT/scripts/init/01-roles.sql" "$OUTPUT/scripts/init/"
cp "$ROOT/scripts/wp8-docker-acceptance.sh" "$OUTPUT/scripts/"
cp "$ROOT/scripts/wp8-docker-release-preflight.sh" "$OUTPUT/scripts/"
chmod 755 "$OUTPUT/deploy" "$OUTPUT/deploy/docker" "$OUTPUT/scripts" "$OUTPUT/scripts/init"
chmod 644 "$OUTPUT/README.md" "$OUTPUT/deploy/docker/wp8-acceptance.compose.yaml" "$OUTPUT/scripts/init/01-roles.sql"
chmod 700 "$OUTPUT/scripts/wp8-docker-acceptance.sh" "$OUTPUT/scripts/wp8-docker-release-preflight.sh"

(
  cd "$OUTPUT"
  sha256sum \
    image.tar \
    image.id \
    image.fingerprint \
    image.ref \
    source.commit \
    README.md \
    deploy/docker/wp8-acceptance.compose.yaml \
    scripts/init/01-roles.sql \
    scripts/wp8-docker-acceptance.sh \
    scripts/wp8-docker-release-preflight.sh \
    >package-files.sha256
)

printf 'Docker 发布目录已生成：%s\n' "$OUTPUT"
printf '来源提交：%s\n' "$SOURCE_COMMIT"
printf '镜像引用：%s\n' "$IMAGE_REF"
printf '镜像 ID：%s\n' "$(cat "$OUTPUT/image.id")"
printf '镜像内容指纹：%s\n' "$(cat "$OUTPUT/image.fingerprint")"
printf '镜像归档 SHA-256：%s\n' "$(sha256sum "$OUTPUT/image.tar" | awk '{print $1}')"
printf '目标机入口：%s/scripts/wp8-docker-release-preflight.sh\n' "$OUTPUT"
