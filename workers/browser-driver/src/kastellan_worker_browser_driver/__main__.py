"""Entry point for `kastellan-worker-browser-driver`.

Reads the operator allowlist from `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` (a JSON
array of `host[:port]`, injected by the host manifest), builds the real
`PlaywrightRenderer`, and runs the stdio JSON-RPC server. The renderer
self-enforces the allowlist per navigation + subresource (defense in depth; the
jail is the hard boundary — see the design spec §6 / issue #263).
"""
import os
import sys

from .allowlist import HostAllowlist
from .render import PlaywrightRenderer
from .server import Server

ALLOWLIST_ENV = "KASTELLAN_BROWSER_DRIVER_ALLOWLIST"


def _build_renderer() -> PlaywrightRenderer:
    # A missing/blank env yields an empty allowlist that permits nothing —
    # fail-closed (every navigation then aborts at the route handler).
    allowlist = HostAllowlist.from_env_json(os.environ.get(ALLOWLIST_ENV, ""))
    return PlaywrightRenderer(allowlist=allowlist)


def main() -> None:
    renderer = _build_renderer()
    Server(renderer=renderer).run(sys.stdin, sys.stdout)


if __name__ == "__main__":
    main()
