#!/usr/bin/env bash
# fetch-guest-kernel.sh — download + verify the pinned micro-VM guest kernel
# into a NON-DEFAULT image dir.
#
#   ./scripts/workers/microvm/fetch-guest-kernel.sh <image-dir>
#
# You do NOT need this for the normal deployment. The default image dir
# /var/lib/kastellan/microvm is provisioned by
# `sudo scripts/linux/install-firecracker-vsock.sh`, which installs the
# kernel root-owned so the agent's own OS user cannot replace it (#479).
#
# This script exists for the alternative layout the build scripts document,
# e.g. KASTELLAN_MICROVM_DIR="$HOME/.local/share/kastellan/microvm". Root
# does not manage that directory, so **it carries no ownership protection
# at all**: whoever owns it can swap the kernel. What still protects you
# there is the boot-time sha256 check in sandbox/src/guest_kernel_pin.rs,
# which is TOCTOU-limited by construction. Prefer the default dir for
# anything you care about.
#
# It is a separate, deliberate command rather than something the build
# scripts do for you because a build that can create the kernel can create
# an agent-owned one in the protected dir — which would silently undo #479.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh." >&2
    exit 1
fi
set -euo pipefail

OUT_DIR="${1:-}"
if [ -z "$OUT_DIR" ]; then
    echo "usage: $0 <image-dir>" >&2
    echo "  e.g. $0 \"\$HOME/.local/share/kastellan/microvm\"" >&2
    exit 1
fi

# shellcheck source=lib/guest-kernel.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/guest-kernel.sh"

if [ "$OUT_DIR" = "/var/lib/kastellan/microvm" ]; then
    echo "Refusing to fetch into the default image dir: $OUT_DIR" >&2
    echo "That dir is root-managed so the kernel cannot be agent-owned (#479)." >&2
    echo "Use: sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"
fetch_guest_kernel "$OUT_DIR"
echo "Verified guest kernel at $OUT_DIR/vmlinux"
echo "NOTE: this dir is not root-managed, so the kernel here has no ownership"
echo "protection — only the boot-time hash check stands behind it."
