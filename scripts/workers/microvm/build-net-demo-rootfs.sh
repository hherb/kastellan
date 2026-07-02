#!/usr/bin/env bash
# Build the net-demo micro-VM rootfs (ext4) beside the shared vmlinux. net-demo is
# a pure-Rust Net::Allowlist worker that does its OWN end-to-end TLS through the
# egress proxy (transparent tunnel). No python. NO OS ca-certificates bundle — the
# worker trusts compiled-in webpki roots; a test origin's CA (when present) is
# delivered per-spawn via the RO-share. /run is the egress-relay mountpoint (4a).
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: ./scripts/workers/microvm/build-net-demo-rootfs.sh" >&2; exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *) echo "Unsupported arch '${HOST_ARCH}'." >&2; exit 1 ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
ROOTFS_MIB=128

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write micro-VM dir: $OUT_DIR — run sudo ./scripts/linux/install-firecracker-vsock.sh or set KASTELLAN_MICROVM_DIR." >&2
    exit 1
fi
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-net-demo -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-net-demo "$WORK/usr/local/bin/kastellan-worker-net-demo"

copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure target/release/kastellan-microvm-init target/release/kastellan-worker-net-demo

# Pseudo-fs + slice-3 share anchors + /run (egress relay, slice 4a).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" "$WORK/run" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"

mkfs.ext4 -q -F -O ^has_journal -L net-demo -d "$WORK" "$OUT_DIR/net-demo.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/net-demo.ext4 (+ shared $OUT_DIR/vmlinux)"
