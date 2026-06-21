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
# the installed env file (`install` REGENERATES the env from CLI flags, dropping
# the Matrix block unless --matrix-* are re-passed).
HS=""; MX_USER=""
if [ -f "$ENV_FILE" ]; then
  HS="$(sed -n 's/^KASTELLAN_MATRIX_HOMESERVER_URL=//p' "$ENV_FILE" | head -1)"
  MX_USER="$(sed -n 's/^KASTELLAN_MATRIX_USER=//p' "$ENV_FILE" | head -1)"
fi

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
echo "==> install"
if [ -n "$HS" ] && [ -n "$MX_USER" ]; then
  ./target/release/kastellan-cli install --matrix-homeserver-url "$HS" --matrix-user "$MX_USER"
else
  echo "    (no Matrix channel configured in $ENV_FILE — installing without it)"
  ./target/release/kastellan-cli install
fi

# ---- 4. optional re-login (matrix-sdk major bump / stale secret) -------------
if [ "$RELOGIN" -eq 1 ]; then
  if [ -z "$HS" ] || [ -z "$MX_USER" ]; then
    echo "ERROR: --relogin requires a Matrix channel configured in $ENV_FILE" >&2
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
  systemctl --user start kastellan-core.service
fi

# ---- 5. verify --------------------------------------------------------------
echo "==> verify (waiting for services + channel)"
sleep 6
echo -n "    services: "
systemctl --user is-active kastellan.target kastellan-core kastellan-postgres | paste -sd' '

if [ -n "$HS" ] && [ -n "$MX_USER" ] && [ -f "$CORE_LOG" ]; then
  # Most recent matrix lifecycle line anywhere in the log. NOTE: this is not
  # scoped to *this* daemon start — on a normal (non-relogin) upgrade a stale
  # "channel bus running" from a prior boot can be read if the new start has not
  # logged its status yet within the wait above. The --relogin path wipes the
  # store and restarts, so there it reflects the current start.
  # `|| true`: with `set -euo pipefail` a no-match grep exits 1 and would abort
  # the script here, making the "(not yet in the log)" fallback below unreachable.
  last="$(grep -aoE '"message":"(matrix channel bus running|matrix worker spawn/login failed[^"]*)"' "$CORE_LOG" 2>/dev/null | tail -1 || true)"
  case "$last" in
    *"channel bus running"*)
      echo "    ✅ Matrix channel is up." ;;
    *"spawn/login failed"*)
      echo "    ⚠️  Matrix channel did NOT start." >&2
      echo "       If this was a matrix-sdk major upgrade, re-run with --relogin" >&2
      echo "       (add -pwd <password> if the keyring secret is also stale)." >&2
      exit 1 ;;
    *)
      echo "    (channel status not yet in the log — check: tail -f $CORE_LOG)" ;;
  esac
fi

echo "==> done. kastellan is running the latest main."
