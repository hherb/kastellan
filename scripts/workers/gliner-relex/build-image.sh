#!/usr/bin/env bash
#
# Build the gliner-relex container image consumed by the macOS
# MacosContainer SandboxBackend. Companion to install.sh — install.sh
# builds the host venv (native Seatbelt/bwrap mode); this builds the
# container image (macOS container mode).
#
# Tag default: kastellan/gliner-relex:dev (overridable via
# KASTELLAN_GLINER_RELEX_IMAGE env, matching the daemon-side knob).
#
# Usage:
#     scripts/workers/gliner-relex/build-image.sh
#     KASTELLAN_GLINER_RELEX_IMAGE=kastellan/gliner-relex:v0.0.1 \
#         scripts/workers/gliner-relex/build-image.sh
#
# Exits non-zero with a clear message if `container` CLI is missing or
# the `container` system service is not running. The image build itself
# takes ~3-5 minutes on a fresh M3 Max (PyTorch + transformers + gliner
# wheels are ~3 GB).

set -euo pipefail

IMAGE_TAG="${KASTELLAN_GLINER_RELEX_IMAGE:-kastellan/gliner-relex:dev}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKER_DIR="$(cd "$SCRIPT_DIR/../../../workers/gliner-relex" && pwd)"

if [[ ! -f "$WORKER_DIR/Containerfile" ]]; then
    echo "error: Containerfile not found at $WORKER_DIR/Containerfile" >&2
    exit 2
fi

if ! command -v container >/dev/null 2>&1; then
    echo "error: 'container' CLI not on PATH" >&2
    echo "  install via: brew install container" >&2
    echo "  then: container system start --enable-kernel-install" >&2
    exit 2
fi

if ! container system status >/dev/null 2>&1; then
    echo "error: 'container' system service is not running" >&2
    echo "  start via: container system start" >&2
    exit 2
fi

echo "Building $IMAGE_TAG from $WORKER_DIR"
container build -t "$IMAGE_TAG" "$WORKER_DIR"

cat <<EOF

Done. To enable container-mode in the daemon, set both:
    export KASTELLAN_GLINER_RELEX_ENABLE=1
    export KASTELLAN_GLINER_RELEX_USE_CONTAINER=1

If you used a non-default image tag, also set:
    export KASTELLAN_GLINER_RELEX_IMAGE=$IMAGE_TAG
EOF
