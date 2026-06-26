#!/usr/bin/env bash
# Build the python-exec micro-VM rootfs (ext4) + fetch the pinned guest kernel.
# Mirrors the macOS build-image.sh cross-build: compile the worker + init for
# the Linux guest in a bind-mounted rust container (or natively on the DGX),
# then assemble a minimal ext4 with python + both binaries + the init as PID1.
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/aarch64/vmlinux-6.1.102"
ROOTFS_MIB=512
mkdir -p "$OUT_DIR"

# 1. Guest kernel (pinned).
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# 2. Cross-build worker + init for the guest (native on the DGX aarch64).
source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-python-exec -p kastellan-microvm-init

# 3. Assemble the ext4 (needs root for mknod-free debugfs; use mkfs.ext4 -d).
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-python-exec "$WORK/usr/local/bin/kastellan-worker-python-exec"
# Minimal python: copy the system python3 + its required libs (or apt extract).
install -D -m0755 "$(command -v python3)" "$WORK/usr/local/bin/python3"
# (Implementer: include python3's shared-lib closure via `ldd` — same
#  out-of-prefix-dep approach as core/src/workers/interpreter_deps.rs.)
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev"
mkfs.ext4 -q -F -L kastellan-rootfs -d "$WORK" "$OUT_DIR/python-exec.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/python-exec.ext4 + $OUT_DIR/vmlinux"
