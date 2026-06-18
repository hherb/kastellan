"""Entry point for `kastellan-worker-browser-driver`.

Reads the operator allowlist from `KASTELLAN_BROWSER_DRIVER_ALLOWLIST`. When the
host force-routes egress (`KASTELLAN_EGRESS_PROXY_UDS` set), starts an in-jail
loopback-TCP<->UDS shim and points Chromium at it via --proxy-server; the
sidecar enforces the allowlist + SSRF at the netns boundary. Without that env
(dev / force-routing off) it runs direct, as before. The renderer also
self-enforces the allowlist per navigation/subresource (defense in depth).
"""
import os
import sys
from typing import Optional, Tuple

from .allowlist import HostAllowlist
from .render import PlaywrightRenderer, build_launch_args
from .server import Server
from .shim import ProxyShim

ALLOWLIST_ENV = "KASTELLAN_BROWSER_DRIVER_ALLOWLIST"
PROXY_UDS_ENV = "KASTELLAN_EGRESS_PROXY_UDS"
# Per-spawn writable scratch dir the host grants on macOS (#283). Keep in sync
# with the Rust constant `kastellan_core::tool_host::ENV_WORKER_SCRATCH`.
WORKER_SCRATCH_ENV = "KASTELLAN_WORKER_SCRATCH"


def _apply_worker_scratch() -> None:
    """Redirect TMPDIR/HOME to the host-provided per-spawn scratch dir, if any.

    Workers that opt into `ephemeral_scratch` are handed a unique per-spawn
    directory on macOS via `KASTELLAN_WORKER_SCRATCH` (#283), instead of a shared
    host `/tmp` grant. Point Chromium's `--user-data-dir` (`$TMPDIR`) and the
    Playwright Node driver's `uv_os_homedir()` (`$HOME`) at it so each browser
    spawn is confined to its own directory.

    On Linux the env is unset — bwrap's per-spawn `/tmp` tmpfs (#89) already
    isolates each spawn — so this is a no-op and the manifest's `TMPDIR=/tmp` /
    `HOME=/tmp` stand. A blank value is treated as unset (fail-safe).
    """
    scratch = os.environ.get(WORKER_SCRATCH_ENV, "").strip()
    if scratch:
        os.environ["TMPDIR"] = scratch
        os.environ["HOME"] = scratch


def _maybe_start_shim() -> Tuple[Optional[ProxyShim], Optional[int]]:
    """Start the egress shim iff force-routed; return (shim, port) or (None, None)."""
    uds = os.environ.get(PROXY_UDS_ENV, "").strip()
    if not uds:
        return (None, None)
    shim = ProxyShim(uds)
    port = shim.start()
    return (shim, port)


def main() -> None:
    # Redirect TMPDIR/HOME to the per-spawn scratch dir before anything writes
    # (Chromium's user-data-dir, the Node driver's home lookup) — see #283.
    _apply_worker_scratch()
    allowlist = HostAllowlist.from_env_json(os.environ.get(ALLOWLIST_ENV, ""))
    shim, port = _maybe_start_shim()
    try:
        renderer = PlaywrightRenderer(
            allowlist=allowlist,
            launch_args=build_launch_args(port),
        )
        Server(renderer=renderer).run(sys.stdin, sys.stdout)
    finally:
        if shim is not None:
            shim.stop()


if __name__ == "__main__":
    main()
