import json

from kastellan_worker_browser_driver.server import Server
from kastellan_worker_browser_driver.errors import (
    METHOD_NOT_FOUND,
    INVALID_INPUT,
    RENDER_FAILED,
    PARSE_ERROR,
)


def handle(renderer, frame):
    return Server(renderer=renderer)._handle_line(json.dumps(frame))


def test_happy_path_returns_result(fake_renderer):
    resp = handle(
        fake_renderer,
        {"jsonrpc": "2.0", "id": 1, "method": "browser.render", "params": {"url": "https://x.test/"}},
    )
    assert resp["result"]["text"] == "body text"
    # defaults applied
    assert fake_renderer.calls[0]["timeout_ms"] == 15000
    assert fake_renderer.calls[0]["wait_until"] == "networkidle"


def test_unknown_method(fake_renderer):
    resp = handle(fake_renderer, {"jsonrpc": "2.0", "id": 1, "method": "nope", "params": {}})
    assert resp["error"]["code"] == METHOD_NOT_FOUND


def test_parse_error_on_garbage(fake_renderer):
    resp = Server(renderer=fake_renderer)._handle_line("{not json")
    assert resp["error"]["code"] == PARSE_ERROR


def test_missing_url_rejected(fake_renderer):
    resp = handle(fake_renderer, {"jsonrpc": "2.0", "id": 1, "method": "browser.render", "params": {}})
    assert resp["error"]["code"] == INVALID_INPUT


def test_non_https_url_rejected(fake_renderer):
    resp = handle(
        fake_renderer,
        {"jsonrpc": "2.0", "id": 1, "method": "browser.render", "params": {"url": "ftp://x.test/"}},
    )
    assert resp["error"]["code"] == INVALID_INPUT


def test_http_loopback_allowed(fake_renderer):
    resp = handle(
        fake_renderer,
        {"jsonrpc": "2.0", "id": 1, "method": "browser.render", "params": {"url": "http://127.0.0.1:8080/"}},
    )
    assert "result" in resp


def test_http_remote_rejected(fake_renderer):
    resp = handle(
        fake_renderer,
        {"jsonrpc": "2.0", "id": 1, "method": "browser.render", "params": {"url": "http://example.com/"}},
    )
    assert resp["error"]["code"] == INVALID_INPUT


def test_timeout_clamped(fake_renderer):
    handle(
        fake_renderer,
        {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
         "params": {"url": "https://x.test/", "timeout_ms": 999999}},
    )
    assert fake_renderer.calls[0]["timeout_ms"] == 30000  # clamped to max


def test_invalid_wait_until_rejected(fake_renderer):
    resp = handle(
        fake_renderer,
        {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
         "params": {"url": "https://x.test/", "wait_until": "bogus"}},
    )
    assert resp["error"]["code"] == INVALID_INPUT


def test_render_failure_is_request_local(renderer_factory):
    r = renderer_factory(raise_exc=RuntimeError("nav timeout"))
    resp = handle(
        r,
        {"jsonrpc": "2.0", "id": 1, "method": "browser.render", "params": {"url": "https://x.test/"}},
    )
    assert resp["error"]["code"] == RENDER_FAILED
