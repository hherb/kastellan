#!/usr/bin/env bash
# install-firecracker.sh
#
# Download and install a pinned Firecracker release binary (v1.16.0, aarch64).
# Mirrors the spike download pattern: fetch the release tarball, extract the
# firecracker binary, install it to ~/.local/bin, and verify with --version.
#
# Run as the kastellan service user (no root needed; writes only to ~/.local):
#   bash scripts/workers/microvm/install-firecracker.sh
set -euo pipefail

FC_VERSION="v1.16.0"
FC_ARCH="aarch64"
FC_BINARY="firecracker-${FC_VERSION}-${FC_ARCH}"
FC_TGZ="${FC_BINARY}.tgz"
FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/${FC_TGZ}"
INSTALL_DIR="${HOME}/.local/bin"

mkdir -p "${INSTALL_DIR}"

TMPDIR=$(mktemp -d); trap 'rm -rf "${TMPDIR}"' EXIT
echo "Downloading Firecracker ${FC_VERSION} (${FC_ARCH})..."
curl -fL --retry 3 -o "${TMPDIR}/${FC_TGZ}" "${FC_URL}"

echo "Extracting..."
tar -xzf "${TMPDIR}/${FC_TGZ}" -C "${TMPDIR}"

# The release tarball unpacks as release-${FC_VERSION}-${FC_ARCH}/firecracker-${FC_VERSION}-${FC_ARCH}
EXTRACTED="${TMPDIR}/release-${FC_VERSION}-${FC_ARCH}/${FC_BINARY}"
if [[ ! -f "${EXTRACTED}" ]]; then
    echo "Unexpected tarball layout. Contents:" >&2
    find "${TMPDIR}" -maxdepth 3 >&2
    exit 1
fi

install -m 0755 "${EXTRACTED}" "${INSTALL_DIR}/firecracker"

echo "Installed ${INSTALL_DIR}/firecracker"
"${INSTALL_DIR}/firecracker" --version
