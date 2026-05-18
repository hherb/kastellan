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

    Triple shape uses `head` / `tail` carrying full Entity dicts inline,
    matching upstream `model.inference(...)` envelope (see spike notes
    correction #2). Consumers can read head.label / head.start without
    a second lookup.
    """
    smith = {"text": "Smith", "label": "person", "start": 0, "end": 5, "score": 0.91}
    asthma = {"text": "asthma", "label": "disease", "start": 13, "end": 19, "score": 0.88}
    m = MagicMock(name="FakeGliNER")
    m.extract.return_value = {
        "entities": [smith, asthma],
        "triples": [
            {"head": smith, "tail": asthma, "relation": "treats", "score": 0.77},
        ],
    }
    return m
