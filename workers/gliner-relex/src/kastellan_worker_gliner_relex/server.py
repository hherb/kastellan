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

        # relation_threshold is optional; defaults to entity threshold per
        # spec + spike correction #3. Production callers should pass >= 0.5
        # to suppress dense noise from overlapping entity subspans.
        relation_threshold = params.get("relation_threshold", threshold)
        if not isinstance(relation_threshold, (int, float)) or not (0.0 <= float(relation_threshold) <= 1.0):
            return error_response(req_id=req_id, code=INVALID_INPUT, message="relation_threshold must be in [0, 1]")

        max_entities = params.get("max_entities", 64)
        if not isinstance(max_entities, int) or max_entities < 1:
            return error_response(req_id=req_id, code=INVALID_INPUT, message="max_entities must be a positive integer")

        try:
            result = self._model.extract(
                text=text,
                entity_labels=entity_labels,
                relation_labels=relation_labels,
                threshold=float(threshold),
                relation_threshold=float(relation_threshold),
                max_entities=int(max_entities),
            )
        except Exception as e:
            return error_response(req_id=req_id, code=INFERENCE_FAILED, message=f"inference failed: {e}")

        return success_response(req_id=req_id, result=result)
