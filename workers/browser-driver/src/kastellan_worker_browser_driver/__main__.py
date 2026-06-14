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


def _maybe_start_shim() -> Tuple[Optional[ProxyShim], Optional[int]]:
    """Start the egress shim iff force-routed; return (shim, port) or (None, None)."""
    uds = os.environ.get(PROXY_UDS_ENV, "").strip()
    if not uds:
        return (None, None)
    shim = ProxyShim(uds)
    port = shim.start()
    return (shim, port)


def main() -> None:
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
