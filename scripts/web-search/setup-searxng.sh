#!/usr/bin/env bash
# Stand up a local SearxNG instance for the kastellan web-search worker.
#
# SearxNG serves plain HTTP on a loopback port and DISABLES the JSON format by
# default — this script writes a settings.yml that enables JSON and runs the
# official container bound to 127.0.0.1:8888. Cross-platform: podman or docker,
# on Linux and macOS. Dev convenience only; not part of the worker's trust
# boundary.
#
# podman is preferred when both are on PATH: rootless podman runs the container
# in a user namespace as the invoking user, with no root daemon, which matches
# kastellan's own `systemd --user` deployment. A docker daemon is root, and its
# group is root-equivalent (docs/devel/notes/2026-11-16-kiri-survey.md).
# KASTELLAN_SEARXNG_RUNTIME=docker|podman overrides the choice.
set -euo pipefail

PORT="${KASTELLAN_SEARXNG_PORT:-8888}"
NAME="${KASTELLAN_SEARXNG_NAME:-kastellan-searxng}"
STATE_DIR="${KASTELLAN_SEARXNG_STATE:-$HOME/.local/state/kastellan/searxng}"
# Fully qualified: podman refuses (or prompts for) a short name under
# `short-name-mode = "enforcing"`; docker reads the same reference unchanged.
IMAGE="docker.io/searxng/searxng:latest"

# Pick a container runtime: an explicit choice, else podman, else docker.
RT="${KASTELLAN_SEARXNG_RUNTIME:-}"
case "$RT" in
  "")
    if command -v podman >/dev/null 2>&1; then
      RT=podman
    elif command -v docker >/dev/null 2>&1; then
      RT=docker
    else
      echo "error: need podman or docker on PATH to run SearxNG" >&2
      exit 1
    fi
    ;;
  podman|docker)
    if ! command -v "$RT" >/dev/null 2>&1; then
      echo "error: KASTELLAN_SEARXNG_RUNTIME=$RT, but $RT is not on PATH" >&2
      exit 1
    fi
    ;;
  *)
    echo "error: KASTELLAN_SEARXNG_RUNTIME must be podman or docker, not '$RT'" >&2
    exit 1
    ;;
esac
echo "using $RT"

# A host set up before podman was preferred may already run this container
# under the OTHER runtime. Starting a second one would fail on the port (or,
# on a different port, leave two instances), so stop here and say how to move.
case "$RT" in podman) OTHER=docker ;; *) OTHER=podman ;; esac
if command -v "$OTHER" >/dev/null 2>&1 \
   && "$OTHER" ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$NAME"; then
  cat >&2 <<MSG
error: a container named $NAME already exists under $OTHER.
  To move it to $RT:  $OTHER rm -f $NAME   and run this script again.
  To keep it on $OTHER:  KASTELLAN_SEARXNG_RUNTIME=$OTHER $0
MSG
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

The same instance backs the web-research worker's live e2e
(core/tests/web_research_e2e.rs::real_research_against_searxng). That test
reads KASTELLAN_WEB_RESEARCH_ENDPOINT (default http://127.0.0.1:${PORT}/search)
and allowlists en.wikipedia.org as the content host itself:

  export KASTELLAN_WEB_RESEARCH_ENDPOINT='http://127.0.0.1:${PORT}/search'
  cargo test -p kastellan-core --test web_research_e2e -- --ignored --nocapture

For the hybrid-ranking live e2e (real_research_with_hybrid_ranking) you also
need an embedding endpoint. The default is a local Ollama with embeddinggemma:

  ollama serve &                       # OpenAI-compat /v1/embeddings on :11434
  ollama pull embeddinggemma
  export KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT='http://127.0.0.1:11434/v1/embeddings'
  # (KASTELLAN_WEB_RESEARCH_EMBED_MODEL defaults to embeddinggemma)
  cargo test -p kastellan-core --test web_research_e2e real_research_with_hybrid_ranking -- --ignored --nocapture

Smoke test the JSON API:

  curl -s 'http://127.0.0.1:${PORT}/search?q=rust&format=json' | head -c 400

Stop it with:  $RT rm -f $NAME
MSG
