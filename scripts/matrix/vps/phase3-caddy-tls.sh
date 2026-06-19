#!/usr/bin/env bash
# =============================================================================
# Phase 3 — Caddy TLS reverse proxy for the kastellan Matrix homeserver
# Host: matrix.kastellan.dev
#
# Run as root, AFTER Phase 2:   sudo bash phase3-caddy-tls.sh
#
# Installs Caddy (from the official Cloudsmith apt repo), which terminates TLS
# on :443 with an automatic Let's Encrypt certificate and reverse-proxies to the
# loopback-bound homeserver on 127.0.0.1:6167. Also serves the Matrix client
# .well-known so any client given just `matrix.kastellan.dev` finds the server.
#
# Federation is OFF, so NO federation .well-known and NO port 8448 — clients
# only. Idempotent — safe to re-run.
# =============================================================================
set -euo pipefail

SERVER_NAME="matrix.kastellan.dev"
BACKEND="127.0.0.1:6167"
CADDYFILE="/etc/caddy/Caddyfile"

log() { printf '\n=== %s ===\n' "$*"; }
if [ "$(id -u)" -ne 0 ]; then echo "Run as root (sudo bash $0)"; exit 1; fi

# -----------------------------------------------------------------------------
# 1. Install Caddy from the official repo (the Ubuntu-archive caddy can lag).
# -----------------------------------------------------------------------------
log "Install Caddy"
if command -v caddy >/dev/null 2>&1; then
  echo "caddy already installed: $(caddy version)"
else
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    debian-keyring debian-archive-keyring apt-transport-https curl gpg >/dev/null
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq caddy >/dev/null
  echo "installed: $(caddy version)"
fi

# -----------------------------------------------------------------------------
# 2. Caddyfile — automatic Let's Encrypt TLS + reverse proxy + client well-known.
# -----------------------------------------------------------------------------
log "Caddyfile -> ${CADDYFILE}"
[ -f "${CADDYFILE}" ] && cp -a "${CADDYFILE}" "${CADDYFILE}.bak.$(date +%s 2>/dev/null || echo prev)" || true
cat > "${CADDYFILE}" <<EOF
${SERVER_NAME} {
	# HSTS — once a browser has seen this, it refuses plaintext for a year.
	header Strict-Transport-Security "max-age=31536000; includeSubDomains"

	# Matrix client auto-discovery: a client given only "${SERVER_NAME}"
	# learns the homeserver base URL from here. (Federation is OFF, so there is
	# deliberately no /.well-known/matrix/server and no port 8448.)
	handle /.well-known/matrix/client {
		header Content-Type application/json
		header Access-Control-Allow-Origin *
		respond \`{"m.homeserver":{"base_url":"https://${SERVER_NAME}"}}\` 200
	}

	# Everything else proxies to the loopback-bound homeserver.
	handle {
		reverse_proxy ${BACKEND}
	}
}
EOF

log "Validate + format Caddyfile"
caddy fmt --overwrite "${CADDYFILE}"
caddy validate --config "${CADDYFILE}"

# -----------------------------------------------------------------------------
# 3. (Re)start Caddy and let ACME obtain the certificate.
# -----------------------------------------------------------------------------
log "Start Caddy"
systemctl enable caddy >/dev/null 2>&1 || true
systemctl restart caddy
echo "waiting for ACME certificate issuance..."
ok=0
for i in $(seq 1 24); do   # up to ~120s
  if curl -fsS --max-time 5 "https://${SERVER_NAME}/_matrix/client/versions" >/dev/null 2>&1; then
    ok=1; break
  fi
  sleep 5
done

# -----------------------------------------------------------------------------
# 4. Verify public HTTPS + valid cert + client API + well-known.
# -----------------------------------------------------------------------------
log "Public verification"
if [ "${ok}" -ne 1 ]; then
  echo "TLS endpoint not answering yet. Inspect:  journalctl -u caddy -n 60 --no-pager"
  exit 1
fi
echo "-- TLS handshake (issuer) --"
echo | openssl s_client -servername "${SERVER_NAME}" -connect "${SERVER_NAME}:443" 2>/dev/null \
  | openssl x509 -noout -issuer -subject -dates 2>/dev/null || true
echo
echo "-- /_matrix/client/versions over HTTPS --"
curl -fsS "https://${SERVER_NAME}/_matrix/client/versions" | head -c 200; echo
echo
echo "-- /.well-known/matrix/client --"
curl -fsS "https://${SERVER_NAME}/.well-known/matrix/client"; echo
echo
echo "Phase 3 done. The homeserver is publicly reachable over HTTPS."
echo "Next: Phase 4 (create operator + @kastellan accounts, then close registration)."
