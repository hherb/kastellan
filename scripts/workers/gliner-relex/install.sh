#!/usr/bin/env bash
# Operator setup for the gliner-relex worker.
# Idempotent; safe to re-run.

set -euo pipefail

# ----- pre-flight -----
command -v uv >/dev/null 2>&1 || {
  echo "error: uv is required (install: https://docs.astral.sh/uv/getting-started/installation/)" >&2
  exit 1
}

if command -v hf >/dev/null 2>&1; then
  HF=hf
elif command -v huggingface-cli >/dev/null 2>&1; then
  HF=huggingface-cli
else
  echo "error: hf or huggingface-cli is required (pip install huggingface_hub)" >&2
  exit 1
fi

# ----- paths -----
REPO_ROOT="$(git rev-parse --show-toplevel)"
WORKER_DIR="$REPO_ROOT/workers/gliner-relex"
DATA_DIR="${HHAGENT_DATA_DIR:-$HOME/.local/share/hhagent}"
WEIGHTS_DIR="$DATA_DIR/workers/gliner-relex/weights"

if [ ! -d "$WORKER_DIR" ]; then
  echo "error: $WORKER_DIR not found; run from a checkout of the hhagent repo" >&2
  exit 1
fi

echo ">>> uv sync in $WORKER_DIR"
(cd "$WORKER_DIR" && uv sync --all-extras)

echo ">>> ensuring $WEIGHTS_DIR"
mkdir -p "$WEIGHTS_DIR"

echo ">>> downloading multi-v1.0 to $WEIGHTS_DIR/multi-v1.0"
"$HF" download knowledgator/gliner-relex-multi-v1.0 \
  --local-dir "$WEIGHTS_DIR/multi-v1.0"

if [ "${HHAGENT_GLINER_RELEX_INSTALL_LARGE:-0}" = "1" ]; then
  echo ">>> downloading large-v0.5 to $WEIGHTS_DIR/large-v0.5"
  "$HF" download knowledgator/gliner-relex-large-v0.5 \
    --local-dir "$WEIGHTS_DIR/large-v0.5"
fi

# ----- license-chain sanity check -----
# multi-v1.0 ships `gliner_config.json` (not a plain `config.json`) and
# `model.safetensors`; both must be present for the worker to load.
for required in gliner_config.json model.safetensors; do
  if [ ! -f "$WEIGHTS_DIR/multi-v1.0/$required" ]; then
    echo "error: $required not found at $WEIGHTS_DIR/multi-v1.0 - download failed" >&2
    exit 2
  fi
done

echo
echo "ok: gliner-relex weights at $WEIGHTS_DIR"
echo "ok: venv at $WORKER_DIR/.venv"
echo "To enable in the daemon, export HHAGENT_GLINER_RELEX_ENABLE=1 before starting hhagent."
