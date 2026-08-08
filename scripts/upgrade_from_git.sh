#!/usr/bin/env bash
# upgrade_from_git.sh — take the local checkout to the latest `main` and redeploy
# a running, supervised kastellan: switch to main, pull, build the release
# binaries (incl. the live-matrix worker), install, restart, and verify.
#
# Keyring-only by default — NO password needed. The Matrix login session persists
# across normal upgrades (the daemon restores it from the on-disk store), so the
# channel just comes back up after the restart.
#
# A matrix-sdk MAJOR upgrade (e.g. 0.8 → 0.18) invalidates the on-disk SQLite
# crypto store, which the daemon cannot restore — the worker then fails to start
# the channel ("worker spawn/login failed"). For that case re-run with
# --relogin: it wipes the store and performs a fresh login using the bot password
# already in the keyring/Vault (secret `matrix_kastellan_password`). A fresh login
# rotates the device id, so re-verify the bot once in your client afterwards.
#
# Only if that keyring secret is itself stale or lost (non-recoverable) do you
# need a password: pass -pwd <password> and the script resets the Vault secret
# (exact bytes, via `secret put --raw`) before logging in. -pwd implies --relogin.
#
# Usage:
#   scripts/upgrade_from_git.sh                      # normal upgrade (no password)
#   scripts/upgrade_from_git.sh --relogin            # + wipe store & re-login from keyring
#   scripts/upgrade_from_git.sh --relogin -pwd <pw>  # + reset the keyring secret first
set -euo pipefail

# ---- args -------------------------------------------------------------------
RELOGIN=0
PASSWORD=""
SECRET_NAME="matrix_kastellan_password"
while [ $# -gt 0 ]; do
  case "$1" in
    --relogin) RELOGIN=1; shift ;;
    -pwd|--password) PASSWORD="${2:?-pwd needs a value}"; RELOGIN=1; shift 2 ;;
    --secret) SECRET_NAME="${2:?--secret needs a value}"; shift 2 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "upgrade_from_git.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

# ---- locations --------------------------------------------------------------
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
CLI="$HOME/.local/bin/kastellan-cli"
ENV_FILE="$HOME/.config/kastellan/kastellan.env"
STORE_DIR="$HOME/.local/state/kastellan/matrix/store"
CORE_LOG="$HOME/.local/state/kastellan/kastellan-core.out"

# Preserve the Matrix channel config across the reinstall by reading it back from
# the installed env files (`install` REGENERATES $ENV_FILE from CLI flags,
# dropping the Matrix block unless --matrix-* are re-passed). Read the overlay
# FIRST, then fall back to the generated file, matching the runtime precedence
# (later file wins) — an operator who moved the Matrix block into
# ${ENV_FILE}.local per this script's own advice below must still be found here.
HS=""; MX_USER=""
for f in "$ENV_FILE.local" "$ENV_FILE"; do
  [ -f "$f" ] || continue
  [ -n "$HS" ]      || HS="$(sed -n 's/^KASTELLAN_MATRIX_HOMESERVER_URL=//p' "$f" | head -1)"
  [ -n "$MX_USER" ] || MX_USER="$(sed -n 's/^KASTELLAN_MATRIX_USER=//p' "$f" | head -1)"
done

# shellcheck disable=SC1090,SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

# ---- 1. sync main -----------------------------------------------------------
echo "==> git: switch to main + fast-forward pull"
git switch main
git pull --ff-only

# ---- 2. build ---------------------------------------------------------------
echo "==> build release binaries (incl. live-matrix worker)"
bash scripts/build-release.sh

# ---- 3. install (preserving the Matrix env) ---------------------------------
# Byte offset of the core log BEFORE the install restarts the daemon, so step 5
# reads only THIS start's lines. The unit is `StandardOutput=append:` (see
# supervisor/src/systemd_user/builder.rs), so the file only ever grows and an
# offset stays valid across the restart — which is what lets the verify step
# stop mistaking a previous boot's "channel bus running" for this one's.
CORE_LOG_OFFSET=0
if [ -f "$CORE_LOG" ]; then
  CORE_LOG_OFFSET="$(wc -c < "$CORE_LOG" | tr -d ' ')"
fi

# NOTE: `install` REGENERATES $ENV_FILE from CLI flags. Operator settings belong
# in ${ENV_FILE}.local, which the installer never writes and whose values win
# (systemd applies EnvironmentFile= directives in order, later winning). If the
# install reports dropped or changed keys, they were still in the generated file
# — move them into the .local and re-run. See issue #458.
echo "==> install"
if [ -n "$HS" ] && [ -n "$MX_USER" ]; then
  ./target/release/kastellan-cli install --matrix-homeserver-url "$HS" --matrix-user "$MX_USER"
else
  echo "    (no Matrix channel configured in $ENV_FILE or $ENV_FILE.local — installing without it)"
  ./target/release/kastellan-cli install
fi

# ---- 4. optional re-login (matrix-sdk major bump / stale secret) -------------
if [ "$RELOGIN" -eq 1 ]; then
  if [ -z "$HS" ] || [ -z "$MX_USER" ]; then
    echo "ERROR: --relogin requires a Matrix channel configured in $ENV_FILE or $ENV_FILE.local" >&2
    exit 1
  fi
  echo "==> re-login: stop core → wipe store → fresh login"
  systemctl --user stop kastellan-core.service
  rm -rf "$STORE_DIR"
  if [ -n "$PASSWORD" ]; then
    echo "    resetting keyring secret '$SECRET_NAME' (exact bytes, no newline)"
    printf '%s' "$PASSWORD" | "$CLI" secret put "$SECRET_NAME" --raw
  fi
  echo "    matrix probe (initial login from keyring secret '$SECRET_NAME')"
  "$CLI" matrix probe --homeserver "$HS" --user "$MX_USER" --secret "$SECRET_NAME"
  echo "==> start core"
  # Re-capture: this start, not the install's, is the one to verify. Without
  # this the window would still contain the pre-relogin start's lines — and on
  # the --relogin path those describe precisely the state we just wiped.
  if [ -f "$CORE_LOG" ]; then
    CORE_LOG_OFFSET="$(wc -c < "$CORE_LOG" | tr -d ' ')"
  fi
  systemctl --user start kastellan-core.service
fi

# ---- 5. verify --------------------------------------------------------------
echo "==> verify (waiting for services + channel)"
sleep 6
echo -n "    services: "
systemctl --user is-active kastellan.target kastellan-core kastellan-postgres | paste -sd' '

if [ -n "$HS" ] && [ -n "$MX_USER" ] && [ -f "$CORE_LOG" ]; then
  # Most recent matrix channel lifecycle line from THIS daemon start: read only
  # past CORE_LOG_OFFSET (captured before the install restarted the core). A
  # shrunken file means someone rotated/truncated it under us — read the whole
  # thing then, and accept the old stale-line caveat rather than reading nothing.
  #
  # Since #514 the supervisor's messages no longer name the channel (it is a
  # structured `channel` field), so match the field and the message on the same
  # JSON line rather than a "matrix …" message prefix that no longer exists.
  # `|| true`: with `set -euo pipefail` a no-match grep exits 1 and would abort
  # the script here, making the "(not yet in the log)" fallback below unreachable.
  matrix_status_line() {
    local since="$CORE_LOG_OFFSET"
    if [ "$(wc -c < "$CORE_LOG" | tr -d ' ')" -lt "$since" ]; then
      since=0
    fi
    tail -c "+$((since + 1))" "$CORE_LOG" 2>/dev/null \
      | grep -a '"channel":"matrix"' \
      | grep -aoE '"message":"(channel bus running|channel bring-up failed; retrying|CHANNEL DISABLED[^"]*|CHANNEL STILL DOWN[^"]*)"' \
      | tail -1 || true
  }

  # Bring-up now RETRIES with capped backoff (1s → ×2 → 60s), so the answer is
  # eventually-consistent rather than final at the first look: a "retrying" line
  # is a snapshot, not a verdict. Poll until the channel is up or a fatal line
  # lands. CHANNEL_WAIT covers ~1+2+4+8+16s of backoff with room to spare; a
  # login failure still failing after that really is broken, not transient.
  CHANNEL_WAIT="${CHANNEL_WAIT:-45}"
  last=""
  for _ in $(seq 1 "$CHANNEL_WAIT"); do
    last="$(matrix_status_line)"
    case "$last" in
      *"channel bus running"*|*"CHANNEL DISABLED"*) break ;;
    esac
    sleep 1
  done

  case "$last" in
    *"channel bus running"*)
      echo "    ✅ Matrix channel is up." ;;
    *"CHANNEL DISABLED"*)
      # Fatal classification: the supervisor will NOT retry (a statically-dead
      # homeserver under force-routing — #459). No amount of waiting helps.
      echo "    ❌ Matrix channel is DISABLED and will not be retried." >&2
      echo "       Fix what the log's \`error\` field names, then restart the daemon:" >&2
      echo "       grep -a 'CHANNEL DISABLED' $CORE_LOG | tail -1" >&2
      exit 1 ;;
    *"CHANNEL STILL DOWN"*|*"channel bring-up failed; retrying"*)
      echo "    ⚠️  Matrix channel did NOT come up within ${CHANNEL_WAIT}s (still retrying)." >&2
      echo "       If this was a matrix-sdk major upgrade, re-run with --relogin" >&2
      echo "       (add -pwd <password> if the keyring secret is also stale)." >&2
      echo "       The daemon keeps retrying meanwhile; watch: tail -f $CORE_LOG" >&2
      exit 1 ;;
    *)
      echo "    (channel status not yet in the log — check: tail -f $CORE_LOG)" ;;
  esac
fi

echo "==> done. kastellan is running the latest main."
