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
