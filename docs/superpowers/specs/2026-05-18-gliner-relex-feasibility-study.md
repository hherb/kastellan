# GLiNER-Relex Feasibility Study — Input for Entity-Extraction v2

**Status:** research artifact, not a design or plan
**Author:** Horst Herb + Claude Opus 4.7 (research subagent + architecture discussion)
**Date:** 2026-05-18
**Companion docs:**
- `docs/superpowers/specs/2026-05-18-entity-extraction-graph-lane-design.md` — the v1 design this study may motivate revising
- `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` — the warm-worker abstraction this study assumes

## Why this document exists

The v1 entity-extraction spec proposes a `HybridEntityExtractor`: a deterministic substring-match primary (requiring a curated `entities` vocab table) with an LLM fallback (high latency + cost on the read path). The vocab-curation burden is the design's weakest point — it's a maintenance tax that compounds with corpus growth and stays on the human.

The user surfaced **GLiNER-Relex** as a possible replacement: a zero-shot encoder-scale joint NER + relation-extraction model that returns (subject, relation, object) triples in a single forward pass with no vocab needed. If the licensing and platform story check out, it collapses both layers of the v1 design into one fast path.

This document captures the feasibility study's findings so the v2 redesign can consume them. **It does not propose changes to the v1 spec.** v1 ships as designed; v2 is a separate slice triggered when the user decides.

## Name disambiguation (load-bearing)

Three projects with confusable names — the licensing story diverges sharply:

| Project | What it is | Code license | Weights license |
|---|---|---|---|
| **GLiNER** (`urchade/GLiNER`) | Base NER library; everything else builds on it | Apache 2.0 | Per-model on HF |
| **GLiREL** (`jackboyla/GLiREL`) | Separate relation-extraction library | **CC BY-NC-SA 4.0** — non-commercial, **AGPL-incompatible** | inherits |
| **GLiNER-Relex** (Knowledgator) | Joint NER+RE model loaded via the upstream `gliner` Python package | Apache 2.0 (upstream lib) | **Apache 2.0** |

**For hhagent's AGPL-3.0 project, only the Knowledgator `gliner-relex-*` models are usable.** GLiREL is a hard block. Confirm-at-install: the model card URL must contain `knowledgator/gliner-relex-` for the licensing chain to hold.

Sources:
- [urchade/GLiNER LICENSE](https://github.com/urchade/GLiNER/blob/main/LICENSE) — Apache 2.0
- [jackboyla/GLiREL](https://github.com/jackboyla/GLiREL) — CC BY-NC-SA 4.0 (hard block)
- [knowledgator/gliner-relex-multi-v1.0 model card](https://huggingface.co/knowledgator/gliner-relex-multi-v1.0) — Apache 2.0
- [knowledgator/gliner-relex-large-v0.5 model card](https://huggingface.co/knowledgator/gliner-relex-large-v0.5) — Apache 2.0

The base encoder for `multi-v1.0` is `microsoft/mdeberta-v3-base` (MIT-licensed); for `large-v0.5` it's `microsoft/deberta-v3-large` (also MIT). The full licensing chain is AGPL-compatible.

## Capability

- **Single-pass joint NER + relation extraction.** Shared encoder, entity-span detection and relation scoring in one forward pass. Per the [arXiv paper](https://arxiv.org/abs/2605.10108v1).
- **Zero-shot, schema-supplied per call.** Caller passes `entity_labels` and `relations` lists per request. Not open-vocabulary discovery — you must enumerate candidate relation types per query, but no fine-tuning. This is a real design constraint for v2: a fixed relation vocabulary (e.g. `mentions`, `occurred_at`, `relates_to`, `caused_by`, `treated_with`) needs to be chosen up front.
- **Base encoders:** `multi-v1.0` uses `microsoft/mdeberta-v3-base` (~280M params, hidden 768, 12 layers); `large-v0.5` uses `microsoft/deberta-v3-large` (hidden 1024).
- **Benchmarks** (from a [Towards Data Science article](https://towardsdatascience.com/gliner2-extracting-structured-information-from-text/)):
  - CoNLL04: 40.4% Micro-F1 (vs GPT-5-mini 42.4%, GLiNER2 34.1%)
  - FewRel: 12.5% (vs GPT-5-mini 15.0%, GLiNER2 16.8%)
  - Competitive with a frontier mini-LLM on joint extraction, weaker on dense relation classification.
- **No built-in coreference.** "Dr. Smith" / "Smith" dedup is the caller's problem — same as the v1 design.

## Cross-platform inference

- **Framework:** PyTorch ≥2.0 + `transformers`, `huggingface_hub`, `onnxruntime`, `sentencepiece`. Also supports ONNX export.
- **Linux (CUDA):** first-class. Fits the DGX Spark host.
- **macOS Apple Silicon (MPS):** **not first-class upstream.** Device-selection logic in `gliner/model.py` only branches CPU/CUDA. Manual `model.to("mps")` should work because DeBERTa-v2 ops are MPS-supported; expect to set `PYTORCH_ENABLE_MPS_FALLBACK=1` and budget half a day to verify on the MacBook before committing.
- **CPU-only:** viable. The project advertises CPU optimisation.
- **Footprint:** `multi-v1.0` `model.safetensors` is ~1.28 GB on disk; large-v0.5 ~2.5 GB. RAM at fp32 inference: ~2-3 GB for `multi-v1.0`, ~4-5 GB for `large-v0.5`. fp16 / ONNX-int8 reduce further.

Sources:
- [urchade/GLiNER pyproject.toml](https://raw.githubusercontent.com/urchade/GLiNER/main/pyproject.toml)
- [gliner/model.py device logic](https://github.com/urchade/GLiNER/blob/main/gliner/model.py)
- [knowledgator/gliner-relex-multi-v1.0 file tree](https://huggingface.co/knowledgator/gliner-relex-multi-v1.0/tree/main)
- [PyTorch MPS / Apple Silicon docs](https://developer.apple.com/metal/pytorch/)

## Operational fit

- **Fully offline after one-time weight download.** Standard pattern: `HF_HUB_OFFLINE=1` / `TRANSFORMERS_OFFLINE=1` + `local_files_only=True`. No extra support needed from gliner itself. Compatible with the sandbox's no-network-egress posture at inference time.
- **No telemetry or phone-home** in the upstream dep tree.
- **No official Docker image** — plain `pip install gliner`. Fits hhagent's bwrap / Seatbelt worker model trivially.

## Warm-worker corollary

`multi-v1.0` at ~2-3 GB resident is a single-digit-percent of either the DGX Spark's or the MacBook's RAM. Keeping the worker warm is operationally cheap. The per-extraction latency collapses to one encoder forward pass (sub-100 ms on CPU for typical memory bodies; faster on CUDA/MPS).

This requires the worker-lifecycle abstraction — see `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`. GLiNER-Relex is the prototypical `idle_timeout` + `stateless = true` worker that motivated the abstraction.

The v2 entity-extraction redesign should not implement until the worker-lifecycle slice has landed and shell-exec has migrated as the `single_use` smoke test — GLiNER-Relex would then arrive as the first `idle_timeout` consumer.

## Effort estimate

~150-250 LOC Python worker: load model in `__main__`, JSON-RPC `extract(text, entity_types, relation_types) → {entities, triples}`, offline-mode env vars, structured error reporting. Reuses the existing JSON-RPC stdio contract from `hhagent-protocol`. The Rust side gains a thin client + the `idle_timeout` lifecycle manifest.

## Loud gotchas

1. **MPS support is untested upstream.** Half-day smoke test on the MacBook is the minimum due diligence. If MPS ops fail, CPU fallback is viable (slower but acceptable for a memory-write path that isn't on the user's hot interaction loop).
2. **Per-call schema is mandatory.** The caller must supply a relation-type list. v2 needs to pick a fixed vocabulary up front. "Discover any relation, schema-free" is not what this tool does — if that's the requirement, an LLM remains the answer.
3. **No coreference / entity resolution.** The v2 design still needs a dedup pass on the surface strings the model returns. This was true of v1 too; the burden doesn't shrink, it just moves.
4. **License confusion is real.** GLiREL (CC BY-NC-SA) and GLiNER-Relex (Apache 2.0) have similar names. The PR that lands the GLiNER-Relex dep must include an explicit manifest line + a comment pointing at the Knowledgator model URL.

## Recommendation

**Worth prototyping before deciding** — not committing to the swap yet. The single decisive reason is that GLiNER-Relex collapses the hybrid extractor's two layers (curated `entities` table + LLM fallback) into one zero-shot pass on a 1.3 GB Apache-2.0 model, eliminating vocab-curation entirely — which is exactly the burden the user wants to drop. CoNLL04 F1 ≈ frontier mini-LLM at encoder-scale latency is the load-bearing benchmark.

**Sequencing** (proposed, subject to user direction):
1. Land entity-extraction v1 spec as designed. (Done — PR #82 merged.)
2. Land worker-lifecycle-policy spec → implementation plan → implementation slice. (Independent of GLiNER-Relex; benefits every future inference worker.)
3. Migrate shell-exec to `single_use` under the new lifecycle abstraction. (Smoke test for the abstraction.)
4. Half-day prototype: GLiNER-Relex worker on Linux (CUDA) + macOS (MPS or CPU). Confirm licensing chain, measure end-to-end latency, validate the relation-vocabulary choice.
5. Based on prototype results: v2 entity-extraction spec either swaps to GLiNER-Relex (if the prototype is convincing) or sticks with the v1 hybrid (if quality / cross-platform / latency disappoint).

The prototype is the gate; this document is its prerequisite reading.
