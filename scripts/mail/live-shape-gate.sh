#!/usr/bin/env bash
# Run the localmail wire-shape drift gate against the REAL service.
#
# `core/tests/mail_daemon_e2e.rs::mock_localmail_shapes_match_real_localmail` is
# the only test in the tree that talks to a live localmail. It is the half of
# #527/#500's protection that the hermetic tests structurally cannot provide:
# every other mail test asserts our fixtures agree with our code, which is true
# whether or not either agrees with the service.
#
# It is `#[ignore]`d (so `cargo test` never picks it up) AND skips-as-pass
# without the two env vars — so it is easy for it to have rotted without anyone
# noticing. It had: until 2026-08-09 it asserted the list route keyed rows under
# `results` (live: `messages`) and read ids with `as_i64()` (live: strings, so
# every row was skipped and its last two assertions never executed at all).
#
# Run this after any localmail upgrade, and before trusting `mock_localmail`.
#
#   scripts/mail/live-shape-gate.sh
#
# Env (both required — the test prints [SKIP] and passes without them):
#   KASTELLAN_MAIL_ENDPOINT   e.g. https://10.0.0.3:8443
#   KASTELLAN_MAIL_TOKEN      a localmail api-user login token
#
# If they are unset, this script reads them from the daemon's own config rather
# than making you retype them: KASTELLAN_MAIL_ENDPOINT out of kastellan.env (or
# its operator overlay kastellan.env.local, which wins — on the Mac that is
# where both mail keys live), and the token out of the file
# KASTELLAN_MAIL_TOKEN_FILE points at.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

env_file="${KASTELLAN_ENV_FILE:-$HOME/.config/kastellan/kastellan.env}"
overlay_file="${env_file}.local"

# Pull a KEY=value out of an env file without sourcing it — the file legitimately
# contains JSON whose inner quotes shell quote-removal would eat (the extra-CA
# key), and sourcing it has broken a hand-run daemon before. The overlay is read
# first, since the daemon loads it after kastellan.env and it overrides; an
# empty value there is skipped here rather than treated as "unset".
read_env_key() {
  local key="$1" f v
  for f in "$overlay_file" "$env_file"; do
    [ -r "$f" ] || continue
    v="$(sed -n "s/^${key}=//p" "$f" | tail -n 1)"
    if [ -n "$v" ]; then
      printf '%s\n' "$v"
      return 0
    fi
  done
  return 1
}

if [ -z "${KASTELLAN_MAIL_ENDPOINT:-}" ]; then
  KASTELLAN_MAIL_ENDPOINT="$(read_env_key KASTELLAN_MAIL_ENDPOINT || true)"
fi
if [ -z "${KASTELLAN_MAIL_TOKEN:-}" ]; then
  token_file="${KASTELLAN_MAIL_TOKEN_FILE:-$(read_env_key KASTELLAN_MAIL_TOKEN_FILE || true)}"
  if [ -n "${token_file:-}" ] && [ -r "$token_file" ]; then
    KASTELLAN_MAIL_TOKEN="$(tr -d '\r\n' < "$token_file")"
  fi
fi

if [ -z "${KASTELLAN_MAIL_ENDPOINT:-}" ] || [ -z "${KASTELLAN_MAIL_TOKEN:-}" ]; then
  echo "error: KASTELLAN_MAIL_ENDPOINT and KASTELLAN_MAIL_TOKEN are required." >&2
  echo "       Neither was in the environment, and $env_file (+ .local) did not supply them." >&2
  echo "       Without them the gate skips as PASS, which is why this script refuses" >&2
  echo "       to run rather than invoking cargo and reporting a green that means nothing." >&2
  exit 2
fi
export KASTELLAN_MAIL_ENDPOINT KASTELLAN_MAIL_TOKEN

echo "==> live localmail: $KASTELLAN_MAIL_ENDPOINT"
# --nocapture so a [SKIP]/[NOTE] line is visible: a silent green here would be
# indistinguishable from the gate having checked nothing.
exec cargo test -p kastellan-core --test mail_daemon_e2e \
  mock_localmail_shapes_match_real_localmail -- --ignored --nocapture
