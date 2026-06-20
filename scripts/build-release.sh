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
set -euo pipefail

# shellcheck disable=SC1090,SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

echo "==> cargo build --release --workspace"
cargo build --release --workspace

echo "==> cargo build --release -p kastellan-worker-matrix --features live-matrix"
cargo build --release -p kastellan-worker-matrix --features live-matrix

echo "==> release binaries ready in target/release/ (matrix worker is the live-matrix build)"
