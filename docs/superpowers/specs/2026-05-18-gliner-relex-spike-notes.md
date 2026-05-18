# GLiNER-Relex Spike Notes — DGX Spark (Linux), 2026-05-18

**Status:** spike artifact — records what the throwaway POC at
`scripts/spike/gliner-relex/` (deleted after this file is written)
learned about GLiNER-Relex behaviour before any implementation.

**Companion docs:**
- `2026-05-18-gliner-relex-worker-design.md` — the design this spike validates
- `2026-05-18-gliner-relex-feasibility-study.md` — the upstream feasibility report this spike grounds
- `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md` — the implementation plan that needs the corrections recorded below

## TL;DR

**Worth implementing.** The Apache 2.0 license chain holds, the model produces sensible (entities, triples) on the kind of memory bodies hhagent will see, and **CPU latency is acceptable** (p50 ~157 ms warm on the GB10 host). The spike surfaced four corrections the design spec + plan need before implementation begins.

## Host

- **Hostname:** `spark-0d2d` (DGX Spark)
- **GPU:** NVIDIA GB10 (unified memory); **vLLM owned 107 GB at spike time → CUDA path unavailable**
- **Python:** 3.12.3
- **torch:** 2.12.0+cu130
- **gliner:** 0.2.26
- **transformers:** 5.1.0
- **uv:** 0.10.10

## License chain

✅ Confirmed. Model card's `README.md` declares `license: apache-2.0`; weight files (`model.safetensors`, `tokenizer.json`, etc.) and the upstream `gliner` package are all Apache 2.0. The feasibility study's "hard block on GLiREL" warning was honoured — we downloaded only `knowledgator/gliner-relex-multi-v1.0`.

## What ran

```sh
hf download knowledgator/gliner-relex-multi-v1.0 \
  --local-dir ~/.local/share/hhagent/workers/gliner-relex/weights/multi-v1.0/
# → 10 files, ~1.3 GB on disk (model.safetensors 1.2 GB)

cd scripts/spike/gliner-relex/
uv sync                                       # → torch 2.12.0, transformers 5.1.0, gliner 0.2.26 in 30s
SPIKE_DEVICE=cpu uv run python spike.py
```

Three samples ran through `model.inference(...)` with `threshold=0.3`:

1. `"Dr Smith treats asthma in his Mosman clinic."` (medical)
2. `"The Rust workspace under hhagent uses uv-managed Python venvs per worker."` (technical)
3. `"PostgreSQL migration 0008 added the deleted_memories AFTER DELETE trigger that journals deleted rows."` (db)

## Latency

| Stage | Time (CPU on GB10 host) |
|---|---|
| Model load (`GLiNER.from_pretrained` + `.to("cpu")`) | **3.71 s** |
| First inference (cold sample 1, includes JIT warmup) | 283 ms |
| Inference (cold samples 2 + 3) | 150 / 176 ms |
| Warm loop (10x sample 1) | min 136 / **p50 157 / p95 174** / max 174 ms |

**Per the spec's design-intent test:** warm calls < 200 ms target — **comfortably met on CPU**. CUDA would be materially faster (sub-100 ms expected per the feasibility study), but **CUDA is not a deployment assumption on this host** while vLLM owns the GPU. CPU is the realistic production posture and is acceptable.

## Quality on the samples

### Sample 1 — `"Dr Smith treats asthma in his Mosman clinic."`

```
entities (4):
  - 'Dr Smith'          -> person   (0.999)
  - 'asthma'            -> disease  (0.999)
  - 'Mosman'            -> location (0.770)
  - 'Mosman clinic'     -> location (0.666)

relations (10 — many duplicates):
  - 'Dr Smith' --[treats]--> 'asthma'         (0.980)   ✅
  - 'Dr Smith' --[located in]--> 'Mosman'     (0.600)   ✅
  - 'Dr Smith' --[located in]--> 'Mosman clinic' (0.753) ✅
  - 'Mosman clinic' --[located in]--> 'Mosman'  (0.562) ✅
  (plus 6 noise/duplicate variants)
```

**Verdict:** clean. The model nails the high-confidence medical triple (`Dr Smith treats asthma`, 0.980) and the location chain.

### Sample 2 — `"The Rust workspace under hhagent uses uv-managed Python venvs per worker."`

```
entities (8):
  - 'Rust workspace'        -> technology   (0.539)
  - 'hhagent'               -> organization (0.943)
  - 'uv-managed Python'     -> technology   (0.712)
  - 'uv-managed Python venvs' -> tool       (0.670)
  - 'worker'                -> person       (0.915)  ⚠️ false positive: "worker" is treated as a person
  - … plus 'Rust', 'Python', 'venvs' (overlapping subspans)

relations (148 — heavy noise)
```

**Verdict:** mixed. Strong entity for `hhagent → organization (0.943)`, but `worker → person (0.915)` is a false positive — a label-vocabulary problem (no `software_worker` distinction). The 148 relations include large numbers of duplicates from overlapping entity spans (`Rust` + `Rust workspace` + `Rust workspace under hhagent` all producing distinct `(head, relation, tail)` triples with the same surface text).

### Sample 3 — `"PostgreSQL migration 0008 added the deleted_memories AFTER DELETE trigger that journals deleted rows."`

```
entities (11):
  - 'PostgreSQL'                          -> organization (0.934)
  - 'PostgreSQL migration 0008'           -> technology   (0.496)
  - 'deleted_memories AFTER DELETE trigger' -> tool       (0.472)
  - … plus 8 overlapping subspans

relations (70):
  - 'PostgreSQL' --[added]--> 'deleted_memories' (0.913)   ✅
  - 'PostgreSQL' --[added]--> 'deleted_memories AFTER DELETE' (0.926) ✅
  - … etc.
```

**Verdict:** the high-confidence `PostgreSQL --[added]--> deleted_memories (0.913)` triple is exactly the kind of fact a graph-lane consumer would want. Still suffers from overlapping-span noise.

## Required corrections to the design spec + implementation plan

These four findings need to land as edits to `2026-05-18-gliner-relex-worker-design.md` and `2026-05-18-gliner-relex-worker.md` before any implementation begins.

### 1. Method name is `inference`, NOT `predict_relations`

The spec's "Spike strategy" pseudocode and the plan's Task 1.4 (`model.py::extract`) both assume the upstream method is `predict_relations`. The actual method on `gliner-relex-multi-v1.0` is `model.inference(...)`.

**Canonical signature** (from the model card README):

```python
entities, relations = model.inference(
    texts=[text],                    # batch input; pass a single-element list
    labels=entity_labels,            # list[str]
    relations=relation_labels,       # list[str]
    threshold=0.3,                   # entity threshold
    relation_threshold=0.5,          # separate threshold for relations
    return_relations=True,
    flat_ner=False,                  # required for relation extraction
)
```

Plan's `model.py` needs to update its wrapper to call `self._instance.inference(...)` with this signature and handle the `texts=[text]` batching.

### 2. Relation envelope is `{head, tail, relation, score}`, NOT `{subject, relation, object, score}`

The spec's "JSON-RPC wire contract" section §"Response" shows:

```json
"triples": [{"subject": "Dr Smith", "relation": "treats", "object": "asthma", "score": 0.79}]
```

The upstream model returns:

```python
{"head": {"text": "Dr Smith", "label": "person", "score": 0.999, ...},
 "tail": {"text": "asthma",   "label": "disease", "score": 0.999, ...},
 "relation": "treats",
 "score": 0.980}
```

**Decision required:** does the worker's wire shape preserve `head`/`tail` (matching upstream) or normalise to `subject`/`object` (matching the spec's original drafting)? **Recommendation: preserve upstream's `head`/`tail` field names.** Reasons:

- The model returns the entity dicts inline, not just the surface strings. A consumer (e.g. the future v2 graph-lane wiring) can pick up `head.label` + `head.start` + `head.end` "for free" without a second lookup. Renaming loses that.
- Less name-translation surface to audit + maintain.
- Matches the upstream Python idiom; one less surprise in the Python worker source.

Plan's Task 2.2 needs the `Triple` Rust type updated:

```rust
pub struct Triple {
    pub head: Entity,
    pub tail: Entity,
    pub relation: String,
    pub score: f32,
}
```

(`subject` / `object` removed; nested `head` / `tail` carry full Entity payloads.)

Plan's Task 1.4 (`model.py`) wrapper still needs to **truncate triples whose head/tail were dropped by `max_entities`** — the existing logic survives, just keying on `e["text"]` membership in the surviving entity set as the spec drafted it.

### 3. Production threshold should be higher; deduplication is the caller's job

Sample 2 produced **148 relations** at threshold 0.3 — almost all duplicates or near-duplicates from overlapping entity subspans (`Rust`/`Rust workspace`/`Rust workspace under hhagent` are three separate entity spans, each producing its own relations with each candidate tail). At `threshold ≥ 0.5` the noise drops significantly but does not disappear.

**Action items for the plan:**
- Spec's "JSON-RPC wire contract" should add a note: **production callers should use `threshold ≥ 0.5`** (the spec's 0.5 default is correct; the spike used 0.3 to characterise behaviour).
- Plan's Task 1.4 model wrapper should expose `relation_threshold` as a separate parameter (it's `relation_threshold=0.5` by default in upstream; the spec lumps them into one `threshold`). Two options: (a) wire `threshold` to both entity + relation thresholds (current design); (b) split into `entity_threshold` + `relation_threshold` (matches upstream + cleaner). **Recommendation (b).** The spec's `ExtractRequest` should gain an optional `relation_threshold` field (defaults to `threshold` if omitted).
- **Deduplication is NOT in scope for the worker.** Multiple triples with the same `(head.text, relation, tail.text)` and slightly different surface forms are legitimate model output; the v2 consumer slice decides how to dedup. Document explicitly.

### 4. CUDA cannot be assumed available

On the DGX Spark, vLLM was consuming 107 GB of unified memory at spike time → `model.to("cuda")` raised `torch.AcceleratorError: CUDA error: out of memory`. The worker code must:

- Resolve `HHAGENT_GLINER_RELEX_DEVICE=auto` based on `torch.cuda.is_available()` AND `torch.cuda.mem_get_info()` (or wrap the `.to("cuda")` in a try-except that falls back to CPU + WARNs).
- CPU is a first-class deployment posture, not a fallback degradation.

Plan's Task 1.5 `__main__.py::_resolve_device` currently does:

```python
if requested == "auto":
    try:
        import torch
        if torch.cuda.is_available():
            return "cuda"
    except Exception:
        pass
    return "cpu"
```

This is **insufficient on the DGX Spark** — `torch.cuda.is_available()` returns `True` even when memory is exhausted. The plan should be updated to:

```python
if requested == "auto":
    try:
        import torch
        if torch.cuda.is_available():
            # Probe for actual memory headroom before committing to cuda.
            try:
                free, total = torch.cuda.mem_get_info(0)
                # Need ~3 GB for fp32; multi-v1.0 + activations + transient.
                if free >= 3 * 1024 * 1024 * 1024:
                    return "cuda"
            except Exception:
                pass
    except Exception:
        pass
    return "cpu"
```

Alternatively, accept that `.to("cuda")` may OOM and catch it explicitly in `model.py::load`. Either pattern is acceptable; the design spec should call out the failure mode and pick one.

## Findings that did NOT motivate spec changes

- **Cold-start at 3.7 s is well under the 30 s ceiling** the spec uses for sandbox-policy `wall_clock_ms` rationale. Plan's `mem_mb: 4_096` is generous for `multi-v1.0` at fp32 on CPU.
- **`uv sync` works cleanly on the DGX Spark** with the spec's pinned deps (`gliner>=0.2`, `transformers>=4.40`, `sentencepiece>=0.2`, `torch>=2.2`). uv resolved to gliner 0.2.26 / torch 2.12.0 / transformers 5.1.0. No CUDA-wheel-mismatch surprises (the host has CUDA 13.0 toolkit; torch pulled `torch==2.12.0+cu130` automatically).
- **The `huggingface_hub` version warning** (`A new version of huggingface_hub (1.15.0) is available! You are using version 1.7.1.`) is operator-side; the worker doesn't care.

## Action items spawned from the spike

1. **Update [`2026-05-18-gliner-relex-worker-design.md`](2026-05-18-gliner-relex-worker-design.md)** with the four corrections from §"Required corrections" above. This should land as a docs-only follow-up commit citing this spike notes file.
2. **Update [`../plans/2026-05-18-gliner-relex-worker.md`](../plans/2026-05-18-gliner-relex-worker.md)** Task 1.4 (`model.py` wrapper signature), Task 1.5 (`_resolve_device` CUDA-memory probe), and Task 2.2 (`Triple` struct field names).
3. **Delete the spike directory** at `scripts/spike/gliner-relex/` after this file is committed (per the spec: "deleted after the spike notes are written; not on a release path").
4. **macOS spike (follow-up session on Apple Silicon)** is still outstanding. None of the findings above change the macOS posture — MPS is still untested upstream; CPU fallback is now blessed for Linux too which simplifies the macOS path.

## Sample raw output

Full spike script output (uncondensed, with all 148/70 relation entries) is preserved in
`scripts/spike/gliner-relex/spike-output.txt` until the spike directory is deleted in step 3 above.
The committed copy of this notes file carries only the summaries.
