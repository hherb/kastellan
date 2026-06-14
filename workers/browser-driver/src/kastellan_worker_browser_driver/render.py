"""Render-result extraction (pure) + the Playwright drive.

`extract_render_result` is pure: given the post-JS HTML + navigation metadata it
produces the `browser.render` result dict (readability text + capped final
HTML). It is unit-tested without a browser.

`PlaywrightRenderer` is the real drive. Its launch is behind a `playwright`
factory seam so the orchestration (route interception → goto → content →
extract) is testable with a fake Playwright; the actual browser launch is
exercised only by the `#[ignore] real_render` e2e. The host:port allowlist
check (`request_is_allowed`) is pure and unit-tested.
"""
from typing import Any, Callable, Optional
from urllib.parse import urlparse

from readability import Document
from lxml import html as lxml_html

from .allowlist import HostAllowlist

# Wire-contract caps. Bumping any requires updating the Rust side + spec table.
MAX_HTML_BYTES = 5 * 1024 * 1024   # 5 MiB, mirrors web-fetch
MAX_TEXT_CHARS = 200_000           # post-readability text char cap

# Default launch flags, pinned by the spike (design spec §3.1): `--no-sandbox`
# defers containment to OUR jail (Chromium's own user-ns sandbox can't nest
# inside bwrap); `--disable-dev-shm-usage` makes Chromium use the profile dir
# instead of /dev/shm so the jail needs no writable /dev/shm.
DEFAULT_LAUNCH_ARGS = ["--no-sandbox", "--disable-dev-shm-usage"]

def build_launch_args(proxy_port: Optional[int]) -> list[str]:
    """Chromium launch args. When force-routed (a shim port is given), route all
    traffic through the in-jail proxy at 127.0.0.1:<port> and remove Chromium's
    implicit loopback bypass so even loopback destinations go through the proxy
    (and are allowlist-checked by the sidecar). Without a port: the dev direct
    path, byte-identical to before."""
    args = list(DEFAULT_LAUNCH_ARGS)
    if proxy_port is not None:
        args.append(f"--proxy-server=127.0.0.1:{proxy_port}")
        args.append("--proxy-bypass-list=<-loopback>")
    return args


# Default ports per scheme, for the subresource allowlist check.
_DEFAULT_PORTS = {"https": 443, "http": 80}


def _truncate_utf8(s: str, max_bytes: int) -> str:
    """Truncate `s` so its UTF-8 encoding is <= max_bytes, never splitting a char."""
    encoded = s.encode("utf-8")
    if len(encoded) <= max_bytes:
        return s
    # Decode the byte-prefix, dropping any partial trailing char.
    return encoded[:max_bytes].decode("utf-8", errors="ignore")


def _text_from_html(html: str) -> str:
    """Best-effort readable text: readability summary, falling back to raw text."""
    try:
        summary_html = Document(html).summary(html_partial=True)
        if summary_html.strip():
            return lxml_html.fromstring(summary_html).text_content()
    except Exception:
        # Readability can throw on degenerate DOMs; fall through to raw extraction.
        pass
    try:
        return lxml_html.fromstring(html).text_content()
    except Exception:
        return ""


def extract_render_result(*, html: str, final_url: str, status: int, title: str) -> dict[str, Any]:
    """Build the `browser.render` result from post-JS HTML + navigation metadata."""
    text = " ".join(_text_from_html(html).split())[:MAX_TEXT_CHARS]
    return {
        "final_url": final_url,
        "status": status,
        "title": title,
        "text": text,
        "html": _truncate_utf8(html, MAX_HTML_BYTES),
    }


def host_port_from_url(url: str) -> Optional[tuple[str, int]]:
    """Extract (host, port) from a URL, defaulting the port by scheme.

    Returns None when there's no host or no resolvable port — the caller treats
    that as not-allowed (fail-closed).
    """
    parsed = urlparse(url)
    host = parsed.hostname
    if not host:
        return None
    scheme = (parsed.scheme or "").lower()
    try:
        port = parsed.port
    except ValueError:
        # Malformed port in the URL → fail closed.
        return None
    if port is None:
        port = _DEFAULT_PORTS.get(scheme)
    if port is None:
        return None
    return (host, port)


def request_is_allowed(url: str, allowlist: HostAllowlist) -> bool:
    """True iff `url`'s host:port is permitted by `allowlist` (fail-closed)."""
    hp = host_port_from_url(url)
    if hp is None:
        return False
    return allowlist.is_allowed_endpoint(*hp)


class RenderNotAllowed(Exception):
    """The navigation landed on a URL outside the operator allowlist.

    Raised when the *final* (post-redirect) URL is off-allowlist, even if the
    initial URL was allowed. The server maps it to `RENDER_FAILED` (fail-closed)
    — no off-allowlist content is ever returned.
    """

    def __init__(self, final_url: str):
        super().__init__(f"final navigation URL not on allowlist: {final_url}")
        self.final_url = final_url


class PlaywrightRenderer:
    """Drives a headless Chromium to render one page and extract its content.

    Per request: launch chromium → intercept every request, aborting any whose
    host:port is off the operator allowlist (the main navigation host must be
    allowed or the render fails closed) → `goto` → settle → `content()` →
    `extract_render_result`.

    `playwright_factory` is a seam: a zero-arg callable returning a Playwright
    "context manager" object with a `.start()` method (yielding a
    Playwright-like object with `.chromium.launch(...)`); the started object
    exposes `.stop()`. It defaults to the real `sync_playwright()`; tests inject
    a fake so the orchestration is exercised without a browser binary.

    We use explicit `.start()`/`.stop()` rather than `with sync_playwright() as
    pw:` on purpose: the `with` form, when a later call (e.g. `chromium.launch`)
    fails, raises an unrelated `AttributeError` in `__exit__` that **masks the
    real error**. Explicit start/stop lets the genuine failure propagate to the
    `RENDER_FAILED` message.
    """

    def __init__(
        self,
        allowlist: HostAllowlist,
        launch_args: Optional[list[str]] = None,
        playwright_factory: Optional[Callable[[], Any]] = None,
    ):
        self._allowlist = allowlist
        self._launch_args = list(launch_args) if launch_args is not None else list(DEFAULT_LAUNCH_ARGS)
        self._playwright_factory = playwright_factory or _default_playwright_factory

    def render(self, *, url: str, timeout_ms: int, wait_until: str) -> dict[str, Any]:
        pw = self._playwright_factory().start()
        try:
            browser = pw.chromium.launch(headless=True, args=self._launch_args)
            try:
                page = browser.new_page()
                page.route("**/*", self._route_handler)
                response = page.goto(url, wait_until=wait_until, timeout=timeout_ms)
                status = response.status if response is not None else 0
                final_url = page.url
                # Defense in depth: the per-request route handler aborts
                # off-allowlist hops, but Playwright's interception of *redirect*
                # hops has varied across versions. So independently verify the
                # final landing URL is on the allowlist before reading any
                # content — a redirect chain that ends off-allowlist fails closed
                # rather than returning off-allowlist content. (issue #263)
                if not request_is_allowed(final_url, self._allowlist):
                    raise RenderNotAllowed(final_url)
                title = page.title()
                html = page.content()
            finally:
                browser.close()
        finally:
            pw.stop()
        return extract_render_result(
            html=html, final_url=final_url, status=status, title=title
        )

    def _route_handler(self, route: Any) -> None:
        """Abort off-allowlist requests; continue allowed ones (per-subresource)."""
        if request_is_allowed(route.request.url, self._allowlist):
            route.continue_()
        else:
            route.abort()


def _default_playwright_factory() -> Any:
    """The real Playwright context manager (lazy import — only when rendering)."""
    from playwright.sync_api import sync_playwright

    return sync_playwright()
