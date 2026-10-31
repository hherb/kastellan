# Prefill and the prompt cache: oMLX vs Ollama on the Mac (2026-10-31)

Why: the #922 re-measure showed planner prompts of 30–58k tokens on the DGX. The question was how much a
byte-stable prompt prefix saves when the backend can reuse it. Script: [`scripts/bench/prefill_bench.py`](../../../scripts/bench/prefill_bench.py).

**Host:** Mac M3 Max, 128 GB unified memory. **Model:** Gemma 4 26B-A4B at 4-bit on both backends (oMLX
`gemma-4-26b-a4b-it-4bit`; Ollama `gemma4:26b-a4b-it-qat`, `num_ctx` 65536). **Prompt:** `prompts/agent_planner.md`
as the system message plus mail-like JSON filler (~2 bytes per token, like a real `plans_so_far`). Output capped
at 64 tokens; thinking off, so the numbers are prefill.

| Prompt | oMLX cold | oMLX, tail changed | oMLX, identical again | Ollama cold | Ollama, tail changed | Ollama, identical again |
|---|---|---|---|---|---|---|
| ~21k tokens | 40–49 s | 2.0 s | 2.0 s | 34–38 s | 0.3 s | 0.04 s |
| ~39k tokens | 69–77 s | 4.2 s | 5.4 s | 126–128 s | 0.2 s | 0.04 s |
| ~56k tokens | 106–107 s | 8.1 s | 8.7 s | 249–264 s | 0.2 s | 0.08 s |

"Cold" includes the case that matters: **only the first line changed** (the old minute-stamped `<now>` block),
which got no reuse at all on either backend.

- **A byte-stable prefix makes a turn's prefill nearly free on both backends.** The old prompt order (PR #943 fixes
  it) got the cold figure on every planner turn.
- **Cold prefill:** oMLX is ~2.3× faster than Ollama at 56k tokens. Ollama's Metal prefill degrades sharply with
  length ([[metal-prompt-processing-is-nonlinear]]).
- **Warm:** Ollama's in-memory reuse of the *last* prompt is fastest for back-to-back turns, but it holds one prefix
  per slot, so an interleaved request (guard, another task) evicts it. oMLX keeps prefill state on SSD, up to an
  operator-set size, so a prefix survives across tasks and restarts. It also reports `cached_tokens`, which lands in
  `llm_usage.cached_prompt_tokens`.
- Decode on the Mac: ~40–60 tok/s on Ollama in these runs (not a careful measurement).
- oMLX serves both OpenAI (`http://127.0.0.1:8000/v1`) and Anthropic (`http://127.0.0.1:8000`) conventions; pointing a
  Mac host's LLM endpoint at it is configuration. **Not yet checked:** its thinking switch dialect for Gemma 4 (cf. #773).

Not measured: the DGX (prefill ~1,760 tok/s, so the absolute saving is smaller).
