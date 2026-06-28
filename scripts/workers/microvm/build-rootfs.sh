#!/usr/bin/env bash
# Build the python-exec micro-VM rootfs (ext4) + fetch the pinned guest kernel.
# Mirrors the macOS build-image.sh cross-build: compile the worker + init for
# the Linux guest in a bind-mounted rust container (or natively on the DGX),
# then assemble a minimal ext4 with python + both binaries + the init as PID1.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/build-rootfs.sh" >&2
    exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
# Pinned guest kernel, selected for the host arch (the firecracker-ci bucket
# publishes the same kernel under x86_64/ and aarch64/). Don't hardcode the
# DGX's aarch64 — the backend must run on any Linux box (CLAUDE.md).
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *)
        echo "Unsupported architecture '${HOST_ARCH}'. The pinned guest kernel is published for x86_64 and aarch64 only." >&2
        exit 1
        ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
ROOTFS_MIB=768

# The output dir defaults to /var/lib/kastellan/microvm, which is provisioned
# (created + chowned to the worker user) by the privileged one-time setup
# `sudo scripts/linux/install-firecracker-vsock.sh`. If it is missing or
# unwritable, point the operator at that step rather than failing on a bare
# `mkdir: Permission denied`.
if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write the micro-VM image dir: $OUT_DIR" >&2
    echo "Run the one-time privileged setup first:" >&2
    echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    echo "or build into a user-writable dir:" >&2
    echo "    KASTELLAN_MICROVM_DIR=\"\$HOME/.local/share/kastellan/microvm\" ./scripts/workers/microvm/build-rootfs.sh" >&2
    echo "(set the same KASTELLAN_MICROVM_DIR in the kastellan service env so the backend finds it)." >&2
    exit 1
fi

# 1. Guest kernel (pinned).
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# 2. Cross-build worker + init for the guest (native on the DGX aarch64).
source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-python-exec -p kastellan-microvm-init

# 3. Assemble the ext4. No root needed: mkfs.ext4 -d stages a dir tree without
#    loop-mounting or mknod. Everything is copied as the building user.
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT

# 3a. Binaries. init is PID1 at /sbin/init; the worker stays at its baked path.
#     python3 goes to its NATIVE prefix (/usr/bin) so CPython's getpath finds
#     the stdlib under the matching /usr/lib/pythonX.Y — the worker runs it with
#     `-I` (isolated), which ignores PYTHON* env, so the layout, not env, must be
#     right. `kastellan-microvm-init` bakes KASTELLAN_PYTHON_EXEC_PYTHON to match.
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-python-exec "$WORK/usr/local/bin/kastellan-worker-python-exec"
PYTHON_BIN="$(command -v python3)"
install -D -m0755 "$PYTHON_BIN" "$WORK/usr/bin/python3"

# 3b. Shared-library closure: the dynamic loader + every ldd-resolved .so for the
#     two Rust binaries, the python interpreter, AND python's lib-dynload C
#     extension modules (so e.g. `socket`/`array` import in-guest). Copied at
#     their real absolute paths. This is the out-of-prefix-dep approach noted in
#     core/src/workers/interpreter_deps.rs, applied to the guest rootfs.
STDLIB="$(python3 -c 'import sysconfig; print(sysconfig.get_path("stdlib"))')"  # e.g. /usr/lib/python3.12
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
    target/release/kastellan-worker-python-exec \
    "$PYTHON_BIN" \
    "$STDLIB"/lib-dynload/*.so

# 3c. Python standard library, copied to its native path (the whole tree,
#     including lib-dynload). Curating it to the minimal import set is a later
#     optimisation; for slice 1 the full stdlib is the robust choice.
mkdir -p "$WORK$(dirname "$STDLIB")"
cp -a "$STDLIB" "$WORK$STDLIB"

# 3d. Pseudo-fs mountpoints (kastellan-microvm-init mounts proc/sys/tmp at boot)
#     + slice-3 host-dir-share anchors: /ro-share holds the RO share mount; the
#     others are empty anchors the init tmpfs-mounts so bind-mount targets can be
#     mkdir'd on the otherwise read-only root. fs_read/fs_write paths MUST live
#     under one of these anchors — build_launch_plan enforces this as an ALLOWLIST
#     (mounts.rs::SHARE_ANCHORS = opt/data/srv/mnt/work/tmp), rejecting any other
#     top-level (system dirs like /usr AND unanchored dirs like /home|/var) up
#     front so a share never silently fails to mount in-guest. Keep the anchor
#     mkdir list below in lockstep with SHARE_ANCHORS (/tmp is mounted at boot).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
mkdir -p "$WORK/run"   # slice 4a: egress relay tmpfs mountpoint (in-guest UDS lives here)

# Build the image journal-less (`-O ^has_journal`). The backend attaches the
# rootfs READ-ONLY (is_read_only:true) and every concurrent VM shares this one
# file, so a journal is both useless (a read-only fs never writes) and actively
# harmful: a journalled ext4 that was ever mounted read-write needs recovery on
# the next mount, and a read-only mount cannot replay it — the guest kernel then
# panics with "recovery required on readonly filesystem ... cannot proceed".
# A journal-less image mounts read-only cleanly every boot. Firecracker
# auto-appends `root=/dev/vda ro` for a read-only drive, so no `ro` boot_arg is
# needed in BASE_BOOT_ARGS.
mkfs.ext4 -q -F -O ^has_journal -L kastellan-rootfs -d "$WORK" "$OUT_DIR/python-exec.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/python-exec.ext4 + $OUT_DIR/vmlinux"
