#!/usr/bin/env bash
# Verification suite for a rendered conduwuit config: HARD-FAILS unless the
# security-critical invariants hold (federation off, loopback bind, registration
# not open). Run on a RENDERED config (placeholders substituted):
#
#   scripts/matrix/check-conduwuit-config.sh <path/to/conduwuit.toml>
#
# Or self-test the committed template (renders it with dummy values + asserts the
# checker accepts a safe config and rejects an open-registration one):
#
#   scripts/matrix/check-conduwuit-config.sh --self-test
set -u

# Validate one rendered config file. Echoes FAILs; returns non-zero on any.
validate() {
  f="$1"
  errs=0
  if [ ! -f "$f" ]; then echo "FAIL: $f not found"; return 1; fi

  if grep -Eq '\{\{[A-Z_]+\}\}' "$f"; then
    echo "FAIL: unsubstituted {{PLACEHOLDER}} (validate a RENDERED config)"; errs=1
  fi
  if ! grep -Eq '^[[:space:]]*allow_federation[[:space:]]*=[[:space:]]*false' "$f"; then
    echo "FAIL: allow_federation must be false (federation-off is required)"; errs=1
  fi
  if ! grep -Eq '^[[:space:]]*address[[:space:]]*=[[:space:]]*"(127\.0\.0\.1|::1)"' "$f"; then
    echo "FAIL: address must bind loopback (\"127.0.0.1\" or \"::1\")"; errs=1
  fi
  # Registration must not be open: if allow_registration=true, a non-empty
  # registration_token is required (token-gated, one-time setup only).
  if grep -Eq '^[[:space:]]*allow_registration[[:space:]]*=[[:space:]]*true' "$f"; then
    if ! grep -Eq '^[[:space:]]*registration_token[[:space:]]*=[[:space:]]*"[^"]+"' "$f"; then
      echo "FAIL: open registration (allow_registration=true) without a registration_token"; errs=1
    fi
  fi
  return "$errs"
}

render_template() {
  # render_template <token> <allow_registration_override_or_empty>
  local tmpl token reg
  tmpl="$(cd "$(dirname "$0")/../../deploy/matrix" && pwd)/conduwuit.toml.template"
  token="$1"
  sed -e "s/{{SERVER_NAME}}/matrix.example.org/" \
      -e "s/{{PORT}}/6167/" \
      -e "s#{{DB_PATH}}#/var/lib/conduwuit#" \
      -e "s/{{REGISTRATION_TOKEN}}/$token/" \
      "$tmpl"
}

self_test() {
  local tmp fails=0
  tmp="$(mktemp)"

  # 1) Safe rendered config (token-gated registration) must PASS.
  render_template "tok-abc123" > "$tmp"
  if validate "$tmp"; then echo "PASS: token-gated rendered config accepted"; else
    echo "FAIL: token-gated rendered config rejected"; fails=1; fi

  # 2) Open registration (true, empty token) must be REJECTED.
  render_template "" > "$tmp"
  if validate "$tmp" >/dev/null 2>&1; then
    echo "FAIL: open-registration config was NOT rejected"; fails=1
  else
    echo "PASS: open-registration config correctly rejected"
  fi

  # 3) A fully-closed config (allow_registration=false) must PASS.
  render_template "unused" | sed 's/^allow_registration = true/allow_registration = false/' > "$tmp"
  if validate "$tmp"; then echo "PASS: closed-registration config accepted"; else
    echo "FAIL: closed-registration config rejected"; fails=1; fi

  rm -f "$tmp"
  return "$fails"
}

if [ "${1:-}" = "--self-test" ]; then
  if self_test; then echo "OK: conduwuit config check self-test passed"; exit 0
  else echo "conduwuit config check self-test FAILED"; exit 1; fi
fi

if [ $# -ne 1 ]; then
  echo "usage: $0 <rendered-conduwuit.toml> | --self-test" >&2
  exit 2
fi
if validate "$1"; then
  echo "OK: $1 satisfies the security invariants"; exit 0
else
  echo "config check FAILED for $1"; exit 1
fi
