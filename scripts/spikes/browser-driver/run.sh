#!/usr/bin/env bash
# Spike runner (throwaway). Stage a venv + Chromium, then probe — unsandboxed
# baseline. The sandboxed legs (Seatbelt / bwrap) are driven separately; see the
# plan tasks 0.2 / 0.3 and the recorded findings in FINDINGS.md.
#
# Works with `uv` if present (Mac), else falls back to python3 -m venv + pip
# (the DGX has python3 but no uv).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
venv="${here}/.venv"

if command -v uv >/dev/null 2>&1; then
  uv venv "${venv}"
  # shellcheck disable=SC1091
  source "${venv}/bin/activate"
  uv pip install playwright readability-lxml
else
  python3 -m venv "${venv}"
  # shellcheck disable=SC1091
  source "${venv}/bin/activate"
  pip install --quiet --upgrade pip
  pip install --quiet playwright readability-lxml
fi

python -m playwright install chromium

echo "=== unsandboxed baseline ==="
python "${here}/probe.py"
