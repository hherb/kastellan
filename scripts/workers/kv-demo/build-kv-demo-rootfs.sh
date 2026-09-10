#!/usr/bin/env bash
# Build the kv-demo micro-VM rootfs (ext4) beside the shared vmlinux. kv-demo is
# a pure-Rust Net::Deny worker; no python, no CA bundle. The /data mountpoint is
# where the persistent RW store image (slice 5b-2) is mounted in-guest.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: ./scripts/workers/kv-demo/build-kv-demo-rootfs.sh" >&2; exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
# Pinned, integrity-checked guest kernel. This script lives outside
# scripts/workers/microvm/, so the shared snippet is one directory across.
source "$(dirname "${BASH_SOURCE[0]}")/../microvm/lib/guest-kernel.sh"

# Repo-relative paths below (`target/release/...`) mean this script must run
# from the workspace root whatever directory it was invoked from (#686).
# Resolved from BASH_SOURCE, which is why it sits AFTER the `source` above:
# that line resolves its own relative path and must not run post-`cd`.
# ⚠️ `cd ""` SUCCEEDS in bash, so an empty REPO_ROOT would silently run the
# build from the operator's cwd and blame a missing `target/release/...`.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)" || REPO_ROOT=""
if [ -z "$REPO_ROOT" ] || ! cd "$REPO_ROOT"; then
    echo "Cannot locate the workspace root from ${BASH_SOURCE[0]}" >&2; exit 1
fi
ROOTFS_MIB=128

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write micro-VM dir: $OUT_DIR — run sudo ./scripts/linux/install-firecracker-vsock.sh or set KASTELLAN_MICROVM_DIR." >&2
    exit 1
fi
require_guest_kernel "$OUT_DIR"

# Guest binaries come from the one canonical producer, never from a narrow
# `cargo build -p ...` here: cargo unifies features per invocation, so package
# selection changes the BYTES of an identical binary, and a private invocation
# leaves a target/release/ reference no image was ever built from — which makes
# the #667 freshness gate call every correct image stale (#682). It is the same
# script scripts/upgrade_from_git.sh deploys with.
# Rationale + the tests that pin it: RELEASE_BUILD_SCRIPT in
# tests-common/src/microvm/images.rs.
bash scripts/build-release.sh

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-kv-demo "$WORK/usr/local/bin/kastellan-worker-kv-demo"

copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure target/release/kastellan-microvm-init target/release/kastellan-worker-kv-demo

mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"

mkfs.ext4 -q -F -O ^has_journal -L kv-demo -d "$WORK" "$OUT_DIR/kv-demo.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/kv-demo.ext4 (+ shared $OUT_DIR/vmlinux)"
