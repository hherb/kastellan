# browser-driver worker (slice #1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the spike-gated scaffold of a read-only `browser.render(url)` net worker — a Playwright-Python worker (GLiNER-shaped) that returns post-JS readable text + final HTML — proving first that a headless browser survives the real OS jail, then building every spike-*independent* part TDD against a fake browser.

**Architecture:** Two phases. **Phase 0** is a throwaway feasibility spike (the gate): get a headless browser to render one page inside the real sandbox on Mac (Seatbelt) + DGX (bwrap), recording the required launch flags / seccomp / `fs_read` set into the spec. **Phase 1** builds the spike-independent scaffold TDD: the Python package (stdio JSON-RPC loop, pure readability extractor, wire validation) with the real Playwright launch isolated behind a duck-typed seam so a fake browser drives the tests; plus the Rust host manifest (`resolve_env` + `ToolEntry` + registry wiring) and the injection-guard flip. The real browser launch, the prelude seccomp/Landlock additions, and the real-sandbox e2e are explicitly **deferred to a Phase-2 plan written from the spike findings** — their code can't be written correctly until the spike runs.

**Tech Stack:** Python 3.11+ / uv / Playwright (Apache-2.0) / readability-lxml (Apache-2.0); Rust (`kastellan-core` host manifest, `kastellan-sandbox`, `web-common::HostAllowlist`). Spec: `docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md`.

---

## File structure

**Phase 0 (throwaway):**
- `scripts/spikes/browser-driver/probe.py` — minimal Playwright render probe.
- `scripts/spikes/browser-driver/run.sh` — launch helpers (unsandboxed baseline, then under each backend).

**Phase 1 (production scaffold):**
- `workers/browser-driver/pyproject.toml` — uv package manifest.
- `workers/browser-driver/README.md` — install/enable note.
- `workers/browser-driver/src/kastellan_worker_browser_driver/__init__.py`
- `workers/browser-driver/src/kastellan_worker_browser_driver/errors.py` — JSON-RPC codes + envelopes.
- `workers/browser-driver/src/kastellan_worker_browser_driver/render.py` — pure `extract_render_result` + (Phase-2) the Playwright drive.
- `workers/browser-driver/src/kastellan_worker_browser_driver/server.py` — stdio JSON-RPC loop + `browser.render` dispatch/validation.
- `workers/browser-driver/src/kastellan_worker_browser_driver/__main__.py` — env/startup → `Server.run`.
- `workers/browser-driver/tests/{__init__,conftest,test_render_extract,test_server}.py`
- `core/src/workers/browser_driver.rs` — host manifest (`resolve_env`, `BrowserDriverEnv`, `ResolveSkipReason`, `browser_driver_entry`, `BrowserDriverManifest`).
- `core/src/workers/mod.rs` — add `pub mod browser_driver;`.
- `core/src/registry_build.rs:20-24` — add `&crate::workers::browser_driver::BrowserDriverManifest` to `WORKER_MANIFESTS`.
- `core/src/cassandra/injection_guard.rs:135-139` — add `browser-driver` to the `Relaxed` arm.
- `core/src/cassandra/injection_guard/tests.rs:426` — update the assertion Strict→Relaxed.

---

## Phase 0 — Feasibility spike (THE GATE — exploratory, not TDD)

> This phase produces **empirical findings**, not tested code. The artifacts are throwaway. The output is a written findings block + a go/no-go decision. Do **not** start Phase 1 until the spike is green (or the driver-stack decision is re-opened).

### Task 0.1: Write the render probe

**Files:**
- Create: `scripts/spikes/browser-driver/probe.py`
- Create: `scripts/spikes/browser-driver/run.sh`

- [ ] **Step 1: Write the probe**

```python
# scripts/spikes/browser-driver/probe.py
"""Throwaway browser-in-jail feasibility probe (spike — delete after slice #1).

Launches headless Chromium via Playwright, renders the URL given as argv[1]
(default: a bundled file:// page), prints the post-JS <title> + a text snippet
to stdout, and exits 0 on success. Any launch/render failure prints the
exception to stderr and exits non-zero. Run it (a) unsandboxed as a baseline,
then (b) under Seatbelt/bwrap to discover the jail adjustments.
"""
import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

# Launch flags under test — rely on OUR jail, not Chromium's own user-ns sandbox.
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
```

- [ ] **Step 2: Write a trivial fixture page**

```html
<!-- scripts/spikes/browser-driver/fixture.html -->
<!doctype html><html><head><title>spike</title></head>
<body><h1>hello</h1><script>document.body.innerHTML += '<p>js-ran</p>';</script></body></html>
```

- [ ] **Step 3: Write the runner**

```bash
# scripts/spikes/browser-driver/run.sh
#!/usr/bin/env bash
# Spike runner. Stage a venv + browser, then probe — first unsandboxed, then
# (manually) under the real backend. Throwaway.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
venv="${here}/.venv"
uv venv "${venv}"
# shellcheck disable=SC1091
source "${venv}/bin/activate"
uv pip install playwright readability-lxml
python -m playwright install chromium
echo "=== unsandboxed baseline ==="
python "${here}/probe.py"
```

- [ ] **Step 4: Commit the spike harness**

```bash
git add scripts/spikes/browser-driver/probe.py scripts/spikes/browser-driver/fixture.html scripts/spikes/browser-driver/run.sh
git commit -m "spike(browser-driver): render probe + runner (throwaway, slice #1 gate)"
```

### Task 0.2: Run on Mac under Seatbelt

- [ ] **Step 1: Baseline.** Run `bash scripts/spikes/browser-driver/run.sh`. Expected: `OK title='spike' snippet=...js-ran...`. If the baseline fails, fix the install before sandboxing.
- [ ] **Step 2: Under Seatbelt.** Wrap the probe in a `sandbox-exec` profile derived from `kastellan-sandbox`'s `MacosSeatbelt` (mirror `sandbox/tests/macos_smoke.rs` flags: deny-default, allow the venv + browser tree + fonts `fs_read`, deny outbound except as needed for `file://`). Iterate the profile until the probe renders. **Record** every `fs_read` path and entitlement that had to be added.
- [ ] **Step 3: Write findings** to a scratch note (`scripts/spikes/browser-driver/FINDINGS.md`): working launch args, the Seatbelt allowances, RAM high-water from Activity Monitor.

### Task 0.3: Run on DGX under bwrap (via `ssh dgx`)

- [ ] **Step 1: Baseline** on the DGX (aarch64): stage the venv + `python -m playwright install chromium`, run the unsandboxed probe. Note: a Chromium aarch64 headless-shell must exist; if Playwright lacks one, this is where the engine falls back to Firefox.
- [ ] **Step 2: Under bwrap + prelude.** Wrap the probe argv with `linux_bwrap::build_argv` flags (`--unshare-all`, `--die-with-parent`, bind the venv + browser tree + fonts `fs_read`) and run it under the worker `prelude::lock_down` seccomp+Landlock. Capture every `SIGSYS` (audit via `dmesg`/`strace -f` outside the seccomp, or `SCMP_ACT_LOG`) and add the syscall to a candidate allow-list. **Record** the seccomp-allow delta vs the existing `WorkerNetClient` profile, the Landlock RO paths, and whether `--single-process` was required.
- [ ] **Step 3: Append DGX findings** to `FINDINGS.md` (working flags, seccomp delta, fs_read set, RAM).

### Task 0.4: Decision gate + fold findings into the spec

- [ ] **Step 1:** Copy the consolidated `FINDINGS.md` content into a new `## 3.1 Spike findings` section of `docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md` (engine chosen; exact launch args; seccomp-allow delta; `fs_read`/Landlock set; `/dev/shm` decision; RAM → `mem_mb`; container-backend needed yes/no).
- [ ] **Step 2: Go/no-go.** GREEN (page text out of the jail on Mac AND DGX) → proceed to Phase 1. RED (cannot contain the browser with acceptable flags) → STOP, re-open the driver-stack decision (Rust-CDP / container backend) with the operator before any worker code.
- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md
git commit -m "spike(browser-driver): record findings + go/no-go in spec §3.1"
```

---

## Phase 1 — Spike-independent scaffold (TDD)

> Everything here is exercised by a **fake browser** and hermetic fixtures — no real browser launch — so it is correct regardless of the spike's flag/seccomp findings. The real `render.py` Playwright launch, the prelude profile additions, and the real-sandbox e2e land in the Phase-2 plan.

### Task 1.1: Python package skeleton

**Files:**
- Create: `workers/browser-driver/pyproject.toml`
- Create: `workers/browser-driver/src/kastellan_worker_browser_driver/__init__.py` (empty)
- Create: `workers/browser-driver/src/kastellan_worker_browser_driver/errors.py`
- Create: `workers/browser-driver/tests/__init__.py` (empty)

- [ ] **Step 1: Write `pyproject.toml`** (mirrors the GLiNER package; AGPL license, console-script shim)

```toml
[project]
name = "kastellan-worker-browser-driver"
version = "0.0.1"
description = "Headless-browser render worker for kastellan (Playwright; JSON-RPC stdio)"
readme = "README.md"
requires-python = ">=3.11"
license = { text = "AGPL-3.0-or-later" }
authors = [{ name = "kastellan contributors" }]
dependencies = [
    "playwright>=1.44",      # Apache-2.0; bundled browsers staged by install, not vendored
    "readability-lxml>=0.8", # Apache-2.0 readability extraction
    "lxml>=5",               # BSD
]

[project.optional-dependencies]
dev = [
    "pytest>=8",
    "pytest-mock>=3.12",
]

[project.scripts]
# uv generates .venv/bin/kastellan-worker-browser-driver == python -m kastellan_worker_browser_driver
kastellan-worker-browser-driver = "kastellan_worker_browser_driver.__main__:main"

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.hatch.build.targets.wheel]
packages = ["src/kastellan_worker_browser_driver"]

[tool.pytest.ini_options]
testpaths = ["tests"]
python_files = ["test_*.py"]
```

- [ ] **Step 2: Write `errors.py`** (byte-for-byte the GLiNER envelope helpers, browser-specific app codes)

```python
"""JSON-RPC 2.0 error envelope helpers + custom application codes.

The codes here are the wire contract between this Python worker and the Rust
caller (core::workers::browser_driver + core/tests/browser_driver_e2e.rs).
Changing a code requires updating both sides.
"""
from typing import Any, Optional, Union

PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

# Application-specific codes — see the spec wire-contract table.
INVALID_INPUT = -32001     # url missing/empty/non-https, timeout/wait_until OOR
RENDER_FAILED = -32003     # request-local; navigation/render error, worker stays alive

JsonRpcId = Union[int, str, None]


def error_response(req_id: JsonRpcId, code: int, message: str, data: Optional[Any] = None) -> dict:
    """Build a JSON-RPC 2.0 error envelope. `data` omitted entirely when None."""
    err: dict[str, Any] = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    return {"jsonrpc": "2.0", "id": req_id, "error": err}


def success_response(req_id: JsonRpcId, result: Any) -> dict:
    """Build a JSON-RPC 2.0 success envelope."""
    return {"jsonrpc": "2.0", "id": req_id, "result": result}
```

- [ ] **Step 3: Commit**

```bash
git add workers/browser-driver/pyproject.toml workers/browser-driver/src/kastellan_worker_browser_driver/__init__.py workers/browser-driver/src/kastellan_worker_browser_driver/errors.py workers/browser-driver/tests/__init__.py
git commit -m "feat(browser-driver): python package skeleton + error envelopes"
```

### Task 1.2: Pure render extractor (`extract_render_result`)

**Files:**
- Create: `workers/browser-driver/src/kastellan_worker_browser_driver/render.py`
- Create: `workers/browser-driver/tests/test_render_extract.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_render_extract.py
from kastellan_worker_browser_driver.render import extract_render_result, MAX_HTML_BYTES, MAX_TEXT_CHARS


def test_extracts_title_text_and_html():
    html = "<html><head><title>Hi</title></head><body><article><p>Hello world body.</p></article></body></html>"
    out = extract_render_result(html=html, final_url="https://x.test/", status=200, title="Hi")
    assert out["final_url"] == "https://x.test/"
    assert out["status"] == 200
    assert out["title"] == "Hi"
    assert "Hello world body." in out["text"]
    assert out["html"].startswith("<")


def test_html_byte_cap_truncates_on_char_boundary():
    big = "<html><body>" + ("é" * (MAX_HTML_BYTES)) + "</body></html>"  # 2-byte chars
    out = extract_render_result(html=big, final_url="https://x.test/", status=200, title="t")
    assert len(out["html"].encode("utf-8")) <= MAX_HTML_BYTES
    # truncation must not split a multibyte char (no replacement/garbage)
    out["html"].encode("utf-8")  # round-trips cleanly


def test_text_char_cap():
    html = "<html><body><p>" + ("a" * (MAX_TEXT_CHARS + 50)) + "</p></body></html>"
    out = extract_render_result(html=html, final_url="https://x.test/", status=200, title="t")
    assert len(out["text"]) <= MAX_TEXT_CHARS
```

- [ ] **Step 2: Run it to verify failure**

Run: `cd workers/browser-driver && uv run pytest tests/test_render_extract.py -v`
Expected: FAIL — `ModuleNotFoundError`/`ImportError: cannot import name 'extract_render_result'`.

- [ ] **Step 3: Write the minimal implementation**

```python
# render.py
"""Render-result extraction (pure) + the Playwright drive (Phase 2).

`extract_render_result` is pure: given the post-JS HTML + navigation metadata
it produces the `browser.render` result dict (readability text + capped final
HTML). It is unit-tested without a browser. The actual Playwright launch
(`render`) is added in the Phase-2 plan behind the same result shape so the
seam stays testable with a fake.
"""
from typing import Any

from readability import Document
from lxml import html as lxml_html

# Wire-contract caps. Bumping any requires updating the Rust side + spec table.
MAX_HTML_BYTES = 5 * 1024 * 1024   # 5 MiB, mirrors web-fetch
MAX_TEXT_CHARS = 200_000           # post-readability text char cap


def _truncate_utf8(s: str, max_bytes: int) -> str:
    """Truncate `s` so its UTF-8 encoding is <= max_bytes, never splitting a char."""
    encoded = s.encode("utf-8")
    if len(encoded) <= max_bytes:
        return s
    # Back off to the last valid char boundary.
    return encoded[:max_bytes].decode("utf-8", errors="ignore")


def extract_render_result(*, html: str, final_url: str, status: int, title: str) -> dict[str, Any]:
    """Build the `browser.render` result from post-JS HTML + nav metadata."""
    try:
        doc = Document(html)
        summary_html = doc.summary(html_partial=True)
        text = lxml_html.fromstring(summary_html).text_content() if summary_html.strip() else ""
    except Exception:
        # Readability can throw on degenerate DOMs; fall back to raw text content.
        try:
            text = lxml_html.fromstring(html).text_content()
        except Exception:
            text = ""
    text = " ".join(text.split())[:MAX_TEXT_CHARS]
    return {
        "final_url": final_url,
        "status": status,
        "title": title,
        "text": text,
        "html": _truncate_utf8(html, MAX_HTML_BYTES),
    }
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cd workers/browser-driver && uv run pytest tests/test_render_extract.py -v`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add workers/browser-driver/src/kastellan_worker_browser_driver/render.py workers/browser-driver/tests/test_render_extract.py
git commit -m "feat(browser-driver): pure render-result extractor + caps (TDD)"
```

### Task 1.3: stdio server + `browser.render` dispatch/validation

**Files:**
- Create: `workers/browser-driver/src/kastellan_worker_browser_driver/server.py`
- Create: `workers/browser-driver/tests/conftest.py`
- Create: `workers/browser-driver/tests/test_server.py`

- [ ] **Step 1: Write the failing tests**

```python
# tests/conftest.py
import pytest


class FakeRenderer:
    """Duck-typed stand-in for the Playwright drive. `.render(...)` returns a
    canned result dict; set `.raise_exc` to simulate a navigation failure."""
    def __init__(self, result=None, raise_exc=None):
        self._result = result or {
            "final_url": "https://x.test/", "status": 200,
            "title": "T", "text": "body text", "html": "<html></html>",
        }
        self.raise_exc = raise_exc
        self.calls = []

    def render(self, *, url, timeout_ms, wait_until):
        self.calls.append({"url": url, "timeout_ms": timeout_ms, "wait_until": wait_until})
        if self.raise_exc:
            raise self.raise_exc
        return self._result


@pytest.fixture
def fake_renderer():
    return FakeRenderer()
```

```python
# tests/test_server.py
import json
from kastellan_worker_browser_driver.server import Server
from kastellan_worker_browser_driver.errors import (
    METHOD_NOT_FOUND, INVALID_INPUT, RENDER_FAILED, PARSE_ERROR,
)
from tests.conftest import FakeRenderer


def handle(renderer, frame):
    return Server(renderer=renderer)._handle_line(json.dumps(frame))


def test_happy_path_returns_result(fake_renderer):
    resp = handle(fake_renderer, {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
                                  "params": {"url": "https://x.test/"}})
    assert resp["result"]["text"] == "body text"
    # defaults applied
    assert fake_renderer.calls[0]["timeout_ms"] == 15000
    assert fake_renderer.calls[0]["wait_until"] == "networkidle"


def test_unknown_method():
    resp = handle(FakeRenderer(), {"jsonrpc": "2.0", "id": 1, "method": "nope", "params": {}})
    assert resp["error"]["code"] == METHOD_NOT_FOUND


def test_parse_error_on_garbage():
    resp = Server(renderer=FakeRenderer())._handle_line("{not json")
    assert resp["error"]["code"] == PARSE_ERROR


def test_missing_url_rejected():
    resp = handle(FakeRenderer(), {"jsonrpc": "2.0", "id": 1, "method": "browser.render", "params": {}})
    assert resp["error"]["code"] == INVALID_INPUT


def test_non_https_url_rejected():
    resp = handle(FakeRenderer(), {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
                                   "params": {"url": "ftp://x.test/"}})
    assert resp["error"]["code"] == INVALID_INPUT


def test_http_loopback_allowed():
    r = FakeRenderer()
    resp = handle(r, {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
                      "params": {"url": "http://127.0.0.1:8080/"}})
    assert "result" in resp


def test_timeout_clamped():
    r = FakeRenderer()
    handle(r, {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
               "params": {"url": "https://x.test/", "timeout_ms": 999999}})
    assert r.calls[0]["timeout_ms"] == 30000  # clamped to max


def test_invalid_wait_until_rejected():
    resp = handle(FakeRenderer(), {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
                                   "params": {"url": "https://x.test/", "wait_until": "bogus"}})
    assert resp["error"]["code"] == INVALID_INPUT


def test_render_failure_is_request_local():
    r = FakeRenderer(raise_exc=RuntimeError("nav timeout"))
    resp = handle(r, {"jsonrpc": "2.0", "id": 1, "method": "browser.render",
                      "params": {"url": "https://x.test/"}})
    assert resp["error"]["code"] == RENDER_FAILED
```

- [ ] **Step 2: Run to verify failure**

Run: `cd workers/browser-driver && uv run pytest tests/test_server.py -v`
Expected: FAIL — cannot import `Server`.

- [ ] **Step 3: Write the implementation**

```python
# server.py
"""JSON-RPC 2.0 stdio loop + browser.render dispatch.

Single-threaded, synchronous. One JSON frame per stdin line, one response per
stdout line. EOF ends the loop. Malformed lines → PARSE_ERROR and continue.
The renderer is duck-typed (a `.render(url, timeout_ms, wait_until)` method);
__main__ injects the Playwright drive, tests inject a fake.
"""
from typing import Any, IO
from urllib.parse import urlparse
import ipaddress
import json

from .errors import (
    error_response, success_response,
    PARSE_ERROR, METHOD_NOT_FOUND, INVALID_REQUEST, INVALID_PARAMS,
    INVALID_INPUT, RENDER_FAILED,
)

DEFAULT_TIMEOUT_MS = 15000
MIN_TIMEOUT_MS = 1000
MAX_TIMEOUT_MS = 30000
VALID_WAIT_UNTIL = {"load", "domcontentloaded", "networkidle"}


def _is_loopback(host: str) -> bool:
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return host == "localhost"


class Server:
    def __init__(self, renderer: Any):
        self._renderer = renderer

    def run(self, stdin: IO[str], stdout: IO[str]) -> None:
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

        # url: required, https-only (http allowed only for loopback, mirrors web-search)
        url = params.get("url")
        if not isinstance(url, str) or url == "":
            return error_response(req_id=req_id, code=INVALID_INPUT, message="url missing or empty")
        parsed = urlparse(url)
        if parsed.scheme == "https":
            pass
        elif parsed.scheme == "http" and _is_loopback(parsed.hostname or ""):
            pass
        else:
            return error_response(req_id=req_id, code=INVALID_INPUT,
                                  message="url must be https (http allowed only for loopback)")

        timeout_ms = params.get("timeout_ms", DEFAULT_TIMEOUT_MS)
        if not isinstance(timeout_ms, int) or isinstance(timeout_ms, bool):
            return error_response(req_id=req_id, code=INVALID_INPUT, message="timeout_ms must be an integer")
        timeout_ms = max(MIN_TIMEOUT_MS, min(MAX_TIMEOUT_MS, timeout_ms))

        wait_until = params.get("wait_until", "networkidle")
        if wait_until not in VALID_WAIT_UNTIL:
            return error_response(req_id=req_id, code=INVALID_INPUT,
                                  message=f"wait_until must be one of {sorted(VALID_WAIT_UNTIL)}")

        try:
            result = self._renderer.render(url=url, timeout_ms=timeout_ms, wait_until=wait_until)
        except Exception as e:
            return error_response(req_id=req_id, code=RENDER_FAILED, message=f"render failed: {e}")
        return success_response(req_id=req_id, result=result)
```

- [ ] **Step 4: Run to verify pass**

Run: `cd workers/browser-driver && uv run pytest tests/test_server.py -v`
Expected: PASS (9 tests).

- [ ] **Step 5: Commit**

```bash
git add workers/browser-driver/src/kastellan_worker_browser_driver/server.py workers/browser-driver/tests/conftest.py workers/browser-driver/tests/test_server.py
git commit -m "feat(browser-driver): stdio JSON-RPC server + browser.render validation (TDD)"
```

### Task 1.4: `__main__.py` startup shim (Phase-2 wires the real renderer)

**Files:**
- Create: `workers/browser-driver/src/kastellan_worker_browser_driver/__main__.py`
- Create: `workers/browser-driver/README.md`

- [ ] **Step 1: Write `__main__.py`** — env read + a `NotImplementedError` placeholder for the real renderer (Phase 2 replaces the import). Keep it import-safe so the package builds.

```python
"""Entry point for `kastellan-worker-browser-driver`.

Phase 1 wires the stdio server but the real Playwright renderer lands in the
Phase-2 plan (it depends on the spike's launch flags). Until then, starting the
worker raises a clear error rather than pretending to render.
"""
import sys

from .server import Server


def _build_renderer():
    # Phase 2: return a PlaywrightRenderer(launch_args=<spike flags>).
    raise NotImplementedError(
        "browser-driver real renderer lands in the Phase-2 plan (spike-gated)"
    )


def main() -> None:
    renderer = _build_renderer()
    Server(renderer=renderer).run(sys.stdin, sys.stdout)


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Write `README.md`** (one paragraph: opt-in via `KASTELLAN_BROWSER_DRIVER_ENABLE=1`, stage the venv + `playwright install`, real render is Phase-2). 

- [ ] **Step 3: Verify the package imports + full Python suite passes**

Run: `cd workers/browser-driver && uv run pytest -v`
Expected: PASS (12 tests: 3 extract + 9 server).

- [ ] **Step 4: Commit**

```bash
git add workers/browser-driver/src/kastellan_worker_browser_driver/__main__.py workers/browser-driver/README.md
git commit -m "feat(browser-driver): startup shim (real renderer deferred to phase 2) + README"
```

### Task 1.5: Rust host manifest — `resolve_env` + skip reasons

**Files:**
- Create: `core/src/workers/browser_driver.rs`
- Modify: `core/src/workers/mod.rs` (add `pub mod browser_driver;`)

- [ ] **Step 1: Write the failing test** (append a `#[cfg(test)] mod tests` to `browser_driver.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn disabled_when_enable_not_set() {
        let env = |_k: &str| None;
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(matches!(resolve_env(env, is_dir, exists), Err(ResolveSkipReason::Disabled)));
    }

    #[test]
    fn shim_missing_surfaces_path() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| false; // shim absent
        match resolve_env(env, is_dir, exists) {
            Err(ResolveSkipReason::ScriptShimMissing { path }) => {
                assert!(path.ends_with("kastellan-worker-browser-driver"));
            }
            other => panic!("expected ScriptShimMissing, got {other:?}"),
        }
    }

    #[test]
    fn resolves_when_enabled_and_shim_present() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        let out = resolve_env(env, is_dir, exists).expect("resolves");
        assert_eq!(out.venv_dir, PathBuf::from("/v"));
        assert!(out.script_path.ends_with("kastellan-worker-browser-driver"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core browser_driver::tests 2>&1 | tail -20`
Expected: FAIL — module/`resolve_env` not found.

- [ ] **Step 3: Write the implementation** (top of `browser_driver.rs` — mirror GLiNER's `resolve_env` venv cascade, drop weights/device)

```rust
//! Host-side manifest for the browser-driver worker (slice #1).
//!
//! A Playwright-Python worker (opt-in via `KASTELLAN_BROWSER_DRIVER_ENABLE=1`)
//! exposing `browser.render`. `resolve_env` is the pure core (env + fs probes →
//! `BrowserDriverEnv` | `ResolveSkipReason`); `browser_driver_entry` builds the
//! `ToolEntry`. Slice #1 runs on the legacy direct-net `Net::Allowlist` path
//! (no `proxy_uds`); egress-proxy force-routing is slice #2. The real browser
//! launch lives in the Python worker; the prelude seccomp/Landlock additions +
//! real-sandbox e2e land in the Phase-2 plan (spike-gated).

use std::path::{Path, PathBuf};

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{Resolution, ResolveCtx, WorkerManifest};

const TOOL_NAME: &str = "browser-driver";
const SHIM_NAME: &str = "kastellan-worker-browser-driver";

/// Resolved config for the browser-driver worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserDriverEnv {
    /// Absolute path to the uv console-script shim the dispatcher spawns.
    pub script_path: PathBuf,
    /// Worker venv root, mounted read-only into the jail.
    pub venv_dir: PathBuf,
}

/// Reason the resolver returned no entry (mirrors GLiNER's skip taxonomy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveSkipReason {
    Disabled,
    VenvDirUnresolvable,
    ScriptShimMissing { path: PathBuf },
}

/// Pure resolver: ENABLE gate + venv-anchor cascade + shim existence.
pub fn resolve_env<E, D, X>(env_lookup: E, _is_dir: D, exists: X) -> Result<BrowserDriverEnv, ResolveSkipReason>
where
    E: Fn(&str) -> Option<String>,
    D: Fn(&Path) -> bool,
    X: Fn(&Path) -> bool,
{
    if env_lookup("KASTELLAN_BROWSER_DRIVER_ENABLE").unwrap_or_default().trim() != "1" {
        return Err(ResolveSkipReason::Disabled);
    }
    let venv_dir = if let Some(v) = env_lookup("KASTELLAN_BROWSER_DRIVER_VENV_DIR") {
        PathBuf::from(v)
    } else if let Some(d) = env_lookup("KASTELLAN_DATA_DIR") {
        PathBuf::from(d).join("workers/browser-driver/.venv")
    } else if let Some(h) = env_lookup("HOME") {
        PathBuf::from(h).join(".local/share/kastellan/workers/browser-driver/.venv")
    } else {
        return Err(ResolveSkipReason::VenvDirUnresolvable);
    };
    let script_path = venv_dir.join("bin").join(SHIM_NAME);
    if !exists(&script_path) {
        return Err(ResolveSkipReason::ScriptShimMissing { path: script_path });
    }
    Ok(BrowserDriverEnv { script_path, venv_dir })
}
```

Add `pub mod browser_driver;` to `core/src/workers/mod.rs` (alphabetical: before `gliner_relex`).

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core browser_driver::tests 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/browser_driver.rs core/src/workers/mod.rs
git commit -m "feat(core/browser-driver): pure resolve_env + skip reasons (TDD)"
```

### Task 1.6: `browser_driver_entry` — `ToolEntry` builder

**Files:**
- Modify: `core/src/workers/browser_driver.rs`

- [ ] **Step 1: Write the failing test** (add to the `mod tests` block)

```rust
    #[test]
    fn entry_has_net_client_policy_and_operator_allowlist() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
        };
        let entry = browser_driver_entry(&env, &["example.com:443".to_string()]);
        assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
        // Slice #1: legacy direct-net path, no proxy_uds.
        assert!(entry.policy.proxy_uds.is_none());
        match &entry.policy.net {
            Net::Allowlist(hosts) => assert_eq!(hosts, &vec!["example.com:443".to_string()]),
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // venv mounted RO; resolver config present for in-jail DNS.
        assert!(entry.policy.fs_read.contains(&PathBuf::from("/v")));
        assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        // operator allowlist injected as env JSON.
        assert!(entry.policy.env.iter().any(|(k, v)|
            k == "KASTELLAN_BROWSER_DRIVER_ALLOWLIST" && v == r#"["example.com:443"]"#));
        assert!(matches!(entry.lifecycle, crate::worker_lifecycle::Lifecycle::SingleUse));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core browser_driver::tests::entry 2>&1 | tail -20`
Expected: FAIL — `browser_driver_entry` not found.

- [ ] **Step 3: Write the implementation** (add to `browser_driver.rs`)

> NOTE: `mem_mb` here is a placeholder 1024; the **spike's RAM finding (§3.1) sets the real value** in Phase 2. `fs_read` lists only the venv + resolver config now; the **browser-binary + fonts paths are spike-gated** and added in Phase 2.

```rust
/// Build the [`ToolEntry`] for the browser-driver worker (slice #1).
///
/// Slice #1 posture: `Net::Allowlist` on the **legacy direct-net path** (no
/// `proxy_uds` — egress-proxy force-routing is slice #2), `WorkerNetClient`
/// profile, `SingleUse` lifecycle. The operator allowlist is injected verbatim
/// as `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` JSON; the worker self-enforces it per
/// navigation + subresource. `mem_mb`/browser `fs_read` are finalized from the
/// spike findings in the Phase-2 plan.
pub fn browser_driver_entry(env: &BrowserDriverEnv, allowlist: &[String]) -> ToolEntry {
    let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![
            env.venv_dir.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(allowlist.to_vec()),
        cpu_ms: 30_000,
        mem_mb: 1024, // placeholder — spike §3.1 RAM finding sets this in Phase 2
        profile: Profile::WorkerNetClient,
        env: vec![
            ("KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(), allow_json),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None, // slice #1: legacy direct-net; force-routing is slice #2
    };
    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: Some(45_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core browser_driver::tests 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/browser_driver.rs
git commit -m "feat(core/browser-driver): ToolEntry builder, legacy direct-net Net::Allowlist (TDD)"
```

### Task 1.7: `BrowserDriverManifest` + register in the worker list

**Files:**
- Modify: `core/src/workers/browser_driver.rs`
- Modify: `core/src/registry_build.rs:20-24`

- [ ] **Step 1: Write the failing test** (add to `mod tests`)

```rust
    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx { get_env, exists, is_dir: &|_p| true, exe_dir: None, allowlist }
    }

    #[test]
    fn manifest_registers_when_enabled() {
        let get_env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["example.com:443".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        assert_eq!(BrowserDriverManifest.name(), "browser-driver");
        assert!(matches!(BrowserDriverManifest.resolve(&c), Resolution::Register(_)));
    }

    #[test]
    fn manifest_disabled_by_default() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);
        assert!(matches!(BrowserDriverManifest.resolve(&c), Resolution::Disabled { .. }));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core browser_driver::tests::manifest 2>&1 | tail -20`
Expected: FAIL — `BrowserDriverManifest` not found.

- [ ] **Step 3: Write the implementation** (add to `browser_driver.rs`)

```rust
/// browser-driver's host-side manifest. Reads its operator allowlist from the
/// `tool_allowlists` table (keyed `"browser-driver"`) and injects it into the
/// worker policy; maps the resolver's skip reasons onto `Resolution`.
pub struct BrowserDriverManifest;

impl WorkerManifest for BrowserDriverManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        match resolve_env(|k| (ctx.get_env)(k), |p| (ctx.is_dir)(p), |p| (ctx.exists)(p)) {
            Ok(env) => {
                let allowlist = (ctx.allowlist)(TOOL_NAME);
                Resolution::Register(browser_driver_entry(&env, &allowlist))
            }
            Err(ResolveSkipReason::Disabled) => Resolution::Disabled {
                detail: "KASTELLAN_BROWSER_DRIVER_ENABLE != \"1\"".to_string(),
            },
            Err(ResolveSkipReason::VenvDirUnresolvable) => Resolution::Misconfigured {
                detail: "venv dir unresolvable (KASTELLAN_BROWSER_DRIVER_VENV_DIR, \
                         KASTELLAN_DATA_DIR, and HOME all unset)".to_string(),
            },
            Err(ResolveSkipReason::ScriptShimMissing { path }) => Resolution::Misconfigured {
                detail: format!("venv shim missing: {}", path.display()),
            },
        }
    }
}
```

Add to `core/src/registry_build.rs` `WORKER_MANIFESTS` (after `gliner_relex`):

```rust
    &crate::workers::browser_driver::BrowserDriverManifest,
```

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core browser_driver 2>&1 | tail -20`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/browser_driver.rs core/src/registry_build.rs
git commit -m "feat(core/browser-driver): WorkerManifest + register in WORKER_MANIFESTS (TDD)"
```

### Task 1.8: Flip browser-driver to `GuardProfile::Relaxed`

**Files:**
- Modify: `core/src/cassandra/injection_guard.rs:135-139`
- Modify: `core/src/cassandra/injection_guard/tests.rs:426`

- [ ] **Step 1: Update the failing assertion first** (`injection_guard/tests.rs:426`)

Change:
```rust
    assert_eq!(GuardProfile::for_tool("browser-driver"), GuardProfile::Strict);
```
to:
```rust
    assert_eq!(GuardProfile::for_tool("browser-driver"), GuardProfile::Relaxed);
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core injection_guard 2>&1 | tail -20`
Expected: FAIL — `for_tool("browser-driver")` still returns Strict.

- [ ] **Step 3: Implement** — add `browser-driver` to the Relaxed arm in `injection_guard.rs`:

```rust
            "web-fetch" | "web-search" | "browser-driver" => GuardProfile::Relaxed,
```

Update the doc comment near line 124 to note browser-driver has joined.

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core injection_guard 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/cassandra/injection_guard.rs core/src/cassandra/injection_guard/tests.rs
git commit -m "feat(core/browser-driver): join GuardProfile::Relaxed (rendered pages carry chat-template tokens)"
```

### Task 1.9: Full-workspace green + clippy

- [ ] **Step 1: Build + test the workspace**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -25`
Expected: all green (macOS skip-as-pass posture; new browser-driver Rust tests pass, GLiNER-style real-model tests `[SKIP]`).

- [ ] **Step 2: Python suite**

Run: `cd workers/browser-driver && uv run pytest -v`
Expected: PASS (12 tests).

- [ ] **Step 3: Clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15`
Expected: clean.

- [ ] **Step 4: Commit (if clippy required any touch-ups)** — otherwise skip.

---

## Phase 2 — deferred (its own plan, written from the spike findings)

Do **not** attempt these here; they depend on §3.1 of the spec:
- `render.py` real Playwright drive using the spike's launch args; wire `__main__._build_renderer`.
- `workers/prelude` seccomp/Landlock additions (the recorded `SIGSYS` delta) + the browser `fs_read`/`/dev/shm` set + real `mem_mb`.
- `scripts/workers/browser-driver/install.sh` (stage venv + `playwright install`).
- `core/tests/browser_driver_e2e.rs` — hermetic deny-path + `#[ignore] real_render_of_loopback_page`, cross-platform (Seatbelt + bwrap).
- Then **slice #2**: egress-proxy integration (loopback-TCP↔UDS shim + in-browser per-instance-CA trust).

---

## Self-review notes

- **Spec coverage:** §3 spike → Phase 0; §4 package/wire → 1.1–1.4; §5 manifest → 1.5–1.7; §6 allowlist semantics → 1.6/1.7 (operator allowlist injected; per-subresource enforcement is in the Phase-2 `render.py`); §7 injection-guard → 1.8, registry → 1.7, handoff → automatic (no task); §8 testing → tests in each task + Phase-2 e2e; §9 out-of-scope respected (no egress/screenshot/interaction). §10 deps pinned in 1.1.
- **Spike-gated values** (`mem_mb`, browser `fs_read`, launch flags, seccomp) are explicitly marked placeholders set in Phase 2 — not silent TODOs.
- **Type consistency:** `resolve_env`/`BrowserDriverEnv`/`ResolveSkipReason`/`browser_driver_entry`/`BrowserDriverManifest`/`TOOL_NAME="browser-driver"`/`SHIM_NAME` used identically across 1.5–1.8; Python `Server(renderer=…)._handle_line`/`extract_render_result(html,final_url,status,title)` consistent across 1.2–1.4.
