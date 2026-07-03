# Model evaluation — InternScience **Agents-A1** as kastellan's general-purpose model

**Date:** 2026-07-03
**Status:** Investigation / candidate assessment (no code change)
**Question:** How far is [InternScience/Agents-A1](https://huggingface.co/InternScience/Agents-A1)
an ideal candidate for the general-purpose reasoning model behind kastellan's
`llm-router`?

> Sources (HF model card returned 403 to automated fetch; facts below are from the
> project site, paper, GitHub and published quant repos — **verify against the repo
> before committing to it**):
> [project site](https://internscience.github.io/Agents-A1/) ·
> [GitHub](https://github.com/InternScience/Agents-A1) ·
> [arXiv 2606.30616](https://arxiv.org/html/2606.30616v1) ·
> [Q4_K_M GGUF](https://huggingface.co/InternScience/Agents-A1-Q4_K_M-GGUF) ·
> [FP8-dynamic](https://huggingface.co/InternScience/Agents-A1-FP8-dynamic) ·
> [Spheron VRAM recommender](https://www.spheron.network/tools/gpu-recommender/InternScience/Agents-A1/)

---

## 1. What the model is

| Property | Value |
| --- | --- |
| Family | MoE, **35B total / ~3B active** per token; Qwen3.5/3.6-35B-A3B derivative, agentic post-train |
| Focus | **Long-horizon agentic**: planning, tool use, inspecting intermediate state, holding constraints across long context |
| License | **Apache-2.0** (reported — **hard gate, verify `LICENSE` on the repo**) |
| Context | **256K** (262,144 tokens) |
| Tool calling | Served with the `qwen3_coder` tool-call parser; trained on ~45K-token trajectories; claims coherence through 15+ tool calls |
| Serving | **vLLM / SGLang**, OpenAI-compatible endpoints |
| Quants published | FP16, **FP8-dynamic**, **Q4_K_M GGUF** |
| Self-reported benchmarks | Seal-0 56.4 · HiPhO 46.4 · FrontierScience-Olympiad 79.0 · IFBench 80.6 · **IFEval 94.8** (unverified; treat as marketing until locally reproduced) |

## 2. Fit against kastellan's hard constraints

| Constraint (`CLAUDE.md`) | Verdict | Notes |
| --- | --- | --- |
| **AGPL-compatible license only** | ✅ *if confirmed* | Apache-2.0 is on the allow-list. This is the gating check — verify the repo `LICENSE`. |
| **Vendor-neutral, local-first** | ✅ | Open weights, runs entirely local behind `llm-router`. No API dependency. |
| **Cross-platform Linux + macOS first-class** | ✅ | vLLM/SGLang (Linux `:8000`) **and** Q4_K_M GGUF for Ollama/llama.cpp (macOS `:11434`) both published — matches the router's per-OS defaults exactly. |
| **No NVIDIA *hard* dependency** | ✅ | Same GGUF runs on Mac/any Linux box; DGX is the *primary* host, not a requirement. |
| **Rust core, Python only in sandboxed workers** | ✅ (n/a) | Model is an out-of-process HTTP backend; no in-process runtime added. |

## 3. Fit against the architecture

- **Drop-in on the existing router.** `KASTELLAN_LLM_LOCAL_URL` already defaults to
  `http://127.0.0.1:8000/v1` (vLLM/SGLang) on Linux and `:11434/v1` (Ollama) on macOS —
  Agents-A1's documented serving path. Adoption is **env config only**
  (`KASTELLAN_LLM_LOCAL_MODEL`); **no `llm-router` code change** to send chat completions.
  See `docs/devel/manual/13-llm-router.md`.
- **Agentic identity match.** kastellan *is* an agent loop
  (scheduler → CASSANDRA → dispatch → finalize) orchestrating sandboxed tool workers over
  many steps. Agents-A1 is purpose-built for exactly that shape. This is a stronger
  philosophical fit than a generic chat model.
- **DGX Spark memory profile.** ~128 GB unified memory swallows FP16 (~70 GB) with margin;
  FP8 (~35 GB) / Q4 (~20 GB) leave lots of room. **3B active params** → low per-token
  compute and low memory-bandwidth pressure, well matched to the Spark's unified-memory
  bandwidth.

## 4. Gaps & caveats (honest)

1. **Covers `Router::send`, not `Router::embed`.** kastellan's recall needs a 256-dim
   Matryoshka *embedding* model (`KASTELLAN_LLM_EMBEDDING_URL`/`_MODEL`). Agents-A1 is a
   chat/agent model — keep a separate embedder (e.g. bge-m3). It's "the reasoning model,"
   not "the only model."
2. **Native tool-calls not wired in kastellan yet.** Ch.13 states `tool_calls`/`function_call`
   schemas are deferred to Phase 1; today CASSANDRA does its own reasoning + dispatch. The
   model's core strength (the `qwen3_coder` tool-call chain) is only unlocked once the
   OpenAI tools schema is wired through the scheduler. Real work — but this model is a strong
   reason to prioritise it.
3. **Provenance / supply chain.** InternScience is a Shanghai-AI-Lab-lineage (Chinese) lab.
   For a security-first system this is worth flagging — but the mitigations are structural and
   strong: the model runs **fully local behind the `llm-router` egress seam** (no data leaves),
   and kastellan's **deterministic policy floor** (CASSANDRA `DataClass` + `injection_guard` +
   per-worker sandbox containment) is **model-independent by design**. Per the threat model,
   worst-case LLM compromise reaches at most the agent's own user; a misaligned/backdoored
   model **cannot bypass policy enforcement**. The architecture already assumes the model is
   untrusted. Still, spot-check weight-level behaviour.
4. **Self-reported SOTA** vs GPT-5.5 / DeepSeek-V4 / Kimi-K2.6 is unverified marketing.
5. **3B active** may cap single-shot reasoning depth vs a dense ~30B; the bet is that agentic
   scaffolding compensates. Measure on hard planning steps.

## 5. Verdict

**Strong candidate — arguably the best-fitting open model for *this specific system*** —
conditional on three checks:

1. Confirm **Apache-2.0** on the repo `LICENSE` (hard gate).
2. Run a **local eval** on the DGX (FP8 via vLLM) against real scheduler/CASSANDRA tasks,
   plus a macOS Q4 smoke test. Don't trust published benchmarks.
3. Treat it as the **reasoning/agent model** (keep a separate embedder) and use it as the
   forcing function to **wire native tool-calls (Phase 1)**.

---

## 6. Eval checklist (concrete)

### 6.1 License gate (do first — blocks everything)
- [ ] Open the repo `LICENSE` / model-card license field; confirm **Apache-2.0** (or another
      AGPL-compatible license). If "source-available" / custom-restrictive → **stop**, it fails
      the hard constraint.

### 6.2 DGX (Linux, aarch64) — vLLM, FP8
- [ ] Serve (OpenAI-compatible, on the router's default port):
      ```sh
      vllm serve InternScience/Agents-A1-FP8-dynamic \
        --host 127.0.0.1 --port 8000 \
        --served-model-name agents-a1 \
        --max-model-len 262144 \
        --enable-auto-tool-choice --tool-call-parser qwen3_coder
      # SGLang alternative:
      # python -m sglang.launch_server --model-path InternScience/Agents-A1-FP8-dynamic \
      #   --host 127.0.0.1 --port 8000 --tool-call-parser qwen3_coder
      ```
- [ ] Point kastellan at it (no code change):
      ```sh
      export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:8000/v1
      export KASTELLAN_LLM_LOCAL_MODEL=agents-a1
      # keep a real embedder separate:
      export KASTELLAN_LLM_EMBEDDING_URL=http://127.0.0.1:8001/v1
      export KASTELLAN_LLM_EMBEDDING_MODEL=bge-m3
      ```
- [ ] Smoke the router path live:
      ```sh
      cargo test -p kastellan-llm-router -- --ignored   # live local-LLM tests (need backend up)
      ```
- [ ] Drive the full prod chain against the real model (this suite runs against a *mock* LLM by
      default — run it once pointed at the live backend to sanity-check plan quality):
      `core/tests/cli_ask_e2e.rs` (CLI → PG → scheduler → CASSANDRA → dispatch → finalize).
- [ ] Record: tokens/s, first-token latency, VRAM/unified-mem footprint, and **plan coherence
      over a ≥10-step tool chain** (its headline claim).

### 6.3 macOS (Ollama, Q4) — cross-platform parity
- [ ] Pull/serve the `Q4_K_M` GGUF via Ollama; confirm it answers on `:11434/v1`.
- [ ] `export KASTELLAN_LLM_LOCAL_MODEL=<ollama-tag>`; re-run the router `--ignored` smoke.
- [ ] Confirm the macOS path degrades gracefully (smaller context / slower) but functions.

### 6.4 Quality bar (kastellan-specific, not generic benchmarks)
- [ ] Instruction-following on **CASSANDRA policy prompts** — does it respect the
      constitutional-refusal framing without jailbreak drift?
- [ ] **Long-horizon tool use**: a task needing ≥10 sandboxed worker calls stays coherent
      (web-fetch → python-exec → memory recall chains).
- [ ] **Refusal / safety** behaviour is acceptable *given* the deterministic floor already
      catches policy violations regardless.
- [ ] Note any regressions vs the current local model on recall-grounded answers.

### 6.5 If adopted
- [ ] File a Phase-1 issue to wire native `tool_calls` (OpenAI tools schema / `qwen3_coder`
      parser) through the scheduler — that's where the model's value is unlocked.
- [ ] Document the chosen quant + serve command in a runbook under `docs/devel/runbooks/`.
- [ ] Update `docs/devel/ROADMAP.md` with the model decision.
