#!/usr/bin/env bash
# install-postgres.sh
#
# One-time setup for the hhagent Postgres dependency on Ubuntu 24.04+.
#
# We install PostgreSQL 18 and pgvector from the official upstream PGDG apt
# repo (apt.postgresql.org). Only the *binaries* end up on the system; the
# *data dir* is created later under ~/.local/share/hhagent/pg/data by
# `hhagent-db-init`, so this install does not start a system-wide cluster.
#
# Idempotent: re-running is safe. If the PGDG sources file is already in
# place we skip writing it; if the packages are already installed apt is a
# no-op.
#
# Why not the Ubuntu noble repo? Noble ships PostgreSQL 16 in main. We need
# 18 for features (and to align with current upstream). PGDG is the canonical
# upstream apt repo, signed with their key, used by the project itself.
#
# Why not Docker? hhagent's deployment model is single-host with native
# OS-level supervision (systemd --user / launchd). Adding Docker just for
# Postgres contradicts that and adds a runtime dependency the rest of the
# project deliberately avoids.
#
# What this script does NOT do:
#   - Stop or disable the system-wide `postgresql` service that apt creates
#     by default. We rely on the user-level supervisor to run our own
#     instance against our own data dir; the system instance can keep
#     running on its own port (5432) without colliding with ours (we
#     listen on a unix socket only, no TCP at all).
#   - Run initdb. That's `hhagent-db-init`'s job, run as your user.
#   - Install AGE or pg_search. Those PG-18 builds are not yet in PGDG
#     (AGE tops out at PG 16, ParadeDB primarily ships via Docker).
#     Tracked as separate GitHub issues; defer to Phase 1.
#
# Run once with sudo:
#   sudo scripts/linux/install-postgres.sh
#
# Verify after:
#   /usr/lib/postgresql/18/bin/postgres --version
#   /usr/lib/postgresql/18/bin/initdb --version
#   apt list --installed 2>/dev/null | grep -E 'postgresql-18|pgvector'

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "This script is Linux-only." >&2
    exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
    echo "This script must run as root (use sudo)." >&2
    exit 1
fi

if [[ ! -f /etc/os-release ]]; then
    echo "Cannot detect distribution: /etc/os-release missing." >&2
    exit 1
fi

# shellcheck disable=SC1091
. /etc/os-release

if [[ "${ID:-}" != "ubuntu" ]]; then
    echo "This script targets Ubuntu (got ID=${ID:-unset})." >&2
    echo "On Debian PGDG also publishes; adapt VERSION_CODENAME if needed." >&2
    exit 1
fi

CODENAME="${VERSION_CODENAME:-noble}"
SOURCES_LIST="/etc/apt/sources.list.d/pgdg.list"
KEYRING_PATH="/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc"

# Step 1 — make sure ca-certificates and curl exist (needed to fetch the
# PGDG signing key). On a stock Ubuntu image these are normally already
# installed but the script must be self-contained.
echo "==> Ensuring ca-certificates and curl are present..."
apt-get update -qq
apt-get install -y --no-install-recommends ca-certificates curl gnupg

# Step 2 — install postgresql-common, which provides the canonical
# /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc keyring file
# and the apt.postgresql.org signing key. PGDG documents this as the
# preferred way to add the repo since 2024.
if ! dpkg -s postgresql-common >/dev/null 2>&1; then
    echo "==> Installing postgresql-common (provides PGDG signing key)..."
    apt-get install -y --no-install-recommends postgresql-common
fi

# Run apt.postgresql.org.sh with -y if it exists (it ships with
# postgresql-common >= 245). This sets up both the keyring and the
# sources.list entry in one shot.
SETUP_SCRIPT="/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh"
if [[ -x "${SETUP_SCRIPT}" ]]; then
    echo "==> Running ${SETUP_SCRIPT} -y..."
    "${SETUP_SCRIPT}" -y
else
    # Fallback: fetch the key + write the sources file manually. This path
    # supports older postgresql-common builds where the helper script is
    # absent.
    echo "==> Setting up PGDG repo manually (no helper script found)..."
    install -d -m 0755 "$(dirname "${KEYRING_PATH}")"
    if [[ ! -f "${KEYRING_PATH}" ]]; then
        curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
            -o "${KEYRING_PATH}"
        chmod 0644 "${KEYRING_PATH}"
    fi
    if [[ ! -f "${SOURCES_LIST}" ]]; then
        cat >"${SOURCES_LIST}" <<EOF
deb [signed-by=${KEYRING_PATH}] https://apt.postgresql.org/pub/repos/apt ${CODENAME}-pgdg main
EOF
    fi
fi

# Step 3 — refresh apt index against PGDG and install PG 18 + pgvector.
# postgresql-client-18 gives us psql without pulling in the system
# postgresql metapackage (which would auto-start a cluster on 5432).
echo "==> Updating apt index (now includes PGDG)..."
apt-get update -qq

echo "==> Installing postgresql-18 + postgresql-client-18 + postgresql-18-pgvector..."
apt-get install -y --no-install-recommends \
    postgresql-18 \
    postgresql-client-18 \
    postgresql-18-pgvector

# Step 4 — defang the auto-created system cluster. Debian's postgresql-18
# package runs `pg_createcluster 18 main` post-install, which spins up a
# system-wide cluster on port 5432. We don't want that for hhagent — our
# user-instance data dir is the only one we manage. Stop and disable it
# (idempotent: noop if already stopped/disabled).
if command -v systemctl >/dev/null 2>&1; then
    if systemctl list-unit-files 'postgresql@18-main.service' 2>/dev/null \
            | grep -q 'postgresql@18-main.service'; then
        echo "==> Stopping and disabling system-wide postgresql@18-main..."
        systemctl stop 'postgresql@18-main.service' 2>/dev/null || true
        systemctl disable 'postgresql@18-main.service' 2>/dev/null || true
    fi
    if systemctl list-unit-files 'postgresql.service' 2>/dev/null \
            | grep -q 'postgresql.service'; then
        echo "==> Disabling system-wide postgresql.service..."
        systemctl stop 'postgresql.service' 2>/dev/null || true
        systemctl disable 'postgresql.service' 2>/dev/null || true
    fi
fi

# Step 5 — sanity print so the operator can confirm the binaries we'll
# point hhagent-db-init at.
PG_BIN="/usr/lib/postgresql/18/bin"
if [[ -x "${PG_BIN}/postgres" && -x "${PG_BIN}/initdb" ]]; then
    echo
    echo "Installed:"
    "${PG_BIN}/postgres" --version
    "${PG_BIN}/initdb" --version
    echo
    echo "Next: as your normal (non-root) user, run hhagent-db-init to"
    echo "create the user-instance data dir under"
    echo "~/.local/share/hhagent/pg/data and configure UDS-only listen,"
    echo "peer auth. The supervisor will pick it up from there."
else
    echo "Warning: ${PG_BIN}/postgres or initdb missing after install." >&2
    exit 1
fi
