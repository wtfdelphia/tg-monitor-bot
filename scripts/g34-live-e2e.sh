#!/usr/bin/env bash
# 专用测试账号/频道的 G3/G4 真机验收；不会进入默认 CI。
set -euo pipefail
umask 077
ROOT="$(git rev-parse --show-toplevel)"
cd -- "$ROOT"

if [ "${1:-run}" != "run" ] || [ "${TGM_LIVE_E2E_ALLOW:-}" != "1" ]; then
  echo "用法：TGM_LIVE_E2E_ALLOW=1 bash scripts/g34-live-e2e.sh run" >&2
  exit 2
fi

ENV_FILE="${TGM_LIVE_ENV_FILE:-$HOME/.config/tgm-live.env}"
[ -f "$ENV_FILE" ] || { echo "缺少脱敏环境文件：$ENV_FILE" >&2; exit 1; }
set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a
export TGM_LIVE_E2E_ALLOW=1
export DATABASE_URL="${DATABASE_URL:-postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control}"
export SQLX_OFFLINE=true
export TGM_S3_ENDPOINT="${TGM_S3_ENDPOINT:-http://127.0.0.1:59000}"
export TGM_S3_BUCKET="${TGM_S3_BUCKET:-tgm-g34-live}"
export TGM_S3_ACCESS_KEY_ID="${TGM_S3_ACCESS_KEY_ID:-minioadmin}"
export TGM_S3_SECRET_ACCESS_KEY="${TGM_S3_SECRET_ACCESS_KEY:-minioadmin}"
export TGM_LIVE_QR_DIR="${TGM_LIVE_QR_DIR:-$HOME/.config/tgm-qr}"
mkdir -p -- "$TGM_LIVE_QR_DIR"
chmod 700 -- "$TGM_LIVE_QR_DIR"
export TGM_LIVE_QR_HTML_FILE="$TGM_LIVE_QR_DIR/qr.html"
export TGM_LIVE_QR_LINK_FILE="$TGM_LIVE_QR_DIR/qr.link"
export TGM_LIVE_2FA_PASSWORD_FILE="$TGM_LIVE_QR_DIR/2fa.txt"
export TGM_LIVE_USER_SESSION_FILE="$TGM_LIVE_QR_DIR/user.session"

cleanup() {
  python3 - "$TGM_LIVE_QR_HTML_FILE" "$TGM_LIVE_QR_LINK_FILE" "$TGM_LIVE_USER_SESSION_FILE" <<'PY'
from pathlib import Path
import sys
for name in sys.argv[1:]:
    path = Path(name)
    if path.is_file() and not path.is_symlink():
        path.unlink()
PY
}
trap cleanup EXIT

cargo test -p tgm-app --test g34_live_e2e live_g34_source_rule_delivery -- --ignored --exact --nocapture --test-threads=1
