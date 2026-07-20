#!/usr/bin/env bash
# Build the matrix micro-VM rootfs (ext4) beside the shared vmlinux. The matrix
# worker is a LONG-LIVED Net::Allowlist worker that does its OWN end-to-end TLS to
# the homeserver through the egress proxy (transparent tunnel). Unlike net-demo,
# matrix-sdk 0.18 validates that TLS against the SYSTEM trust store, so this rootfs
# BAKES the OS CA bundle (/etc, /usr are not share anchors — it cannot ride
# fs_read). /run is the egress-relay mountpoint (slice 4a); /data is the persistent
# crypto/session store mountpoint (slice 5b-2).
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: ./scripts/workers/microvm/build-matrix-rootfs.sh" >&2; exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
# Pinned, integrity-checked guest kernel (shared with every sibling script).
source "$(dirname "${BASH_SOURCE[0]}")/lib/guest-kernel.sh"
# matrix-sdk + tokio + rustls/ring + bundled-sqlite closure — net-demo's 128 is far
# too small. 512 MiB leaves headroom for the worker's read-only rootfs.
ROOTFS_MIB=512

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write micro-VM dir: $OUT_DIR — run sudo ./scripts/linux/install-firecracker-vsock.sh or set KASTELLAN_MICROVM_DIR." >&2
    exit 1
fi
fetch_guest_kernel "$OUT_DIR"

source "$HOME/.cargo/env"
# bundled-sqlite ⇒ no host libsqlite3 needed; rustls-tls ⇒ no host OpenSSL needed.
cargo build --release -p kastellan-worker-matrix --features live-matrix -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-matrix "$WORK/usr/local/bin/kastellan-worker-matrix"

copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure target/release/kastellan-microvm-init target/release/kastellan-worker-matrix

# Bake the OS CA trust store: matrix-sdk 0.18 reads the SYSTEM store to validate the
# homeserver leaf. /etc and /usr are not share anchors, so unlike every other rootfs
# this one ships certificates. Copy whatever this build host provides (Debian/Ubuntu
# layout first, RH layout as a fallback); at least one must exist.
CA_FOUND=0
for ca in /etc/ssl/certs /etc/ssl/cert.pem /usr/share/ca-certificates /etc/pki/tls/certs; do
    if [ -e "$ca" ]; then
        install -d "$WORK$(dirname "$ca")"
        cp -a "$ca" "$WORK$ca"
        CA_FOUND=1
    fi
done
if [ "$CA_FOUND" -eq 0 ]; then
    echo "No OS CA trust store found on this build host — matrix TLS validation would fail in-guest." >&2
    exit 1
fi

# Pseudo-fs + slice-3 share anchors + /run (egress relay, slice 4a) + /data
# (persistent crypto store mountpoint, slice 5b-2).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" "$WORK/run" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"

mkfs.ext4 -q -F -O ^has_journal -L matrix -d "$WORK" "$OUT_DIR/matrix.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/matrix.ext4 (+ shared $OUT_DIR/vmlinux)"
