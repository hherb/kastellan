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
# Reversible: remove /etc/udev/rules.d/99-kastellan-microvm.rules and
# /etc/modules-load.d/kastellan-vsock.conf, then `udevadm control --reload`.
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

echo
echo "Done. Verify as ${TARGET_USER}:"
echo "  [ -r /dev/vhost-vsock ] && [ -w /dev/vhost-vsock ] && echo 'vsock OK'"
