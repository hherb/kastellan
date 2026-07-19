#!/usr/bin/env bash
# Build the browser-driver micro-VM rootfs (ext4) into the SHARED image dir,
# beside python-exec.ext4 + the shared vmlinux. The dir + kernel are shared
# across workers; only the rootfs filename differs
# (KASTELLAN_MICROVM_ROOTFS=browser-driver.ext4).
#
# Unlike its sibling scripts, the staging tree comes from `docker export`
# rather than an `ldd` closure: Chromium dlopen's NSS modules, fontconfig
# backends and SwiftShader at runtime, none of which `ldd` can see. See
# scripts/workers/microvm/Dockerfile.browser-driver for the full rationale.
# Docker is a BUILD-TIME tool only — the runtime is pure Firecracker, exactly
# like every other worker, with no new runtime dependency.
#
# No system CA bundle is baked in: browser-driver runs the egress sidecar in
# no-MITM transparent-tunnel mode (force_route.rs::disable_mitm_for names this
# worker), because the browser does end-to-end TLS itself.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/build-browser-driver-rootfs.sh" >&2
    exit 1
fi
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
IMAGE_TAG="kastellan-browser-driver-rootfs:latest"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *)
        echo "Unsupported architecture '${HOST_ARCH}'. The pinned guest kernel is published for x86_64 and aarch64 only." >&2
        exit 1
        ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
# Measured from the docker-export staging tree (1006 MB after the strip pass on
# aarch64), x1.2 headroom. Re-measure if the Dockerfile's browser set changes —
# the script prints the staging size on every run so drift is visible.
ROOTFS_MIB=1280

if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required to BUILD this rootfs (build-time only; not a runtime dep)." >&2
    exit 1
fi

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write the micro-VM image dir: $OUT_DIR" >&2
    echo "Run the one-time privileged setup first:" >&2
    echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    echo "or build into a user-writable dir (set the same KASTELLAN_MICROVM_DIR in the service env):" >&2
    echo "    KASTELLAN_MICROVM_DIR=\"\$HOME/.local/share/kastellan/microvm\" ./scripts/workers/microvm/build-browser-driver-rootfs.sh" >&2
    exit 1
fi

# Shared guest kernel (pinned). Reused if another build-*-rootfs.sh fetched it.
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# Guest PID1, built on the host with cargo (native on the DGX aarch64), exactly
# as every sibling script does. The worker itself is staged by the container
# image, not by cargo — it is Python.
source "$HOME/.cargo/env"
cargo build --release -p kastellan-microvm-init

WORK=$(mktemp -d)
CID=""
cleanup() {
    rm -rf "$WORK"
    [ -n "$CID" ] && docker rm -f "$CID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Staging tree from the container image. The build context is
# workers/browser-driver (NOT the repo root) so the huge target/ tree is never
# sent to the docker daemon.
docker build -f "$REPO_ROOT/scripts/workers/microvm/Dockerfile.browser-driver" \
    -t "$IMAGE_TAG" "$REPO_ROOT/workers/browser-driver"
CID=$(docker create "$IMAGE_TAG")
docker export "$CID" | tar -C "$WORK" -xf -

# Strip what a read-only render VM never reads. Keep fonts and NSS — Chromium
# dlopen's both. (Small: ~7 MB. The big saving is upstream in the Dockerfile,
# which installs only chromium-headless-shell and not the full 620 MB bundle.)
rm -rf "$WORK/var/lib/apt/lists" "$WORK/var/cache/apt" \
       "$WORK/usr/share/doc" "$WORK/usr/share/man" "$WORK/usr/share/locale" \
       "$WORK/root/.cache"

# PID1 at /sbin/init. The worker is already staged by the image at
# /usr/local/bin/kastellan-worker-browser-driver (slice 2's MICROVM_WORKER_BIN).
install -D -m0755 "$REPO_ROOT/target/release/kastellan-microvm-init" "$WORK/sbin/init"

# Fail closed if the image did not stage the worker entrypoint: without it the
# guest PID1 execv's a missing path, panics, and boot-loops — which presents as
# a dispatch hang to wall-clock rather than as an obvious error.
#
# The entrypoint is an ABSOLUTE symlink into the venv, so it must be resolved
# against the staging root, not the host root. A bare `test -x` here would
# follow /usr/local/lib/... to the BUILD HOST's filesystem (where it does not
# exist) and report a false failure, even though the link resolves correctly
# inside the guest where / IS this tree.
entry_link="$WORK/usr/local/bin/kastellan-worker-browser-driver"
if [ ! -e "$entry_link" ] && [ ! -L "$entry_link" ]; then
    echo "staging tree is missing /usr/local/bin/kastellan-worker-browser-driver" >&2
    exit 1
fi
if [ -L "$entry_link" ]; then
    entry_target="$(readlink "$entry_link")"
    case "$entry_target" in
        /*) entry_resolved="$WORK$entry_target" ;;
        *)  entry_resolved="$(dirname "$entry_link")/$entry_target" ;;
    esac
else
    entry_resolved="$entry_link"
fi
if [ ! -x "$entry_resolved" ]; then
    echo "worker entrypoint does not resolve to an executable inside the staging tree" >&2
    echo "  link:   /usr/local/bin/kastellan-worker-browser-driver" >&2
    echo "  target: ${entry_target:-<not a symlink>}" >&2
    exit 1
fi

# Pseudo-fs mountpoints (microvm-init mounts proc/sys/tmp at boot; the guest
# kernel auto-mounts devtmpfs on /dev, CONFIG_DEVTMPFS_MOUNT=y) + slice-3
# host-dir-share anchors + the slice-4a /run egress relay tmpfs mountpoint.
# Keep this anchor list in lockstep with mounts.rs::SHARE_ANCHORS
# (opt/data/srv/mnt/work/tmp) and the sibling build scripts.
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
mkdir -p "$WORK/run"

STAGE_MIB=$(du -sm "$WORK" | cut -f1)
echo "staging size: ${STAGE_MIB} MB (image will be ${ROOTFS_MIB}M)"
if [ "$STAGE_MIB" -ge "$ROOTFS_MIB" ]; then
    echo "staging (${STAGE_MIB} MB) does not fit in ROOTFS_MIB=${ROOTFS_MIB}; raise it to >= $(( STAGE_MIB * 12 / 10 ))" >&2
    exit 1
fi

# Journal-less ext4 (read-only at runtime, shared across concurrent VMs).
mkfs.ext4 -q -F -O ^has_journal -L browser-driver -d "$WORK" \
    "$OUT_DIR/browser-driver.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/browser-driver.ext4 (+ shared $OUT_DIR/vmlinux)"
