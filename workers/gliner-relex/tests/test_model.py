"""GLiNER wrapper tests.

We mock `gliner.GLiNER` entirely — the real model load is a 1.3 GB
operation that doesn't belong in unit tests. The integration test on
the Rust side (slice-2 `gliner_relex_e2e.rs`) covers the real-model
round-trip; the manual smoke test in the README is the operator's
sanity check.

The mock returns batched output (`[[...]]`) because spike correction
#1 confirmed the upstream method is `model.inference(texts=[text], ...)`
which returns parallel batches; the wrapper unwraps index 0 since we
always pass a single text.
"""
from unittest.mock import patch, MagicMock

import pytest

from hhagent_worker_gliner_relex.model import GlinerModel


@pytest.fixture
def fake_gliner_class():
    """Patch `gliner.GLiNER.from_pretrained` so model load is instant.

    The returned MagicMock instance carries a `.inference` method
    matching the upstream gliner API (per spike correction #1); tests
    configure its return value per case.
    """
    with patch("hhagent_worker_gliner_relex.model.GLiNER") as mock_cls:
        instance = MagicMock(name="GliNERInstance")
        mock_cls.from_pretrained.return_value = instance
        yield mock_cls, instance


# Helper: a one-entity, one-triple canned result already in batched
# shape — the model's `inference()` returns parallel batches.
def _canned_batch_result(entities: list, triples: list) -> tuple:
    return ([entities], [triples])


def test_load_calls_gliner_from_pretrained_with_offline_kwargs(fake_gliner_class):
    mock_cls, _ = fake_gliner_class
    GlinerModel.load(
        weights_dir="/data/weights/multi-v1.0",
        model_id="knowledgator/gliner-relex-multi-v1.0",
        device="cuda",
    )
    mock_cls.from_pretrained.assert_called_once()
    call_kwargs = mock_cls.from_pretrained.call_args.kwargs
    assert call_kwargs.get("local_files_only") is True


def test_load_passes_device_to_instance(fake_gliner_class):
    _, instance = fake_gliner_class
    GlinerModel.load(
        weights_dir="/data/weights/multi-v1.0",
        model_id="knowledgator/gliner-relex-multi-v1.0",
        device="cuda",
    )
    # Model objects don't take `device=` in from_pretrained; we call
    # `.to(device)` afterwards. Verify that happened.
    instance.to.assert_called_once_with("cuda")


def test_extract_returns_envelope_shape(fake_gliner_class):
    _, instance = fake_gliner_class
    # `inference` returns (entities_batch, relations_batch). The wrapper
    # unwraps batch index 0 since we always send a single text.
    smith = {"text": "Smith", "label": "person", "start": 0, "end": 5, "score": 0.91}
    asthma = {"text": "asthma", "label": "disease", "start": 13, "end": 19, "score": 0.88}
    instance.inference.return_value = _canned_batch_result(
        entities=[smith, asthma],
        triples=[
            {"head": smith, "tail": asthma, "relation": "treats", "score": 0.77},
        ],
    )
    model = GlinerModel.load(weights_dir="/x", model_id="y", device="cpu")
    result = model.extract(
        text="Smith treats asthma.",
        entity_labels=["person", "disease"],
        relation_labels=["treats"],
        threshold=0.5,
        relation_threshold=0.5,
        max_entities=64,
    )
    assert result == {
        "entities": [smith, asthma],
        "triples": [
            {"head": smith, "tail": asthma, "relation": "treats", "score": 0.77},
        ],
    }


def test_extract_calls_inference_with_canonical_kwargs(fake_gliner_class):
    # Spike correction #1: the upstream signature is
    # `inference(texts=[text], labels=..., relations=..., threshold=...,
    # relation_threshold=..., return_relations=True, flat_ner=False)`.
    # This test pins all six kwargs at the boundary so a future upstream
    # rename trips immediately.
    _, instance = fake_gliner_class
    instance.inference.return_value = _canned_batch_result(entities=[], triples=[])
    model = GlinerModel.load(weights_dir="/x", model_id="y", device="cpu")
    model.extract(
        text="hello",
        entity_labels=["e1"],
        relation_labels=["r1"],
        threshold=0.4,
        relation_threshold=0.7,
        max_entities=64,
    )
    instance.inference.assert_called_once()
    kwargs = instance.inference.call_args.kwargs
    assert kwargs["texts"] == ["hello"]  # batch-of-one shape
    assert kwargs["labels"] == ["e1"]
    assert kwargs["relations"] == ["r1"]
    assert kwargs["threshold"] == 0.4
    assert kwargs["relation_threshold"] == 0.7
    assert kwargs["return_relations"] is True
    assert kwargs["flat_ner"] is False


def test_extract_truncates_entities_to_max(fake_gliner_class):
    _, instance = fake_gliner_class
    too_many = [
        {"text": f"e{i}", "label": "x", "start": 0, "end": 1, "score": 0.9}
        for i in range(10)
    ]
    instance.inference.return_value = _canned_batch_result(entities=too_many, triples=[])
    model = GlinerModel.load(weights_dir="/x", model_id="y", device="cpu")
    result = model.extract(
        text="x" * 10,
        entity_labels=["x"],
        relation_labels=[],
        threshold=0.5,
        relation_threshold=0.5,
        max_entities=3,
    )
    assert len(result["entities"]) == 3


def test_extract_filters_triples_to_surviving_entity_spans(fake_gliner_class):
    # Spike correction #2: triple envelope is {head, tail, relation,
    # score} with head/tail carrying full entity dicts. The filter keys
    # on head["text"] and tail["text"] membership in the surviving
    # entities set so triples dropped by max_entities truncation get
    # filtered too.
    _, instance = fake_gliner_class
    alpha = {"text": "alpha", "label": "x", "start": 0, "end": 5, "score": 0.9}
    beta = {"text": "beta", "label": "x", "start": 6, "end": 10, "score": 0.4}
    instance.inference.return_value = _canned_batch_result(
        entities=[alpha],  # beta did not pass the threshold
        triples=[
            {"head": alpha, "tail": alpha, "relation": "r", "score": 0.8},
            # This triple's tail references beta — beta is not in the
            # surviving entity set, so the wrapper drops the triple.
            {"head": alpha, "tail": beta, "relation": "r", "score": 0.7},
        ],
    )
    model = GlinerModel.load(weights_dir="/x", model_id="y", device="cpu")
    result = model.extract(
        text="alpha",
        entity_labels=["x"],
        relation_labels=["r"],
        threshold=0.5,
        relation_threshold=0.5,
        max_entities=64,
    )
    assert len(result["triples"]) == 1
    assert result["triples"][0]["tail"]["text"] == "alpha"
