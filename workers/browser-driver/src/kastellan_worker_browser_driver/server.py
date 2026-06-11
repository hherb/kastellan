"""JSON-RPC 2.0 stdio loop + browser.render dispatch.

Single-threaded, synchronous. One JSON frame per stdin line, one response per
stdout line. EOF ends the loop. Malformed lines → PARSE_ERROR and continue. The
renderer is duck-typed (a `.render(url, timeout_ms, wait_until)` method);
`__main__` injects the Playwright drive, tests inject a fake.
"""
from typing import Any, IO
from urllib.parse import urlparse
import ipaddress
import json

from .errors import (
    error_response,
    success_response,
    PARSE_ERROR,
    METHOD_NOT_FOUND,
    INVALID_REQUEST,
    INVALID_PARAMS,
    INVALID_INPUT,
    RENDER_FAILED,
)

DEFAULT_TIMEOUT_MS = 15000
MIN_TIMEOUT_MS = 1000
MAX_TIMEOUT_MS = 30000
VALID_WAIT_UNTIL = {"load", "domcontentloaded", "networkidle"}


def _is_loopback(host: str) -> bool:
    """True for loopback IP literals or `localhost` (http is allowed only here)."""
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return host == "localhost"


class Server:
    def __init__(self, renderer: Any):
        self._renderer = renderer

    def run(self, stdin: IO[str], stdout: IO[str]) -> None:
        """Drive the stdio loop until stdin EOF."""
        for line in stdin:
            line = line.strip()
            if not line:
                continue
            stdout.write(json.dumps(self._handle_line(line)) + "\n")
            stdout.flush()

    def _handle_line(self, line: str) -> dict:
        try:
            frame = json.loads(line)
        except json.JSONDecodeError as e:
            return error_response(req_id=None, code=PARSE_ERROR, message=f"parse failed: {e}")
        if not isinstance(frame, dict):
            return error_response(req_id=None, code=INVALID_REQUEST, message="frame is not an object")

        req_id = frame.get("id")
        method = frame.get("method")
        params = frame.get("params", {})
        if frame.get("jsonrpc") != "2.0" or method is None:
            return error_response(req_id=req_id, code=INVALID_REQUEST, message="missing jsonrpc/method")
        if method != "browser.render":
            return error_response(req_id=req_id, code=METHOD_NOT_FOUND, message=f"unknown method: {method}")
        if not isinstance(params, dict):
            return error_response(req_id=req_id, code=INVALID_PARAMS, message="params must be an object")

        # url: required, https-only (http allowed only for loopback, mirrors web-search).
        url = params.get("url")
        if not isinstance(url, str) or url == "":
            return error_response(req_id=req_id, code=INVALID_INPUT, message="url missing or empty")
        parsed = urlparse(url)
        if parsed.scheme == "https":
            pass
        elif parsed.scheme == "http" and _is_loopback(parsed.hostname or ""):
            pass
        else:
            return error_response(
                req_id=req_id,
                code=INVALID_INPUT,
                message="url must be https (http allowed only for loopback)",
            )

        # timeout_ms: optional int, clamped. Reject bool (a bool is an int subclass).
        timeout_ms = params.get("timeout_ms", DEFAULT_TIMEOUT_MS)
        if not isinstance(timeout_ms, int) or isinstance(timeout_ms, bool):
            return error_response(req_id=req_id, code=INVALID_INPUT, message="timeout_ms must be an integer")
        timeout_ms = max(MIN_TIMEOUT_MS, min(MAX_TIMEOUT_MS, timeout_ms))

        wait_until = params.get("wait_until", "networkidle")
        if wait_until not in VALID_WAIT_UNTIL:
            return error_response(
                req_id=req_id,
                code=INVALID_INPUT,
                message=f"wait_until must be one of {sorted(VALID_WAIT_UNTIL)}",
            )

        try:
            result = self._renderer.render(url=url, timeout_ms=timeout_ms, wait_until=wait_until)
        except Exception as e:
            return error_response(req_id=req_id, code=RENDER_FAILED, message=f"render failed: {e}")
        return success_response(req_id=req_id, result=result)
