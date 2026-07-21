# shellcheck shell=bash
#
# Shared sha256 verification helpers for the provisioning scripts.
#
# SOURCE this file, do not execute it:
#
#     source "$(dirname "${BASH_SOURCE[0]}")/lib/verify.sh"
#     verify_sha256 "$file" "$expected_hex"
#
# ---------------------------------------------------------------------
# Why this file exists (issue #386)
# ---------------------------------------------------------------------
#
# `verify_sha256` started life inside `guest-kernel.sh`, the shared guest-
# kernel pin (#471). It is not kernel-specific, though: it is a pure "does
# this file's sha256 equal this hex string" check, and #386 needed the
# very same check for two *other* unverified downloads —
#
#   * install-firecracker.sh (the Firecracker VMM binary), and
#   * scripts/matrix/vps/phase2-homeserver.sh (the Continuwuity binary).
#
# So the two pure helpers were lifted here, `guest-kernel.sh` now sources
# this file, and install-firecracker.sh sources it too. One hasher, one
# place to get it right. (phase2-homeserver.sh cannot source it — it runs
# standalone on the VPS with only the phase scripts copied alongside, no
# repo — so it carries its own inline copy of the same three lines. There
# is no repo path it could source at deploy time.)

# sha256 of a file, printed as bare hex.
#
# Linux ships `sha256sum`, macOS ships `shasum`. The provisioning scripts
# only ever run on Linux, but the unit tests that prove this file fails
# closed run on the dev Mac too — and a check that is only exercised on
# one host is a check that is half-verified.
_kastellan_sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        echo "Need sha256sum (Linux) or shasum (macOS) to verify downloads." >&2
        return 1
    fi
}

# verify_sha256 <path> <expected-hex>
#
# Succeeds only on an exact match. Prints both sums on failure so the
# operator can tell "truncated download" from "different artefact"
# without re-running anything.
#
# Pure: reads the file, touches nothing else, and never deletes. Callers
# decide what to do with a bad file.
#
# The local is called `file`, not `path`, deliberately. In zsh `path` is
# the array tied to `$PATH`, so `local path=…` silently destroys command
# lookup for the rest of the function — `command -v sha256sum` then finds
# nothing and this reports "no hasher available" instead of the mismatch
# it was asked about. These scripts run under bash, where `path` is an
# ordinary name, so that only bites someone sourcing this from an
# interactive zsh — but a verifier that fails for the wrong reason is the
# one thing a verifier must never do.
verify_sha256() {
    local file="$1" expected="$2" actual
    actual="$(_kastellan_sha256_of "$file")" || return 1
    if [ "$actual" != "$expected" ]; then
        echo "sha256 mismatch for $file" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        return 1
    fi
}
