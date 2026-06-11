"""Entry point for `kastellan-worker-browser-driver`.

Phase 1 wires the stdio server but the real Playwright renderer lands in the
Phase-2 plan (it depends on the spike's launch flags + a `Profile::BrowserClient`
seccomp profile — see the design spec §3.1). Until then, starting the worker
raises a clear error rather than pretending to render.
"""
import sys

from .server import Server


def _build_renderer():
    # Phase 2: return a PlaywrightRenderer(launch_args=["--no-sandbox",
    # "--disable-dev-shm-usage"]) per the spike findings (spec §3.1).
    raise NotImplementedError(
        "browser-driver real renderer lands in the Phase-2 plan (spike-gated)"
    )


def main() -> None:
    renderer = _build_renderer()
    Server(renderer=renderer).run(sys.stdin, sys.stdout)


if __name__ == "__main__":
    main()
