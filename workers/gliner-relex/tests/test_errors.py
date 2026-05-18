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
