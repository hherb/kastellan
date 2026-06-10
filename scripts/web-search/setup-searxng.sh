#!/usr/bin/env bash
# Stand up a local SearxNG instance for the kastellan web-search worker.
#
# SearxNG serves plain HTTP on a loopback port and DISABLES the JSON format by
# default — this script writes a settings.yml that enables JSON and runs the
# official container bound to 127.0.0.1:8888. Cross-platform: Docker Desktop on
# macOS, docker or podman on Linux. Dev convenience only; not part of the
# worker's trust boundary.
set -euo pipefail

PORT="${KASTELLAN_SEARXNG_PORT:-8888}"
NAME="${KASTELLAN_SEARXNG_NAME:-kastellan-searxng}"
STATE_DIR="${KASTELLAN_SEARXNG_STATE:-$HOME/.local/state/kastellan/searxng}"
IMAGE="searxng/searxng:latest"

# Pick a container runtime.
if command -v docker >/dev/null 2>&1; then
  RT=docker
elif command -v podman >/dev/null 2>&1; then
  RT=podman
else
  echo "error: need docker or podman on PATH to run SearxNG" >&2
  exit 1
fi

mkdir -p "$STATE_DIR"
SETTINGS="$STATE_DIR/settings.yml"

# Generate a random secret_key and enable the JSON output format.
if [ ! -f "$SETTINGS" ]; then
  SECRET="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  cat >"$SETTINGS" <<YAML
# Minimal SearxNG settings for kastellan web-search (dev). The key line is
# search.formats — the JSON API is off by default.
use_default_settings: true
server:
  secret_key: "$SECRET"
  bind_address: "0.0.0.0"
  port: 8080
search:
  formats:
    - html
    - json
YAML
  echo "wrote $SETTINGS"
fi

# (Re)start the container.
if "$RT" ps -a --format '{{.Names}}' | grep -qx "$NAME"; then
  echo "restarting existing container $NAME"
  "$RT" rm -f "$NAME" >/dev/null
fi

"$RT" run -d \
  --name "$NAME" \
  -p "127.0.0.1:${PORT}:8080" \
  -v "$SETTINGS:/etc/searxng/settings.yml:ro" \
  "$IMAGE" >/dev/null

cat <<MSG

SearxNG running at http://127.0.0.1:${PORT}/

Export these for the kastellan daemon / web-search worker:

  export KASTELLAN_WEB_SEARCH_ENDPOINT='http://127.0.0.1:${PORT}/search'
  export KASTELLAN_WEB_SEARCH_ALLOWLIST='["127.0.0.1"]'

Smoke test the JSON API:

  curl -s 'http://127.0.0.1:${PORT}/search?q=rust&format=json' | head -c 400

Stop it with:  $RT rm -f $NAME
MSG
