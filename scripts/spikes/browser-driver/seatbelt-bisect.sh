#!/usr/bin/env bash
# Bisect the MINIMAL Seatbelt capability additions a headless Chromium child
# needs over kastellan's strict base profile. Each candidate set is appended to
# the same base; we print PASS/FAIL per set. Throwaway spike tooling.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
venv="${here}/.venv"
mspw="${HOME}/Library/Caches/ms-playwright"
real_py="$("${venv}/bin/python" -c 'import sys,os; print(os.path.realpath(sys.executable))')"
py_root="$(dirname "$(dirname "${real_py}")")"

base="(version 1)
(deny default)
(allow process-fork)
(allow process-exec*)
(allow file-read* (literal \"/\"))
(allow file-read* (subpath \"/usr/lib\"))
(allow file-read* (subpath \"/usr/libexec\"))
(allow file-read* (subpath \"/System/Library\"))
(allow file-read-metadata (subpath \"/\"))
(allow sysctl-read)
(allow file-read* file-write* (literal \"/dev/null\"))
(allow file-read* file-write* (literal \"/dev/zero\"))
(allow file-read* (literal \"/dev/random\"))
(allow file-read* (literal \"/dev/urandom\"))
(allow file-read* file-write* (subpath \"/dev/fd\"))
(allow file-read* (subpath \"${here}\"))
(allow file-read* (subpath \"${venv}\"))
(allow file-read* (subpath \"${py_root}\"))
(allow file-read* (subpath \"${mspw}\"))
(allow file-read* (subpath \"/Library/Fonts\"))
(allow file-read* (subpath \"${HOME}/Library/Fonts\"))
(allow file-read* file-write* (subpath \"${here}/scratch\"))
(allow network*)"

try() {
  local label="$1"; shift
  local extras="$1"
  rm -rf "${here}/scratch" && mkdir -p "${here}/scratch"
  local prof="${base}
${extras}"
  if TMPDIR="${here}/scratch" sandbox-exec -p "${prof}" "${venv}/bin/python" "${here}/probe.py" >/dev/null 2>&1; then
    echo "PASS  ${label}"
  else
    echo "FAIL  ${label}"
  fi
}

try "shm-only"            "(allow ipc-posix-shm*)"
try "iokit-only"          "(allow iokit-open) (allow iokit-get-properties)"
try "mach-only"           "(allow mach-lookup) (allow mach-register)"
try "shm+iokit"           "(allow ipc-posix-shm*) (allow iokit-open) (allow iokit-get-properties)"
try "shm+mach"            "(allow ipc-posix-shm*) (allow mach-lookup) (allow mach-register)"
try "shm+iokit+mach"      "(allow ipc-posix-shm*) (allow iokit-open) (allow iokit-get-properties) (allow mach-lookup) (allow mach-register)"
