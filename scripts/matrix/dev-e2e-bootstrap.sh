#!/usr/bin/env bash
# Headless homeserver + account/room bootstrap for the live Matrix e2e
# (core/tests/matrix_live_e2e.rs). DEV ONLY — a throwaway, loopback-bound,
# plaintext-HTTP homeserver in a container, used to prove the kastellan matrix
# worker's matrix-rust-sdk integration (login + E2E send/recv) end to end.
#
# This is NOT the production homeserver. Production = the hardened conduwuit
# SYSTEM unit on a dedicated host (federation off, TLS, dedicated user) — see
# docs/deploy/matrix-homeserver.md. By design this script:
#   * binds 127.0.0.1 only (never faces a network),
#   * uses matrix-conduit (conduwuit's upstream — same client-server API + E2E
#     relay, all the worker exercises; its OCI image is reliably public),
#   * registers TWO accounts (bot + peer) and one ENCRYPTED room they both join,
#   * writes the env block the e2e sources to "$ENV_OUT".
#
# Usage:
#   scripts/matrix/dev-e2e-bootstrap.sh up       # bring up + bootstrap (default)
#   scripts/matrix/dev-e2e-bootstrap.sh down     # stop the container + wipe state
#
# Then run the e2e (on the same host; the live worker must be built with
# `--features live-matrix`):
#   source ~/.matrix-e2e.env
#   cargo test -p kastellan-core --test matrix_live_e2e -- --ignored --nocapture
#
# Override defaults via env: PORT, KASTELLAN_E2E_MATRIX_IMAGE, STATE, ENV_OUT,
# DOCKER (docker|podman).
set -euo pipefail

PORT="${PORT:-6167}"
HS="http://127.0.0.1:${PORT}"
STATE="${STATE:-$HOME/.local/state/kastellan/matrix-e2e}"
IMAGE="${KASTELLAN_E2E_MATRIX_IMAGE:-matrixconduit/matrix-conduit:latest}"
ENV_OUT="${ENV_OUT:-$HOME/.matrix-e2e.env}"
DOCKER="${DOCKER:-docker}"
CNAME="kastellan-matrix-e2e"

down() {
  echo "### stopping $CNAME + wiping $STATE"
  "$DOCKER" rm -f "$CNAME" >/dev/null 2>&1 || true
  rm -rf "$STATE"
  rm -f "$ENV_OUT"
  echo "done"
}

up() {
  command -v "$DOCKER" >/dev/null 2>&1 || { echo "need '$DOCKER' on PATH (set DOCKER=podman?)" >&2; exit 1; }
  command -v jq >/dev/null 2>&1 || { echo "need 'jq' on PATH" >&2; exit 1; }
  command -v curl >/dev/null 2>&1 || { echo "need 'curl' on PATH" >&2; exit 1; }

  local TOKEN BOT_PW OP_PW
  TOKEN="e2e-$(openssl rand -hex 8)"
  BOT_PW="botpw-$(openssl rand -hex 6)"
  OP_PW="oppw-$(openssl rand -hex 6)"

  echo "### tearing down any prior instance + state"
  "$DOCKER" rm -f "$CNAME" >/dev/null 2>&1 || true
  rm -rf "$STATE"
  mkdir -p "$STATE/db"
  # World-writable so the container's (image-defined, often non-root) uid can
  # write the bind-mounted store regardless of host uid. Acceptable only because
  # this is a dev-only, loopback, throwaway homeserver under "$HOME/.local/state".
  chmod -R 777 "$STATE"

  # Loopback, federation-off, token-gated registration (mirrors the production
  # security invariants; conduit-flavoured config keys).
  cat > "$STATE/conduit.toml" <<EOF
[global]
server_name = "localhost"
database_path = "/var/lib/matrix-conduit/db"
database_backend = "rocksdb"
address = "0.0.0.0"
port = $PORT
allow_registration = true
registration_token = "$TOKEN"
allow_federation = false
allow_check_for_updates = false
trusted_servers = []
max_request_size = 20000000
EOF

  echo "### starting homeserver ($IMAGE) detached on 127.0.0.1:$PORT"
  "$DOCKER" run -d --rm \
    -p "127.0.0.1:${PORT}:${PORT}" \
    -v "$STATE:/var/lib/matrix-conduit" \
    -e CONDUIT_CONFIG="/var/lib/matrix-conduit/conduit.toml" \
    --name "$CNAME" "$IMAGE" >/dev/null

  echo "### waiting for the client API"
  local i
  for i in $(seq 1 60); do
    if curl -fsS "$HS/_matrix/client/versions" >/dev/null 2>&1; then break; fi
    sleep 1
    if [ "$i" = 60 ]; then echo "homeserver did not come up"; "$DOCKER" logs "$CNAME" | tail -30; exit 1; fi
  done
  echo "client API is up"

  echo "### registering @bot and @op"
  local BOT_ID BOT_TOK OP_ID OP_TOK ROOM
  read -r BOT_ID BOT_TOK < <(register bot "$BOT_PW" "$TOKEN")
  read -r OP_ID  OP_TOK  < <(register op  "$OP_PW"  "$TOKEN")
  echo "bot=$BOT_ID op=$OP_ID"

  echo "### creating an encrypted room as @op, inviting @bot"
  ROOM=$(curl -sS -X POST "$HS/_matrix/client/v3/createRoom" \
    -H "Authorization: Bearer $OP_TOK" \
    -d "$(jq -nc --arg bot "$BOT_ID" '{preset:"trusted_private_chat",invite:[$bot],initial_state:[{type:"m.room.encryption",state_key:"",content:{algorithm:"m.megolm.v1.aes-sha2"}}]}')" \
    | jq -r '.room_id')
  echo "room=$ROOM"

  echo "### @bot joins"
  curl -sS -X POST "$HS/_matrix/client/v3/join/$ROOM" -H "Authorization: Bearer $BOT_TOK" -d '{}' >/dev/null

  cat > "$ENV_OUT" <<EOF
export KASTELLAN_MATRIX_LIVE_E2E=1
export KASTELLAN_MATRIX_HOMESERVER_URL=$HS
export KASTELLAN_MATRIX_USER=$BOT_ID
export KASTELLAN_MATRIX_PASSWORD=$BOT_PW
export KASTELLAN_MATRIX_PEER_USER=$OP_ID
export KASTELLAN_MATRIX_PEER_PASSWORD=$OP_PW
export KASTELLAN_MATRIX_ROOM=$ROOM
EOF
  echo "### bootstrap complete; env written to $ENV_OUT"
  cat "$ENV_OUT"
  echo
  echo "Next: source $ENV_OUT && cargo test -p kastellan-core --test matrix_live_e2e -- --ignored --nocapture"
  echo "Tear down with: $0 down"
}

# Register a user via the registration-token UIAA flow (token stage, then a dummy
# stage if the homeserver requires one). Echoes "user_id access_token".
register() {
  local user="$1" pw="$2" token="$3" body resp session uid tok
  body=$(jq -nc --arg u "$user" --arg p "$pw" '{username:$u,password:$p,inhibit_login:false}')
  resp=$(curl -sS -X POST "$HS/_matrix/client/v3/register" -d "$body")
  session=$(echo "$resp" | jq -r '.session // empty')
  [ -n "$session" ] || { echo "register $user: no UIAA session: $resp" >&2; return 1; }

  body=$(jq -nc --arg u "$user" --arg p "$pw" --arg t "$token" --arg s "$session" \
    '{username:$u,password:$p,inhibit_login:false,auth:{type:"m.login.registration_token",token:$t,session:$s}}')
  resp=$(curl -sS -X POST "$HS/_matrix/client/v3/register" -d "$body")
  if [ -z "$(echo "$resp" | jq -r '.access_token // empty')" ]; then
    body=$(jq -nc --arg u "$user" --arg p "$pw" --arg s "$session" \
      '{username:$u,password:$p,inhibit_login:false,auth:{type:"m.login.dummy",session:$s}}')
    resp=$(curl -sS -X POST "$HS/_matrix/client/v3/register" -d "$body")
  fi
  uid=$(echo "$resp" | jq -r '.user_id // empty')
  tok=$(echo "$resp" | jq -r '.access_token // empty')
  [ -n "$uid" ] && [ -n "$tok" ] || { echo "register $user failed: $resp" >&2; return 1; }
  echo "$uid $tok"
}

case "${1:-up}" in
  up)   up ;;
  down) down ;;
  *)    echo "usage: $0 [up|down]" >&2; exit 2 ;;
esac
