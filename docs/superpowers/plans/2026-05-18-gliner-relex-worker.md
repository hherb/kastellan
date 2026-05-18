# GLiNER-Relex Worker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship hhagent's first idle_timeout consumer — a Python worker that runs Knowledgator's GLiNER-Relex (Apache 2.0; joint NER + relation extraction in one forward pass) under bwrap/Seatbelt, serving repeated `extract` requests across the same warm process.

**Architecture:** Two-slice delivery (split by language boundary). **Slice 1** ships the Python package at `workers/gliner-relex/` with a uv-managed venv, JSON-RPC stdio loop, model load, and Python unit + smoke tests — operator-runnable but no Rust caller. **Slice 2** adds the Rust manifest entry (`core::workers::gliner_relex::gliner_relex_entry -> ToolEntry`), wire-shape serde types, conditional daemon registration via `HHAGENT_GLINER_RELEX_ENABLE=1`, and an end-to-end integration test that spawns the real Python worker and verifies warm-reuse via worker-lifecycle slice-2's `_test_slot_has_warm` accessor. A typed Rust client wrapping the call is deferred to the v2 entity-extraction consumer slice — the dispatcher's `report_crash` chokepoint makes premature client design wasteful.

**Tech Stack:** Python 3.11+ (uv-managed venv per worker; uv lockfile committed), `gliner >= 0.2`, `transformers`, `sentencepiece`, `torch` (CUDA on Linux), Rust workspace (existing `core` + `hhagent-protocol`), JSON-RPC 2.0 line-delimited over stdio (matches the contract `hhagent-protocol` already speaks), `cargo test --workspace` is the regression gate, `pytest` runs inside the worker venv via `uv run pytest`.

**Spec:** `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`. **Companion specs:** `docs/superpowers/specs/2026-05-18-gliner-relex-feasibility-study.md` (license chain + capability), `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` (the `Lifecycle::IdleTimeout` runtime this worker consumes).

**Pre-requisite for execution:** the operator must have run `scripts/workers/gliner-relex/install.sh` on the target host before attempting any test that exercises the real model. Without weights, Slice 1's pytest tests still pass (they mock the model); Slice 2's integration tests skip-as-pass.

---

## File Structure

### Slice 1 — Python worker

```
workers/gliner-relex/
├── pyproject.toml                                            # uv project: deps, [project.scripts] shim
├── uv.lock                                                   # committed for reproducibility
├── .gitignore                                                # .venv/, __pycache__/, .pytest_cache/
├── README.md                                                 # operator install + smoke command
└── src/hhagent_worker_gliner_relex/
    ├── __init__.py                                           # package marker (empty)
    ├── __main__.py                                           # entry point: env parsing + model load + server.run()
    ├── errors.py                                             # custom JSON-RPC codes + envelope helpers
    ├── server.py                                             # stdio JSON-RPC framing + dispatch
    └── model.py                                              # GLiNER model load + extract() method
└── tests/
    ├── __init__.py                                           # (empty)
    ├── conftest.py                                           # mocked-model fixture
    ├── test_errors.py                                        # error envelope shape pins
    ├── test_server.py                                        # stdio dispatch + framing tests
    └── test_model.py                                         # extract() with mocked GLiNER object

scripts/workers/gliner-relex/install.sh                       # operator setup: uv sync + hf download
```

Plus a single root-level `.gitignore` change to add `workers/*/.venv/`.

### Slice 2 — Rust manifest + e2e

```
core/src/workers/
├── mod.rs                                                    # `pub mod gliner_relex;`
└── gliner_relex.rs                                           # GlinerRelexEnv, gliner_relex_entry, wire types, unit tests
core/tests/gliner_relex_e2e.rs                                # NEW integration test (skip-as-pass without venv/weights)
core/src/lib.rs                                               # MODIFIED: `pub mod workers;`
core/src/main.rs                                              # MODIFIED: conditional registration
docs/devel/handovers/HANDOVER.md                              # MODIFIED: session entry + Next TODO
docs/devel/ROADMAP.md                                         # MODIFIED: mark slice complete
```

**Responsibility split:**
- `workers/gliner-relex/` is a self-contained Python package; no Rust links into it.
- `core::workers::gliner_relex` owns the manifest entry, the wire-shape serde types, and the env builder. It does NOT own a client wrapper — that lands with the v2 consumer slice.
- `core::main` registers the entry conditionally; existing deployments without the env flag are byte-equivalent.

---

## SLICE 1 — Python worker (separate PR)

### Task 1.1: Scaffold `workers/gliner-relex/` directory with pyproject.toml + .gitignore

**Files:**
- Create: `workers/gliner-relex/pyproject.toml`
- Create: `workers/gliner-relex/.gitignore`
- Modify: `.gitignore` (workspace root, ensure `workers/*/.venv/` is ignored)

- [ ] **Step 1: Verify uv is available on the host**

```bash
uv --version
```

Expected: prints a version string `≥ 0.5.0`. If missing, install per https://docs.astral.sh/uv/getting-started/installation/. Stop if not available.

- [ ] **Step 2: Create the directory layout**

```bash
mkdir -p workers/gliner-relex/src/hhagent_worker_gliner_relex
mkdir -p workers/gliner-relex/tests
```

- [ ] **Step 3: Write `workers/gliner-relex/pyproject.toml`**

```toml
[project]
name = "hhagent-worker-gliner-relex"
version = "0.0.1"
description = "GLiNER-Relex inference worker for hhagent (Apache 2.0 model; JSON-RPC stdio)"
readme = "README.md"
requires-python = ">=3.11"
license = { text = "AGPL-3.0-or-later" }
authors = [{ name = "hhagent contributors" }]
dependencies = [
    # GLiNER pulls the upstream `gliner` library (Apache 2.0). The
    # Knowledgator gliner-relex-* model weights load via this same lib;
    # no separate inference framework needed.
    "gliner>=0.2",
    "transformers>=4.40",
    "sentencepiece>=0.2",
    # Torch is implicit via transformers, but pin it explicitly so an
    # accidental wheel-resolution surprise (CUDA version mismatch) is
    # caught at uv sync time rather than at model load.
    "torch>=2.2",
]

[project.optional-dependencies]
dev = [
    "pytest>=8",
    "pytest-mock>=3.12",
]

[project.scripts]
# uv generates an executable shim at .venv/bin/hhagent-worker-gliner-relex
# equivalent to: python -m hhagent_worker_gliner_relex
# The manifest's `binary: PathBuf` field points at this shim — keeps
# the existing ToolEntry schema unchanged (no `args` field needed).
hhagent-worker-gliner-relex = "hhagent_worker_gliner_relex.__main__:main"

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.hatch.build.targets.wheel]
packages = ["src/hhagent_worker_gliner_relex"]

[tool.pytest.ini_options]
testpaths = ["tests"]
python_files = ["test_*.py"]
```

- [ ] **Step 4: Write `workers/gliner-relex/.gitignore`**

```gitignore
.venv/
__pycache__/
*.pyc
.pytest_cache/
.ruff_cache/
*.egg-info/
build/
dist/
```

- [ ] **Step 5: Ensure workspace `.gitignore` covers `.venv` for all future workers**

Read the workspace root `.gitignore`. If it does not contain `workers/*/.venv/` (or equivalent), append it.

- [ ] **Step 6: Run `uv sync` to create the venv + lockfile**

```bash
cd workers/gliner-relex
uv sync --all-extras
```

Expected: creates `.venv/` and `uv.lock`. First run will download torch + transformers + gliner; allow ~3-5 minutes.

- [ ] **Step 7: Verify the console-script shim exists**

```bash
ls -l workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex
```

Expected: file exists, mode `-rwxr-xr-x`. (It is currently broken — the entry point doesn't exist yet — but the shim itself must be present after `uv sync` so we know Task 1.5's `main:main` reference resolves.)

- [ ] **Step 8: Commit**

```bash
git add workers/gliner-relex/pyproject.toml workers/gliner-relex/.gitignore workers/gliner-relex/uv.lock .gitignore
git commit -m "feat(workers/gliner-relex): scaffold uv project + .venv lockfile"
```

---

### Task 1.2: Write `errors.py` — JSON-RPC error envelope helpers

**Files:**
- Create: `workers/gliner-relex/src/hhagent_worker_gliner_relex/errors.py`
- Create: `workers/gliner-relex/src/hhagent_worker_gliner_relex/__init__.py` (empty marker)
- Create: `workers/gliner-relex/tests/__init__.py` (empty marker)
- Create: `workers/gliner-relex/tests/test_errors.py`

- [ ] **Step 1: Create the empty `__init__.py` files**

```bash
: > workers/gliner-relex/src/hhagent_worker_gliner_relex/__init__.py
: > workers/gliner-relex/tests/__init__.py
```

- [ ] **Step 2: Write the failing test `tests/test_errors.py`**

```python
"""Pin the JSON-RPC error envelope shape and the custom-code mapping.

The codes here are part of the wire contract — changing one requires a
corresponding update in the Rust-side mapping in
core::workers::gliner_relex (Slice 2). See the spec's "JSON-RPC wire
contract" section.
"""
from hhagent_worker_gliner_relex.errors import (
    error_response,
    INVALID_INPUT,
    MODEL_LOAD_FAILED,
    INFERENCE_FAILED,
    UNSUPPORTED_DEVICE,
)


def test_error_response_shape_matches_jsonrpc_2_0():
    env = error_response(req_id=42, code=INVALID_INPUT, message="text empty")
    assert env == {
        "jsonrpc": "2.0",
        "id": 42,
        "error": {
            "code": INVALID_INPUT,
            "message": "text empty",
        },
    }


def test_error_response_passes_data_field_when_provided():
    env = error_response(req_id=7, code=INFERENCE_FAILED, message="CUDA OOM", data={"layer": 12})
    assert env["error"]["data"] == {"layer": 12}


def test_error_response_omits_data_when_none():
    env = error_response(req_id=1, code=INVALID_INPUT, message="x")
    assert "data" not in env["error"]


def test_error_response_accepts_string_id():
    env = error_response(req_id="call-abc", code=INVALID_INPUT, message="x")
    assert env["id"] == "call-abc"


def test_error_response_accepts_null_id_for_parse_errors():
    # Per JSON-RPC 2.0: id may be null when the request couldn't be parsed
    # enough to know its id (PARSE_ERROR / INVALID_REQUEST).
    env = error_response(req_id=None, code=-32700, message="parse fail")
    assert env["id"] is None


def test_custom_codes_are_in_the_application_range():
    # JSON-RPC reserves -32768..-32000 for the protocol; application codes
    # should be elsewhere. Knowledgator workers use -32001..-32099.
    for code in (INVALID_INPUT, MODEL_LOAD_FAILED, INFERENCE_FAILED, UNSUPPORTED_DEVICE):
        assert -32099 <= code <= -32001 or code in (-32604,), f"{code} out of range"
```

- [ ] **Step 3: Run the test to confirm it fails**

```bash
cd workers/gliner-relex
uv run pytest tests/test_errors.py -v
```

Expected: 6 collection errors (`ImportError: cannot import name '...' from 'hhagent_worker_gliner_relex.errors'`).

- [ ] **Step 4: Write `src/hhagent_worker_gliner_relex/errors.py`**

```python
"""JSON-RPC 2.0 error envelope helpers + custom application codes.

The codes here are the wire contract between this Python worker and any
Rust caller (today: the future v2 entity-extraction consumer slice; the
slice-2 e2e test in core/tests/gliner_relex_e2e.rs also pins them).
Changing a code requires updating both sides.
"""
from typing import Any, Optional, Union

# Standard JSON-RPC 2.0 codes (re-exported here for convenience; the
# server-side `dispatch` uses them directly).
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

# Application-specific codes — see the spec table for semantics.
INVALID_INPUT = -32001         # text empty, labels empty/over-cap, threshold OOR
MODEL_LOAD_FAILED = -32002     # startup-only; worker exits 1 after writing this
INFERENCE_FAILED = -32003      # request-local; worker stays alive
UNSUPPORTED_DEVICE = -32604    # startup-only; worker exits 2 (mismatched manifest)


# JSON-RPC id is `int | str | None` per spec.
JsonRpcId = Union[int, str, None]


def error_response(
    req_id: JsonRpcId,
    code: int,
    message: str,
    data: Optional[Any] = None,
) -> dict:
    """Build a JSON-RPC 2.0 error envelope.

    The `data` field is omitted entirely when None (per the spec it is
    optional). A passed-through dict, list, or string lands verbatim
    under `error.data`.
    """
    err: dict[str, Any] = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "error": err,
    }


def success_response(req_id: JsonRpcId, result: Any) -> dict:
    """Build a JSON-RPC 2.0 success envelope."""
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "result": result,
    }
```

- [ ] **Step 5: Run the test to confirm it passes**

```bash
uv run pytest tests/test_errors.py -v
```

Expected: 6 passed.

- [ ] **Step 6: Commit**

```bash
git add workers/gliner-relex/src workers/gliner-relex/tests
git commit -m "feat(workers/gliner-relex): JSON-RPC error envelope helpers + custom codes"
```

---

### Task 1.3: Write `server.py` — stdio JSON-RPC framing + dispatch

**Files:**
- Create: `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`
- Create: `workers/gliner-relex/tests/test_server.py`
- Create: `workers/gliner-relex/tests/conftest.py`

- [ ] **Step 1: Write `conftest.py` with a mocked-model fixture**

```python
"""Shared pytest fixtures.

The mocked-model fixture lets us exercise server.py + model.py contract
without loading 1.3 GB of weights. The real-model load is covered by
the manual smoke test (operator-runnable, not in CI) and by the Rust
side's slice-2 integration test (skip-as-pass without weights).
"""
from unittest.mock import MagicMock

import pytest


@pytest.fixture
def fake_model():
    """A minimal stand-in for the loaded GLiNER object.

    Returns canned (entities, triples) regardless of input — enough for
    server.py's dispatch path tests. test_model.py exercises the real
    GLiNER wrapper separately with its own MagicMock that returns
    fine-grained per-call values.
    """
    m = MagicMock(name="FakeGliNER")
    m.extract.return_value = {
        "entities": [
            {"text": "Smith", "label": "person", "start": 0, "end": 5, "score": 0.91},
        ],
        "triples": [
            {"subject": "Smith", "relation": "treats", "object": "asthma", "score": 0.77},
        ],
    }
    return m
```

- [ ] **Step 2: Write the failing test `tests/test_server.py`**

```python
"""Stdio JSON-RPC dispatch tests for server.py.

Server reads JSON-RPC frames from a readable, dispatches to the model,
writes responses to a writable. We use StringIO to exercise the loop in
pytest without spawning a subprocess.
"""
import io
import json

import pytest

from hhagent_worker_gliner_relex.server import Server
from hhagent_worker_gliner_relex.errors import (
    METHOD_NOT_FOUND,
    INVALID_INPUT,
    PARSE_ERROR,
)


def _drive(server: Server, stdin_text: str) -> list[dict]:
    """Drive the server's loop with canned stdin and return parsed
    response lines. The server exits on stdin EOF."""
    stdin = io.StringIO(stdin_text)
    stdout = io.StringIO()
    server.run(stdin, stdout)
    raw = stdout.getvalue().rstrip("\n")
    if not raw:
        return []
    return [json.loads(line) for line in raw.split("\n")]


def _request(req_id, method: str, params: dict) -> str:
    return json.dumps({
        "jsonrpc": "2.0", "id": req_id, "method": method, "params": params,
    }) + "\n"


def test_happy_path_extract_call_returns_canned_result(fake_model):
    server = Server(model=fake_model)
    req = _request(1, "extract", {
        "text": "Smith treats asthma.",
        "entity_labels": ["person", "disease"],
        "relation_labels": ["treats"],
    })
    responses = _drive(server, req)
    assert len(responses) == 1
    resp = responses[0]
    assert resp["jsonrpc"] == "2.0"
    assert resp["id"] == 1
    assert resp["result"]["entities"][0]["label"] == "person"
    assert resp["result"]["triples"][0]["relation"] == "treats"


def test_unknown_method_returns_method_not_found(fake_model):
    server = Server(model=fake_model)
    req = _request(2, "bogus", {})
    responses = _drive(server, req)
    assert len(responses) == 1
    assert responses[0]["error"]["code"] == METHOD_NOT_FOUND
    assert responses[0]["id"] == 2


def test_missing_text_returns_invalid_input(fake_model):
    server = Server(model=fake_model)
    req = _request(3, "extract", {"entity_labels": ["x"], "relation_labels": []})
    responses = _drive(server, req)
    assert responses[0]["error"]["code"] == INVALID_INPUT


def test_empty_text_returns_invalid_input(fake_model):
    server = Server(model=fake_model)
    req = _request(4, "extract", {
        "text": "", "entity_labels": ["x"], "relation_labels": []
    })
    assert _drive(server, req)[0]["error"]["code"] == INVALID_INPUT


def test_empty_entity_labels_returns_invalid_input(fake_model):
    server = Server(model=fake_model)
    req = _request(5, "extract", {
        "text": "ok", "entity_labels": [], "relation_labels": ["x"]
    })
    assert _drive(server, req)[0]["error"]["code"] == INVALID_INPUT


def test_empty_relation_labels_is_valid_entity_only_mode(fake_model):
    # Empty relation_labels means "skip RE pass, return entities only".
    # Caller's responsibility to handle the empty triples array.
    server = Server(model=fake_model)
    req = _request(6, "extract", {
        "text": "Smith treats asthma.",
        "entity_labels": ["person", "disease"],
        "relation_labels": [],
    })
    responses = _drive(server, req)
    assert "result" in responses[0]
    # fake_model returns canned triples; we just verify the request
    # was accepted. test_model.py covers the entity-only model behaviour.


def test_malformed_json_returns_parse_error_and_continues(fake_model):
    # PARSE_ERROR must not kill the worker — the dispatcher's slice-2
    # classifier maps Decode errors to "dead", but a Python-side parse
    # error of one frame must not exit the process; only structured
    # exits do (startup MODEL_LOAD_FAILED / UNSUPPORTED_DEVICE).
    server = Server(model=fake_model)
    bad = "this is not json\n"
    good = _request(7, "extract", {
        "text": "ok", "entity_labels": ["x"], "relation_labels": [],
    })
    responses = _drive(server, bad + good)
    # First response is a parse error with id=None (per JSON-RPC 2.0).
    assert responses[0]["error"]["code"] == PARSE_ERROR
    assert responses[0]["id"] is None
    # Second response is the canned success for the good frame.
    assert responses[1]["id"] == 7
    assert "result" in responses[1]


def test_label_cap_64_rejected(fake_model):
    server = Server(model=fake_model)
    big_labels = [f"label_{i}" for i in range(65)]
    req = _request(8, "extract", {
        "text": "x", "entity_labels": big_labels, "relation_labels": [],
    })
    assert _drive(server, req)[0]["error"]["code"] == INVALID_INPUT


def test_text_over_8192_bytes_rejected(fake_model):
    server = Server(model=fake_model)
    big_text = "a" * 8193
    req = _request(9, "extract", {
        "text": big_text, "entity_labels": ["x"], "relation_labels": [],
    })
    assert _drive(server, req)[0]["error"]["code"] == INVALID_INPUT
```

- [ ] **Step 3: Run the test to confirm it fails**

```bash
uv run pytest tests/test_server.py -v
```

Expected: collection error — `cannot import name 'Server' from 'hhagent_worker_gliner_relex.server'`.

- [ ] **Step 4: Write `src/hhagent_worker_gliner_relex/server.py`**

```python
"""JSON-RPC 2.0 stdio loop + extract dispatch.

The loop is single-threaded and synchronous. Each line on stdin is one
JSON-RPC frame. Each response is one line on stdout. EOF on stdin ends
the loop (cleanly; lifecycle eviction via SIGTERM also drops stdin).

The frame parser is tolerant: a malformed line emits a PARSE_ERROR and
continues. The dispatcher rejects unknown methods, missing params, and
out-of-bound inputs with INVALID_INPUT / METHOD_NOT_FOUND / etc.

Model-side failures (`INFERENCE_FAILED`) are caught here and surfaced
as a request-local error; the worker stays alive.

The startup-only errors (`MODEL_LOAD_FAILED`, `UNSUPPORTED_DEVICE`)
happen BEFORE this loop runs (in `__main__.main`); we never see them.
"""
from typing import Any, IO

import json

from .errors import (
    error_response,
    success_response,
    PARSE_ERROR,
    METHOD_NOT_FOUND,
    INVALID_REQUEST,
    INVALID_PARAMS,
    INVALID_INPUT,
    INFERENCE_FAILED,
)

# Wire-contract limits. Bumping any requires updating the Rust side
# (core::workers::gliner_relex serde validators) AND the spec table.
MAX_TEXT_BYTES = 8192
MAX_ENTITY_LABELS = 64
MAX_RELATION_LABELS = 64


class Server:
    """Owns the dispatch table; holds a reference to the loaded model.

    The model interface is duck-typed (a `.extract(...)` method returning
    `{"entities": [...], "triples": [...]}`). model.py provides the
    production implementation; tests inject a MagicMock.
    """

    def __init__(self, model: Any):
        self._model = model

    def run(self, stdin: IO[str], stdout: IO[str]) -> None:
        """Drive the stdio loop until stdin EOF."""
        for line in stdin:
            line = line.strip()
            if not line:
                continue
            response = self._handle_line(line)
            stdout.write(json.dumps(response) + "\n")
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

        if method != "extract":
            return error_response(req_id=req_id, code=METHOD_NOT_FOUND, message=f"unknown method: {method}")

        if not isinstance(params, dict):
            return error_response(req_id=req_id, code=INVALID_PARAMS, message="params must be an object")

        # Validate required fields + sizes — all rejections share INVALID_INPUT
        # so the Rust client can branch on one code.
        text = params.get("text")
        if not isinstance(text, str) or text == "":
            return error_response(req_id=req_id, code=INVALID_INPUT, message="text missing or empty")
        if len(text.encode("utf-8")) > MAX_TEXT_BYTES:
            return error_response(req_id=req_id, code=INVALID_INPUT, message=f"text > {MAX_TEXT_BYTES} bytes")

        entity_labels = params.get("entity_labels")
        if not isinstance(entity_labels, list) or len(entity_labels) == 0:
            return error_response(req_id=req_id, code=INVALID_INPUT, message="entity_labels must be a non-empty array")
        if len(entity_labels) > MAX_ENTITY_LABELS:
            return error_response(req_id=req_id, code=INVALID_INPUT, message=f"entity_labels > {MAX_ENTITY_LABELS}")

        relation_labels = params.get("relation_labels")
        if not isinstance(relation_labels, list):
            return error_response(req_id=req_id, code=INVALID_INPUT, message="relation_labels must be an array (may be empty)")
        if len(relation_labels) > MAX_RELATION_LABELS:
            return error_response(req_id=req_id, code=INVALID_INPUT, message=f"relation_labels > {MAX_RELATION_LABELS}")

        threshold = params.get("threshold", 0.5)
        if not isinstance(threshold, (int, float)) or not (0.0 <= float(threshold) <= 1.0):
            return error_response(req_id=req_id, code=INVALID_INPUT, message="threshold must be in [0, 1]")

        max_entities = params.get("max_entities", 64)
        if not isinstance(max_entities, int) or max_entities < 1:
            return error_response(req_id=req_id, code=INVALID_INPUT, message="max_entities must be a positive integer")

        try:
            result = self._model.extract(
                text=text,
                entity_labels=entity_labels,
                relation_labels=relation_labels,
                threshold=float(threshold),
                max_entities=int(max_entities),
            )
        except Exception as e:
            return error_response(req_id=req_id, code=INFERENCE_FAILED, message=f"inference failed: {e}")

        return success_response(req_id=req_id, result=result)
```

- [ ] **Step 5: Run the test to confirm it passes**

```bash
uv run pytest tests/test_server.py -v
```

Expected: 9 passed.

- [ ] **Step 6: Commit**

```bash
git add workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py workers/gliner-relex/tests/test_server.py workers/gliner-relex/tests/conftest.py
git commit -m "feat(workers/gliner-relex): stdio JSON-RPC server + extract dispatch + validators"
```

---

### Task 1.4: Write `model.py` — GLiNER wrapper with mocked-load tests

**Files:**
- Create: `workers/gliner-relex/src/hhagent_worker_gliner_relex/model.py`
- Create: `workers/gliner-relex/tests/test_model.py`

- [ ] **Step 1: Write the failing test `tests/test_model.py`**

```python
"""GLiNER wrapper tests.

We mock `gliner.GLiNER` entirely — the real model load is a 1.3 GB
operation that doesn't belong in unit tests. The integration test on
the Rust side (slice-2 `gliner_relex_e2e.rs`) covers the real-model
round-trip; the manual smoke test in the README is the operator's
sanity check.
"""
from unittest.mock import patch, MagicMock

import pytest

from hhagent_worker_gliner_relex.model import GlinerModel


@pytest.fixture
def fake_gliner_class():
    """Patch `gliner.GLiNER.from_pretrained` so model load is instant.

    The returned MagicMock instance carries a `.predict_relations`
    method (matching the upstream gliner API); tests can configure it
    per case.
    """
    with patch("hhagent_worker_gliner_relex.model.GLiNER") as mock_cls:
        instance = MagicMock(name="GliNERInstance")
        mock_cls.from_pretrained.return_value = instance
        yield mock_cls, instance


def test_load_calls_gliner_from_pretrained_with_offline_kwargs(fake_gliner_class):
    mock_cls, _ = fake_gliner_class
    GlinerModel.load(weights_dir="/data/weights/multi-v1.0", model_id="knowledgator/gliner-relex-multi-v1.0", device="cuda")
    mock_cls.from_pretrained.assert_called_once()
    call_kwargs = mock_cls.from_pretrained.call_args.kwargs
    assert call_kwargs.get("local_files_only") is True


def test_load_passes_device_to_instance(fake_gliner_class):
    _, instance = fake_gliner_class
    GlinerModel.load(weights_dir="/data/weights/multi-v1.0", model_id="knowledgator/gliner-relex-multi-v1.0", device="cuda")
    # Model objects don't take `device=` in from_pretrained; we call
    # `.to(device)` afterwards. Verify that happened.
    instance.to.assert_called_once_with("cuda")


def test_extract_returns_envelope_shape(fake_gliner_class):
    _, instance = fake_gliner_class
    # GLiNER's predict_relations returns a list of entities + a list of
    # relations. The wrapper folds them into the envelope shape the
    # server.py consumer expects.
    instance.predict_relations.return_value = (
        [
            {"text": "Smith", "label": "person", "start": 0, "end": 5, "score": 0.91},
        ],
        [
            {"subject": "Smith", "relation": "treats", "object": "asthma", "score": 0.77},
        ],
    )
    model = GlinerModel.load(weights_dir="/x", model_id="y", device="cpu")
    result = model.extract(
        text="Smith treats asthma.",
        entity_labels=["person", "disease"],
        relation_labels=["treats"],
        threshold=0.5,
        max_entities=64,
    )
    assert result == {
        "entities": [{"text": "Smith", "label": "person", "start": 0, "end": 5, "score": 0.91}],
        "triples": [{"subject": "Smith", "relation": "treats", "object": "asthma", "score": 0.77}],
    }


def test_extract_truncates_entities_to_max(fake_gliner_class):
    _, instance = fake_gliner_class
    too_many = [
        {"text": f"e{i}", "label": "x", "start": 0, "end": 1, "score": 0.9}
        for i in range(10)
    ]
    instance.predict_relations.return_value = (too_many, [])
    model = GlinerModel.load(weights_dir="/x", model_id="y", device="cpu")
    result = model.extract(
        text="x" * 10,
        entity_labels=["x"],
        relation_labels=[],
        threshold=0.5,
        max_entities=3,
    )
    assert len(result["entities"]) == 3


def test_extract_filters_triples_to_surviving_entity_spans(fake_gliner_class):
    _, instance = fake_gliner_class
    instance.predict_relations.return_value = (
        [
            {"text": "alpha", "label": "x", "start": 0, "end": 5, "score": 0.9},
        ],
        [
            {"subject": "alpha", "relation": "r", "object": "alpha", "score": 0.8},
            # This triple's object ("beta") never appears as an entity —
            # filter it out.
            {"subject": "alpha", "relation": "r", "object": "beta", "score": 0.7},
        ],
    )
    model = GlinerModel.load(weights_dir="/x", model_id="y", device="cpu")
    result = model.extract(
        text="alpha", entity_labels=["x"], relation_labels=["r"],
        threshold=0.5, max_entities=64,
    )
    assert len(result["triples"]) == 1
    assert result["triples"][0]["object"] == "alpha"
```

- [ ] **Step 2: Run the test to confirm it fails**

```bash
uv run pytest tests/test_model.py -v
```

Expected: collection error — `cannot import name 'GlinerModel' from 'hhagent_worker_gliner_relex.model'`.

- [ ] **Step 3: Write `src/hhagent_worker_gliner_relex/model.py`**

```python
"""GLiNER-Relex model wrapper.

Loads the Knowledgator gliner-relex-* model from a pre-downloaded
on-disk weights directory (operator runs `install.sh` once; daemon
fails-closed when weights are missing). Exposes a single `.extract()`
method matching the server.py dispatch contract.

The wrapper folds GLiNER's `(entities, relations)` tuple into the
`{"entities": [...], "triples": [...]}` envelope shape the Rust caller
expects. It also enforces the `max_entities` cap and filters triples
whose subject or object isn't among the surviving entities.
"""
from typing import Any

from gliner import GLiNER  # pulled into sandbox via venv fs_read


class GlinerModel:
    """Thin wrapper around a loaded GLiNER instance.

    Construction goes through `.load()` rather than `__init__` so the
    error path (no weights / unsupported device) can produce structured
    errors without raising during attribute access.
    """

    def __init__(self, instance: Any) -> None:
        self._instance = instance

    @classmethod
    def load(cls, weights_dir: str, model_id: str, device: str) -> "GlinerModel":
        """Load the model from a pre-downloaded weights directory.

        `weights_dir` is the absolute path to the model snapshot (e.g.
        `/data/.../weights/multi-v1.0/`); the contents match what
        `hf download <model_id> --local-dir <weights_dir>` produces.

        `device` is one of `cuda`, `cpu` (Linux); `mps` is reserved for
        the macOS follow-up. The upstream GLiNER library doesn't take
        `device` in `from_pretrained`, so we call `.to(device)` after
        load. `auto` resolution happens in `__main__.py` (CUDA if
        available, else CPU).
        """
        instance = GLiNER.from_pretrained(
            weights_dir,
            local_files_only=True,
        )
        instance.to(device)
        return cls(instance)

    def extract(
        self,
        text: str,
        entity_labels: list[str],
        relation_labels: list[str],
        threshold: float,
        max_entities: int,
    ) -> dict:
        """Run joint NER + RE and shape the envelope for server.py."""
        # GLiNER's predict_relations returns (entities, relations) where
        # entities is a list of dicts and relations is a list of
        # {subject, relation, object, score} dicts. If `relation_labels`
        # is empty, the model still runs the entity pass; relations are
        # an empty list. The wrapper's filter below makes that explicit.
        entities, relations = self._instance.predict_relations(
            text,
            labels=entity_labels,
            relations=relation_labels,
            threshold=threshold,
        )

        # Cap entities at max_entities (preserves the model's internal
        # score-descending order).
        entities = entities[:max_entities]

        # Build a set of surviving surface strings so we can filter
        # out triples whose subject or object got dropped by the cap.
        surviving_texts = {e["text"] for e in entities}

        triples = [
            t for t in relations
            if t["subject"] in surviving_texts and t["object"] in surviving_texts
        ]

        return {
            "entities": entities,
            "triples": triples,
        }
```

- [ ] **Step 4: Run the test to confirm it passes**

```bash
uv run pytest tests/test_model.py -v
```

Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add workers/gliner-relex/src/hhagent_worker_gliner_relex/model.py workers/gliner-relex/tests/test_model.py
git commit -m "feat(workers/gliner-relex): GLiNER model wrapper + envelope shaping + max_entities cap"
```

---

### Task 1.5: Write `__main__.py` — entry point with env parsing + startup errors

**Files:**
- Create: `workers/gliner-relex/src/hhagent_worker_gliner_relex/__main__.py`

There are no automated tests for `__main__.py` — the entry point's behaviour (env-driven model load, startup error reporting, then handoff to `Server.run`) is exercised by the manual smoke test (Task 1.7) and Slice 2's `gliner_relex_e2e.rs`.

- [ ] **Step 1: Write `src/hhagent_worker_gliner_relex/__main__.py`**

```python
"""Entry point for `hhagent-worker-gliner-relex` (uv-generated shim).

Reads the required env vars (see the spec's "Manifest entry" section
for the canonical list), resolves the device, loads the model, and
hands off to Server.run(stdin, stdout).

Startup errors (`MODEL_LOAD_FAILED`, `UNSUPPORTED_DEVICE`) write one
JSON-encoded line to STDERR and exit with a non-zero status BEFORE
the stdio loop starts. The slice-2 crash classifier in the Rust side
maps these to `ClientError::EarlyExit` → "dead".
"""
import json
import os
import sys

from .errors import MODEL_LOAD_FAILED, UNSUPPORTED_DEVICE
from .model import GlinerModel
from .server import Server


def _exit_with_error(code: int, message: str, status: int) -> None:
    """Write a structured stderr line and exit with the given status."""
    print(json.dumps({"level": "error", "code": code, "message": message}), file=sys.stderr, flush=True)
    sys.exit(status)


def _resolve_device(requested: str) -> str:
    """Resolve `auto` to `cuda` (if available) or `cpu`. Reject `mps` on
    Linux (the macOS follow-up will widen this)."""
    if requested == "auto":
        try:
            import torch
            if torch.cuda.is_available():
                return "cuda"
        except Exception:
            pass
        return "cpu"
    if requested in ("cuda", "cpu"):
        return requested
    if requested == "mps":
        _exit_with_error(
            UNSUPPORTED_DEVICE,
            f"device=mps not supported on this platform (Linux build); set HHAGENT_GLINER_RELEX_DEVICE to auto|cuda|cpu",
            status=2,
        )
    _exit_with_error(
        UNSUPPORTED_DEVICE,
        f"unknown device: {requested}",
        status=2,
    )
    # Unreachable; keep mypy/pyright happy.
    return requested


def main() -> None:
    weights_dir = os.environ.get("HHAGENT_GLINER_RELEX_WEIGHTS_DIR")
    model_id = os.environ.get("HHAGENT_GLINER_RELEX_MODEL")
    device_requested = os.environ.get("HHAGENT_GLINER_RELEX_DEVICE", "auto")

    if not weights_dir:
        _exit_with_error(
            MODEL_LOAD_FAILED,
            "HHAGENT_GLINER_RELEX_WEIGHTS_DIR is unset",
            status=1,
        )
    if not model_id:
        _exit_with_error(
            MODEL_LOAD_FAILED,
            "HHAGENT_GLINER_RELEX_MODEL is unset",
            status=1,
        )
    if not os.path.isdir(weights_dir):
        _exit_with_error(
            MODEL_LOAD_FAILED,
            f"weights directory missing: {weights_dir}",
            status=1,
        )

    device = _resolve_device(device_requested)

    try:
        model = GlinerModel.load(weights_dir=weights_dir, model_id=model_id, device=device)
    except Exception as e:
        _exit_with_error(
            MODEL_LOAD_FAILED,
            f"GLiNER.from_pretrained failed: {e}",
            status=1,
        )

    server = Server(model=model)
    server.run(sys.stdin, sys.stdout)


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Verify the package imports cleanly**

```bash
cd workers/gliner-relex
uv run python -c "from hhagent_worker_gliner_relex import __main__; print('ok')"
```

Expected: prints `ok`. No `ImportError`. (We don't call `main()` here — calling it without the env vars would exit.)

- [ ] **Step 3: Verify the console-script shim resolves to `main`**

```bash
uv run python -c "import importlib.metadata as m; [print(e.value) for e in m.entry_points(group='console_scripts') if 'gliner-relex' in e.name]"
```

Expected: prints `hhagent_worker_gliner_relex.__main__:main`. If empty, `uv sync` needs to re-run after the pyproject change.

- [ ] **Step 4: Verify the full test suite still passes**

```bash
uv run pytest -v
```

Expected: 20 passed (6 errors tests + 9 server tests + 5 model tests).

- [ ] **Step 5: Commit**

```bash
git add workers/gliner-relex/src/hhagent_worker_gliner_relex/__main__.py
git commit -m "feat(workers/gliner-relex): entry point + env parsing + startup error reporting"
```

---

### Task 1.6: Write README.md + install.sh

**Files:**
- Create: `workers/gliner-relex/README.md`
- Create: `scripts/workers/gliner-relex/install.sh`

- [ ] **Step 1: Write `workers/gliner-relex/README.md`**

```markdown
# hhagent-worker-gliner-relex

hhagent's GLiNER-Relex inference worker. Runs Knowledgator's joint NER + relation-extraction model under bwrap/Seatbelt, serving repeated `extract` JSON-RPC requests across the same warm process.

**Model:** `knowledgator/gliner-relex-multi-v1.0` (default; Apache 2.0; ~1.3 GB on disk, ~2-3 GB resident).
Optionally also supports `knowledgator/gliner-relex-large-v0.5` (~2.5 GB) when `HHAGENT_GLINER_RELEX_INSTALL_LARGE=1` at install time.

**Lifecycle:** `idle_timeout` (warm-keep; 10 min idle; daily rotation; per-spec).

## Installation

```sh
# One-time on each target host:
./scripts/workers/gliner-relex/install.sh
```

This:
1. Runs `uv sync` in `workers/gliner-relex/` to create `.venv` with pinned deps.
2. Downloads `gliner-relex-multi-v1.0` weights to `$HHAGENT_DATA_DIR/workers/gliner-relex/weights/multi-v1.0/`.
3. (Optional) Downloads `gliner-relex-large-v0.5` when the env knob is set.

Required tools on PATH: `uv`, `hf` (or `huggingface-cli`), `python3`.

## Smoke test (operator-runnable; not in cargo test)

```sh
cd workers/gliner-relex
HHAGENT_GLINER_RELEX_WEIGHTS_DIR=$HHAGENT_DATA_DIR/workers/gliner-relex/weights/multi-v1.0 \
HHAGENT_GLINER_RELEX_MODEL=knowledgator/gliner-relex-multi-v1.0 \
HHAGENT_GLINER_RELEX_DEVICE=auto \
echo '{"jsonrpc":"2.0","id":1,"method":"extract","params":{"text":"Dr Smith treats asthma in Mosman.","entity_labels":["person","disease","location"],"relation_labels":["treats","located_in"]}}' \
  | uv run hhagent-worker-gliner-relex
```

Expected: a single JSON-RPC response line on stdout with at least one entity and one triple. Cold start ~10-30 s on first run; warm calls < 200 ms on CUDA.

## Environment variables

| Name | Required | Description |
|------|----------|-------------|
| `HHAGENT_GLINER_RELEX_WEIGHTS_DIR` | yes | absolute path to the model snapshot directory |
| `HHAGENT_GLINER_RELEX_MODEL` | yes | HF repo ID (`knowledgator/gliner-relex-multi-v1.0` or `…large-v0.5`) |
| `HHAGENT_GLINER_RELEX_DEVICE` | no (default `auto`) | `auto` \| `cuda` \| `cpu` (`mps` is reserved for the macOS follow-up) |
| `HF_HUB_OFFLINE` | injected by daemon | `1` — offline-only |
| `TRANSFORMERS_OFFLINE` | injected by daemon | `1` — offline-only |

## Testing

```sh
cd workers/gliner-relex
uv run pytest -v
```

Tests mock the GLiNER load — no weights or GPU needed. The real-model round-trip lives on the Rust side: `cargo test -p hhagent-core --test gliner_relex_e2e` (skip-as-pass without venv + weights).

## License

The worker code is AGPL-3.0-or-later (matches the hhagent project). The GLiNER library is Apache 2.0; the model weights from Knowledgator are Apache 2.0 on both code and weights. The confusable GLiREL (`jackboyla/GLiREL`) is CC BY-NC-SA — do NOT swap it in; it is AGPL-incompatible.

See `docs/superpowers/specs/2026-05-18-gliner-relex-feasibility-study.md` for the full licensing chain.
```

- [ ] **Step 2: Write `scripts/workers/gliner-relex/install.sh`**

```bash
mkdir -p scripts/workers/gliner-relex
```

Then create the file:

```sh
#!/usr/bin/env bash
# Operator setup for the gliner-relex worker.
# Idempotent; safe to re-run.

set -euo pipefail

# ----- pre-flight -----
command -v uv >/dev/null 2>&1 || {
  echo "error: uv is required (install: https://docs.astral.sh/uv/getting-started/installation/)" >&2
  exit 1
}

if command -v hf >/dev/null 2>&1; then
  HF=hf
elif command -v huggingface-cli >/dev/null 2>&1; then
  HF=huggingface-cli
else
  echo "error: hf or huggingface-cli is required (pip install huggingface_hub)" >&2
  exit 1
fi

# ----- paths -----
REPO_ROOT="$(git rev-parse --show-toplevel)"
WORKER_DIR="$REPO_ROOT/workers/gliner-relex"
DATA_DIR="${HHAGENT_DATA_DIR:-$HOME/.local/share/hhagent}"
WEIGHTS_DIR="$DATA_DIR/workers/gliner-relex/weights"

if [ ! -d "$WORKER_DIR" ]; then
  echo "error: $WORKER_DIR not found; run from a checkout of the hhagent repo" >&2
  exit 1
fi

echo ">>> uv sync in $WORKER_DIR"
(cd "$WORKER_DIR" && uv sync --all-extras)

echo ">>> ensuring $WEIGHTS_DIR"
mkdir -p "$WEIGHTS_DIR"

echo ">>> downloading multi-v1.0 to $WEIGHTS_DIR/multi-v1.0"
"$HF" download knowledgator/gliner-relex-multi-v1.0 \
  --local-dir "$WEIGHTS_DIR/multi-v1.0"

if [ "${HHAGENT_GLINER_RELEX_INSTALL_LARGE:-0}" = "1" ]; then
  echo ">>> downloading large-v0.5 to $WEIGHTS_DIR/large-v0.5"
  "$HF" download knowledgator/gliner-relex-large-v0.5 \
    --local-dir "$WEIGHTS_DIR/large-v0.5"
fi

# ----- license-chain sanity check -----
if [ ! -f "$WEIGHTS_DIR/multi-v1.0/config.json" ]; then
  echo "error: model card files not found at $WEIGHTS_DIR/multi-v1.0 — download failed" >&2
  exit 2
fi

echo
echo "ok: gliner-relex weights at $WEIGHTS_DIR"
echo "ok: venv at $WORKER_DIR/.venv"
echo "To enable in the daemon, export HHAGENT_GLINER_RELEX_ENABLE=1 before starting hhagent."
```

- [ ] **Step 3: Make the script executable**

```bash
chmod +x scripts/workers/gliner-relex/install.sh
```

- [ ] **Step 4: Lint-check the script with `bash -n`**

```bash
bash -n scripts/workers/gliner-relex/install.sh
```

Expected: no output (parse OK).

- [ ] **Step 5: Commit**

```bash
git add workers/gliner-relex/README.md scripts/workers/gliner-relex/install.sh
git commit -m "feat(workers/gliner-relex): README + operator install script"
```

---

### Task 1.7: Run the operator smoke test (manual, gated on installed weights)

This task is a **GATE**, not a code change. It does not produce a commit. Skip it if weights are not on the host; record the skip in the spike notes.

- [ ] **Step 1: Run the install script**

```bash
./scripts/workers/gliner-relex/install.sh
```

Expected: completes with the `ok: gliner-relex weights at ...` line. May take 5-10 minutes (uv install + hf download).

- [ ] **Step 2: Run the smoke command from the README**

```bash
cd workers/gliner-relex
HHAGENT_GLINER_RELEX_WEIGHTS_DIR="${HHAGENT_DATA_DIR:-$HOME/.local/share/hhagent}/workers/gliner-relex/weights/multi-v1.0" \
HHAGENT_GLINER_RELEX_MODEL=knowledgator/gliner-relex-multi-v1.0 \
HHAGENT_GLINER_RELEX_DEVICE=auto \
echo '{"jsonrpc":"2.0","id":1,"method":"extract","params":{"text":"Dr Smith treats asthma in Mosman.","entity_labels":["person","disease","location"],"relation_labels":["treats","located_in"]}}' \
  | uv run hhagent-worker-gliner-relex
```

Expected: one JSON-RPC success response on stdout with `result.entities` non-empty.

- [ ] **Step 3: Run the full test suite one more time as a regression check**

```bash
uv run pytest -v
```

Expected: 20 passed.

---

### Task 1.8: Slice 1 — final commit + PR boundary

- [ ] **Step 1: Run `cargo test --workspace` to confirm Rust workspace is unaffected**

```bash
cd "$(git rev-parse --show-toplevel)"
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{
  for (i=1; i<=NF; i++) {
    if ($i == "passed;") p += $(i-1);
    if ($i == "failed;") f += $(i-1);
    if ($i == "ignored;") ig += $(i-1);
  }
} END {print "PASSED:", p, "FAILED:", f, "IGNORED:", ig}'
```

Expected: `PASSED: 751 FAILED: 0 IGNORED: 4` (or whatever the baseline at branch creation was — Slice 1 adds no Rust tests).

- [ ] **Step 2: Open a PR titled "feat(workers/gliner-relex): Python worker (Slice 1 of 2)"**

PR body should explain: this is the Python half of the slice; no Rust caller yet; Slice 2 (separate PR) adds the manifest + e2e tests.

---

## SLICE 2 — Rust manifest + lifecycle wiring + e2e (separate PR; depends on Slice 1 merged)

### Task 2.1: Scaffold `core/src/workers/` module

**Files:**
- Create: `core/src/workers/mod.rs`
- Create: `core/src/workers/gliner_relex.rs` (initial scaffolding)
- Modify: `core/src/lib.rs`

- [ ] **Step 1: Write the failing test in `core/src/workers/gliner_relex.rs`**

```rust
//! GLiNER-Relex worker manifest + wire-shape types.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! for the design. This module ships:
//!
//! - `GlinerRelexEnv` — builder populated from env vars at daemon
//!   startup; carries the resolved weights/venv paths + model id.
//! - `gliner_relex_entry(env)` — produces the `ToolEntry` registered in
//!   the dispatcher's `ToolRegistry`.
//! - `ExtractRequest` / `ExtractResponse` / `Entity` / `Triple` —
//!   serde shape types that match the Python worker's wire contract.
//!
//! The slice deliberately ships NO typed client wrapper; the future v2
//! entity-extraction consumer slice will design that around its actual
//! call site.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_compiles() {
        assert!(true);
    }
}
```

- [ ] **Step 2: Write `core/src/workers/mod.rs`**

```rust
//! Worker manifests + wire-shape types.
//!
//! Each submodule owns one worker's `ToolEntry` constructor + its
//! request/response serde types. Manifests stay as Rust functions
//! (per worker-lifecycle slice 1's shell_exec_entry precedent); the
//! TOML-manifest-on-disk debate remains deferred.

pub mod gliner_relex;
```

- [ ] **Step 3: Modify `core/src/lib.rs` to expose the new module**

Find the `pub mod` declarations near the top of `core/src/lib.rs` and add:

```rust
pub mod workers;
```

Place it in alphabetical order relative to the other top-level modules.

- [ ] **Step 4: Verify the workspace builds**

```bash
source "$HOME/.cargo/env"
cargo build --workspace
```

Expected: builds cleanly. New module is empty but compiles.

- [ ] **Step 5: Run the placeholder test**

```bash
cargo test -p hhagent-core workers::gliner_relex -- --nocapture
```

Expected: `placeholder_compiles ... ok`.

- [ ] **Step 6: Commit**

```bash
git add core/src/lib.rs core/src/workers/mod.rs core/src/workers/gliner_relex.rs
git commit -m "feat(core/workers): scaffold gliner_relex module"
```

---

### Task 2.2: Define wire-shape serde types

**Files:**
- Modify: `core/src/workers/gliner_relex.rs`

- [ ] **Step 1: Write the failing test for the wire-shape types**

Append to the `tests` module in `core/src/workers/gliner_relex.rs`:

```rust
#[test]
fn extract_request_serialises_with_expected_keys() {
    let req = ExtractRequest {
        text: "Smith treats asthma.".to_string(),
        entity_labels: vec!["person".to_string(), "disease".to_string()],
        relation_labels: vec!["treats".to_string()],
        threshold: Some(0.5),
        max_entities: Some(64),
    };
    let v = serde_json::to_value(&req).unwrap();
    let obj = v.as_object().unwrap();
    let keys: std::collections::BTreeSet<&str> = obj.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from(["text", "entity_labels", "relation_labels", "threshold", "max_entities"]),
    );
}

#[test]
fn extract_request_omits_optional_fields_when_none() {
    let req = ExtractRequest {
        text: "x".to_string(),
        entity_labels: vec!["x".to_string()],
        relation_labels: vec![],
        threshold: None,
        max_entities: None,
    };
    let v = serde_json::to_value(&req).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("threshold"));
    assert!(!obj.contains_key("max_entities"));
}

#[test]
fn extract_response_round_trips_through_serde() {
    let canned = serde_json::json!({
        "entities": [{"text": "Smith", "label": "person", "start": 0, "end": 5, "score": 0.91}],
        "triples":  [{"subject": "Smith", "relation": "treats", "object": "asthma", "score": 0.77}],
    });
    let resp: ExtractResponse = serde_json::from_value(canned.clone()).unwrap();
    assert_eq!(resp.entities.len(), 1);
    assert_eq!(resp.entities[0].text, "Smith");
    assert_eq!(resp.triples[0].relation, "treats");
    // Round-trip back to JSON is byte-identical.
    let re_serialised = serde_json::to_value(&resp).unwrap();
    assert_eq!(re_serialised, canned);
}

#[test]
fn label_caps_match_python_side() {
    assert_eq!(MAX_ENTITY_LABELS, 64);
    assert_eq!(MAX_RELATION_LABELS, 64);
    assert_eq!(MAX_TEXT_BYTES, 8192);
}
```

- [ ] **Step 2: Run the test to confirm it fails**

```bash
cargo test -p hhagent-core workers::gliner_relex
```

Expected: compile errors — `cannot find ExtractRequest` etc.

- [ ] **Step 3: Add the types to `core/src/workers/gliner_relex.rs`**

Replace the placeholder body of `core/src/workers/gliner_relex.rs` (before `#[cfg(test)] mod tests`) with:

```rust
use serde::{Deserialize, Serialize};

/// Maximum number of distinct entity labels per `extract` request.
///
/// Matches `MAX_ENTITY_LABELS` in `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`.
/// Bumping either side requires bumping both.
pub const MAX_ENTITY_LABELS: usize = 64;

/// Maximum number of distinct relation labels per `extract` request.
/// Empty is valid and signals entity-only mode.
pub const MAX_RELATION_LABELS: usize = 64;

/// Maximum UTF-8 byte length of the `text` field.
pub const MAX_TEXT_BYTES: usize = 8192;

/// Wire shape of an `extract` request's `params`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractRequest {
    pub text: String,
    pub entity_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_entities: Option<u32>,
}

/// Wire shape of an `extract` response's `result`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractResponse {
    pub entities: Vec<Entity>,
    pub triples: Vec<Triple>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub text: String,
    pub label: String,
    pub start: u32,
    pub end: u32,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Triple {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub score: f32,
}
```

- [ ] **Step 4: Run the test to confirm it passes**

```bash
cargo test -p hhagent-core workers::gliner_relex
```

Expected: 4 new tests pass plus the placeholder. Replace the placeholder with the new tests' arrival — drop the `placeholder_compiles` test by removing it from the test module.

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/gliner_relex.rs
git commit -m "feat(core/workers): ExtractRequest/Response/Entity/Triple wire types"
```

---

### Task 2.3: `GlinerRelexEnv` + `gliner_relex_entry()`

**Files:**
- Modify: `core/src/workers/gliner_relex.rs`

- [ ] **Step 1: Write the failing test for the manifest constructor**

Append to the `tests` module:

```rust
#[test]
fn gliner_relex_entry_carries_idle_timeout_lifecycle() {
    use crate::worker_lifecycle::Lifecycle;
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    match entry.lifecycle {
        Lifecycle::IdleTimeout { caps, contract } => {
            assert!(contract.stateless, "must declare stateless=true for idle_timeout");
            assert_eq!(caps.idle_seconds, 600);
            assert_eq!(caps.max_requests, 10_000);
            assert_eq!(caps.max_age_seconds, 86_400);
            assert_eq!(caps.grace_period_seconds, 5);
        }
        Lifecycle::SingleUse => panic!("expected idle_timeout"),
    }
}

#[test]
fn gliner_relex_entry_disables_cpu_rlimit_for_warm_worker() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    assert_eq!(entry.policy.cpu_ms, 0, "cpu_ms must be 0 for warm workers (rlimit is cumulative; would kill after first few inferences)");
    assert!(entry.wall_clock_ms.is_none(), "wall_clock_ms must be None; lifecycle.max_age_seconds is the rotation budget");
}

#[test]
fn gliner_relex_entry_denies_network() {
    use crate::worker_lifecycle::Lifecycle;
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    // Match by discriminant only — `Net::Deny` may carry no inner state, but
    // the comparison form below avoids depending on PartialEq on `Net`.
    match entry.policy.net {
        hhagent_sandbox::Net::Deny => {}
        other => panic!("expected Net::Deny, got {:?}", other),
    }
    // sanity check the lifecycle is wired
    matches!(entry.lifecycle, Lifecycle::IdleTimeout { .. });
}

#[test]
fn gliner_relex_entry_includes_weights_and_venv_in_fs_read() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    assert!(entry.policy.fs_read.contains(&env.weights_dir));
    assert!(entry.policy.fs_read.contains(&env.venv_dir));
    assert!(entry.policy.fs_write.is_empty(), "stateless worker: no fs_write");
}

#[test]
fn gliner_relex_entry_carries_offline_env_vars() {
    let env = test_env();
    let entry = gliner_relex_entry(&env);
    let env_map: std::collections::HashMap<&str, &str> = entry.policy.env.iter()
        .map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(env_map.get("HF_HUB_OFFLINE"), Some(&"1"));
    assert_eq!(env_map.get("TRANSFORMERS_OFFLINE"), Some(&"1"));
    assert_eq!(env_map.get("HHAGENT_GLINER_RELEX_WEIGHTS_DIR"), Some(&env.weights_dir.to_string_lossy().as_ref()));
    assert_eq!(env_map.get("HHAGENT_GLINER_RELEX_MODEL"), Some(&env.model_id.as_str()));
    assert_eq!(env_map.get("HHAGENT_GLINER_RELEX_DEVICE"), Some(&env.device.as_str()));
}

fn test_env() -> GlinerRelexEnv {
    GlinerRelexEnv {
        script_path: std::path::PathBuf::from("/tmp/fake/.venv/bin/hhagent-worker-gliner-relex"),
        venv_dir: std::path::PathBuf::from("/tmp/fake/.venv"),
        weights_dir: std::path::PathBuf::from("/tmp/fake/weights/multi-v1.0"),
        model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
        device: "auto".to_string(),
    }
}
```

- [ ] **Step 2: Run the test to confirm it fails**

```bash
cargo test -p hhagent-core workers::gliner_relex
```

Expected: compile errors — `cannot find GlinerRelexEnv` etc.

- [ ] **Step 3: Add the env builder + manifest constructor**

Append to `core/src/workers/gliner_relex.rs` (before the `tests` module):

```rust
use std::path::PathBuf;

use hhagent_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::tool_dispatch::ToolEntry;
use crate::worker_lifecycle::{Contract, IdleTimeoutCaps, Lifecycle};

/// Resolved paths + config for the GLiNER-Relex worker, populated by
/// the daemon startup code from environment variables.
#[derive(Debug, Clone)]
pub struct GlinerRelexEnv {
    /// Absolute path to the uv-generated console-script shim:
    /// `<worker_dir>/.venv/bin/hhagent-worker-gliner-relex`.
    pub script_path: PathBuf,
    /// Absolute path to the worker venv root: `<worker_dir>/.venv/`.
    /// Mounted into the sandbox via `policy.fs_read` so the Python
    /// interpreter + site-packages are visible.
    pub venv_dir: PathBuf,
    /// Absolute path to the model snapshot directory (operator-staged
    /// by `scripts/workers/gliner-relex/install.sh`).
    pub weights_dir: PathBuf,
    /// HF repo ID; one of `knowledgator/gliner-relex-multi-v1.0` or
    /// `knowledgator/gliner-relex-large-v0.5`.
    pub model_id: String,
    /// `auto` / `cuda` / `cpu` (`mps` reserved for macOS follow-up).
    pub device: String,
}

/// Construct the GLiNER-Relex tool registry entry.
///
/// The returned entry is registered in `core::main` when
/// `HHAGENT_GLINER_RELEX_ENABLE=1` is set and the weights directory
/// exists. Without those preconditions, the entry is skip-registered
/// and calls to `gliner-relex` return `UNKNOWN_TOOL` per the existing
/// dispatcher path.
pub fn gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry {
    let policy = SandboxPolicy {
        fs_read: vec![env.weights_dir.clone(), env.venv_dir.clone()],
        fs_write: vec![],
        net: Net::Deny,
        // cpu_ms = 0 disables setrlimit(RLIMIT_CPU). The rlimit is
        // cumulative-process CPU time; on a warm worker doing many
        // inferences it would fire long before any single request is
        // pathological. cgroup cpu_quota_pct + lifecycle max_age_seconds
        // are the right knobs. Per-request hang detection is dispatcher
        // work, out of scope for this slice (per the worker-lifecycle
        // spec's punt).
        cpu_ms: 0,
        // multi-v1.0 ~2-3 GB resident; large-v0.5 ~4-5 GB. 4 GiB is
        // enough for multi-v1.0 with headroom; operators picking
        // large-v0.5 need to bump this.
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        env: vec![
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR".to_string(), env.weights_dir.to_string_lossy().into_owned()),
            ("HHAGENT_GLINER_RELEX_MODEL".to_string(), env.model_id.clone()),
            ("HHAGENT_GLINER_RELEX_DEVICE".to_string(), env.device.clone()),
            ("HF_HUB_OFFLINE".to_string(), "1".to_string()),
            ("TRANSFORMERS_OFFLINE".to_string(), "1".to_string()),
        ],
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
    };

    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        // wall_clock_ms = None: warm workers are long-lived by design;
        // lifecycle.max_age_seconds (24h) is the rotation budget.
        wall_clock_ms: None,
        lifecycle: Lifecycle::idle_timeout(
            IdleTimeoutCaps {
                idle_seconds: 600,
                max_requests: 10_000,
                max_age_seconds: 86_400,
                grace_period_seconds: 5,
            },
            Contract { stateless: true },
        )
        .expect("manifest defines valid idle_timeout caps"),
    }
}
```

- [ ] **Step 4: Run the test to confirm it passes**

```bash
cargo test -p hhagent-core workers::gliner_relex
```

Expected: 9 tests passed (4 wire-shape + 5 manifest).

- [ ] **Step 5: Verify the workspace still builds and tests are green**

```bash
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{
  for (i=1; i<=NF; i++) {
    if ($i == "passed;") p += $(i-1);
    if ($i == "failed;") f += $(i-1);
    if ($i == "ignored;") ig += $(i-1);
  }
} END {print "PASSED:", p, "FAILED:", f, "IGNORED:", ig}'
```

Expected: `PASSED: 760 FAILED: 0 IGNORED: 4` (baseline 751 + 9 new).

- [ ] **Step 6: Commit**

```bash
git add core/src/workers/gliner_relex.rs
git commit -m "feat(core/workers): gliner_relex_entry manifest + GlinerRelexEnv builder"
```

---

### Task 2.4: Daemon conditional registration

**Files:**
- Modify: `core/src/main.rs`

- [ ] **Step 1: Skim `core/src/main.rs` to find the existing `build_tool_registry` (or equivalent) call**

```bash
grep -n "build_tool_registry\|ToolRegistry\|shell_exec_entry" core/src/main.rs
```

Note the line range where the registry is constructed today. The patch must add gliner-relex registration *after* the existing shell-exec registration, so the same multi-tool registry serves both lanes.

- [ ] **Step 2: Add a helper in `core/src/main.rs` (or its dedicated `startup` module if it exists; if not, inline) that reads the env + constructs the manifest**

Sketch (adapt to the actual surrounding code shape — the goal is one helper called from the daemon's startup that returns `Option<ToolEntry>`):

```rust
/// Build the GLiNER-Relex tool entry from environment variables.
///
/// Returns `None` and logs a `tracing::info!` when the worker is
/// opted-out (default — `HHAGENT_GLINER_RELEX_ENABLE` unset or `0`),
/// preserving byte-equivalent startup with existing deployments.
///
/// Returns `None` and logs a `tracing::error!` (fatal: the daemon
/// continues but the operator-facing log says why) when the env is
/// requested but weights are missing. **Default posture per spec is
/// fail-closed**: callers may upgrade this to `panic!` if the operator
/// would rather have a hard refusal than a degraded startup.
fn build_gliner_relex_entry() -> Option<hhagent_core::scheduler::tool_dispatch::ToolEntry> {
    use std::path::PathBuf;
    use hhagent_core::workers::gliner_relex::{GlinerRelexEnv, gliner_relex_entry};

    let enable = std::env::var("HHAGENT_GLINER_RELEX_ENABLE").unwrap_or_default();
    if enable != "1" {
        tracing::info!("gliner-relex: HHAGENT_GLINER_RELEX_ENABLE != 1; skip registering");
        return None;
    }

    // Required env: weights dir + model id + (optional) device.
    let weights_dir = match std::env::var("HHAGENT_GLINER_RELEX_WEIGHTS_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            tracing::error!("gliner-relex enabled but HHAGENT_GLINER_RELEX_WEIGHTS_DIR is unset; skip registering");
            return None;
        }
    };
    if !weights_dir.is_dir() {
        tracing::error!("gliner-relex enabled but weights dir missing at {}; skip registering", weights_dir.display());
        return None;
    }
    let model_id = std::env::var("HHAGENT_GLINER_RELEX_MODEL")
        .unwrap_or_else(|_| "knowledgator/gliner-relex-multi-v1.0".to_string());
    let device = std::env::var("HHAGENT_GLINER_RELEX_DEVICE")
        .unwrap_or_else(|_| "auto".to_string());

    // Resolve venv + shim path: $REPO_ROOT/workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex
    // The daemon does not know its own source-tree path at runtime; the
    // operator points us at the venv via env. Reasonable default is
    // $HHAGENT_DATA_DIR/workers/gliner-relex/.venv but the operator may override.
    let venv_dir = std::env::var("HHAGENT_GLINER_RELEX_VENV_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let data = std::env::var("HHAGENT_DATA_DIR")
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                    format!("{home}/.local/share/hhagent")
                });
            PathBuf::from(data).join("workers/gliner-relex/.venv")
        });
    let script_path = venv_dir.join("bin").join("hhagent-worker-gliner-relex");
    if !script_path.exists() {
        tracing::error!("gliner-relex enabled but script shim missing at {}; skip registering", script_path.display());
        return None;
    }

    let env = GlinerRelexEnv { script_path, venv_dir, weights_dir, model_id, device };
    Some(gliner_relex_entry(&env))
}
```

- [ ] **Step 3: Wire the helper into the existing registry construction**

Find where shell-exec is added today (something like `registry.insert("shell-exec", shell_exec_entry(...))`). After that line, add:

```rust
if let Some(entry) = build_gliner_relex_entry() {
    registry.insert("gliner-relex".to_string(), entry);
    tracing::info!("gliner-relex: registered");
}
```

The exact `registry` variable name will depend on the surrounding code; match it.

- [ ] **Step 4: Verify the workspace builds**

```bash
cargo build --workspace
```

Expected: builds cleanly.

- [ ] **Step 5: Verify existing tests stay green (daemon doesn't break when env unset)**

```bash
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{
  for (i=1; i<=NF; i++) {
    if ($i == "passed;") p += $(i-1);
    if ($i == "failed;") f += $(i-1);
    if ($i == "ignored;") ig += $(i-1);
  }
} END {print "PASSED:", p, "FAILED:", f, "IGNORED:", ig}'
```

Expected: same as Task 2.3 (760 passed). Skip-register path means no behaviour change for default deployments.

- [ ] **Step 6: Commit**

```bash
git add core/src/main.rs
git commit -m "feat(core/main): conditionally register gliner-relex when HHAGENT_GLINER_RELEX_ENABLE=1"
```

---

### Task 2.5: Integration test scaffolding — skip-as-pass without venv

**Files:**
- Create: `core/tests/gliner_relex_e2e.rs`

- [ ] **Step 1: Write the test file with the skip helper + a placeholder test**

```rust
//! End-to-end integration tests for the gliner-relex worker.
//!
//! These tests spawn the real Python worker (`workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex`)
//! against a real model. Without the venv + weights, they skip-as-pass —
//! the daemon's default deployment posture (env unset) matches this.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! § "Slice 2 — Manifest + lifecycle wiring + e2e" for what each test
//! pins.

use std::path::PathBuf;

/// Resolve the venv shim path relative to the workspace root. Returns
/// `None` and prints a `[SKIP]` line when the path doesn't exist — the
/// same pattern existing integration tests use (see e.g.
/// `core/tests/shell_exec_e2e.rs`).
fn resolve_worker_script() -> Option<PathBuf> {
    let workspace_root = std::env::var("CARGO_MANIFEST_DIR")
        .map(|d| PathBuf::from(d).parent().unwrap().to_path_buf())
        .unwrap_or_else(|_| PathBuf::from("."));
    let script = workspace_root
        .join("workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex");
    if !script.exists() {
        eprintln!("[SKIP] gliner-relex venv not built: {} missing — run scripts/workers/gliner-relex/install.sh", script.display());
        return None;
    }
    Some(script)
}

fn resolve_weights_dir() -> Option<PathBuf> {
    let data_dir = std::env::var("HHAGENT_DATA_DIR")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.local/share/hhagent")))
        .ok()
        .map(PathBuf::from)?;
    let weights = data_dir.join("workers/gliner-relex/weights/multi-v1.0");
    if !weights.is_dir() {
        eprintln!("[SKIP] gliner-relex weights missing: {} — run scripts/workers/gliner-relex/install.sh", weights.display());
        return None;
    }
    Some(weights)
}

#[tokio::test(flavor = "multi_thread")]
async fn skip_helper_compiles() {
    // Smoke test for the resolution helpers themselves; the real tests
    // arrive in Tasks 2.6-2.8.
    let _ = resolve_worker_script();
    let _ = resolve_weights_dir();
}
```

- [ ] **Step 2: Run the placeholder integration test**

```bash
cargo test -p hhagent-core --test gliner_relex_e2e -- --nocapture
```

Expected: 1 passed. On a host without the venv + weights, two `[SKIP]` lines print on stderr but the test still passes.

- [ ] **Step 3: Commit**

```bash
git add core/tests/gliner_relex_e2e.rs
git commit -m "test(core/gliner_relex_e2e): skip-as-pass scaffolding for the integration suite"
```

---

### Task 2.6: Integration test — happy-path round-trip

**Files:**
- Modify: `core/tests/gliner_relex_e2e.rs`

This task only fires on a host with weights installed. The `[SKIP]` path is exercised by Task 2.5; this task adds the real-model assertion.

- [ ] **Step 1: Write the failing test**

Append to `core/tests/gliner_relex_e2e.rs`:

```rust
use hhagent_core::workers::gliner_relex::{
    gliner_relex_entry, ExtractRequest, ExtractResponse, GlinerRelexEnv,
};

// Bring in the shared sandbox + lifecycle scaffolding from the existing
// integration helpers. Match the pattern in
// core/tests/worker_lifecycle_idle_timeout_e2e.rs.
use hhagent_core::scheduler::tool_dispatch::{ToolEntry, ToolRegistry};
use hhagent_core::tool_host;
use hhagent_core::worker_lifecycle::{IdleTimeoutLifecycle, WorkerLifecycleManager};
use std::sync::Arc;

/// Build the lifecycle manager + manifest entry, spawning a real
/// worker (or returning None and skipping cleanly). Pulls the e2e
/// helpers used in the existing worker-lifecycle e2e suite.
async fn try_acquire_worker(
) -> Option<(impl WorkerLifecycleManager, ToolEntry)> {
    let script = resolve_worker_script()?;
    let weights = resolve_weights_dir()?;
    let venv_dir = script.parent().unwrap().parent().unwrap().to_path_buf();
    let env = GlinerRelexEnv {
        script_path: script,
        venv_dir,
        weights_dir: weights,
        model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
        device: "auto".to_string(),
    };
    let entry = gliner_relex_entry(&env);
    let sandbox = hhagent_tests_common::default_sandbox_backend();
    let lifecycle = IdleTimeoutLifecycle::new(Arc::from(sandbox));
    Some((lifecycle, entry))
}

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_extract_returns_entities_and_triples() {
    let Some((lifecycle, entry)) = try_acquire_worker().await else { return };

    let mut handle = lifecycle.acquire("gliner-relex", &entry).await
        .expect("acquire");

    let pool = hhagent_tests_common::bring_up_pg_cluster_or_skip().await;
    let Some(pool) = pool else { eprintln!("[SKIP] no PG"); return };

    let req = ExtractRequest {
        text: "Dr Smith treats asthma in Mosman.".to_string(),
        entity_labels: vec!["person".into(), "disease".into(), "location".into()],
        relation_labels: vec!["treats".into(), "located_in".into()],
        threshold: Some(0.5),
        max_entities: Some(64),
    };
    let params = serde_json::to_value(&req).unwrap();

    let result_value = tool_host::dispatch(
        &pool,
        handle.worker_mut(),
        "gliner-relex",
        "extract",
        params,
    ).await.expect("dispatch");

    let response: ExtractResponse = serde_json::from_value(result_value).expect("decode");
    assert!(!response.entities.is_empty(), "model should find at least one entity");
    // We don't assert a specific triple count — that depends on model
    // version. The shape pin is sufficient; quality is what the spike
    // notes record.
}
```

- [ ] **Step 2: Run the test**

```bash
cargo test -p hhagent-core --test gliner_relex_e2e happy_path -- --nocapture
```

Expected on a fully-installed host: 1 passed (test takes ~10-30 s the first time due to cold-start; subsequent runs warmer if the daemon keeps the model loaded).

Expected on a host without weights/venv: the test prints `[SKIP]` lines for venv + weights and exits successfully.

- [ ] **Step 3: Commit**

```bash
git add core/tests/gliner_relex_e2e.rs
git commit -m "test(core/gliner_relex_e2e): happy-path round-trip against real model"
```

---

### Task 2.7: Integration test — warm-reuse pin

**Files:**
- Modify: `core/tests/gliner_relex_e2e.rs`

- [ ] **Step 1: Append the warm-reuse test**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn warm_reuse_serves_two_calls_from_one_worker() {
    let Some((lifecycle, entry)) = try_acquire_worker().await else { return };
    let pool = hhagent_tests_common::bring_up_pg_cluster_or_skip().await;
    let Some(pool) = pool else { eprintln!("[SKIP] no PG"); return };

    let request = || ExtractRequest {
        text: "alpha beta gamma".to_string(),
        entity_labels: vec!["term".into()],
        relation_labels: vec![],
        threshold: Some(0.3),
        max_entities: Some(8),
    };

    // First call: cold spawn.
    {
        let mut handle = lifecycle.acquire("gliner-relex", &entry).await.expect("acquire 1");
        let params = serde_json::to_value(&request()).unwrap();
        tool_host::dispatch(&pool, handle.worker_mut(), "gliner-relex", "extract", params)
            .await.expect("dispatch 1");
    }

    // After Drop of the first handle, the slice-2 runtime should have
    // returned the worker to the warm slot. The accessor proves it.
    assert!(
        lifecycle._test_slot_has_warm("gliner-relex"),
        "expected warm worker in slot after first call"
    );

    // Second call: must hit the same warm worker (no new spawn).
    {
        let mut handle = lifecycle.acquire("gliner-relex", &entry).await.expect("acquire 2");
        let params = serde_json::to_value(&request()).unwrap();
        tool_host::dispatch(&pool, handle.worker_mut(), "gliner-relex", "extract", params)
            .await.expect("dispatch 2");
    }

    // Slot is still warm after the second call.
    assert!(lifecycle._test_slot_has_warm("gliner-relex"));
}
```

- [ ] **Step 2: Run the test**

```bash
cargo test -p hhagent-core --test gliner_relex_e2e warm_reuse -- --nocapture
```

Expected on installed host: passes. The second call is materially faster than the first (warm reuse — no model reload).

- [ ] **Step 3: Commit**

```bash
git add core/tests/gliner_relex_e2e.rs
git commit -m "test(core/gliner_relex_e2e): warm-reuse pin via _test_slot_has_warm"
```

---

### Task 2.8: Integration test — error propagation pin

**Files:**
- Modify: `core/tests/gliner_relex_e2e.rs`

- [ ] **Step 1: Append the error-propagation test**

```rust
use hhagent_core::tool_host::ToolHostError;

#[tokio::test(flavor = "multi_thread")]
async fn invalid_input_surfaces_as_invalid_input_rpc_error() {
    let Some((lifecycle, entry)) = try_acquire_worker().await else { return };
    let pool = hhagent_tests_common::bring_up_pg_cluster_or_skip().await;
    let Some(pool) = pool else { eprintln!("[SKIP] no PG"); return };

    let mut handle = lifecycle.acquire("gliner-relex", &entry).await.expect("acquire");

    // Empty text triggers INVALID_INPUT (-32001) on the Python side.
    let req = ExtractRequest {
        text: "".to_string(),
        entity_labels: vec!["x".into()],
        relation_labels: vec![],
        threshold: Some(0.5),
        max_entities: Some(8),
    };
    let params = serde_json::to_value(&req).unwrap();

    let outcome = tool_host::dispatch(&pool, handle.worker_mut(), "gliner-relex", "extract", params).await;
    let err = outcome.expect_err("empty text must error");
    match err {
        ToolHostError::Client(client_err) => {
            // The hhagent-protocol Rpc variant carries the JSON-RPC code.
            // The exact match shape depends on ClientError's surface; we
            // assert the code is INVALID_INPUT (-32001) by string match
            // on the Display impl — adapt to the actual API if more
            // structured access exists.
            let msg = client_err.to_string();
            assert!(msg.contains("-32001") || msg.contains("INVALID_INPUT"),
                "expected INVALID_INPUT, got: {msg}");
        }
        other => panic!("expected ToolHostError::Client, got {other:?}"),
    }

    // Worker stays alive after a request-local error — the next call
    // must succeed (or skip cleanly).
    let good_req = ExtractRequest {
        text: "ok".to_string(),
        entity_labels: vec!["x".into()],
        relation_labels: vec![],
        threshold: Some(0.3),
        max_entities: Some(8),
    };
    let good_params = serde_json::to_value(&good_req).unwrap();
    tool_host::dispatch(&pool, handle.worker_mut(), "gliner-relex", "extract", good_params)
        .await.expect("dispatch after error must still succeed");
}
```

- [ ] **Step 2: Run the test**

```bash
cargo test -p hhagent-core --test gliner_relex_e2e invalid_input -- --nocapture
```

Expected on installed host: passes. The test also implicitly verifies that INVALID_INPUT does not trigger `report_crash` (worker stays alive).

- [ ] **Step 3: Commit**

```bash
git add core/tests/gliner_relex_e2e.rs
git commit -m "test(core/gliner_relex_e2e): error propagation + worker stays alive after INVALID_INPUT"
```

---

### Task 2.9: Update HANDOVER + ROADMAP

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Add a "Recently completed" section to `HANDOVER.md`**

Insert at the top of the "Recently completed" cluster (immediately after the header), per the existing pattern (see the slice-2 worker-lifecycle entry as a template). Include:

- Branch name + commit range
- File summary (new modules, modified files)
- Test count delta (workspace + per-crate)
- What's deliberately NOT in scope (v2 entity-extraction integration)
- LOC of new files (track 500-LOC soft cap)

Also bump the "Last updated", "Last commit on main", and "Session-end verification" header fields.

- [ ] **Step 2: Tick the matching items in `ROADMAP.md`**

The roadmap already has the worker-lifecycle slices marked complete. Add a new entry:

```markdown
- [x] **GLiNER-Relex worker — Slice 1 (Python package)** — landed YYYY-MM-DD on branch `feat/gliner-relex-slice-1` (N commits); merged to main via PR #XX at `<sha>`. New `workers/gliner-relex/` Python package (uv-managed venv, `pyproject.toml`, `[project.scripts]` shim, JSON-RPC stdio loop, GLiNER wrapper, custom error codes, 20 pytest tests covering errors/server/model). Operator install script + README. License chain (Apache 2.0 model + Apache 2.0 upstream lib) holds per the feasibility study. Pre-req for the next-natural slice (Slice 2: Rust manifest + e2e). Workspace cargo count unchanged.
- [x] **GLiNER-Relex worker — Slice 2 (Rust manifest + e2e)** — landed YYYY-MM-DD on branch `feat/gliner-relex-slice-2` (N commits); merged to main via PR #XX at `<sha>`. New `core::workers::gliner_relex` module ships `GlinerRelexEnv` builder + `gliner_relex_entry() -> ToolEntry` (manifest constants: `Lifecycle::IdleTimeout { idle_seconds: 600, max_requests: 10_000, max_age_seconds: 86_400, grace_period_seconds: 5 }` + `Contract { stateless: true }` + `cpu_ms: 0` + `wall_clock_ms: None` per spec rationale). Wire-shape serde types (`ExtractRequest`/`ExtractResponse`/`Entity`/`Triple`). Conditional daemon registration via `HHAGENT_GLINER_RELEX_ENABLE=1`; skip-register by default (existing deployments byte-equivalent). 9 unit tests + 3 integration tests in new `core/tests/gliner_relex_e2e.rs` (skip-as-pass without venv/weights). Linux-first; macOS MPS path documented as a separate follow-up slice. Test count 751 → ~763 on Linux. Typed Rust client deferred to the v2 entity-extraction consumer slice.
```

- [ ] **Step 3: Commit**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover,roadmap): GLiNER-Relex worker slices 1+2 shipped"
```

---

### Task 2.10: Final workspace verification + PR

- [ ] **Step 1: Confirm the workspace is green**

```bash
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{
  for (i=1; i<=NF; i++) {
    if ($i == "passed;") p += $(i-1);
    if ($i == "failed;") f += $(i-1);
    if ($i == "ignored;") ig += $(i-1);
  }
} END {print "PASSED:", p, "FAILED:", f, "IGNORED:", ig}'
```

Expected on a host without venv + weights: `PASSED: ~759 FAILED: 0 IGNORED: 4` (baseline 751 + 9 unit tests; integration tests skip-as-pass and don't count toward `passed`).

Expected on a host with the worker installed: `PASSED: ~762 FAILED: 0 IGNORED: 4` (integration tests now run).

- [ ] **Step 2: Confirm no `[SKIP]` surprises**

```bash
cargo test --workspace 2>&1 | grep -E "^\[SKIP\]" | head -20
```

Expected on a host without venv + weights: 4 `[SKIP]` lines per integration test (venv + weights — 2 per test × 3 tests, give or take, depending on which helper resolves first). Expected on a fully-installed host: no `[SKIP]` lines.

- [ ] **Step 3: Open a PR titled "feat(core/workers): GLiNER-Relex manifest + e2e (Slice 2 of 2)"**

PR body should reference Slice 1's merge commit, the design spec, and the worker-lifecycle slice-2 PR (#83) as upstream dependencies.

---

## Plan self-review

**Spec coverage:**
- [x] Python package shape — Tasks 1.1–1.6
- [x] JSON-RPC wire contract (extract method, request/response, errors) — Tasks 1.2, 1.3
- [x] Rust manifest entry (Lifecycle::IdleTimeout + Contract::stateless) — Task 2.3
- [x] Sandbox boundary (fs_read, fs_write, net, cpu_ms=0, wall_clock_ms=None) — Task 2.3
- [x] Operator setup script + README — Task 1.6
- [x] Slice 1 / Slice 2 split — sections labelled
- [x] Linux-first + macOS gap — documented in Task 2.3's tests pinning Linux device strings; macOS follow-up is a separate plan
- [x] Spike strategy — covered separately in the session's spike run (not in this plan; this plan is the implementation that follows the spike)

**Placeholder scan:** no `TBD`, `TODO`, or "fill in later" markers in steps. Commit hashes / PR numbers in Task 2.9 are intentional `<sha>` / `#XX` placeholders to be filled at write time — these are the only ones.

**Type consistency:**
- `GlinerRelexEnv` field names match between Task 2.3 (definition) and the test fixture in Task 2.3 (`test_env()`) and Task 2.6 (`try_acquire_worker`).
- `ExtractRequest` / `ExtractResponse` / `Entity` / `Triple` field names match between Task 2.2 (definition), Task 2.6 (consumer in happy-path test), and the Python side (`workers/gliner-relex/.../server.py` validators in Task 1.3, model wrapper in Task 1.4).
- `MAX_TEXT_BYTES` / `MAX_ENTITY_LABELS` / `MAX_RELATION_LABELS` constants are pinned identically on both sides (Task 1.3 Python, Task 2.2 Rust).
- The `_test_slot_has_warm` accessor used in Task 2.7 matches worker-lifecycle slice-2's public test surface.

**Open follow-ups recorded:**
- A typed Rust client wrapping `tool_host::dispatch` lands with the v2 entity-extraction consumer slice — not in this plan.
- macOS MPS device branch is a follow-up plan.
- A consumer-side `entity_labels` + `relation_labels` vocabulary is the v2 consumer slice's design call — Task 1.4 only mocks; production caller picks.

---

## Execution Handoff

**Plan complete and saved to** `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md`.

This plan is **not** scheduled for execution in the current session — the session ends after running the Python POC spike whose results (e.g. CUDA wheel resolution surprises, latency numbers, sensible vocabulary findings) may motivate a revision to this plan before any future session begins implementation.

When the operator schedules implementation:

**1. Subagent-Driven (recommended for Slice 1)** — Tasks 1.1 through 1.7 are mostly mechanical TDD steps; a fresh subagent per task with two-stage review is the natural shape, mirroring the L1 promotion writer rhythm. Slice 1's no-Rust-caller endpoint is the right PR boundary.

**2. Inline Execution (recommended for Slice 2)** — once Slice 1 has merged, Slice 2's Rust + e2e tasks (Task 2.1 through Task 2.10) share enough context that inline batch execution with checkpoints is more efficient than subagent dispatch.

Either approach is compatible with the plan's TDD ordering. The plan's commits structure the review cadence regardless of executor choice.
