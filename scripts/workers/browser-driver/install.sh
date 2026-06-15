#!/usr/bin/env bash
# Operator setup for the browser-driver worker (Phase 2).
# Idempotent; safe to re-run.
#
# Builds a SELF-CONTAINED venv with system `python3 -m venv` (NOT uv): the spike
# found a uv-created venv symlinks `python` to an external uv-managed CPython
# whose libpython lives outside the venv dir, which the jail blocks. A
# system-venv keeps the interpreter resolvable within a stable prefix, so only
# the venv dir needs binding into the sandbox.
#
# Browsers are installed INSIDE the venv (PLAYWRIGHT_BROWSERS_PATH=<venv>/browsers)
# so the host manifest needs no separate browser-cache fs_read bind — see
# core/src/workers/browser_driver.rs.

set -euo pipefail

command -v python3 >/dev/null 2>&1 || {
  echo "error: python3 is required" >&2
  exit 1
}

# ----- paths (match the host manifest's resolve_env anchor cascade) -----
REPO_ROOT="$(git rev-parse --show-toplevel)"
WORKER_DIR="$REPO_ROOT/workers/browser-driver"
DATA_DIR="${KASTELLAN_DATA_DIR:-$HOME/.local/share/kastellan}"
VENV_DIR="${KASTELLAN_BROWSER_DRIVER_VENV_DIR:-$DATA_DIR/workers/browser-driver/.venv}"
BROWSERS_DIR="$VENV_DIR/browsers"

if [ ! -d "$WORKER_DIR" ]; then
  echo "error: $WORKER_DIR not found; run from a checkout of the kastellan repo" >&2
  exit 1
fi

echo ">>> creating self-contained venv at $VENV_DIR"
mkdir -p "$(dirname "$VENV_DIR")"
python3 -m venv "$VENV_DIR"

echo ">>> installing the worker into the venv (non-editable)"
# NON-editable on purpose: the jailed worker only fs_reads the venv, so the
# package must be copied INTO venv site-packages. An editable (`-e`) install
# leaves the source in the repo via a `.pth`, which the sandbox can't read.
"$VENV_DIR/bin/pip" install --upgrade pip >/dev/null
# Two steps so a re-run always stages the CURRENT worker source:
#   1. Plain install pulls in the runtime deps (readability/lxml/playwright);
#      pip skips any already-satisfied versioned dep, so re-runs are fast.
#   2. Force-reinstall the local package WITHOUT deps. The package version is
#      static (0.0.1), so a plain `pip install <path>` on a re-run reports
#      "already satisfied" and SILENTLY KEEPS STALE worker code after the source
#      changes — e.g. egress slice #2 added shim.py + rewired __main__.py, and a
#      stale venv (no shim, no --proxy-server) made Chromium bypass the egress
#      sidecar on macOS, which looked like a forced-egress code bug (issue #287)
#      but was just an out-of-date install. --force-reinstall always recopies.
"$VENV_DIR/bin/pip" install "$WORKER_DIR"
"$VENV_DIR/bin/pip" install --force-reinstall --no-deps "$WORKER_DIR"

echo ">>> installing the chromium headless shell into $BROWSERS_DIR"
# Keep the browser tree inside the venv so only the venv needs an fs_read bind.
PLAYWRIGHT_BROWSERS_PATH="$BROWSERS_DIR" "$VENV_DIR/bin/playwright" install chromium

# ----- sanity check: the console-script shim the manifest looks for -----
SHIM="$VENV_DIR/bin/kastellan-worker-browser-driver"
if [ ! -x "$SHIM" ]; then
  echo "error: console-script shim not found at $SHIM — pip install failed" >&2
  exit 2
fi

echo
echo "ok: venv at $VENV_DIR"
echo "ok: browsers at $BROWSERS_DIR"
echo "To enable in the daemon, export KASTELLAN_BROWSER_DRIVER_ENABLE=1 before starting kastellan."
echo "Set the per-tool allowlist (KASTELLAN_BROWSER_DRIVER_ALLOWLIST / tool_allowlists row)."
echo
echo "Browser-driver is now egress-proxy-routed in the default force-routed deployment"
echo "(private netns → per-worker egress sidecar; no escape hatch needed)."
