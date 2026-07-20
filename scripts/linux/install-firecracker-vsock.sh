#!/usr/bin/env bash
# install-firecracker-vsock.sh
#
# One-time host setup for kastellan's Linux Firecracker micro-VM backend
# (SandboxBackendKind::FirecrackerVm). Same shape as
# install-bwrap-apparmor-profile.sh: a privileged prerequisite the sandbox
# backend's probe() checks for and refuses to run without.
#
# It (1) loads the vhost_vsock kernel module and persists it across reboots,
# and (2) grants the kastellan worker user read+write on /dev/vhost-vsock via
# an ACL that a udev rule re-applies on every boot (devtmpfs drops manual ACLs
# at boot). /dev/kvm is left alone unless --kvm is passed.
#
# Run once with sudo:
#   sudo scripts/linux/install-firecracker-vsock.sh [--user <name>] [--kvm]
#
# It also provisions the micro-VM image dir /var/lib/kastellan/microvm (owned
# by the worker user, mode 1755) so the unprivileged build-rootfs.sh + the
# per-user service can write it without further root, and installs the pinned
# guest kernel there as root:root 0644 — the sticky bit is what stops the
# worker user from unlinking and replacing it (issue #479). Re-run this script
# after bumping the pinned kernel version.
#
# Reversible: remove /etc/udev/rules.d/99-kastellan-microvm.rules and
# /etc/modules-load.d/kastellan-vsock.conf, then `udevadm control --reload`.
# The image dir /var/lib/kastellan/microvm is left in place (holds the built
# rootfs + kernel); `rm -rf` it manually to fully uninstall.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    exit 1
fi
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "This script is Linux-only (macOS uses the Apple container backend)." >&2
    exit 1
fi
if [[ "${EUID}" -ne 0 ]]; then
    echo "This script must run as root (use sudo)." >&2
    exit 1
fi

TARGET_USER=""
GRANT_KVM=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --user) TARGET_USER="${2:-}"; shift 2 ;;
        --kvm) GRANT_KVM=1; shift ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done
TARGET_USER="${TARGET_USER:-${SUDO_USER:-}}"
if [[ -z "${TARGET_USER}" ]]; then
    echo "Could not determine target user. Pass --user <name>." >&2
    exit 1
fi
if ! id "${TARGET_USER}" >/dev/null 2>&1; then
    echo "User '${TARGET_USER}' does not exist." >&2
    exit 1
fi

SETFACL_BIN="$(command -v setfacl || true)"
if [[ -z "${SETFACL_BIN}" ]]; then
    echo "setfacl not found. Install 'acl' (apt install acl) and re-run." >&2
    exit 1
fi

# 1. Kernel module: load now + persist.
if ! modprobe vhost_vsock; then
    echo "modprobe vhost_vsock failed — is the module available for this kernel?" >&2
    exit 1
fi
echo vhost_vsock > /etc/modules-load.d/kastellan-vsock.conf
echo "Loaded vhost_vsock; persisted in /etc/modules-load.d/kastellan-vsock.conf"

# 2. udev rule: re-apply the ACL on each boot.
RULES_PATH="/etc/udev/rules.d/99-kastellan-microvm.rules"
{
    echo "# Installed by kastellan (scripts/linux/install-firecracker-vsock.sh)."
    echo "# Grants the kastellan worker user rw on the micro-VM devices,"
    echo "# re-applied on every boot."
    echo "KERNEL==\"vhost-vsock\", RUN+=\"${SETFACL_BIN} -m u:${TARGET_USER}:rw /dev/vhost-vsock\""
    if [[ "${GRANT_KVM}" -eq 1 ]]; then
        echo "KERNEL==\"kvm\", RUN+=\"${SETFACL_BIN} -m u:${TARGET_USER}:rw /dev/kvm\""
    fi
} > "${RULES_PATH}"
echo "Wrote ${RULES_PATH}"

# 3. Apply immediately so no reboot is needed (ACL is checked at open time, so
#    the running per-user kastellan service picks it up without a restart).
udevadm control --reload
"${SETFACL_BIN}" -m "u:${TARGET_USER}:rw" /dev/vhost-vsock
[[ "${GRANT_KVM}" -eq 1 ]] && "${SETFACL_BIN}" -m "u:${TARGET_USER}:rw" /dev/kvm

# 4. Provision the micro-VM image dir + install the pinned guest kernel.
#
#    The dir stays owned by the worker user so the eight unprivileged
#    build-*-rootfs.sh scripts keep working without root. Issue #479 adds two
#    things on top of that:
#
#      * mode 1755 — the STICKY bit, and it is load-bearing rather than
#        belt-and-braces. POSIX directory write permission on its own permits
#        unlink() and rename() of ANY entry, regardless of that entry's owner
#        and mode. So root-owning vmlinux inside a worker-writable dir would
#        stop nothing at all: the agent could remove it and drop in its own.
#        With +t, removal and rename are restricted to the entry's owner. The
#        agent still freely creates and replaces its own *.ext4 images (it
#        owns those); it cannot touch root's kernel.
#
#      * the guest kernel is fetched HERE, as root, and left root:root 0644.
#        docs/threat-model.md assumes a worst-case compromise reaches the
#        agent's own OS user — and the micro-VM is the containment boundary
#        while the guest kernel is what enforces it. It is the one artefact in
#        that directory an attacker at that level must not be able to rewrite.
#        (kastellan also re-verifies it at every VM boot; see
#        sandbox/src/guest_kernel_pin.rs. That check is TOCTOU-limited, which
#        is why this ownership step is the half that actually closes it.)
#
#    The rootfs images are deliberately NOT protected this way: a tampered
#    rootfs is not an escalation — the guest userland is already assumed
#    hostile and the VM is what contains it.
MICROVM_DIR="/var/lib/kastellan/microvm"
mkdir -p "${MICROVM_DIR}"
chown "${TARGET_USER}:$(id -gn "${TARGET_USER}")" "${MICROVM_DIR}"
chmod 1755 "${MICROVM_DIR}"
echo "Provisioned ${MICROVM_DIR} (owner ${TARGET_USER}, sticky)"

# The same pinned+verified fetch the rootfs builds use — one place the URL,
# the arch table and the sums are written down (issue #471).
# shellcheck source=../workers/microvm/lib/guest-kernel.sh
source "$(dirname "${BASH_SOURCE[0]}")/../workers/microvm/lib/guest-kernel.sh"
fetch_guest_kernel "${MICROVM_DIR}"
chown root:root "${MICROVM_DIR}/vmlinux"
chmod 0644 "${MICROVM_DIR}/vmlinux"
echo "Installed pinned guest kernel root-owned at ${MICROVM_DIR}/vmlinux"

echo
echo "Done. Verify as ${TARGET_USER}:"
echo "  [ -r /dev/vhost-vsock ] && [ -w /dev/vhost-vsock ] && echo 'vsock OK'"
