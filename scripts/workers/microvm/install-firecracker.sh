#!/usr/bin/env bash
# install-firecracker.sh
#
# Download and install a pinned Firecracker release binary (v1.16.0, aarch64).
# Mirrors the spike download pattern: fetch the release tarball, extract the
# firecracker binary, install it to ~/.local/bin, and verify with --version.
#
# Run as the kastellan service user (no root needed; writes only to ~/.local):
#   ./scripts/workers/microvm/install-firecracker.sh   (do NOT use sudo or sh)
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/install-firecracker.sh" >&2
    exit 1
fi
set -euo pipefail

# Shared sha256 verifier (#386). The tarball is a third-party binary we are
# about to unpack and run, so it is verified against a pinned sum before
# `tar` ever touches it.
# shellcheck source=scripts/workers/microvm/lib/verify.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/verify.sh"

# Per-user install (writes only to ~/.local/bin). Refuse root so the binary
# does not land in /root/.local/bin, off the worker user's PATH (a sudo run
# resets HOME to root's).
if [ "$(id -u)" -eq 0 ]; then
    echo "Run this as the kastellan service user, NOT root — sudo would install firecracker to /root/.local/bin." >&2
    exit 1
fi

FC_VERSION="v1.16.0"

# sha256 of the release tarball for each published architecture.
#
# These are the exact sums upstream publishes next to the tarballs as
# `firecracker-${FC_VERSION}-${arch}.tgz.sha256.txt`, re-verified when this
# pin was recorded (2026-07-20/21) by fetching the tarball from two hosts on
# separate network paths (the DGX and the dev Mac) and comparing against the
# published sum — all three agreed. The aarch64 build additionally matches
# the binary the DGX has been running since 2026-06-27 (a ~3-week temporal
# witness: a substitution would have had to be in place the whole time).
#
# Not a cryptographic signature — upstream publishes these as plain SHA-256
# checksums, not a detached signature we could chain to a key — but far above
# trust-on-first-use. Bump both together with FC_VERSION; never paste in
# whatever a mismatch prints.
FC_SHA256_X86_64="bd04e26952d4e158085778c6230a0b383d2619c319182e27eaa9d61a212e92d6"
FC_SHA256_AARCH64="531c713cdbc37d4b8bc2533d851aabc0267096afa1768086a37672abb668efd7"
# Match the host architecture (Firecracker ships x86_64 and aarch64; `uname -m`
# already prints exactly those names). Don't hardcode the DGX's aarch64 — the
# backend must run on any Linux box (CLAUDE.md cross-platform constraint).
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64) FC_ARCH="${HOST_ARCH}"; FC_SHA256="${FC_SHA256_X86_64}" ;;
    aarch64) FC_ARCH="${HOST_ARCH}"; FC_SHA256="${FC_SHA256_AARCH64}" ;;
    *)
        echo "Unsupported architecture '${HOST_ARCH}'. Firecracker ships x86_64 and aarch64 only." >&2
        exit 1
        ;;
esac
FC_BINARY="firecracker-${FC_VERSION}-${FC_ARCH}"
FC_TGZ="${FC_BINARY}.tgz"
FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/${FC_TGZ}"
INSTALL_DIR="${HOME}/.local/bin"

mkdir -p "${INSTALL_DIR}"

WORKDIR=$(mktemp -d); trap 'rm -rf "${WORKDIR}"' EXIT
echo "Downloading Firecracker ${FC_VERSION} (${FC_ARCH})..."
curl -fL --retry 3 -o "${WORKDIR}/${FC_TGZ}" "${FC_URL}"

# Verify BEFORE we unpack or run it. A tarball checked only after extraction
# and install protects nothing — the malicious binary is already in place.
if ! verify_sha256 "${WORKDIR}/${FC_TGZ}" "${FC_SHA256}"; then
    echo "Downloaded Firecracker tarball does not match the pinned sha256 — refusing to install." >&2
    echo "  source: ${FC_URL}" >&2
    exit 1
fi

echo "Extracting..."
tar -xzf "${WORKDIR}/${FC_TGZ}" -C "${WORKDIR}"

# The release tarball unpacks as release-${FC_VERSION}-${FC_ARCH}/firecracker-${FC_VERSION}-${FC_ARCH}
EXTRACTED="${WORKDIR}/release-${FC_VERSION}-${FC_ARCH}/${FC_BINARY}"
if [[ ! -f "${EXTRACTED}" ]]; then
    echo "Unexpected tarball layout. Contents:" >&2
    find "${WORKDIR}" -maxdepth 3 >&2
    exit 1
fi

install -m 0755 "${EXTRACTED}" "${INSTALL_DIR}/firecracker"

echo "Installed ${INSTALL_DIR}/firecracker"
"${INSTALL_DIR}/firecracker" --version
