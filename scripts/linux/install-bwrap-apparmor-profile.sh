#!/usr/bin/env bash
# install-bwrap-apparmor-profile.sh
#
# One-time setup for kastellan on Ubuntu 24.04+ (and other distros with
# kernel.apparmor_restrict_unprivileged_userns=1).
#
# Without this, bwrap can't create unprivileged user namespaces, because the
# kernel transitions the userns into the audit-deny-everything
# `unprivileged_userns` AppArmor profile. We give bwrap its own unconfined
# profile (the same approach Flatpak uses) so workers can actually be jailed.
#
# Run once with sudo:
#   sudo scripts/linux/install-bwrap-apparmor-profile.sh
#
# This installs /etc/apparmor.d/bwrap and reloads it. Reversible by removing
# that file and reloading AppArmor.

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "This script is Linux-only." >&2
    exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
    echo "This script must run as root (use sudo)." >&2
    exit 1
fi

if ! command -v bwrap >/dev/null 2>&1; then
    echo "bwrap not found. Install bubblewrap first:" >&2
    echo "  apt install bubblewrap" >&2
    exit 1
fi

BWRAP_BIN="$(command -v bwrap)"
PROFILE_PATH="/etc/apparmor.d/bwrap"

if [[ ! -d /etc/apparmor.d ]]; then
    echo "/etc/apparmor.d does not exist. Is AppArmor installed?" >&2
    echo "If your system doesn't use AppArmor, you don't need this script." >&2
    exit 0
fi

cat >"${PROFILE_PATH}" <<EOF
# Installed by kastellan (scripts/linux/install-bwrap-apparmor-profile.sh).
# Allows bwrap to create unprivileged user namespaces. Same shape as the
# stock /etc/apparmor.d/flatpak profile that ships with Ubuntu.

abi <abi/4.0>,
include <tunables/global>

profile bwrap ${BWRAP_BIN} flags=(unconfined) {
  userns,

  # Site-specific additions and overrides.
  include if exists <local/bwrap>
}
EOF

if ! command -v apparmor_parser >/dev/null 2>&1; then
    echo "apparmor_parser not found. Wrote ${PROFILE_PATH} but did not load." >&2
    exit 1
fi

apparmor_parser -r "${PROFILE_PATH}"

echo "Installed and loaded AppArmor profile at ${PROFILE_PATH}"
echo
echo "Verify with:"
echo "  bwrap --unshare-user --ro-bind /usr /usr --proc /proc --dev /dev --tmpfs /tmp /usr/bin/true"
echo "  echo \$?  # should be 0"
