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
# It also provisions the micro-VM image dir /var/lib/kastellan/microvm as
# root:<worker-group> mode 1775, so the unprivileged build-rootfs.sh + the
# per-user service can still write their images there, and installs the pinned
# guest kernel as root:root 0644. Root owning BOTH the directory and the kernel
# is what makes the sticky bit bite: unlink(2) exempts the directory's owner as
# well as the file's, so a worker-owned directory would leave the kernel
# replaceable (issue #479). Re-run this script after bumping the pinned kernel
# version; the ownership fixes are unconditional, so a re-run also repairs an
# older install.
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
#    The worker must still be able to write its eight *.ext4 images here
#    unprivileged, but must NOT be able to replace the guest kernel. Issue
#    #479 gets both with root:<worker-group> ownership, mode 1775:
#
#      * ROOT owns the directory, the worker's GROUP has write. Group write
#        is what keeps build-*-rootfs.sh unprivileged (creating a new entry
#        needs write+execute on the directory, not ownership of it).
#
#      * mode 1775 — the STICKY bit, and it is load-bearing rather than
#        belt-and-braces. POSIX directory write permission on its own permits
#        unlink() and rename() of ANY entry, regardless of that entry's owner
#        and mode. So root-owning vmlinux in a group-writable dir without +t
#        would stop nothing: the agent could remove it and drop in its own.
#
#        The DIRECTORY's owner matters as much as the file's. unlink(2):
#        removal is refused when the sticky bit is set and the process's UID
#        "is neither the UID of the file to be deleted nor that of the
#        directory containing it". So a worker-OWNED directory would exempt
#        the worker no matter who owns vmlinux — which is exactly why the
#        chown below names root and not ${TARGET_USER}. With root owning both
#        the directory and the kernel, the worker matches neither exemption
#        and cannot unlink or rename it; it can still freely create and
#        replace its own *.ext4 images, because it owns those.
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
#
#    The chown/chmod are unconditional so a re-run repairs an older install
#    whose dir is still worker-owned.
MICROVM_DIR="/var/lib/kastellan/microvm"
MICROVM_PARENT="$(dirname "${MICROVM_DIR}")"

# The PARENT matters as much as the dir itself. Unlink/rename permission on
# `microvm` is governed by ITS parent, so an agent-owned /var/lib/kastellan
# would let the agent `mv microvm microvm.old; mkdir microvm` and walk away
# with a directory it owns entirely. `mkdir -p` is a no-op on an existing
# dir and would silently inherit whatever ownership it already had — and
# other kastellan state (matrix/, kv/) lives under here and may well have
# been created by hand as the agent user. So assert it, unconditionally.
mkdir -p "${MICROVM_PARENT}"
chown root:root "${MICROVM_PARENT}"
chmod 0755 "${MICROVM_PARENT}"

mkdir -p "${MICROVM_DIR}"
chown "root:$(id -gn "${TARGET_USER}")" "${MICROVM_DIR}"
chmod 1775 "${MICROVM_DIR}"
echo "Provisioned ${MICROVM_DIR} (root-owned, group $(id -gn "${TARGET_USER}") writable, sticky)"

# A pre-existing `vmlinux` may be a SYMLINK the agent planted before this
# dir was locked down. `[ -f ]` and `chown`/`chmod` all follow symlinks, so
# without this the fetch below would verify through the link, the chown
# would retarget its destination, and the LINK would stay agent-owned —
# leaving the agent able to re-point it whenever it likes, while this
# script cheerfully reported success. Remove the link, never follow it.
if [ -L "${MICROVM_DIR}/vmlinux" ]; then
    echo "Removing a pre-existing symlink at ${MICROVM_DIR}/vmlinux (must be a regular file)."
    rm -f "${MICROVM_DIR}/vmlinux"
fi

# The same pinned+verified fetch the rootfs builds used to do — one place
# the URL, the arch table and the sums are written down (issue #471). Since
# #479 the builds only *verify* (require_guest_kernel); creating the kernel
# is root's job, here, so it can never end up agent-owned.
# shellcheck source=../workers/microvm/lib/guest-kernel.sh
source "$(dirname "${BASH_SOURCE[0]}")/../workers/microvm/lib/guest-kernel.sh"
fetch_guest_kernel "${MICROVM_DIR}"
chown root:root "${MICROVM_DIR}/vmlinux"
chmod 0644 "${MICROVM_DIR}/vmlinux"

# Verify rather than assume before claiming success: this script's whole
# contribution to #479 is that the kernel ends up beyond the agent's reach,
# and a message asserting that without checking is the failure mode #471's
# quarantine bug already taught us about.
KERNEL_UID="$(stat -c '%u' "${MICROVM_DIR}/vmlinux")"
if [ "${KERNEL_UID}" != "0" ]; then
    echo "Guest kernel is not root-owned after install (uid ${KERNEL_UID})." >&2
    echo "Refusing to report success; the agent could replace it." >&2
    exit 1
fi
echo "Installed pinned guest kernel root-owned at ${MICROVM_DIR}/vmlinux"

echo
echo "Done. Verify as ${TARGET_USER}:"
echo "  [ -r /dev/vhost-vsock ] && [ -w /dev/vhost-vsock ] && echo 'vsock OK'"
