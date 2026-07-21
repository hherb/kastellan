#!/usr/bin/env bash
# =============================================================================
# Phase 2 — Continuwuity Matrix homeserver install (federation-off, loopback)
# Host: matrix.kastellan.dev
#
# Run as root, AFTER Phase 1:   sudo bash phase2-homeserver.sh
#
# Installs the Continuwuity binary (the maintained conduwuit continuation — the
# original conduwuit is archived), a dedicated unprivileged `matrix` user, a
# hardened federation-off config bound to loopback, and the hardened systemd
# unit. Idempotent — safe to re-run (it will NOT regenerate the registration
# token if one already exists).
#
# The server is reachable ONLY on 127.0.0.1:6167 after this phase. Public TLS
# (Caddy) is Phase 3; account creation is Phase 4.
# =============================================================================
set -euo pipefail

SERVER_NAME="matrix.kastellan.dev"
PORT=6167
DATA_DIR="/var/lib/conduwuit"
CONF_DIR="/etc/kastellan"
CONF="${CONF_DIR}/conduwuit.toml"
BIN="/usr/local/bin/conduwuit"
UNIT="/etc/systemd/system/kastellan-matrix.service"
TOKEN_FILE="/root/kastellan-registration-token.txt"
VERSION="v0.5.9"
# Haswell-optimised build — this CPU has AVX2 (verified). 64-bit dynamic binary;
# needs libjemalloc2 (already installed) + kernel io_uring (kernel 7.0, fine).
BIN_URL="https://forgejo.ellis.link/continuwuation/continuwuity/releases/download/${VERSION}/conduwuit-haswell-linux-amd64-maxperf"

# sha256 of that exact binary variant (#386). This is the one download in the
# VPS bring-up that installs a third-party binary root:root 0755 and runs it
# under systemd, so it is verified before install.
#
# HONEST LIMITATION — this is trust-on-first-use: Continuwuity publishes no
# checksum or signature alongside its release binaries (only the bare files),
# so we cannot chain to an upstream attestation. What raises it above a single
# blind fetch: the sum was corroborated three ways when recorded (2026-07-21)
# — the binary already running on the live matrix.kastellan.dev box (installed
# 2026-06-19), plus a fresh fetch from two hosts on separate network paths
# (the DGX and the dev Mac), all three identical. The one-month-old live copy
# is a temporal witness: a substitution would have had to be in place since
# before this deployment existed. If it changes for a variant/version, re-pin
# deliberately (re-fetch + compare); never paste in whatever a mismatch prints.
BIN_SHA256="7489e33c541f9e7fad8d10a93209ed7c5cada84c9292b53b02971ff30be79460"

log() { printf '\n=== %s ===\n' "$*"; }

# verify_sha256 <path> <expected-hex> — exact-match or non-zero.
#
# Deliberately inline rather than sourced. Every other provisioning script
# shares scripts/workers/microvm/lib/verify.sh, but the VPS deployment copies
# only these phase scripts into ~/ (see scripts/matrix/vps/README.md) — there
# is no repo on the box to source from — so this carries its own copy of the
# same three lines. The VPS is always Linux, so `sha256sum` is assumed present.
verify_sha256() {
  local file="$1" expected="$2" actual
  actual="$(sha256sum "$file" | cut -d' ' -f1)" || return 1
  if [ "$actual" != "$expected" ]; then
    echo "sha256 mismatch for $file" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    return 1
  fi
}
if [ "$(id -u)" -ne 0 ]; then echo "Run as root (sudo bash $0)"; exit 1; fi

# -----------------------------------------------------------------------------
# 1. Runtime deps (jemalloc already present; liburing2 is cheap insurance).
# -----------------------------------------------------------------------------
log "Dependencies"
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libjemalloc2 liburing2 curl openssl >/dev/null || \
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libjemalloc2 curl openssl >/dev/null
echo "ok"

# -----------------------------------------------------------------------------
# 2. Dedicated unprivileged `matrix` user + data dir (RocksDB store).
# -----------------------------------------------------------------------------
log "matrix user + data dir"
if id matrix >/dev/null 2>&1; then
  echo "user 'matrix' already exists"
else
  useradd --system --home "${DATA_DIR}" --shell /usr/sbin/nologin matrix
  echo "created system user 'matrix'"
fi
mkdir -p "${DATA_DIR}"
chown matrix:matrix "${DATA_DIR}"
chmod 700 "${DATA_DIR}"

# -----------------------------------------------------------------------------
# 3. Download the Continuwuity binary (version-pinned, over HTTPS).
# -----------------------------------------------------------------------------
log "Continuwuity ${VERSION} binary"
tmpbin="$(mktemp)"
curl -fSL --proto '=https' --tlsv1.2 -o "${tmpbin}" "${BIN_URL}"
# Verify BEFORE install. The previous version of this script only *printed*
# the sha after installing — which verifies nothing: by then an attacker-
# substituted binary is already root:root 0755 and one systemctl start from
# running. Compare against the pin and refuse on mismatch.
if ! verify_sha256 "${tmpbin}" "${BIN_SHA256}"; then
  rm -f "${tmpbin}"
  echo "Downloaded Continuwuity binary does not match the pinned sha256 — refusing to install." >&2
  echo "  source: ${BIN_URL}" >&2
  exit 1
fi
install -m 0755 -o root -g root "${tmpbin}" "${BIN}"
rm -f "${tmpbin}"
echo "version check (also smoke-tests jemalloc/io_uring load):"
"${BIN}" --version

# -----------------------------------------------------------------------------
# 4. Registration token — generated ON THE BOX, never leaves it. Stored 0600.
#    Re-runs keep the existing token (so accounts created with it stay valid).
# -----------------------------------------------------------------------------
log "Registration token"
if [ -s "${TOKEN_FILE}" ]; then
  echo "reusing existing token at ${TOKEN_FILE}"
else
  umask 077
  openssl rand -hex 24 > "${TOKEN_FILE}"
  chmod 600 "${TOKEN_FILE}"
  echo "generated new token -> ${TOKEN_FILE}"
fi
REG_TOKEN="$(cat "${TOKEN_FILE}")"

# -----------------------------------------------------------------------------
# 5. Render the hardened, federation-OFF config (Continuwuity-correct keys).
#    Bound to loopback; only the Caddy proxy (Phase 3) will reach it.
#    Config is root:matrix 0640 so the token is not world-readable.
# -----------------------------------------------------------------------------
log "Config -> ${CONF}"
install -d -m 0755 "${CONF_DIR}"
cat > "${CONF}" <<EOF
# kastellan single-user Matrix homeserver — Continuwuity, federation OFF.
# Security invariants (do not weaken): federation off, loopback bind, no open
# registration. After creating the operator + @kastellan accounts (Phase 4),
# set allow_registration = false and restart.
[global]
server_name = "${SERVER_NAME}"
database_path = "${DATA_DIR}"

# Loopback only — the TLS reverse proxy (Caddy) is the sole network face.
address = "127.0.0.1"
port = ${PORT}

# Token-gated registration for one-time account creation. DISABLE in Phase 4.
allow_registration = true
registration_token = "${REG_TOKEN}"

# Federation OFF — removes the entire federation attack surface.
allow_federation = false

# Appliance hygiene: no announcement phone-home, no trans-flag display suffix,
# no trusted key servers (federation is off anyway), conservative request cap.
allow_announcements_check = false
new_user_displayname_suffix = ""
trusted_servers = []
max_request_size = 20_000_000
EOF
chown root:matrix "${CONF}"
chmod 0640 "${CONF}"

# Validate against kastellan's security checker (uploaded alongside this script).
if [ -f "$(dirname "$0")/check-conduwuit-config.sh" ]; then
  log "Validating config (kastellan security invariants)"
  bash "$(dirname "$0")/check-conduwuit-config.sh" "${CONF}"
else
  echo "WARN: check-conduwuit-config.sh not found next to this script; skipping validation"
fi

# -----------------------------------------------------------------------------
# 6. Hardened systemd unit (dedicated user, no-new-privs, RO system, syscall
#    filter; writable ONLY to the data dir). Contains a homeserver RCE to the
#    matrix user + its store.
# -----------------------------------------------------------------------------
log "systemd unit -> ${UNIT}"
cat > "${UNIT}" <<EOF
[Unit]
Description=kastellan Matrix homeserver (Continuwuity, federation-off)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=matrix
Group=matrix
ExecStart=${BIN} --config ${CONF}
Environment=CONDUWUIT_CONFIG=${CONF}
Restart=on-failure
RestartSec=5

# --- Hardening ---
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
ReadWritePaths=${DATA_DIR}

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable kastellan-matrix >/dev/null 2>&1 || true
systemctl restart kastellan-matrix
sleep 4

# -----------------------------------------------------------------------------
# 7. Verify it came up and answers the client API on loopback.
# -----------------------------------------------------------------------------
log "Status"
systemctl --no-pager --full status kastellan-matrix | sed -n '1,12p' || true
echo
log "Local client API probe"
if curl -fsS "http://127.0.0.1:${PORT}/_matrix/client/versions" | head -c 400; then
  echo; echo; echo "OK — homeserver is up on 127.0.0.1:${PORT}"
else
  echo "PROBE FAILED — inspect:  journalctl -u kastellan-matrix -n 60 --no-pager"
  exit 1
fi
echo
echo "Phase 2 done. Registration token is in ${TOKEN_FILE} (root-only)."
echo "Next: Phase 3 (Caddy TLS reverse proxy for ${SERVER_NAME})."
