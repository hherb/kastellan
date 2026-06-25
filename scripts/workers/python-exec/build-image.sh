#!/usr/bin/env bash
#
# Build the python-exec container image consumed by the macOS
# MacosContainer SandboxBackend (Phase 4 micro-VM mode). Companion to the
# host-mode path (Seatbelt/bwrap), which needs no image.
#
# Tag default: kastellan/python-exec:dev (overridable via
# KASTELLAN_PYTHON_EXEC_IMAGE, matching the daemon-side knob).
#
# Usage:
#     scripts/workers/python-exec/build-image.sh
#     KASTELLAN_PYTHON_EXEC_IMAGE=kastellan/python-exec:v0.0.1 \
#         scripts/workers/python-exec/build-image.sh
#
# Env overrides:
#     KASTELLAN_PYTHON_EXEC_IMAGE        runtime image tag (default kastellan/python-exec:dev)
#     KASTELLAN_PYTHON_EXEC_BUILD_IMAGE  builder image for the cross-compile (default rust:1-slim)
#
# Two-step build (see workers/python-exec/Containerfile for the full rationale):
#   1. Cross-build the worker for the guest arch in a bind-mounted `rust`
#      container, reusing the HOST cargo cache with `--offline`. This sidesteps
#      Apple `container`'s BuildKit, which cannot transfer the multi-GB
#      workspace as a build context and whose in-image network can't reach
#      crates.io reliably.
#   2. Build the runtime image (python:3.12-slim + the lone prebuilt binary).
#
# Prereq: the worker's dependencies must already be in the host cargo cache
# (~/.cargo/registry). They are, after any prior workspace build; if the
# `--offline` step fails with "no matching package", run once on the host:
#     cargo build -p kastellan-worker-python-exec
# The reused cache is the registry's *source* crates (arch-neutral — the same
# .crate tarballs feed any target), NOT host-arch compiled artifacts, so the
# in-container build for the guest arch (aarch64 on Apple Silicon) compiles them
# fresh; only the downloads are skipped.
#
# Exits non-zero with a clear message if `container` CLI is missing, its
# system service is down, or the host cargo cache is absent.

set -euo pipefail

IMAGE_TAG="${KASTELLAN_PYTHON_EXEC_IMAGE:-kastellan/python-exec:dev}"
# Builder image for the cross-compile. PINNED to the SAME Debian suite as the
# runtime image (`python:3.12-slim-bookworm` in workers/python-exec/Containerfile):
# the binary is linked against this image's glibc and run against the runtime
# image's glibc, so the two suites MUST match or the worker fails to load in the
# VM with `version 'GLIBC_2.xx' not found`. If you override this, keep the suite
# in lockstep with the Containerfile's `FROM`.
BUILD_IMAGE="${KASTELLAN_PYTHON_EXEC_BUILD_IMAGE:-rust:1-slim-bookworm}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
CONTAINERFILE="$REPO_ROOT/workers/python-exec/Containerfile"
CARGO_REGISTRY="${CARGO_HOME:-$HOME/.cargo}/registry"

if [[ ! -f "$CONTAINERFILE" ]]; then
    echo "error: Containerfile not found at $CONTAINERFILE" >&2
    exit 2
fi

if ! command -v container >/dev/null 2>&1; then
    echo "error: Apple \`container\` CLI not found on PATH." >&2
    echo "       Install with: brew install container" >&2
    exit 3
fi

if ! container system status >/dev/null 2>&1; then
    echo "error: \`container\` system service is not running." >&2
    echo "       Start it with: container system start" >&2
    exit 4
fi

if [[ ! -d "$CARGO_REGISTRY" ]]; then
    echo "error: host cargo registry not found at $CARGO_REGISTRY." >&2
    echo "       The image cross-build reuses the host cache (offline). Run a" >&2
    echo "       host build first: cargo build -p kastellan-worker-python-exec" >&2
    exit 5
fi

# Scratch dirs under /tmp. Two Apple-`container` path rules learned the hard way:
#   * The build CONTEXT must be a /tmp/... path. `container build` shares /tmp
#     into the builder VM but NOT its real target /private/tmp, so resolving
#     the symlink (e.g. `pwd -P`) makes the context arrive EMPTY (~2 bytes).
#     Likewise $TMPDIR (/var/folders/...) is rejected outright. So we keep the
#     raw `mktemp -d /tmp/...` path and never canonicalize it.
#   * The context dir must be world-readable: `mktemp -d` makes it 0700, which
#     the builder (a different uid) cannot traverse — again an empty context.
#     `chmod 755` makes it readable. (The dir holds only the throwaway binary.)
OUT_DIR="$(mktemp -d /tmp/kastellan-pyexec-out.XXXXXX)"
CTX_DIR="$(mktemp -d /tmp/kastellan-pyexec-ctx.XXXXXX)"
chmod 755 "$OUT_DIR" "$CTX_DIR"
trap 'rm -rf "$OUT_DIR" "$CTX_DIR"' EXIT

# Step 1: cross-build the worker for the guest arch.
#   * source bind-mounted READ-ONLY at /src (no context transfer);
#   * target-dir at /out (writable);
#   * host cargo registry mounted so `--offline` finds every locked crate.
echo "[1/2] Cross-building kastellan-worker-python-exec for the container guest ..."
container run --rm \
    --mount type=bind,source="$REPO_ROOT",target=/src,readonly \
    --mount type=bind,source="$OUT_DIR",target=/out \
    --mount type=bind,source="$CARGO_REGISTRY",target=/usr/local/cargo/registry \
    -w /src "$BUILD_IMAGE" \
    sh -c 'cargo build --release --locked --offline \
        --manifest-path /src/Cargo.toml \
        -p kastellan-worker-python-exec --target-dir /out'

BIN="$OUT_DIR/release/kastellan-worker-python-exec"
if [[ ! -f "$BIN" ]]; then
    echo "error: cross-build did not produce $BIN" >&2
    exit 6
fi

# Step 2: build the runtime image from a lone-file context (just the binary).
# `--no-cache`: the runtime layer is a single tiny COPY, so caching buys
# nothing — and Apple `container`'s BuildKit can silently match a stale COPY
# layer (producing an image MISSING the binary while `python3` still works).
# Forcing a fresh COPY guarantees the binary is actually present.
cp "$BIN" "$CTX_DIR/"
# Make the exec bit explicit on the staged file. cargo already emits 0755 and
# standard COPY preserves the source mode, so the binary lands executable for
# `USER nobody` in the runtime image. Belt-and-suspenders; the smoke-check below
# is the real guard.
chmod 0755 "$CTX_DIR"/*
echo "[2/2] Building $IMAGE_TAG (runtime image, lone-file context) ..."
container build \
    --no-cache \
    --tag "$IMAGE_TAG" \
    --file "$CONTAINERFILE" \
    "$CTX_DIR"

# Smoke-check: actually EXECUTE the worker binary inside the runtime image. The
# build succeeding only proves the COPY ran — it does NOT prove the binary can
# load and run against the runtime rootfs. This catches the two failure modes a
# green build would otherwise hide until agent runtime in the VM:
#   * a GLIBC mismatch (build suite != runtime suite) → the dynamic loader aborts
#     before main with `version 'GLIBC_2.xx' not found`;
#   * a lost exec bit / broken COPY → `Permission denied` / `Exec format error`.
# We feed /dev/null on stdin so the worker's stdio serve loop sees EOF and exits
# promptly instead of blocking; its own exit code is irrelevant (it may exit
# non-zero resolving env without the daemon's setup), so we don't gate on it.
# We gate ONLY on the loader/exec failure signatures, which appear before main.
echo "Smoke-checking the worker binary inside $IMAGE_TAG ..."
set +e
smoke_out="$(container run --rm "$IMAGE_TAG" \
    /usr/local/bin/kastellan-worker-python-exec </dev/null 2>&1)"
set -e
if printf '%s' "$smoke_out" | grep -qiE \
    "GLIBC_|error while loading shared libraries|cannot execute|exec format error|permission denied"; then
    echo "error: the worker binary fails to load/execute in $IMAGE_TAG:" >&2
    printf '%s\n' "$smoke_out" >&2
    echo "       Likely a build/runtime glibc mismatch or a lost exec bit." >&2
    exit 7
fi

echo "Built $IMAGE_TAG. Verify the interpreter:"
echo "    container run --rm $IMAGE_TAG /usr/local/bin/python3 --version"
