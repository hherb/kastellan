#!/usr/bin/env bash
# Build the kv-demo micro-VM rootfs (ext4) beside the shared vmlinux. kv-demo is
# a pure-Rust Net::Deny worker; no python, no CA bundle. The /data mountpoint is
# where the persistent RW store image (slice 5b-2) is mounted in-guest.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: ./scripts/workers/kv-demo/build-kv-demo-rootfs.sh" >&2; exit 1
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
cargo build --release -p kastellan-worker-kv-demo -p kastellan-microvm-init

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
