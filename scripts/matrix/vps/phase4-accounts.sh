#!/usr/bin/env bash
# =============================================================================
# Phase 4 — Create accounts, then CLOSE registration  (v3 — bootstrap token)
# Host: matrix.kastellan.dev
#
# Run as root, AFTER Phase 2:   sudo bash phase4-accounts.sh
#
# Continuwuity gate for a brand-new server: the FIRST account (which becomes the
# admin) must be created with a one-time, SERVER-GENERATED bootstrap token that
# Continuwuity prints to its log at startup. The `registration_token` from the
# config only starts working AFTER that first admin account exists. So:
#
#   1. OPERATOR (you)  -> registered with the BOOTSTRAP token  => admin.
#   2. @kastellan BOT  -> registered with the CONFIG token (now active).
#
# The bootstrap token is read live from the journal (so a restart that rotates
# it is handled). Passwords are read interactively. Re-running is safe.
# =============================================================================
set -uo pipefail

HS="http://127.0.0.1:6167"
API="${HS}/_matrix/client/v3/register"
CONF="/etc/kastellan/conduwuit.toml"

log() { printf '\n=== %s ===\n' "$*"; }
if [ "$(id -u)" -ne 0 ]; then echo "Run as root (sudo bash $0)"; exit 1; fi
command -v jq >/dev/null 2>&1 || { apt-get install -y -qq jq >/dev/null; }

# The current bootstrap token (latest mention in the log).
BOOT="$(journalctl -u kastellan-matrix --no-pager 2>/dev/null \
  | grep -oE 'using the registration token [A-Za-z0-9]+' | tail -1 | awk '{print $NF}')"
# The config token (activates once the first admin exists).
CFG="$(grep -m1 '^registration_token' "$CONF" | sed 's/^[^=]*= *//; s/^"//; s/"$//')"

# register <username> <password> <token>
register() {
  local u="$1" p="$2" t="$3" out code session
  out="$(mktemp)"; chmod 600 "${out}"
  curl -sS -o "${out}" -X POST "${API}" -H 'Content-Type: application/json' \
    -d "$(jq -nc --arg u "$u" --arg p "$p" '{username:$u,password:$p,inhibit_login:true}')" >/dev/null || true
  if jq -e '.user_id' "${out}" >/dev/null 2>&1; then echo "  @${u}: created (no UIA)"; rm -f "${out}"; return 0; fi
  if grep -q M_USER_IN_USE "${out}"; then echo "  @${u}: already exists — skipping"; rm -f "${out}"; return 0; fi
  session="$(jq -r '.session // empty' "${out}")"
  if [ -z "${session}" ]; then echo "  @${u}: no UIA session:"; cat "${out}"; echo; rm -f "${out}"; return 1; fi
  code="$(curl -sS -o "${out}" -w '%{http_code}' -X POST "${API}" -H 'Content-Type: application/json' \
    -d "$(jq -nc --arg u "$u" --arg p "$p" --arg t "$t" --arg s "$session" \
          '{username:$u,password:$p,inhibit_login:true,auth:{type:"m.login.registration_token",token:$t,session:$s}}')")"
  if [ "${code}" = "200" ]; then echo "  @${u}: created -> $(jq -r '.user_id // empty' "${out}")"; rm -f "${out}"; return 0; fi
  if grep -q M_USER_IN_USE "${out}"; then echo "  @${u}: already exists — skipping"; rm -f "${out}"; return 0; fi
  echo "  @${u}: registration failed (HTTP ${code}) — server said:"; cat "${out}"; echo
  rm -f "${out}"; return 1
}

log "Bootstrap token discovery"
if [ -z "${BOOT}" ]; then
  echo "Could not find a bootstrap token in the log. If the admin already exists,"
  echo "the operator step will say M_USER_IN_USE and that is fine. Otherwise restart"
  echo "the service (systemctl restart kastellan-matrix) and re-run to get a fresh one."
else
  echo "bootstrap token found in journal (len ${#BOOT})"
fi
[ -n "${CFG}" ] && echo "config token present (len ${#CFG})" || { echo "no config token found"; exit 1; }

log "Account details"
read -rp "Operator (admin) username [horst]: " OP; OP="${OP:-horst}"
read -rsp "Password for @${OP} (operator/admin): " OPPW; echo
read -rsp "  confirm: " OPPW2; echo
[ "${OPPW}" = "${OPPW2}" ] || { echo "operator passwords do not match"; exit 1; }
[ -n "${OPPW}" ] || { echo "empty password rejected"; exit 1; }
read -rsp "Password for @kastellan (agent bot): " BOTPW; echo
read -rsp "  confirm: " BOTPW2; echo
[ "${BOTPW}" = "${BOTPW2}" ] || { echo "bot passwords do not match"; exit 1; }
[ -n "${BOTPW}" ] || { echo "empty password rejected"; exit 1; }

# Register operator FIRST with the BOOTSTRAP token (=> admin).
log "Registering operator @${OP} with the bootstrap token (=> admin)"
if ! register "${OP}" "${OPPW}" "${BOOT}"; then
  echo "Operator registration failed; aborting before the bot + close steps so we can inspect."
  exit 1
fi

# Now the config token is active — register the bot with it.
log "Registering @kastellan with the config token"
register "kastellan" "${BOTPW}" "${CFG}"

# Close registration + restart + re-validate.
log "Closing registration"
sed -i 's/^allow_registration = true/allow_registration = false/' "${CONF}"
[ -f "$(dirname "$0")/check-conduwuit-config.sh" ] && bash "$(dirname "$0")/check-conduwuit-config.sh" "${CONF}" || true
systemctl restart kastellan-matrix
sleep 3

log "Verify registration is closed"
code="$(curl -sS -o /tmp/regclosed.out -w '%{http_code}' -X POST "${API}" \
  -H 'Content-Type: application/json' -d '{"username":"shouldfail","password":"x"}')"
echo "register attempt now returns HTTP ${code}:"; head -c 200 /tmp/regclosed.out; echo; rm -f /tmp/regclosed.out
echo "allow_registration is now: $(grep '^allow_registration' "${CONF}")"
echo
echo "Phase 4 done."
echo "  * Operator @${OP}:matrix.kastellan.dev is the admin (Element: https://matrix.kastellan.dev)"
echo "  * @kastellan:matrix.kastellan.dev is the agent account — store its password as a kastellan secret"
