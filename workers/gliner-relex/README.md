# kastellan-worker-gliner-relex

kastellan's GLiNER-Relex inference worker. Runs Knowledgator's joint NER + relation-extraction model under bwrap/Seatbelt, serving repeated `extract` JSON-RPC requests across the same warm process.

**Model:** `knowledgator/gliner-relex-multi-v1.0` (default; Apache 2.0; ~1.3 GB on disk, ~2-3 GB resident).
Optionally also supports `knowledgator/gliner-relex-large-v0.5` (~2.5 GB) when `KASTELLAN_GLINER_RELEX_INSTALL_LARGE=1` at install time.

**Lifecycle:** `idle_timeout` (warm-keep; 10 min idle; daily rotation; per-spec).

## Installation

```sh
# One-time on each target host:
./scripts/workers/gliner-relex/install.sh
```

This:
1. Runs `uv sync` in `workers/gliner-relex/` to create `.venv` with pinned deps.
2. Downloads `gliner-relex-multi-v1.0` weights to `$KASTELLAN_DATA_DIR/workers/gliner-relex/weights/multi-v1.0/`.
3. (Optional) Downloads `gliner-relex-large-v0.5` when the env knob is set.

Required tools on PATH: `uv`, `hf` (or `huggingface-cli`), `python3`.

## Smoke test (operator-runnable; not in cargo test)

```sh
cd workers/gliner-relex
echo '{"jsonrpc":"2.0","id":1,"method":"extract","params":{"text":"Dr Smith treats asthma in Mosman.","entity_labels":["person","disease","location"],"relation_labels":["treats","located_in"]}}' \
  | env KASTELLAN_GLINER_RELEX_WEIGHTS_DIR="$KASTELLAN_DATA_DIR/workers/gliner-relex/weights/multi-v1.0" \
        KASTELLAN_GLINER_RELEX_MODEL=knowledgator/gliner-relex-multi-v1.0 \
        KASTELLAN_GLINER_RELEX_DEVICE=auto \
    uv run kastellan-worker-gliner-relex
```

The `env` wrapper is load-bearing: a bare `VAR=value cmd1 | cmd2` shell prefix only sets the env for `cmd1` (the echo), not `cmd2` (the worker). With `env` on the right side of the pipe, the env vars reach `uv run`.

Expected: a single JSON-RPC response line on stdout with at least one entity and one triple. Cold start ~3-5 s on CPU (per the POC spike on the DGX Spark), warm calls ~157 ms p50 on CPU / sub-100 ms on CUDA.

## JSON-RPC contract

Method: `extract` (the only method served). Params:

| Field | Type | Default | Notes |
|------|------|---------|-------|
| `text` | string | — | required; UTF-8; ≤ 8192 bytes |
| `entity_labels` | array[string] | — | required; non-empty; ≤ 64 entries; use natural-language strings |
| `relation_labels` | array[string] | — | required; may be empty (entity-only mode); ≤ 64 entries |
| `threshold` | float | 0.5 | entity score threshold; range [0, 1] |
| `relation_threshold` | float | `= threshold` | optional separate relation threshold; production callers should pass ≥ 0.5 to suppress dense candidate-triple noise from overlapping entity subspans |
| `max_entities` | int | 64 | cap on returned entities; triples whose head or tail got dropped are filtered too |

Result envelope (per spike correction #2 — head and tail carry full entity dicts inline):

```json
{
  "entities": [{"text": "Dr Smith", "label": "person", "start": 0, "end": 8, "score": 0.999}],
  "triples":  [{"head": {"text": "Dr Smith", "type": "person", "start": 0, "end": 8, "entity_idx": 0},
                "tail": {"text": "asthma",   "type": "disease", "start": 16, "end": 22, "entity_idx": 1},
                "relation": "treats", "score": 0.995}]
}
```

Field-key naming observed on real `multi-v1.0` output (2026-05-18 smoke test):
- Top-level `entities[]` items use `text`, `label`, `start`, `end`, `score`.
- Nested `head` / `tail` items use `text`, `type` (NOT `label`), `start`, `end`, `entity_idx`. No `score` on the nested copy.

A consumer that wants the entity type from the triple's head/tail can read `head.type` directly, or index back into `entities[head.entity_idx]` for the full top-level shape including the score. The worker passes the upstream shape through unchanged.

Triple-level deduplication is NOT performed by the worker — consumers decide their own policy.

## Environment variables

| Name | Required | Description |
|------|----------|-------------|
| `KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` | yes | absolute path to the model snapshot directory |
| `KASTELLAN_GLINER_RELEX_MODEL` | yes | HF repo ID (`knowledgator/gliner-relex-multi-v1.0` or `…large-v0.5`) |
| `KASTELLAN_GLINER_RELEX_DEVICE` | no (default `auto`) | `auto` (CUDA if `mem_get_info` reports ≥ 3 GiB free, else CPU) \| `cuda` (forced; will OOM if memory unavailable) \| `cpu` (`mps` reserved for the macOS follow-up) |
| `HF_HUB_OFFLINE` | injected by daemon | `1` — offline-only |
| `TRANSFORMERS_OFFLINE` | injected by daemon | `1` — offline-only |

## Testing

```sh
cd workers/gliner-relex
uv run pytest -v
```

24 tests (6 errors + 12 server + 6 model). All mock the GLiNER load — no weights or GPU needed. The real-model round-trip lives on the Rust side: `cargo test -p kastellan-core --test gliner_relex_e2e` (skip-as-pass without venv + weights; Slice 2 of the implementation plan).

## License

The worker code is AGPL-3.0-or-later (matches the kastellan project). The GLiNER library is Apache 2.0; the model weights from Knowledgator are Apache 2.0 on both code and weights. The confusable GLiREL (`jackboyla/GLiREL`) is CC BY-NC-SA — do NOT swap it in; it is AGPL-incompatible.

See `docs/superpowers/specs/2026-05-18-gliner-relex-feasibility-study.md` for the full licensing chain.
