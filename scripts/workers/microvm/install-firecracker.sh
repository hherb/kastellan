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

# Per-user install (writes only to ~/.local/bin). Refuse root so the binary
# does not land in /root/.local/bin, off the worker user's PATH (a sudo run
# resets HOME to root's).
if [ "$(id -u)" -eq 0 ]; then
    echo "Run this as the kastellan service user, NOT root — sudo would install firecracker to /root/.local/bin." >&2
    exit 1
fi

FC_VERSION="v1.16.0"
# Match the host architecture (Firecracker ships x86_64 and aarch64; `uname -m`
# already prints exactly those names). Don't hardcode the DGX's aarch64 — the
# backend must run on any Linux box (CLAUDE.md cross-platform constraint).
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) FC_ARCH="${HOST_ARCH}" ;;
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
