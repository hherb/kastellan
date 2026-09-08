#!/usr/bin/env bash
# Build every release binary the installer copies, INCLUDING the Matrix worker
# with its `live-matrix` feature.
#
# `cargo build --release --workspace` builds `kastellan-worker-matrix` WITHOUT
# `live-matrix` (the feature is opt-in to keep default builds free of the heavy
# matrix-rust-sdk subtree). A worker built that way refuses to run. The installer
# copies whatever sits in `target/release/`, so the Matrix worker must be rebuilt
# with the feature here — otherwise a configured Matrix channel fails at spawn.
#
# Run this before `kastellan-cli install` (which copies from target/release/).
#
# ⚠️ THIS IS TWO INVOCATIONS, AND THE SECOND ONE IS REVERSIBLE.
#
# Both write `target/release/kastellan-worker-matrix` and the last writer wins,
# so the order below is load-bearing. It also means the result is NOT stable
# against anything else touching that path: a bare `cargo build --release
# --workspace` re-uplifts the non-featured artefact in well under a second,
# prints nothing but `Finished`, and leaves a matrix worker that refuses to run
# and a `matrix.ext4` the #667 freshness gate calls stale. Measured 2026-09-08:
#
#     live-matrix build   0ca537c2…
#     bare --workspace    9d29cd49…   (0.32 s, no compilation, no warning)
#
# So: after ANY hand-run `cargo build --release`, run this script again. The
# digest printed at the end is there to make the flip visible rather than
# something you discover from a stale-image panic.
#
# Everything that writes `target/release/` for a shipped artefact goes through
# here — `scripts/upgrade_from_git.sh` and all eight `build-*-rootfs.sh`.
# `tests-common/src/microvm/images.rs::RELEASE_BUILD_SCRIPT` carries the full
# rationale (issue #682) and the tests that pin it.
set -euo pipefail

# Callers invoke this by path from wherever they happen to be, and cargo's
# upward Cargo.toml search would otherwise pick up whatever workspace the
# caller's cwd sits in. Anchor on the script's own location instead.
# `cd ""` SUCCEEDS in bash, so an empty REPO_ROOT would silently build the
# wrong tree — same trap rebuild-all-rootfs.sh guards.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" || REPO_ROOT=""
if [ -z "$REPO_ROOT" ] || ! cd "$REPO_ROOT"; then
    echo "Cannot locate the workspace root from ${BASH_SOURCE[0]}" >&2; exit 1
fi

# shellcheck disable=SC1090,SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
# The `|| true` above swallows a missing env file, so say what is actually
# wrong rather than letting the first invocation fail with a bare
# `cargo: command not found`. Same shape as build-browser-driver-rootfs.sh's
# `command -v docker` preflight.
if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is not on PATH and \$HOME/.cargo/env did not provide it." >&2
    echo "Install Rust (https://rustup.rs) or source the right env first." >&2
    exit 1
fi

echo "==> cargo build --release --workspace"
cargo build --release --workspace

# MUST run after the workspace build, never before: it overwrites what that
# one just wrote for this single binary.
echo "==> cargo build --release -p kastellan-worker-matrix --features live-matrix"
cargo build --release -p kastellan-worker-matrix --features live-matrix

# Report the matrix digest, because it is the one artefact here whose correct
# value a later `cargo build --release --workspace` silently replaces. An
# operator comparing this line against the panic from a stale-image gate can
# tell invocation skew from a genuinely stale image in one step.
matrix_digest() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -c1-16
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -c1-16
    else
        echo "(no sha256 tool)"
    fi
}
echo "==> release binaries ready in target/release/ (matrix worker is the live-matrix build)"
echo "    kastellan-worker-matrix sha256: $(matrix_digest target/release/kastellan-worker-matrix)…"
echo "    a later bare \`cargo build --release --workspace\` will silently change that digest."
