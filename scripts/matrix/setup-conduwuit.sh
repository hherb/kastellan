#!/usr/bin/env bash
# Stand up a local conduwuit Matrix homeserver for kastellan (Tier C / dev).
#
# Renders a hardened, federation-OFF config from deploy/matrix/conduwuit.toml.template,
# validates it (fail-closed via check-conduwuit-config.sh), and runs conduwuit
# bound to loopback. Account creation (operator + kastellan bot) is a documented
# manual step using the printed registration token — registration UX varies by
# client, so this script does not automate it. Dev convenience; the production
# deployment uses the hardened SYSTEM unit in deploy/matrix/ (see
# docs/deploy/matrix-homeserver.md).
#
# Cross-platform: container runtime (docker/podman) or a local conduwuit binary
# (KASTELLAN_CONDUWUIT_BIN).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TEMPLATE="$(cd "$HERE/../../deploy/matrix" && pwd)/conduwuit.toml.template"
CHECK="$HERE/check-conduwuit-config.sh"

SERVER_NAME="${KASTELLAN_MATRIX_SERVER_NAME:-}"
if [ -z "$SERVER_NAME" ]; then
  echo "error: set KASTELLAN_MATRIX_SERVER_NAME (e.g. matrix.example.org or localhost)" >&2
  exit 2
fi
PORT="${KASTELLAN_MATRIX_PORT:-6167}"
STATE_DIR="${KASTELLAN_MATRIX_STATE:-$HOME/.local/state/kastellan/matrix-homeserver}"
DB_PATH="$STATE_DIR/db"
CONFIG="$STATE_DIR/conduwuit.toml"
IMAGE="${KASTELLAN_CONDUWUIT_IMAGE:-ghcr.io/girlbossceo/conduwuit:latest}"

mkdir -p "$DB_PATH"

# Generate a one-time registration token (used to create the 2 accounts, then
# disable registration).
gen_token() {
  if command -v openssl >/dev/null 2>&1; then openssl rand -hex 16
  else head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'; fi
}
TOKEN="${KASTELLAN_MATRIX_REG_TOKEN:-$(gen_token)}"

# Render the config from the template.
sed -e "s/{{SERVER_NAME}}/$SERVER_NAME/" \
    -e "s/{{PORT}}/$PORT/" \
    -e "s#{{DB_PATH}}#$DB_PATH#" \
    -e "s/{{REGISTRATION_TOKEN}}/$TOKEN/" \
    "$TEMPLATE" > "$CONFIG"

# Fail-closed: refuse to start an insecure config.
bash "$CHECK" "$CONFIG"

echo
echo "Rendered hardened config: $CONFIG"
echo "Homeserver will bind 127.0.0.1:$PORT (federation OFF)."
echo

# Pick how to run conduwuit.
RUN=""
if [ -n "${KASTELLAN_CONDUWUIT_BIN:-}" ]; then
  RUN="binary"
elif command -v docker >/dev/null 2>&1; then
  RUN="docker"
elif command -v podman >/dev/null 2>&1; then
  RUN="podman"
else
  echo "error: need KASTELLAN_CONDUWUIT_BIN, or docker/podman on PATH" >&2
  exit 1
fi

cat <<EOF
Next steps (one-time):
  1. With the server running, create TWO accounts using the registration token
     below (via Element 'Register' against http://127.0.0.1:$PORT, or the
     conduwuit register API): your operator account and the kastellan bot.
        registration token: $TOKEN
  2. Then set 'allow_registration = false' in $CONFIG and restart to fully close
     the server.
  3. Point kastellan at it:
        export KASTELLAN_MATRIX_HOMESERVER="http://127.0.0.1:$PORT"
        export KASTELLAN_MATRIX_USER="@kastellan:$SERVER_NAME"
        # store the bot access token as a kastellan secret (see docs/deploy)
  4. Pair your operator account:  kastellan-cli pair issue   (send the code from
     your operator account to the bot).

For production, install the hardened SYSTEM unit instead — see
docs/deploy/matrix-homeserver.md.

EOF

case "$RUN" in
  binary)
    echo "Starting conduwuit binary ($KASTELLAN_CONDUWUIT_BIN)…"
    exec "$KASTELLAN_CONDUWUIT_BIN" --config "$CONFIG"
    ;;
  docker|podman)
    echo "Starting conduwuit via $RUN ($IMAGE)…"
    exec "$RUN" run --rm \
      -p "127.0.0.1:$PORT:$PORT" \
      -v "$STATE_DIR:$STATE_DIR" \
      -e CONDUWUIT_CONFIG="$CONFIG" \
      --name kastellan-conduwuit \
      "$IMAGE"
    ;;
esac
