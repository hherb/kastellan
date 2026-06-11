#!/usr/bin/env bash
# Seatbelt spike (throwaway). Render the fixture under a sandbox-exec profile
# that mirrors kastellan's `macos_seatbelt::build_profile` for the browser-driver
# slice-#1 policy (deny-default, base reads, allow network*, NO mach-lookup),
# plus the spike-specific reads (venv + the Playwright browser cache) and a
# single writable scratch (Chromium's user-data-dir via $TMPDIR).
#
# Usage: seatbelt-run.sh [with-mach]
#   default     → strict profile (no mach-lookup) — the kastellan posture
#   with-mach   → adds (allow mach-lookup) to isolate whether the browser needs it
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
venv="${here}/.venv"
scratch="${here}/scratch"
mspw="${HOME}/Library/Caches/ms-playwright"
rm -rf "${scratch}" && mkdir -p "${scratch}"

# Capability clusters to bisect what a headless Chromium child needs beyond the
# base file reads. Pass one as $1:
#   (none)   → strict kastellan posture (no extras)
#   mach     → + mach-lookup
#   chromium → + the full Chromium-on-macOS cluster (mach + shm + iokit + sysctl-write)
mach_rule=""
case "${1:-}" in
  mach)
    mach_rule='(allow mach-lookup)'
    ;;
  chromium)
    mach_rule='(allow mach-lookup)
(allow mach-register)
(allow ipc-posix-shm*)
(allow iokit-open)
(allow iokit-get-properties)
(allow sysctl-write)
(allow system-socket)'
    ;;
esac

# uv-created venvs symlink `python` to an external uv-managed CPython, whose
# libpython lives OUTSIDE venv_dir. Resolve the real interpreter dir and add it
# to the read set. (Finding: the production worker venv must either be
# self-contained or have this interpreter root mounted — see FINDINGS.md.)
real_py="$("${venv}/bin/python" -c 'import sys,os; print(os.path.realpath(sys.executable))')"
py_root="$(dirname "$(dirname "${real_py}")")"

profile="$(cat <<EOF
(version 1)
(deny default)
(allow process-fork)
(allow process-exec*)
(allow file-read* (literal "/"))
(allow file-read* (subpath "/usr/lib"))
(allow file-read* (subpath "/usr/libexec"))
(allow file-read* (subpath "/System/Library"))
(allow file-read-metadata (subpath "/"))
(allow sysctl-read)
${mach_rule}
(allow file-read* file-write* (literal "/dev/null"))
(allow file-read* file-write* (literal "/dev/zero"))
(allow file-read* (literal "/dev/random"))
(allow file-read* (literal "/dev/urandom"))
(allow file-read* file-write* (subpath "/dev/fd"))
(allow file-read* (subpath "${here}"))
(allow file-read* (subpath "${venv}"))
(allow file-read* (subpath "${py_root}"))
(allow file-read* (subpath "${mspw}"))
(allow file-read* (subpath "/Library/Fonts"))
(allow file-read* (subpath "${HOME}/Library/Fonts"))
(allow file-read* file-write* (subpath "${scratch}"))
(allow network*)
EOF
)"

echo "=== seatbelt profile (mach=${1:-none}) ==="
echo "${profile}"
echo "=== run ==="
TMPDIR="${scratch}" sandbox-exec -p "${profile}" "${venv}/bin/python" "${here}/probe.py"
echo "exit=$?"
