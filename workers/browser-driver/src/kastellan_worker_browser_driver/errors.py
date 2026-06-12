"""JSON-RPC 2.0 error envelope helpers + custom application codes.

The codes here are the wire contract between this Python worker and the Rust
caller (core::workers::browser_driver + core/tests/browser_driver_e2e.rs).
Changing a code requires updating both sides.
"""
from typing import Any, Optional, Union

# Standard JSON-RPC 2.0 codes.
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

# Application-specific codes — see the spec wire-contract table.
INVALID_INPUT = -32001     # url missing/empty/non-https, timeout/wait_until OOR
RENDER_FAILED = -32003     # request-local; navigation/render error, worker stays alive

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
    optional). A passed-through dict/list/string lands verbatim under
    `error.data`.
    """
    err: dict[str, Any] = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    return {"jsonrpc": "2.0", "id": req_id, "error": err}


def success_response(req_id: JsonRpcId, result: Any) -> dict:
    """Build a JSON-RPC 2.0 success envelope."""
    return {"jsonrpc": "2.0", "id": req_id, "result": result}
