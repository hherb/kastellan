"""Stdio JSON-RPC dispatch tests for server.py.

Server reads JSON-RPC frames from a readable, dispatches to the model,
writes responses to a writable. We use StringIO to exercise the loop in
pytest without spawning a subprocess.
"""
import io
import json

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
    # Spike correction #2: head and tail carry full entity dicts inline.
    assert resp["result"]["triples"][0]["head"]["label"] == "person"
    assert resp["result"]["triples"][0]["tail"]["label"] == "disease"


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


# --- Spike correction #3: relation_threshold field ---
# Production callers should pass relation_threshold >= 0.5 to suppress
# the dense candidate-triple noise from overlapping entity subspans
# (148 triples on one sample at threshold 0.3 — see spike notes §
# "Quality on the samples").


def test_relation_threshold_defaults_to_entity_threshold(fake_model):
    # When relation_threshold is omitted, the server passes the entity
    # `threshold` as the relation threshold to the model. This preserves
    # the spec's "one threshold field" ergonomic for callers that don't
    # need to tune them separately.
    server = Server(model=fake_model)
    req = _request(10, "extract", {
        "text": "x",
        "entity_labels": ["x"],
        "relation_labels": ["r"],
        "threshold": 0.7,
    })
    _drive(server, req)
    fake_model.extract.assert_called_once()
    kwargs = fake_model.extract.call_args.kwargs
    assert kwargs["threshold"] == 0.7
    assert kwargs["relation_threshold"] == 0.7


def test_relation_threshold_overrides_entity_threshold_when_supplied(fake_model):
    server = Server(model=fake_model)
    req = _request(11, "extract", {
        "text": "x",
        "entity_labels": ["x"],
        "relation_labels": ["r"],
        "threshold": 0.3,
        "relation_threshold": 0.6,
    })
    _drive(server, req)
    kwargs = fake_model.extract.call_args.kwargs
    assert kwargs["threshold"] == 0.3
    assert kwargs["relation_threshold"] == 0.6


def test_relation_threshold_out_of_range_returns_invalid_input(fake_model):
    server = Server(model=fake_model)
    req = _request(12, "extract", {
        "text": "x",
        "entity_labels": ["x"],
        "relation_labels": ["r"],
        "relation_threshold": 1.5,
    })
    assert _drive(server, req)[0]["error"]["code"] == INVALID_INPUT
