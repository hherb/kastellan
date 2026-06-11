"""Throwaway browser-in-jail feasibility probe (spike — delete after slice #1).

Launches headless Chromium via Playwright, renders the URL given as argv[1]
(default: the bundled file:// fixture page), prints the post-JS <title> + a text
snippet to stdout, and exits 0 on success. Any launch/render failure prints the
exception to stderr and exits non-zero. Run it (a) unsandboxed as a baseline,
then (b) under Seatbelt/bwrap to discover the jail adjustments.

Launch flags under test: rely on OUR OS jail, not Chromium's own user-namespace
sandbox (--no-sandbox), and avoid the shared-memory /dev/shm dependency
(--disable-dev-shm-usage). If a backend still SIGSYS/crashes, the next knob to
try is --single-process (see spec §3.1).
"""
import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

LAUNCH_ARGS = ["--no-sandbox", "--disable-dev-shm-usage"]


def main() -> int:
    url = sys.argv[1] if len(sys.argv) > 1 else (
        "file://" + str(Path(__file__).with_name("fixture.html"))
    )
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True, args=LAUNCH_ARGS)
        try:
            page = browser.new_page()
            page.goto(url, wait_until="networkidle", timeout=15000)
            title = page.title()
            text = (page.inner_text("body") or "")[:200]
            print(f"OK title={title!r} snippet={text!r}")
            return 0
        finally:
            browser.close()


if __name__ == "__main__":
    sys.exit(main())
