#!/usr/bin/env bash
# Build the web-research micro-VM rootfs (ext4) into the SHARED image dir, beside
# python-exec.ext4 + web-fetch.ext4 + the shared vmlinux. The dir + kernel are
# shared across workers (build-rootfs.sh provisions them); only the rootfs
# filename differs (KASTELLAN_MICROVM_ROOTFS=web-research.ext4). web-research is a
# pure-Rust net worker: no python, and NO system CA bundle — egress (SearxNG
# search, page fetches, optional embed POSTs) is MITM-only and the only trusted
# root is the per-instance proxy CA delivered per-spawn via the slice-3 RO-share.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/build-web-research-rootfs.sh" >&2
    exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *)
        echo "Unsupported architecture '${HOST_ARCH}'. The pinned guest kernel is published for x86_64 and aarch64 only." >&2
        exit 1
        ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
ROOTFS_MIB=256

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write the micro-VM image dir: $OUT_DIR" >&2
    echo "Run the one-time privileged setup first:" >&2
    echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    echo "or build into a user-writable dir (set the same KASTELLAN_MICROVM_DIR in the service env):" >&2
    echo "    KASTELLAN_MICROVM_DIR=\"\$HOME/.local/share/kastellan/microvm\" ./scripts/workers/microvm/build-web-research-rootfs.sh" >&2
    exit 1
fi

# Shared guest kernel (pinned). Reused if build-rootfs.sh already fetched it.
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# Cross-build worker + init for the guest (native on the DGX aarch64).
source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-web-research -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT

# Binaries: init is PID1 at /sbin/init; the worker at its in-rootfs path
# (matches MICROVM_WORKER_BIN in core/src/workers/web_research.rs).
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-web-research "$WORK/usr/local/bin/kastellan-worker-web-research"

# Shared-library closure for both Rust binaries (dynamic loader + ldd .so's),
# copied at their real absolute paths.
copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure \
    target/release/kastellan-microvm-init \
    target/release/kastellan-worker-web-research

# Pseudo-fs mountpoints (microvm-init mounts proc/sys/tmp at boot) + slice-3
# host-dir-share anchors + slice-4a /run egress relay tmpfs mountpoint. Keep this
# anchor list in lockstep with mounts.rs::SHARE_ANCHORS (opt/data/srv/mnt/work/tmp)
# and build-rootfs.sh. The per-instance ca.pem binds under /tmp (a boot tmpfs).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
mkdir -p "$WORK/run"

# Journal-less ext4 (read-only at runtime, shared across concurrent VMs).
mkfs.ext4 -q -F -O ^has_journal -L web-research -d "$WORK" "$OUT_DIR/web-research.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/web-research.ext4 (+ shared $OUT_DIR/vmlinux)"
