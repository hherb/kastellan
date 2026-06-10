# GLiNER-Relex Worker — Design Spec

**Status:** design — implementation plan written, POC spike completed 2026-05-18
**Author:** Horst Herb + Claude Opus 4.7 (brainstorming pass 2026-05-18)
**Date:** 2026-05-18
**Companion docs:**
- `docs/superpowers/specs/2026-05-18-gliner-relex-spike-notes.md` — **READ FIRST.** POC spike findings; supersedes four points in this spec (method name, relation envelope, threshold defaults, CUDA-availability detection).
- `docs/superpowers/specs/2026-05-18-gliner-relex-feasibility-study.md` — license chain + capability + cross-platform notes; this spec assumes its findings
- `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` — defines `Lifecycle::IdleTimeout` + `Contract { stateless: true }`; this worker is the first consumer
- `docs/superpowers/specs/2026-05-18-entity-extraction-graph-lane-design.md` — v1 entity-extraction design; this worker MAY become its v2 replacement on a later slice (out of scope here)
- `docs/threat-model.md` — per-worker sandbox invariant the worker preserves

## Updates from the POC spike (2026-05-18)

The spike at `scripts/spike/gliner-relex/` (deleted; results in the spike notes file) found four places where this spec's initial drafting needs correction before implementation begins. **The relation envelope correction in §"JSON-RPC wire contract" below already reflects spike finding #2; the other three are recorded inline in the implementation plan's task notes and are summarised here:**

1. **Upstream method is `model.inference(texts=[text], labels=..., relations=..., threshold=..., relation_threshold=..., return_relations=True, flat_ner=False)`**, not `predict_relations`. (Plan's Task 1.4 update.)
2. **Relation envelope is `{head: Entity, tail: Entity, relation: str, score: f32}`** — both head and tail carry full entity dicts inline, not just surface strings. (This section's "Response" example below has been updated; plan's Task 2.2 `Triple` struct ditto.)
3. **Threshold defaults: entity ≥ 0.5, relation ≥ 0.5.** The model produces heavy noise at threshold 0.3 (148 relations on one sample from overlapping entity subspans). The `ExtractRequest` schema below gains an optional `relation_threshold` field separate from `threshold`.
4. **CUDA availability is not the same as CUDA memory availability.** On the DGX Spark, vLLM owned the GPU; `torch.cuda.is_available()` returned `True` but `model.to("cuda")` OOMed. Plan's Task 1.5 `_resolve_device` needs a `torch.cuda.mem_get_info()` probe before committing to `cuda`. CPU is a first-class production posture — p50 warm latency on the spike's CPU run was 157 ms, well under the design's 200 ms warm-call target.

## Why this document exists

The worker-lifecycle policy slice 2 (merged 2026-05-18 in PR #83) shipped the `idle_timeout` runtime — per-tool warm cache, post-completion cap evaluation, passive crash detection, exponential restart backoff. The first natural consumer is GLiNER-Relex: Knowledgator's joint NER + relation-extraction model (Apache 2.0 weights, ~1.3 GB resident at fp32, single forward pass for entities + triples). This spec defines the worker itself — Python package, JSON-RPC contract, manifest, sandbox boundary, operator setup — and explicitly scopes out the downstream consumer (v2 entity extraction) so the worker can land standalone.

The spec also establishes the convention for **every future Python worker** in the tree (embedding-as-worker, sentiment, classification, OCR). Until now, `workers/prelude` is Rust and `workers/shell-exec` is Rust; this is kastellan's first Python worker. Tooling choices here cascade.

## What this spec defines

1. The Python worker package shape (`workers/gliner-relex/`) with uv-managed venv + pinned deps.
2. The JSON-RPC `extract` method's request / response / error envelope.
3. The Rust-side manifest entry (`gliner_relex_entry() -> ToolEntry`) with `Lifecycle::IdleTimeout` + `Contract { stateless: true }`.
4. The sandbox boundary (`fs_read`, `fs_write`, `net`, env vars, profile, CPU/RAM budgets).
5. The operator setup script (`scripts/workers/gliner-relex/install.sh`) and the standardised on-disk weights path.
6. The slice boundary between Slice 1 (Python worker, standalone) and Slice 2 (Rust client + lifecycle wiring + e2e).
7. The Linux-first posture + the documented gap for macOS (deferred to a follow-up session when the operator is on Apple Silicon hardware).
8. The spike strategy that runs in the brainstorming session itself and feeds back into the implementation plan.

## What this spec does NOT do

- **No consumer wiring.** The v2 entity-extraction redesign that consumes this worker on the read path is a separate slice. v1 entity-extraction (spec at `docs/superpowers/specs/2026-05-18-entity-extraction-graph-lane-design.md`) stays untouched.
- **No graph-lane write-side population.** Auto-linking memories to extracted entities at memory-write time stays the same future slice the v1 entity-extraction spec scopes out.
- **No macOS implementation.** The plan ships Linux-first; the spec records what the macOS follow-up needs to validate but does not pre-stub the MPS device branch.
- **No fine-tuning, no LoRA, no on-the-fly adapter swap.** The worker is read-only against the static model weights. Any future improvement would be a separate model + a new manifest + a new worker.
- **No coreference / entity resolution.** The model returns surface strings; if a downstream consumer needs "Dr. Smith" = "Smith" dedup, it implements its own dedup pass. This was true of v1 entity extraction too; the burden moves but does not shrink.
- **No model auto-download by the worker.** Operator runs a one-time install script; daemon fails-closed at startup if weights are missing (matches the `db::probe::run` posture).
- **No worker-level pool concurrency.** Single-threaded per worker; concurrent same-tool callers serialise via worker-lifecycle slice 2's `Arc<TokioMutex<ToolState>>` — the existing slice-2 runtime handles it for free.

## Architecture

Two processes communicating over JSON-RPC 2.0 line-delimited stdio — the same contract `kastellan-protocol` uses today for shell-exec.

```
kastellan (core, Rust)
  │
  │  spawn under SandboxPolicy via Lifecycle::IdleTimeout manager
  │
  ▼
.venv/bin/kastellan-worker-gliner-relex               (Python worker, sandboxed;
                                                     uv-generated console-script shim
                                                     equivalent to: python -m kastellan_worker_gliner_relex)
  │
  ├── on startup: load model from KASTELLAN_GLINER_RELEX_WEIGHTS_DIR
  ├── stdio loop: read JSON-RPC frames, dispatch `extract`, write response
  └── exits on stdin EOF or SIGTERM (lifecycle eviction)
```

Effort estimate: ~250 LOC Python (model load + stdio loop + extract handler + error mapping), ~200 LOC Rust (manifest entry + wire-shape serde types + sandbox policy + tests; no typed client — that lands with the v2 consumer slice), plus install script + spike notes.

## JSON-RPC wire contract

The worker advertises exactly one method: `extract`. The wire shape is shared between Python (server) and Rust (client); changing it requires updating both sides + the e2e pin test.

### Request

```json
{
  "jsonrpc": "2.0",
  "id": "<caller-supplied>",
  "method": "extract",
  "params": {
    "text": "Dr Smith treats asthma in his Mosman clinic.",
    "entity_labels": ["person", "organization", "location", "disease"],
    "relation_labels": ["treats", "located in", "works at"],
    "threshold": 0.5,
    "relation_threshold": 0.5,
    "max_entities": 64
  }
}
```

- `text` (required, string): UTF-8 input. Empty string → `INVALID_INPUT`.
- `entity_labels` (required, array of strings): zero-shot entity types to look for. Must be non-empty. Use natural-language strings — the model card uses `"located in"` not `"located_in"`.
- `relation_labels` (required, array of strings): zero-shot relation types. Empty array is valid — the worker skips the RE pass and returns `triples: []`. Useful for consumers that only need entities.
- `threshold` (optional, float, default `0.5`): score threshold for entity detection. Anything below this is filtered before the response.
- `relation_threshold` (optional, float, default `= threshold`): separate score threshold for relations. The model can produce dense candidate triples from overlapping entity subspans (spike measured 148 triples on one input at 0.3), so a 0.5 floor on relations specifically is recommended in production. Omitting this field reuses the entity `threshold`.
- `max_entities` (optional, integer, default `64`): cap on returned entity count to bound payload size. Triples filtered to those whose `head.text` and `tail.text` both survive the entity cap.

### Response

```json
{
  "jsonrpc": "2.0",
  "id": "<echoed>",
  "result": {
    "entities": [
      {"text": "Dr Smith", "label": "person", "start": 0, "end": 8, "score": 0.999},
      {"text": "asthma", "label": "disease", "start": 16, "end": 22, "score": 0.999},
      {"text": "Mosman", "label": "location", "start": 30, "end": 36, "score": 0.770}
    ],
    "triples": [
      {
        "head":     {"text": "Dr Smith", "label": "person", "start": 0, "end": 8, "score": 0.999},
        "tail":     {"text": "asthma",   "label": "disease", "start": 16, "end": 22, "score": 0.999},
        "relation": "treats",
        "score":    0.980
      }
    ]
  }
}
```

Each triple carries the full `head` and `tail` entity dicts inline (matches upstream `model.inference` shape; consumers reading `head.label` / `head.start` get them for free without a second lookup). `relation` is the natural-language relation label string verbatim from the caller's `relation_labels` array. Empty `entities` and `triples` arrays are valid and signal "no extractions above threshold." Caller treats this as success, not failure.

**Triple-level deduplication is out of scope for the worker.** The model emits multiple near-identical triples from overlapping entity subspans (e.g. `("Dr Smith", "treats", "asthma")` may appear alongside `("Smith", "treats", "asthma")`); the consumer slice decides how to dedup since the right policy is consumer-specific.

### Errors

JSON-RPC error envelope with custom application codes:

| Code     | Name                  | When                                                              | Worker alive after? |
|----------|-----------------------|-------------------------------------------------------------------|---------------------|
| `-32700` | `PARSE_ERROR`         | Stdin frame is not valid JSON                                     | Yes                 |
| `-32600` | `INVALID_REQUEST`     | Frame is JSON but not a valid JSON-RPC 2.0 request                | Yes                 |
| `-32601` | `METHOD_NOT_FOUND`    | `method` is not `extract`                                         | Yes                 |
| `-32602` | `INVALID_PARAMS`      | Required field missing / wrong type                               | Yes                 |
| `-32001` | `INVALID_INPUT`       | Text empty, `entity_labels` empty, threshold out of range         | Yes                 |
| `-32002` | `MODEL_LOAD_FAILED`   | Emitted as a structured stderr line before the stdio loop begins  | **No (exit 1)**     |
| `-32003` | `INFERENCE_FAILED`    | Model raised at inference time (CUDA OOM, dtype mismatch, ...)    | Yes                 |
| `-32604` | `UNSUPPORTED_DEVICE`  | Manifest device is `mps` on Linux, or otherwise unresolvable      | **No (exit 2)**     |

`MODEL_LOAD_FAILED` and `UNSUPPORTED_DEVICE` are startup errors: the worker writes a single JSON object to stderr (`{"level": "error", "code": -32002, ...}`) and exits with a non-zero code BEFORE the stdio loop starts. The slice-2 crash classifier maps this to `ClientError::EarlyExit` → dead, so the warm registry tears the worker down and the next dispatch attempts a fresh spawn (which will fail again if the underlying problem is unfixed — the operator sees the loop in the audit log).

All other errors keep the worker alive — these are request-local failures. The `idle_timeout` runtime's restart-on-crash path is unaffected.

### Per-call schema constraints

- `entity_labels` length capped at 32 — beyond that, GLiNER's per-label embedding pass slows materially. Caller enforces; worker rejects with `INVALID_INPUT` above 64 (slack for accidental over-supply).
- `relation_labels` length capped at 32 with the same logic.
- `text` capped at 8192 UTF-8 bytes. Memory bodies are typically much shorter; 8 KiB is a generous ceiling and matches the audit-log payload envelope (4 KiB) with 2× headroom.

These limits are enforced in Python (defence in depth) and pinned in Rust client tests. Bumping any limit requires updating both sides.

## Worker manifest entry

The manifest lives as a Rust function returning `ToolEntry`, per worker-lifecycle slice 1's pattern (`shell_exec_entry()`). The on-disk TOML manifest discussed in the worker-lifecycle spec's open question 1 stays deferred — no operator has yet asked to edit it.

`ToolEntry`'s schema today has `binary: PathBuf` + `policy: SandboxPolicy` + `wall_clock_ms: Option<u64>` + `lifecycle: Lifecycle`. It does NOT have an `args` field — `WorkerSpec.args` is constructed inside `SingleUseLifecycle::acquire` as `&[]`. To launch `python -m kastellan_worker_gliner_relex` without extending `ToolEntry`, the worker exposes a `[project.scripts]` entry in its `pyproject.toml`:

```toml
# workers/gliner-relex/pyproject.toml
[project.scripts]
kastellan-worker-gliner-relex = "kastellan_worker_gliner_relex.__main__:main"
```

After `uv sync`, `.venv/bin/kastellan-worker-gliner-relex` is a real executable shim. The manifest's `binary` field points at that path; no args needed.

```rust
// core/src/workers/gliner_relex.rs (new module)

pub fn gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry {
    ToolEntry {
        binary: env.script_path.clone(),  // .venv/bin/kastellan-worker-gliner-relex (uv-generated shim)
        policy: SandboxPolicy {
            fs_read: vec![
                env.weights_dir.clone(),
                env.venv_dir.clone(),
                // standard system libs come from the existing bwrap base mounts
            ],
            fs_write: vec![],         // stateless
            net: Net::Deny,
            profile: Profile::WorkerStrict,
            cpu_ms: 0,                // 0 = disabled rlimit; cumulative-CPU rlimit is wrong for warm workers
                                      // (would kill after first few inferences). Per-request hang detection
                                      // is dispatcher work — out of scope per worker-lifecycle spec.
            mem_mb: 4_096,            // multi-v1.0 ~2-3 GB resident at fp32 + headroom;
                                      // large-v0.5 ~4-5 GB → bump to 6_144 if operator picks large
            cpu_quota_pct: Some(400), // 4 cores worth via cgroup CPUQuota; rate-limits, not budget
            tasks_max: Some(64),
            env: env.derived_env_vars(),
            ..Default::default()
        },
        wall_clock_ms: None,          // warm workers are long-lived; lifecycle.max_age_seconds (24h)
                                      // is the rotation budget. wall_clock_ms = Some(30_000) on shell-exec
                                      // matches its single-use semantics; that's the wrong shape here.
        lifecycle: Lifecycle::idle_timeout(
            IdleTimeoutCaps {
                idle_seconds: 600,            // 10 min idle teardown
                max_requests: 10_000,         // slow-leak hygiene
                max_age_seconds: 86_400,      // daily rotation
                grace_period_seconds: 5,      // SIGTERM → wait → SIGKILL
            },
            Contract { stateless: true },     // required for idle_timeout per spec v1
        ).expect("manifest defines valid idle_timeout caps"),
    }
}
```

`GlinerRelexEnv` is a small builder that the daemon's startup populates from environment variables. It carries:

- `script_path: PathBuf` — `${workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex}` resolved absolute.
- `venv_dir: PathBuf` — `${workers/gliner-relex/.venv/}` for `fs_read` (covers Python interpreter + site-packages).
- `weights_dir: PathBuf` — `$KASTELLAN_DATA_DIR/workers/gliner-relex/weights/<model-slug>/`.
- `model_id: String` — Knowledgator HF repo ID, e.g. `knowledgator/gliner-relex-multi-v1.0`.
- `device: String` — `auto` (Linux default → CUDA if available else CPU), `cuda`, `cpu`, `mps` (macOS follow-up only).

`env.derived_env_vars()` produces the env passed to the worker via `--setenv`:

```
KASTELLAN_GLINER_RELEX_WEIGHTS_DIR=<absolute path>
KASTELLAN_GLINER_RELEX_MODEL=<HF repo ID>
KASTELLAN_GLINER_RELEX_DEVICE=<auto|cuda|cpu|mps>
HF_HUB_OFFLINE=1
TRANSFORMERS_OFFLINE=1
KASTELLAN_LANDLOCK_RW=<derived per existing prelude>
KASTELLAN_SECCOMP_PROFILE=worker-strict
```

The `HF_HUB_OFFLINE` + `TRANSFORMERS_OFFLINE` pair is belt-and-suspenders: even if `Net::Deny` were misconfigured the worker still refuses to phone home.

## Sandbox boundary in detail

| Surface | Policy | Why |
|---|---|---|
| `fs_read` | weights_dir, venv_dir | model load + Python interpreter resolution |
| `fs_write` | none | worker is stateless; `TMPDIR` already provided by `core::workspace` per-process scratch |
| `net` | `Net::Deny` | offline inference; `HF_HUB_OFFLINE` enforces inside Python too |
| `cpu_ms` | 0 (disabled) | `setrlimit(RLIMIT_CPU)` is cumulative-process; would kill a warm worker after the first few inferences. cgroup `cpu_quota_pct` covers rate-limiting. Per-request hang detection is dispatcher work, deferred per worker-lifecycle spec |
| `mem_mb` | 4_096 | room for multi-v1.0 (~2-3 GB) + headroom; large-v0.5 (~4-5 GB) needs mem_mb bumped to 6_144 in its manifest variant |
| `cpu_quota_pct` | Some(400) | 4 cores; inference is compute-bound on CPU. CUDA path mostly bound by GPU |
| `tasks_max` | Some(64) | PyTorch spawns helper threads; 64 is generous |
| `profile` | `Profile::WorkerStrict` | no network syscalls needed |
| `env` | see above | minimal; offline mode toggles + paths + lockdown signals |
| `wall_clock_ms` | `None` | warm workers are long-lived by design; `lifecycle.max_age_seconds` is the rotation budget. wall_clock_ms applies to the whole process lifetime (shell-exec uses 30 s — single-use semantics, wrong shape for `idle_timeout`) |

A poisoned warm GLiNER-Relex worker can read its weights, occupy its sandbox, and (failing the `Net::Deny` boundary) reach nothing else. Per-request statelessness (the `Contract { stateless: true }` declaration) is enforced by code review at PR time — the worker's Python source must scope all per-request mutable state to the request handler's local variables, never module globals.

**Per-request hang detection is genuinely out of scope.** The worker-lifecycle policy spec explicitly punts it to the JSON-RPC dispatcher and notes it's a future slice. If a single inference wedges, today's behaviour is: cgroup CPU throttling caps the burn rate, `mem_mb` caps the memory blast, but the request stays pending until the next dispatch attempt times out at the protocol level (the JSON-RPC client has no per-request deadline yet either). The mitigation is "operator notices via audit log + restarts the daemon." Adding a per-request timeout is a follow-up slice that affects every worker, not just GLiNER-Relex.

## Operator setup

New shell script: `scripts/workers/gliner-relex/install.sh`. Idempotent; safe to re-run.

```sh
#!/usr/bin/env bash
set -euo pipefail

# Pre-flight: require uv on PATH
command -v uv >/dev/null 2>&1 || {
  echo "error: uv is required (install: https://github.com/astral-sh/uv)"
  exit 1
}

# Pre-flight: require hf on PATH (huggingface-cli)
command -v hf >/dev/null 2>&1 || command -v huggingface-cli >/dev/null 2>&1 || {
  echo "error: hf or huggingface-cli is required"
  exit 1
}

REPO_ROOT="$(git rev-parse --show-toplevel)"
WORKER_DIR="$REPO_ROOT/workers/gliner-relex"
DATA_DIR="${KASTELLAN_DATA_DIR:-$HOME/.local/share/kastellan}"
WEIGHTS_DIR="$DATA_DIR/workers/gliner-relex/weights"

# 1. uv sync — creates .venv with pinned deps
(cd "$WORKER_DIR" && uv sync)

# 2. ensure weights directory
mkdir -p "$WEIGHTS_DIR"

# 3. download multi-v1.0 (default model)
hf download knowledgator/gliner-relex-multi-v1.0 \
  --local-dir "$WEIGHTS_DIR/multi-v1.0"

# 4. (optional, opt-in) download large-v0.5
if [ "${KASTELLAN_GLINER_RELEX_INSTALL_LARGE:-0}" = "1" ]; then
  hf download knowledgator/gliner-relex-large-v0.5 \
    --local-dir "$WEIGHTS_DIR/large-v0.5"
fi

# 5. license-chain sanity check
test -f "$WEIGHTS_DIR/multi-v1.0/config.json" || {
  echo "error: model card files not found; download failed"
  exit 2
}

echo "ok: gliner-relex weights at $WEIGHTS_DIR"
echo "ok: venv at $WORKER_DIR/.venv"
```

Daemon startup behaviour on missing weights:

- **Default posture: fail-closed.** If `KASTELLAN_GLINER_RELEX_ENABLE=1` is set and the weights directory is missing, the daemon exits at startup with a structured error pointing at the install script.
- **Opt-out posture: skip-register.** If `KASTELLAN_GLINER_RELEX_ENABLE` is unset (or `0`), the daemon does not register the gliner-relex `ToolEntry` at all. Calls to `gliner-relex` return `UNKNOWN_TOOL` per the existing dispatcher path. This is the default; existing deployments are unaffected by the slice landing.

The flag is *enable*, not *disable*, so accidental opt-in is impossible.

## Slice boundaries

### Slice 1 — Python worker (separate PR)

**Ships:**

- `workers/gliner-relex/` directory with:
  - `pyproject.toml` (project metadata, deps: `gliner>=…`, `transformers>=…`, `sentencepiece`, `onnxruntime` as optional)
  - `uv.lock` (committed; reproducible installs)
  - `src/kastellan_worker_gliner_relex/__init__.py`
  - `src/kastellan_worker_gliner_relex/__main__.py` (entry point)
  - `src/kastellan_worker_gliner_relex/server.py` (stdio loop, JSON-RPC framing)
  - `src/kastellan_worker_gliner_relex/model.py` (model load + extract method)
  - `src/kastellan_worker_gliner_relex/errors.py` (custom error codes + envelope helpers)
  - `tests/test_server.py` (wire-shape, error envelope, label validation; ~10 tests)
  - `tests/test_model.py` (mocked model; entity/triple shape assertions; ~3 tests; the real-model test is the spike + slice-2 e2e)
  - `README.md` (operator install steps, env-var reference)
- `scripts/workers/gliner-relex/install.sh`
- Worker entries in `.gitignore` for `.venv/`, `__pycache__/`, `.pytest_cache/`.

**Smoke test (Linux, manual operator step):**

```sh
cd workers/gliner-relex
KASTELLAN_GLINER_RELEX_WEIGHTS_DIR=$KASTELLAN_DATA_DIR/workers/gliner-relex/weights/multi-v1.0 \
KASTELLAN_GLINER_RELEX_MODEL=knowledgator/gliner-relex-multi-v1.0 \
KASTELLAN_GLINER_RELEX_DEVICE=cuda \
echo '{"jsonrpc":"2.0","id":1,"method":"extract","params":{"text":"Dr Smith treats asthma.","entity_labels":["person","disease"],"relation_labels":["treats"]}}' \
  | uv run kastellan-worker-gliner-relex     # uv-generated console-script shim
```

Expected: stdout carries a single line, valid JSON-RPC response, with at least one entity and one triple.

**What's deliberately NOT in Slice 1:**
- Rust code of any kind.
- Operator-facing `kastellan-cli` command to invoke the worker.
- Lifecycle integration (no manifest entry, no `gliner_relex_entry()`).
- A `cargo test`-runnable integration test (Python tests run via `uv run pytest` in the worker directory, not under cargo).

**Acceptance:**
- `uv run pytest` passes in the worker directory on Linux.
- Manual smoke test produces a valid response.
- README install instructions reproduce on a clean DGX Spark.

### Slice 2 — Manifest + lifecycle wiring + e2e (separate PR)

**Ships:**

- `core/src/workers/mod.rs` + `core/src/workers/gliner_relex.rs` (new module tree; existing `tool_host` stays where it is, and `gliner_relex_entry` lives alongside the future inference-worker manifests).
- `GlinerRelexEnv` builder + `gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry` returning the manifest.
- Wire-shape types: `ExtractRequest`, `ExtractResponse`, `Entity`, `Triple`, all `#[derive(Serialize, Deserialize)]`. These are JSON shape types, not a typed client. They serve two purposes: serde-pin tests on the wire contract, and a future consumer slice can use them as the param/result types without re-deriving.
- Unit tests in `core/src/workers/gliner_relex.rs::tests` (~5 tests): `ExtractRequest`/`ExtractResponse` serialisation pins (match the Python side's wire shape byte-for-byte), label-cap validation, payload-size cap, manifest-shape pins (`Lifecycle::IdleTimeout`, `Contract::stateless == true`, `cpu_ms == 0`, `wall_clock_ms == None`).
- Integration test `core/tests/gliner_relex_e2e.rs` (~3-4 tests): skip-as-pass if venv or weights missing; happy-path round-trip via raw `tool_host::dispatch(pool, handle.worker_mut(), "gliner-relex", "extract", params)` against a real Python worker; warm-reuse pin (2 consecutive calls hit the same warm worker, asserted via slice-2's `_test_slot_has_warm` accessor); error propagation (canned `INVALID_INPUT` from the Python side surfaces as a JSON-RPC error code at the Rust side).
- Daemon wiring: `core::main` registers `gliner_relex_entry` conditionally when `KASTELLAN_GLINER_RELEX_ENABLE=1` AND weights dir exists.
- `HANDOVER.md` + `ROADMAP.md` updates.

**Deliberately NOT in Slice 2 (deferred to the v2 entity-extraction consumer slice):**
- A typed Rust client like `pub async fn extract(handle: &mut WorkerHandle, req: ExtractRequest) -> Result<ExtractResponse, ExtractError>`. The dispatcher's `dispatch_step` flow today calls `handle.report_crash()` between `tool_host::dispatch` and `map_dispatch_result` (slice-2 of worker-lifecycle); any typed client outside that flow has to either duplicate the crash-classifier logic or couple to a lifecycle manager. The v2 entity-extraction consumer is the slice that will discover the right client shape based on where it needs to invoke gliner-relex in the agent's plan/recall pipeline — speculating about that here would lock in the wrong shape.

**Cross-platform posture:**
- Linux: full implementation + integration test.
- macOS: documented gap. The MPS device path is added in a follow-up session on Apple Silicon hardware. Slice 2's manifest accepts `device=mps` only on macOS — Linux rejects with `UNSUPPORTED_DEVICE` at startup. The Python worker code can be `device='cuda' if available else 'cpu'`-only initially; the MPS branch is the macOS follow-up's first task.

**Acceptance:**
- `cargo test --workspace` stays green on Linux with the gliner-relex tests running (when venv + weights present) and skip-as-pass (when not).
- The slice-2 integration test proves warm-reuse: two consecutive `tool_host::dispatch(...)` calls against the same lifecycle handle hit the same warm worker, confirming the lifecycle abstraction is wired correctly.
- Daemon startup with `KASTELLAN_GLINER_RELEX_ENABLE=1` succeeds on a properly-installed host; fails-closed with a structured error on missing weights.
- Daemon startup with the env unset is byte-equivalent to today (no behaviour change).

## Linux-first + macOS gap

This session targets Linux only. The macOS follow-up needs to validate:

1. **MPS device fallback.** `gliner/model.py`'s device-selection logic branches CPU/CUDA upstream. Manual `model.to("mps")` should work because DeBERTa-v2 ops are MPS-supported, but the spike (when the operator is on macOS) must confirm. `PYTORCH_ENABLE_MPS_FALLBACK=1` may be required for ops the MPS backend doesn't support yet.
2. **Weight path canonicalisation.** macOS Seatbelt requires absolute, canonicalised paths in `fs_read`. `~/Library/Caches/huggingface/` and friends need the existing `linux_bwrap`-style up-front canonicalisation step.
3. **Sandbox-policy parity.** The `cpu_quota_pct` and `tasks_max` cgroup fields are Linux-only; macOS Seatbelt has no equivalent, so the manifest sets them but the macOS backend ignores them silently (existing slice-2 behaviour from PR #56).
4. **Worker binary discovery.** The `python_binary` path resolves under `workers/gliner-relex/.venv/bin/python` on both platforms (uv venv layout is identical).
5. **Spike on macOS hardware.** Half-day budget per the feasibility study; measure cold-start, warm-call latency, and quality on representative memory bodies. If MPS ops fail, CPU fallback is the documented escape hatch (slower but acceptable for a write-path operation).

The macOS follow-up is a separate plan slice; this spec does not pre-stub the MPS device branch.

## Spike strategy

The throwaway POC is **not** the full worker. It is the smallest thing that de-risks the spec before any code lands. Lives under `scripts/spike/gliner-relex/`, deleted after the spike notes are written.

**What the spike must demonstrate (this session, Linux DGX):**

1. **Install path holds.** `uv sync` + `gliner` installs without dependency surprises (e.g. CUDA wheel resolution).
2. **License chain holds.** Model card URL contains `knowledgator/gliner-relex-` per the feasibility study's load-bearing check; the downloaded `config.json` references the expected base encoder.
3. **Model load works on CUDA on the DGX Spark.** No segfaults, no missing-op errors.
4. **Sample inference works.** Three representative memory bodies (technical English, mid-length, agent-like content) produce sensible `(entities, triples)` for a thoughtful relation vocabulary.
5. **Latency is acceptable.** Cold-start once + warm-call ×10 to confirm the `idle_timeout` design intent. Target: warm call < 200 ms on CUDA; cold-start < 30 s.
6. **The relation vocabulary I picked produces non-trivial output.** If every relation comes back below threshold, the vocabulary is the wrong one and the v2 consumer spec needs to revisit.

**Output:** `docs/superpowers/specs/2026-05-18-gliner-relex-spike-notes.md` records measurements + observed surface strings + any surprises that should feed back into the spec.

**Spike script shape:**

```python
# scripts/spike/gliner-relex/spike.py — throwaway
from gliner import GLiNER
import time

model = GLiNER.from_pretrained(
    "knowledgator/gliner-relex-multi-v1.0",
    local_files_only=True,
    cache_dir="<weights_dir>"
)

samples = [
    "Dr Smith treats asthma in his Mosman clinic.",
    "The Rust workspace under kastellan uses uv-managed Python venvs per worker.",
    "PostgreSQL migration 0008 added the deleted_memories AFTER DELETE trigger.",
]

entity_labels = ["person", "organization", "location", "disease", "technology", "concept"]
relation_labels = ["treats", "located_in", "uses", "added", "depends_on"]

# cold start
for s in samples:
    t = time.perf_counter()
    result = model.predict_relations(s, labels=entity_labels, relations=relation_labels, threshold=0.5)
    print(time.perf_counter() - t, result)

# warm loop
for _ in range(10):
    for s in samples:
        t = time.perf_counter()
        result = model.predict_relations(s, labels=entity_labels, relations=relation_labels, threshold=0.5)
        print(time.perf_counter() - t)
```

The spike does NOT exercise the JSON-RPC stdio loop, the sandbox, or the Rust client. Those land in Slice 1 / Slice 2. The spike validates the model is what we think it is and the wire shape can carry the kind of output we expect — nothing more.

## Testing strategy

| Surface              | Tool          | What                                                                                |
|----------------------|---------------|-------------------------------------------------------------------------------------|
| Python unit          | `pytest`      | wire-shape, error envelope, label validation; mocked model — ~10 tests              |
| Python smoke (manual) | shell echo   | model load + one real inference; operator-runnable; not in CI                       |
| Rust unit            | `cargo test`  | request/response serde pins, manifest constants, error-code mapping — ~5 tests       |
| Rust integration     | `cargo test`  | spawn real Python worker; happy path + warm-reuse + error propagation — ~3-4 tests; skip-as-pass without venv/weights |
| Lifecycle integration | `cargo test` | leverages slice-2's `_test_slot_has_warm` accessor; existing `worker_lifecycle_idle_timeout_e2e.rs` pattern |

Total: ~8-9 new Rust tests (Slice 2: ~5 unit + ~3-4 integration) + ~13 new Python tests (Slice 1; not in cargo count). Workspace cargo count moves 751 → ~759.

## Open questions (for the planning slice, not this design slice)

1. **Default `entity_labels` and `relation_labels` for the consumer.** Out of scope for the worker — the consumer (v2 entity extraction) picks. The spike uses a generic set (`person`, `organization`, `location`, `date`, `technology`, `disease`, `medication`; relations `mentions`, `treats`, `located_in`, `uses`, `affects`) to validate the worker, not to lock in vocabulary.
2. **Whether to ship `large-v0.5` alongside `multi-v1.0` in the v1 install.** Spec says it's manifest-configurable; install script gates `large-v0.5` behind `KASTELLAN_GLINER_RELEX_INSTALL_LARGE=1`. If the spike shows `multi-v1.0` is enough, the large variant may stay opt-in indefinitely.
3. **PyTorch wheel choice on the DGX Spark.** CUDA toolkit version detection happens at `uv sync`. If multiple PyTorch wheel variants are available, the install script may need a `--index-url` flag for the right CUDA version. Spike will surface this if it's a real issue.
4. **macOS spike sequencing.** Follow-up session on Apple Silicon hardware. Spec does not pre-commit to a date.

## Next slice

A planning slice consumes this spec and produces an implementation plan with TDD-ordered tasks, mirroring `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-2.md`. The plan should cover Slice 1 (Python worker) in detail; Slice 2 (Rust client + lifecycle wiring) can be covered at a coarser grain since its structure mirrors `shell_exec_entry` + `tool_dispatch` precedent. The plan is followed by a Linux POC spike (this session) that informs any spec revisions before Slice 1 implementation begins in a future session.
