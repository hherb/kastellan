# shellcheck shell=bash
#
# Shared, integrity-checked guest-kernel fetch for every build-*-rootfs.sh.
#
# SOURCE this file, do not execute it:
#
#     source "$(dirname "${BASH_SOURCE[0]}")/lib/guest-kernel.sh"
#     fetch_guest_kernel "$OUT_DIR"
#
# ---------------------------------------------------------------------
# Why this file exists (issue #471)
# ---------------------------------------------------------------------
#
# All eight rootfs build scripts boot the *same* guest kernel, and each
# one used to carry its own copy of the URL, the architecture `case`, and
# an unchecked download:
#
#     [ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o … "$KERNEL_URL"
#
# Two problems, in increasing order of seriousness:
#
#   1. The URL was version-pinned but never integrity-checked, so a
#      compromised mirror, a MITM, or plain corruption on the wire hands
#      us an arbitrary kernel — which then boots *inside* the containment
#      boundary. The micro-VM is the boundary; the kernel is the thing
#      enforcing it. This is close to the worst artefact in the project
#      to fetch on trust.
#
#   2. `[ -f … ] ||` means a kernel already on disk was reused **forever,
#      unchecked**. Whatever landed there the first time — however it got
#      there — is what every later build boots. Point 1 is a fetch-time
#      risk; point 2 makes it permanent.
#
# Eight copies of that pattern also meant eight chances to drift, which
# issue #475 had just finished cleaning up on the test side. So the fix
# is one shared file rather than eight parallel edits: the URL, the arch
# table and the sums are written down exactly once, and the unit tests in
# `tests-common/src/microvm.rs` fail if a build script grows its own copy
# again.
#
# ---------------------------------------------------------------------
# On trusting these sums
# ---------------------------------------------------------------------
#
# The sums below were recorded by downloading the artefacts, which on its
# own is trust-on-first-use: it pins the kernel against *future* drift or
# tampering, but cannot prove the bytes were honest the first time.
# Upstream publishes no signature for this CI bucket, so TOFU is the
# ceiling available to us here.
#
# What raises confidence above bare TOFU, per arch:
#
#   * aarch64 — confirmed against a copy downloaded independently three
#     weeks earlier on a different host (the DGX's working `vmlinux`,
#     fetched 2026-06-27 and used for every micro-VM run since). Two
#     fetches that far apart agreeing means a substitution would have had
#     to be in place the whole time.
#
#   * x86_64 — re-fetched 2026-07-20 from two hosts on separate network
#     paths (the DGX and the dev Mac), both matching. That rules out a
#     local MITM, a poisoned resolver, and a mis-recorded sum; it does
#     not rule out a substitution at the origin, and there is no temporal
#     separation. So: still honest TOFU, just not a single-fetch one.
#
# Neither is a signature. If upstream ever publishes one, prefer it.
#
# When the pinned kernel version changes, re-record both sums in the same
# deliberate step as the version bump; never "fix" a mismatch by pasting
# in whatever the failure printed.

# Pinned guest kernel. The firecracker-ci bucket publishes the same
# kernel version under x86_64/ and aarch64/.
KASTELLAN_GUEST_KERNEL_VERSION="6.1.102"
KASTELLAN_GUEST_KERNEL_CI_TAG="v1.10"
KASTELLAN_GUEST_KERNEL_BASE_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci"

# sha256 of vmlinux-6.1.102 for each published architecture.
#
# Recorded 2026-07-20. Bump these together with the version above.
KASTELLAN_GUEST_KERNEL_SHA256_X86_64="49ba99a5299444ac59dda2efc3569cc2d58a5d72ea6475a6bfc37aa0bf322e54"
KASTELLAN_GUEST_KERNEL_SHA256_AARCH64="bb1f50912d63a8ca5e92d488984875e1177eb9283050ffa592a8cb455cada52d"

# sha256 of a file, printed as bare hex.
#
# Linux ships `sha256sum`, macOS ships `shasum`. The build scripts only
# ever run on Linux, but the unit tests that prove this file fails closed
# run on the dev Mac too — and a check that is only exercised on one host
# is a check that is half-verified.
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

# _kastellan_quarantine <file> <evidence-prefix>
#
# Move a rejected artefact aside as `<evidence-prefix>.rejected.<sha256>`
# and echo where it went.
#
# Named by *content*, for two reasons. A second bad kernel cannot clobber
# the evidence from the first — the point of keeping the bytes is being
# able to answer "what exactly did we almost boot?", and that answer is
# worth much less if only the most recent attempt survives. And re-running
# the build on the same bad file is idempotent: same content, same name,
# no pile of near-identical corpses.
#
# The prefix is passed separately so a rejection from the download path
# lands on `vmlinux.rejected.<sum>` too, rather than on the internal
# `vmlinux.partial.<pid>` name — an operator looking for evidence should
# find one naming scheme, not two.
#
# Fails if the move fails. The caller must not claim to have preserved
# something it did not: a verifier that misreports what it did is barely
# better than one that never checked.
_kastellan_quarantine() {
    local file="$1" prefix="$2" sum target
    # A hash failure here is not fatal — we still want the bytes moved
    # out of the way — but it does mean we cannot name them by content.
    sum="$(_kastellan_sha256_of "$file" 2>/dev/null)" || sum="unhashable"
    target="$prefix.rejected.$sum"
    if ! mv -f "$file" "$target"; then
        echo "Could not quarantine $file (tried: $target)." >&2
        echo "If it is root-owned (the default since #479), re-run the privileged" >&2
        echo "installer, which owns the kernel and can replace it:" >&2
        echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
        echo "Otherwise remove it by hand; the build will keep refusing until you do." >&2
        return 1
    fi
    echo "$target"
}

# guest_kernel_sha256 <arch>
#
# The recorded sum for a supported architecture. Fails — rather than
# printing an empty string — for anything else, so an unknown arch can
# never degrade into an unverified download.
guest_kernel_sha256() {
    case "$1" in
        x86_64) echo "$KASTELLAN_GUEST_KERNEL_SHA256_X86_64" ;;
        aarch64) echo "$KASTELLAN_GUEST_KERNEL_SHA256_AARCH64" ;;
        *)
            echo "No recorded guest-kernel sha256 for architecture '$1'." >&2
            return 1
            ;;
    esac
}

# The host architecture, validated against what upstream publishes.
#
# Deliberately does not map macOS's `arm64` onto `aarch64`: these scripts
# build a Linux guest rootfs and only run on Linux. Silently accepting a
# Mac would swap a clear error for a confusing failure later.
guest_kernel_arch() {
    local host_arch
    host_arch="$(uname -m)"
    case "${host_arch}" in
        x86_64 | aarch64) echo "${host_arch}" ;;
        *)
            echo "Unsupported architecture '${host_arch}'. The pinned guest kernel is published for x86_64 and aarch64 only." >&2
            return 1
            ;;
    esac
}

# guest_kernel_url <arch>
guest_kernel_url() {
    echo "${KASTELLAN_GUEST_KERNEL_BASE_URL}/${KASTELLAN_GUEST_KERNEL_CI_TAG}/$1/vmlinux-${KASTELLAN_GUEST_KERNEL_VERSION}"
}

# require_guest_kernel <out_dir> [arch]
#
# Verify an EXISTING `$out_dir/vmlinux` and never create one. This is what
# the eight build-*-rootfs.sh call; `fetch_guest_kernel` below is for the
# privileged installer only.
#
# Why builds must not fetch (issue #479). The image dir is
# `root:<worker-group>` mode 1775, so the worker has GROUP WRITE — it owns
# and manages its own `*.ext4` images there. Group write is also enough to
# CREATE a new entry. So if `vmlinux` is ever absent, an unprivileged build
# calling `fetch_guest_kernel` would happily rename its download into place
# and leave an **agent-owned** kernel: no unlink of root's file is needed,
# nothing fails, and the ownership half of #479 is silently gone from that
# moment on. unlink(2)'s first exemption (the file's own owner) then
# applies and the agent can replace the kernel at will.
#
# Verifying-but-never-creating removes that path entirely. A missing kernel
# becomes a loud, actionable stop instead of a quiet downgrade.
require_guest_kernel() {
    local out_dir="$1" arch="${2:-}" expected dest
    if [ -z "$arch" ]; then
        arch="$(guest_kernel_arch)" || return 1
    fi
    expected="$(guest_kernel_sha256 "$arch")" || return 1
    dest="$out_dir/vmlinux"

    if [ ! -e "$dest" ]; then
        echo "No guest kernel at $dest." >&2
        echo "Builds do not fetch it: it must be installed by root so the agent" >&2
        echo "cannot replace it (issue #479). Install it with:" >&2
        echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
        echo "For a non-default KASTELLAN_MICROVM_DIR (which root does not manage," >&2
        echo "so it carries no ownership protection) fetch it deliberately with:" >&2
        echo "    ./scripts/workers/microvm/fetch-guest-kernel.sh \"$out_dir\"" >&2
        return 1
    fi
    if [ -L "$dest" ]; then
        echo "$dest is a symlink, not a regular file." >&2
        echo "Refusing to build: a symlink is owned by whoever created it, so it" >&2
        echo "can be re-pointed even when its target is root-owned (issue #479)." >&2
        echo "Remove it and re-run sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
        return 1
    fi
    if ! verify_sha256 "$dest" "$expected"; then
        echo "Refusing to build on an unverified guest kernel." >&2
        echo "Re-install it: sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
        return 1
    fi
}

# fetch_guest_kernel <out_dir> [arch]
#
# Leaves a verified `$out_dir/vmlinux` in place, or fails without leaving
# an unverified one. `arch` defaults to this host's; the tests pass it
# explicitly so they can exercise the logic on either dev box.
#
# Three properties worth keeping:
#
#   * An **already-present** kernel is verified rather than trusted. This
#     is the actual gap from issue #471 — the old `[ -f … ] ||` guard
#     meant a bad file, once written, was reused by every later build.
#
#   * A **rejected** kernel is moved to `vmlinux.rejected.<sha256>`
#     rather than deleted. The build still stops, and the next run still
#     starts clean, but the suspect bytes survive for investigation — if
#     this ever fires for real, "what exactly did we almost boot?" is the
#     first question, and deleting the evidence answers it badly.
#
#   * The download lands on a per-process `vmlinux.partial.<pid>` and is
#     renamed only after it verifies, so an interrupted or failed run can
#     never leave something at `vmlinux` that a later build would treat
#     as good — and two build scripts running at once cannot scribble
#     over each other's in-flight download (which was harmless, in that
#     the sum caught it, but produced a baffling spurious rejection).
#
#     The cost of the per-process name: a run killed hard (SIGKILL, power
#     loss) leaves its `.partial.<pid>` behind, where the old single
#     `.partial` would have been truncated and reused. Deliberately not
#     swept here — this function cannot tell a corpse from a download
#     another live build is midway through, and deleting the latter to
#     tidy up the former is a bad trade. Sweep by hand if they pile up.
#
# Callers: the privileged installer, and fetch-guest-kernel.sh (a
# deliberate operator action for a non-default image dir). NOT the build
# scripts — since #479 they call `require_guest_kernel` above, because a
# build that can create the kernel can create an agent-owned one.
fetch_guest_kernel() {
    local out_dir="$1" arch="${2:-}" expected url dest tmp quarantined
    if [ -z "$arch" ]; then
        arch="$(guest_kernel_arch)" || return 1
    fi
    expected="$(guest_kernel_sha256 "$arch")" || return 1
    url="$(guest_kernel_url "$arch")"
    dest="$out_dir/vmlinux"
    tmp="$dest.partial.$$"

    if [ -f "$dest" ]; then
        if verify_sha256 "$dest" "$expected"; then
            return 0
        fi
        quarantined="$(_kastellan_quarantine "$dest" "$dest")" || return 1
        echo "Refusing to build on an unverified guest kernel." >&2
        echo "  quarantined: $quarantined" >&2
        echo "  re-run this script to fetch a fresh copy." >&2
        return 1
    fi

    echo "Fetching pinned guest kernel (${arch}, ${KASTELLAN_GUEST_KERNEL_VERSION})..."
    if ! curl -fL --retry 3 -o "$tmp" "$url"; then
        rm -f "$tmp"
        echo "Guest-kernel download failed: $url" >&2
        return 1
    fi
    if ! verify_sha256 "$tmp" "$expected"; then
        quarantined="$(_kastellan_quarantine "$tmp" "$dest")" || return 1
        echo "Downloaded guest kernel does not match the recorded sha256." >&2
        echo "  source:      $url" >&2
        echo "  quarantined: $quarantined" >&2
        return 1
    fi
    # Verified. Only now does it get the name a build will trust — and if
    # even this fails, say so rather than returning a bare non-zero: the
    # bytes were good, so "download failed" would be the wrong story.
    if ! mv -f "$tmp" "$dest"; then
        echo "Verified guest kernel, but could not move it into place: $dest" >&2
        echo "  it is still at: $tmp" >&2
        return 1
    fi
}
