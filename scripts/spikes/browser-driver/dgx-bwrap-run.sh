#!/usr/bin/env bash
# DGX bwrap spike (throwaway). Render the fixture under a bwrap jail mirroring
# kastellan's `linux_bwrap::build_argv` for the browser-driver slice-#1 policy
# (legacy direct-net Net::Allowlist → --unshare-all + --share-net), plus the
# spike-specific binds (venv + the Playwright browser cache). Hermetic file://
# render, so no real egress is needed.
#
# Run ON the DGX: bash ~/browser-spike/dgx-bwrap-run.sh
set -uo pipefail
home="${HOME}"
spike="${home}/browser-spike"
venv="${spike}/.venv"
mspw="${home}/.cache/ms-playwright"

if ! command -v bwrap >/dev/null 2>&1; then
  echo "bwrap not installed on the DGX" >&2; exit 3
fi

set -x
bwrap \
  --unshare-all --share-net \
  --die-with-parent --new-session --as-pid-1 --clearenv \
  --setenv PATH /usr/bin:/bin \
  --setenv HOME "${home}" \
  --setenv TMPDIR /tmp \
  --proc /proc --dev /dev --tmpfs /tmp \
  --ro-bind /usr /usr \
  --symlink usr/bin /bin --symlink usr/sbin /sbin \
  --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
  --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
  --ro-bind-try /etc /etc \
  --ro-bind "${spike}" "${spike}" \
  --ro-bind "${mspw}" "${mspw}" \
  -- "${venv}/bin/python" "${spike}/probe.py"
