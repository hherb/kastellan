"""GLiNER-Relex model wrapper.

Loads the Knowledgator gliner-relex-* model from a pre-downloaded
on-disk weights directory (operator runs `install.sh` once; daemon
fails-closed when weights are missing). Exposes a single `.extract()`
method matching the server.py dispatch contract.

The wrapper folds the upstream `(entities_batch, relations_batch)`
return shape into the `{"entities": [...], "triples": [...]}` envelope
the Rust caller expects. It also enforces the `max_entities` cap and
filters triples whose head or tail isn't among the surviving entities.

Upstream method shape (per spike correction #1):
    entities_batch, relations_batch = model.inference(
        texts=[text],                # single-element list; unwrap [0]
        labels=entity_labels,
        relations=relation_labels,
        threshold=...,               # entity threshold
        relation_threshold=...,      # separate relation threshold
        return_relations=True,
        flat_ner=False,              # required when using relations
    )

Triple envelope (per spike correction #2):
    {head: Entity, tail: Entity, relation: str, score: float}
where head and tail each carry the full entity dict inline; a consumer
reading head.label or head.start does so without a second lookup.
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
        load. `auto` resolution (with CUDA memory probe per spike
        correction #4) happens in `__main__.py`.
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
        relation_threshold: float,
        max_entities: int,
    ) -> dict:
        """Run joint NER + RE and shape the envelope for server.py."""
        entities_batch, relations_batch = self._instance.inference(
            texts=[text],
            labels=entity_labels,
            relations=relation_labels,
            threshold=threshold,
            relation_threshold=relation_threshold,
            return_relations=True,
            flat_ner=False,
        )
        entities = entities_batch[0]
        relations = relations_batch[0]

        # Cap entities at max_entities (preserves the model's internal
        # score-descending order).
        entities = entities[:max_entities]

        # Build a set of surviving surface strings so we can filter
        # out triples whose head or tail got dropped by the cap or
        # never made the entity threshold.
        surviving_texts = {e["text"] for e in entities}

        triples = [
            t for t in relations
            if t["head"]["text"] in surviving_texts
            and t["tail"]["text"] in surviving_texts
        ]

        return {
            "entities": entities,
            "triples": triples,
        }
