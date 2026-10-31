#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT="$(git rev-parse --show-toplevel)"
SUFFIX="${UID}-$$"
PROJECT="tgm-wp8-acceptance-$SUFFIX"
IMAGE="tgm-wp8-acceptance:$SUFFIX"
BUILDER="tgm-wp8-acceptance-$SUFFIX"
COMPOSE_FILE="$ROOT/deploy/docker/wp8-acceptance.compose.yaml"
TEMP_DIR="$(mktemp -d /tmp/tgm-wp8-docker-acceptance.XXXXXX)"
COMPOSE=(docker compose --project-name "$PROJECT" --file "$COMPOSE_FILE")
export TGM_ACCEPTANCE_IMAGE="$IMAGE"
export TGM_ACCEPTANCE_OWNER_ENV="$TEMP_DIR/owner.env"
export TGM_ACCEPTANCE_PURGE_ENV="$TEMP_DIR/purge.env"
BUILDER_CREATED=0
IMAGE_OWNED=0
RESOURCES_OWNED=0
LOCK_PID=""

cleanup() {
  local result=$?
  trap - EXIT
  if [ -n "$LOCK_PID" ]; then
    kill "$LOCK_PID" 2>/dev/null || true
    wait "$LOCK_PID" 2>/dev/null || true
  fi
  if [ "$RESOURCES_OWNED" -eq 1 ]; then
    "${COMPOSE[@]}" down --volumes --remove-orphans || result=1
  fi
  if [ "$IMAGE_OWNED" -eq 1 ]; then
    docker image rm "$IMAGE" >/dev/null || result=1
  fi
  if [ "$BUILDER_CREATED" -eq 1 ]; then
    docker buildx rm --force "$BUILDER" >/dev/null || result=1
  fi
  rm -rf -- "$TEMP_DIR"
  exit "$result"
}
trap cleanup EXIT

command -v docker >/dev/null
command -v sha256sum >/dev/null
[ -f "$COMPOSE_FILE" ]

if [ -n "$("${COMPOSE[@]}" ps --all --quiet)" ]; then
  printf '拒绝执行：Compose project %s 已有容器。\n' "$PROJECT" >&2
  exit 1
fi
if docker image inspect "$IMAGE" >/dev/null 2>&1; then
  printf '拒绝执行：镜像标签 %s 已存在。\n' "$IMAGE" >&2
  exit 1
fi
if docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
  printf '拒绝执行：Buildx builder %s 已存在。\n' "$BUILDER" >&2
  exit 1
fi

printf 'TGM_PROFILE=development\nTGM_DB_URL_OWNER=postgres://postgres:pw@postgres:5432/tgm\n' \
  >"$TGM_ACCEPTANCE_OWNER_ENV"
printf 'TGM_PROFILE=development\nTGM_DB_URL_TENANT_PURGE=postgres://tenant_purge:pupw@postgres:5432/tgm\n' \
  >"$TGM_ACCEPTANCE_PURGE_ENV"
chmod 600 "$TGM_ACCEPTANCE_OWNER_ENV" "$TGM_ACCEPTANCE_PURGE_ENV"

docker buildx create --name "$BUILDER" --driver docker-container >/dev/null
BUILDER_CREATED=1
IMAGE_OWNED=1
docker buildx build --builder "$BUILDER" --progress plain --no-cache --load --tag "$IMAGE" \
  --file "$ROOT/Dockerfile" "$ROOT"
IMAGE_ID="$(docker image inspect --format '{{.Id}}' "$IMAGE")"
IMAGE_SHA="$(docker image save "$IMAGE" | sha256sum | cut -d ' ' -f 1)"
printf '镜像 ID：%s\n镜像归档 SHA-256：%s\n' "$IMAGE_ID" "$IMAGE_SHA"
docker image inspect --format '{{.Config.User}}' "$IMAGE" | grep -qx '10001:10001'
if docker image inspect --format '{{json .Config.Env}}' "$IMAGE" | grep -q 'SQLX_OFFLINE'; then
  printf '镜像运行配置意外包含构建期 SQLX_OFFLINE。\n' >&2
  exit 1
fi
docker run --rm --entrypoint /bin/sh "$IMAGE" -ec \
  'test "$(id -u)" = 10001 && test ! -e /src && test ! -e /root/.cargo'

"${COMPOSE[@]}" config --quiet
RESOURCES_OWNED=1
"${COMPOSE[@]}" up --detach --wait postgres
"${COMPOSE[@]}" run --rm --no-deps tgm-owner migrate
"${COMPOSE[@]}" run --rm --no-deps tgm-owner audit-rls
"${COMPOSE[@]}" run --rm --no-deps tgm-purge purge-deleted-tenants --max-tenants 1

create_tenant() {
  local name="$1"
  local status="$2"
  local age="$3"
  if [ "$age" = "none" ]; then
    "${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -qAt -v ON_ERROR_STOP=1 \
      -c "INSERT INTO tenants(name, owner_user_id, status, deleted_at)
          VALUES ('$name', 99111, '$status', NULL)
          RETURNING id" | head -n 1
  else
    "${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -qAt -v ON_ERROR_STOP=1 \
      -c "INSERT INTO tenants(name, owner_user_id, status, deleted_at)
          VALUES ('$name', 99111, '$status', statement_timestamp() - '$age'::interval)
          RETURNING id" | head -n 1
  fi
}

run_purge_expect() {
  local tenant_id="$1"
  local expected="$2"
  local output="$TEMP_DIR/purge-$tenant_id.log"
  if "${COMPOSE[@]}" run --rm --no-deps tgm-purge \
    purge-deleted-tenants --tenant-id "$tenant_id" >"$output" 2>&1; then
    printf '预期清理命令非零退出：tenant=%s result=%s\n' "$tenant_id" "$expected" >&2
    return 1
  fi
  grep -q "result=$expected" "$output"
}

assert_tenant_exists() {
  local tenant_id="$1"
  local expected="$2"
  local count
  count="$("${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -At -v ON_ERROR_STOP=1 \
    -c "SELECT count(*) FROM tenants WHERE id=$tenant_id")"
  [ "$count" = "$expected" ]
}

RECENT_TENANT="$(create_tenant tgm-wp8-recent deleted '29 days')"
ACTIVE_TENANT="$(create_tenant tgm-wp8-active active none)"
run_purge_expect "$RECENT_TENANT" not_due
run_purge_expect "$ACTIVE_TENANT" not_deleted
assert_tenant_exists "$RECENT_TENANT" 1
assert_tenant_exists "$ACTIVE_TENANT" 1
printf '状态与宽限期反证：pass\n'

LOCKED_TENANT="$(create_tenant tgm-wp8-locked deleted '31 days')"
"${COMPOSE[@]}" exec --no-TTY --env PGAPPNAME=tgm-wp8-lock-probe postgres \
  psql -U postgres -d tgm -v ON_ERROR_STOP=1 \
  -c "SELECT pg_advisory_lock(hashtextextended('tenant_lifecycle:$LOCKED_TENANT', 419871)); SELECT pg_sleep(20);" \
  >"$TEMP_DIR/lock-holder.log" 2>&1 &
LOCK_PID=$!
LOCK_READY=0
for attempt in $(seq 1 100); do
  LOCK_COUNT="$("${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -At -v ON_ERROR_STOP=1 \
    -c "SELECT count(*) FROM pg_stat_activity WHERE application_name='tgm-wp8-lock-probe'")"
  if [ "$LOCK_COUNT" -gt 0 ]; then
    LOCK_READY=1
    break
  fi
  sleep 0.2
done
if [ "$LOCK_READY" -ne 1 ]; then
  printf '没有观察到测试 advisory lock。\n' >&2
  exit 1
fi
run_purge_expect "$LOCKED_TENANT" locked
wait "$LOCK_PID"
LOCK_PID=""
"${COMPOSE[@]}" run --rm --no-deps tgm-purge \
  purge-deleted-tenants --tenant-id "$LOCKED_TENANT"
assert_tenant_exists "$LOCKED_TENANT" 0
printf '锁冲突、解除后重跑与幂等删除：pass\n'

SHARED_TENANT="$(create_tenant tgm-wp8-shared deleted '31 days')"
OTHER_TENANT="$(create_tenant tgm-wp8-other active none)"
SHARED_IDENTITY="$("${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -qAt -v ON_ERROR_STOP=1 \
  -c "INSERT INTO identities(tenant_id, kind) VALUES (NULL, 'bot') RETURNING id" | head -n 1)"
"${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -v ON_ERROR_STOP=1 \
  -c "INSERT INTO source_subscriptions(tenant_id, tg_chat_id, receiver_identity_id)
      VALUES ($SHARED_TENANT, -10099111, $SHARED_IDENTITY),
             ($OTHER_TENANT, -10099112, $SHARED_IDENTITY)" >/dev/null
SHARED_EVENT="$("${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -qAt -v ON_ERROR_STOP=1 \
  -c "INSERT INTO canonical_events(adapter, receiver_identity_id, tg_chat_id, tg_message_id, message_date)
      VALUES ('bot', $SHARED_IDENTITY, -10099111, 1, now()) RETURNING id" | head -n 1)"
"${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -v ON_ERROR_STOP=1 \
  -c "INSERT INTO tenant_events(tenant_id, canonical_event_id, source_ref)
      VALUES ($SHARED_TENANT, $SHARED_EVENT, (SELECT id FROM source_subscriptions WHERE tenant_id=$SHARED_TENANT LIMIT 1)),
             ($OTHER_TENANT, $SHARED_EVENT, (SELECT id FROM source_subscriptions WHERE tenant_id=$OTHER_TENANT LIMIT 1))" \
  >/dev/null
run_purge_expect "$SHARED_TENANT" blocked
assert_tenant_exists "$SHARED_TENANT" 1
"${COMPOSE[@]}" exec --no-TTY postgres psql -U postgres -d tgm -v ON_ERROR_STOP=1 \
  -c "DELETE FROM tenant_events WHERE tenant_id=$OTHER_TENANT" >/dev/null
"${COMPOSE[@]}" run --rm --no-deps tgm-purge \
  purge-deleted-tenants --tenant-id "$SHARED_TENANT"
assert_tenant_exists "$SHARED_TENANT" 0
assert_tenant_exists "$OTHER_TENANT" 1
printf '共享引用阻塞、解除后重跑与非目标租户保留：pass\n'

printf 'Docker-only 非生产验收通过；仅清理本脚本随机命名的 project、镜像与 builder。\n'
