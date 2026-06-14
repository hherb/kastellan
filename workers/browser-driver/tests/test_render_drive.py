"""Tests for the PlaywrightRenderer drive + the pure URL/allowlist helpers.

The real browser launch is exercised only by the `#[ignore] real_render` e2e;
here a fake Playwright stack drives the orchestration without a browser binary.
"""
from contextlib import contextmanager

import pytest

from kastellan_worker_browser_driver.allowlist import HostAllowlist
from kastellan_worker_browser_driver.render import (
    DEFAULT_LAUNCH_ARGS,
    PlaywrightRenderer,
    host_port_from_url,
    request_is_allowed,
)


# ---- pure helpers ----

def test_host_port_defaults_by_scheme():
    assert host_port_from_url("https://x.test/p") == ("x.test", 443)
    assert host_port_from_url("http://x.test/p") == ("x.test", 80)


def test_host_port_explicit_port():
    assert host_port_from_url("https://x.test:8443/p") == ("x.test", 8443)


def test_host_port_none_without_host():
    assert host_port_from_url("not-a-url") is None


def test_request_is_allowed_uses_host_and_port():
    a = HostAllowlist.from_endpoints(["x.test:443"])
    assert request_is_allowed("https://x.test/style.css", a)
    assert not request_is_allowed("https://evil.test/x.js", a)
    assert not request_is_allowed("https://x.test:8443/x.js", a)  # wrong port


# ---- route handler ----

class FakeRoute:
    def __init__(self, url):
        self.request = type("Req", (), {"url": url})()
        self.action = None

    def continue_(self):
        self.action = "continue"

    def abort(self):
        self.action = "abort"


def test_route_handler_continues_allowed_and_aborts_others():
    a = HostAllowlist.from_endpoints(["good.test"])
    r = PlaywrightRenderer(allowlist=a, playwright_factory=lambda: None)

    allowed = FakeRoute("https://good.test/app.js")
    r._route_handler(allowed)
    assert allowed.action == "continue"

    blocked = FakeRoute("https://cdn.evil.test/track.js")
    r._route_handler(blocked)
    assert blocked.action == "abort"


# ---- full render orchestration with a fake Playwright ----

class FakeResponse:
    def __init__(self, status):
        self.status = status


class FakePage:
    def __init__(self, html, title, final_url, status):
        self._html, self._title, self.url, self._status = html, title, final_url, status
        self.routed = []
        self.goto_args = None

    def route(self, pattern, handler):
        self.routed.append((pattern, handler))

    def goto(self, url, wait_until=None, timeout=None):
        self.goto_args = {"url": url, "wait_until": wait_until, "timeout": timeout}
        return FakeResponse(self._status)

    def title(self):
        return self._title

    def content(self):
        return self._html


class FakeBrowser:
    def __init__(self, page):
        self._page = page
        self.closed = False
        self.launch_args = None

    def new_page(self):
        return self._page

    def close(self):
        self.closed = True


class FakeChromium:
    def __init__(self, browser):
        self._browser = browser

    def launch(self, headless=None, args=None):
        self._browser.launch_args = {"headless": headless, "args": args}
        return self._browser


class FakePlaywright:
    def __init__(self, browser):
        self.chromium = FakeChromium(browser)


def make_factory(browser):
    @contextmanager
    def factory():
        yield FakePlaywright(browser)

    return factory


def test_render_drives_goto_and_extracts():
    page = FakePage(
        html="<html><head><title>Hi</title></head><body><article><p>Body here.</p></article></body></html>",
        title="Hi",
        final_url="https://x.test/final",
        status=200,
    )
    browser = FakeBrowser(page)
    r = PlaywrightRenderer(
        allowlist=HostAllowlist.from_endpoints(["x.test"]),
        playwright_factory=make_factory(browser),
    )

    out = r.render(url="https://x.test/", timeout_ms=12000, wait_until="networkidle")

    # Result shape from extract_render_result.
    assert out["final_url"] == "https://x.test/final"
    assert out["status"] == 200
    assert out["title"] == "Hi"
    assert "Body here." in out["text"]

    # goto was driven with the request's args.
    assert page.goto_args == {"url": "https://x.test/", "wait_until": "networkidle", "timeout": 12000}
    # A route interceptor was registered and the browser was closed.
    assert page.routed and page.routed[0][0] == "**/*"
    assert browser.closed
    assert browser.launch_args == {"headless": True, "args": DEFAULT_LAUNCH_ARGS}


def test_render_handles_missing_response_status():
    page = FakePage(html="<html></html>", title="", final_url="https://x.test/", status=200)
    # Override goto to return None (e.g. a same-document navigation).
    page.goto = lambda url, wait_until=None, timeout=None: None
    browser = FakeBrowser(page)
    r = PlaywrightRenderer(
        allowlist=HostAllowlist.from_endpoints(["x.test"]),
        playwright_factory=make_factory(browser),
    )
    out = r.render(url="https://x.test/", timeout_ms=1000, wait_until="load")
    assert out["status"] == 0  # no response → status 0, not a crash
