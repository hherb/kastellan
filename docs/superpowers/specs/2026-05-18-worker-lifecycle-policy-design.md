# Worker Lifecycle Policy — Design Spec

**Status:** design, not yet planned
**Author:** Horst Herb + Claude Opus 4.7
**Date:** 2026-05-18
**Companion docs:**
- `docs/superpowers/specs/2026-05-18-entity-extraction-graph-lane-design.md` (v1; the slice that exposed this gap)
- `docs/threat-model.md` (the per-worker sandbox invariant this spec preserves)

## Why now

The only worker that exists today, `workers/shell-exec`, spawns per request, executes one argv, and exits. The lifecycle is implicit in the binary's behaviour — there is no explicit lifecycle abstraction in the core. That has worked because shell-exec's startup cost is microseconds and there is no state worth preserving across calls.

This breaks the moment we add an inference worker. The GLiNER-Relex feasibility study (`vNEXT` note next to the entity-extraction v1 spec) measured the model at ~1.3 GB on disk and ~2-3 GB resident at fp32 inference, with cold-start dominated by `transformers` model load. Spawn-per-request would pay that cost on every memory write. The same shape recurs for sentiment, embedding, classification, OCR, and any future small-model worker — none of which want to be cold-started 50 times an hour.

The right abstraction is **lifecycle policy declared per worker type**, with the `kastellan-supervisor` crate (currently a stub) growing into the role of "manages worker processes." This spec defines the policy enum, the manifest schema additions, the cap semantics, the stateless contract, and the migration story for shell-exec.

## What this spec defines

1. The `Lifecycle` policy enum: `single_use`, `idle_timeout`. (Pool support is deferred — see §"What this spec does NOT do".)
2. The `[lifecycle]` manifest section worker authors fill in.
3. The `[contract]` manifest section that declares per-request statelessness.
4. The **cap-check semantics**: when each cap fires, what action it triggers, and the load-bearing invariant that no cap is ever a mid-flight kill.
5. The supervisor's responsibilities for `idle_timeout` workers: spawn-on-demand, idle teardown, restart-on-crash, request routing.
6. The security model: how `stateless = true` workers preserve the threat-model invariant despite long uptimes.
7. Migration: shell-exec stays `single_use` with zero behaviour change.

## What this spec does NOT do

- **No pool support.** Concurrent throughput via N warm workers is deferred; today's workload is single-user, single-task at a time. When a real throughput bottleneck appears, `pool` slots in as a third policy variant without touching `single_use` or `idle_timeout`.
- **No per-request timeouts.** A worker hanging mid-request is a separate concern (handled by the JSON-RPC dispatcher, not the lifecycle layer). This spec does not introduce request-level deadlines.
- **No new IPC layer.** The supervisor is a crate the core links against, not a separate process. The existing JSON-RPC-over-stdio contract from `kastellan-protocol` is unchanged.
- **No sandbox-policy changes.** Every worker still runs under its own `SandboxBackend` with its own `SandboxPolicy`. A long-lived worker has the same sandbox as a short-lived one.
- **No GLiNER-Relex implementation.** That's the next slice. This spec is the prerequisite the next slice consumes.

## The two policies

### `single_use`

> Spawn → serve one request → exit. The current shell-exec behaviour.

- Each request gets a fresh process, fresh sandbox, fresh model load (if any).
- The right policy for **truly transient operations** where per-request isolation is the security model itself: shell-exec, file ops, allowlisted subprocess calls.
- Caps don't apply (the process exits after one request by construction).

### `idle_timeout`

> Spawn on first request, stay alive, tear down after a configurable idle window.

- The right policy for **stateless inference workers** with non-trivial startup cost: GLiNER-Relex, sentiment, embedding, classification, OCR, audio transcription.
- The supervisor holds a single live process per worker type. Subsequent requests reuse it via the same JSON-RPC stdio pair.
- Three caps apply (all post-completion-only):
  - `idle_seconds`: tear down after no in-flight or queued request for N seconds.
  - `max_requests`: rotate after serving N requests cumulative (hygiene against slow memory leaks).
  - `max_age_seconds`: rotate after the process has been alive for N seconds (hygiene against drift).

## Cap-check semantics (load-bearing)

**No cap is ever a mid-flight kill.** This is the invariant that makes `idle_timeout` safe to enable on any stateless inference worker.

The supervisor evaluates caps at exactly **one point in the worker's lifecycle**: immediately after a request completes (the JSON-RPC response has been written to the worker's stdout and read by the core).

Pseudocode for the post-completion check:

```text
on request_complete(worker):
    worker.request_count += 1
    worker.last_completion_at = now()

    if worker.request_count >= manifest.max_requests:
        graceful_shutdown(worker, reason="max_requests")
        return

    if (now() - worker.spawned_at) >= manifest.max_age_seconds:
        graceful_shutdown(worker, reason="max_age_seconds")
        return

    # idle_seconds is checked on a separate timer, also at quiescent state
    schedule_idle_check(worker, in=manifest.idle_seconds)

on idle_check_fires(worker):
    if worker.has_in_flight_request():
        # NEVER interrupt — re-arm the timer, re-evaluate after completion
        return
    if (now() - worker.last_completion_at) >= manifest.idle_seconds:
        graceful_shutdown(worker, reason="idle_timeout")
```

**Graceful shutdown** is SIGTERM + a configurable grace period (default 5 s), then SIGKILL. The worker is expected to close file handles, flush logs, and exit. No in-flight request can be active when this fires — the supervisor only invokes shutdown from the quiescent state.

**Mid-flight termination** happens in exactly two cases, both outside this spec's scope but called out for completeness:
1. **Daemon shutdown.** The supervisor's own teardown sends SIGTERM to all workers regardless of state; per-request work in flight is lost. This is the existing process-tree shutdown behaviour.
2. **Crash recovery.** If the OS reports the worker process as dead (SIGCHLD), the supervisor restarts it. Any in-flight request fails with a retry-safe error code on the JSON-RPC channel; the caller decides whether to retry. This is no different from today's shell-exec behaviour when a child segfaults.

Per-request hang detection (a request running far longer than expected) belongs to the JSON-RPC dispatcher, not the lifecycle layer. The dispatcher's per-request timeout (already implicit in shell-exec; explicit in inference workers) is what catches a wedged forward pass — not `max_age_seconds`.

## The stateless contract

The threat-model invariant from `docs/threat-model.md` says a worst-case worker compromise reaches at most the worker's own OS user, sandbox, and the endpoints explicitly allowlisted for that one worker. **Nothing in this spec changes that.** Each worker still has its own sandbox; a poisoned GLiNER-Relex process cannot reach the embedding worker, the LLM router, or core memory — same as today.

What's new: a long-lived worker serves multiple requests, so we need a separate invariant — **no per-request data crosses request boundaries inside one warm worker.**

This is a *contract declared by the worker author*, expressed in the manifest:

```toml
[contract]
stateless = true   # per-request: context in, result out, no cross-request memory
```

A worker that declares `stateless = true` is asserting:

- Inference inputs are scoped to the call (request-local Python variables, not module-level globals).
- No per-request value (input text, intermediate tensors, output) is cached, logged at INFO+, or written to persistent storage.
- The model weights are read-only after load; no fine-tuning, no LoRA-adapter swap, no embedding-table append.
- Any reusable structures (tokenizer, model object, CUDA context) are request-agnostic and depend only on static configuration.

Code review on a `stateless = true` worker checks these properties at PR time. The declaration is a contract, not a runtime enforcement primitive — but it makes the contract a structural part of the manifest rather than a tribal-knowledge expectation.

If a future worker genuinely needs cross-request state (sliding-window aggregator, conversation buffer), it declares `stateless = false` and gets a different security review: explicit per-key isolation, bounded retention, etc. The `stateless = false` path is **not in scope for this spec** — it requires its own threat analysis. The `idle_timeout` policy in v1 is restricted to `stateless = true` workers.

For shell-exec (`single_use`), the contract field is irrelevant — there is no "next request" in the same process.

## Manifest schema additions

Workers today have an implicit "manifest" that lives in the worker binary's argv allowlist env var + the core's `tool_host` registration code. This spec proposes formalising a per-worker TOML manifest as the canonical source of policy.

Strawman shape (subject to refinement during implementation):

```toml
# workers/gliner-relex/manifest.toml
name = "gliner-relex"
binary = "kastellan-worker-gliner-relex"

[lifecycle]
kind = "idle_timeout"
idle_seconds = 600            # tear down after 10 minutes of inactivity
max_requests = 10000          # rotate after 10k requests (slow-leak hygiene)
max_age_seconds = 86400       # rotate daily (hygiene)
grace_period_seconds = 5      # SIGTERM → wait → SIGKILL

[contract]
stateless = true              # per-request stateless; required for idle_timeout

[sandbox]
# ... existing SandboxPolicy fields ...
```

```toml
# workers/shell-exec/manifest.toml
name = "shell-exec"
binary = "kastellan-worker-shell-exec"

[lifecycle]
kind = "single_use"
# caps + contract are inapplicable

[sandbox]
# ... existing SandboxPolicy fields ...
```

Manifest discovery, registration, and parsing are an implementation question for the slice that lands this spec — out of scope here. The shape above is what the supervisor needs to honour, not necessarily the on-disk file format.

## Supervisor responsibilities

The `kastellan-supervisor` crate, currently a stub, grows the following responsibilities for `idle_timeout` workers:

1. **Spawn-on-demand.** First incoming request for a worker type whose process is not running triggers a spawn under the worker's `SandboxPolicy`. Subsequent requests reuse the live process.
2. **Request serialisation.** A single warm worker serves requests one at a time (no in-process concurrency in v1; the worker is single-threaded by contract). The supervisor queues concurrent callers for the same worker. Queue depth and backpressure are an implementation question — the simplest answer is "tokio mpsc, unbounded, since the agent's plan cap bounds concurrency upstream." If contention becomes real, the `pool` policy variant addresses it.
3. **Cap evaluation.** Post-completion checks per §"Cap-check semantics" above.
4. **Crash detection.** SIGCHLD handler / `wait()` poll; restart on death with exponential backoff, max-restart-rate cap to avoid restart loops.
5. **Graceful teardown.** SIGTERM → grace period → SIGKILL on lifecycle eviction and on daemon shutdown.
6. **Health introspection.** Expose `(worker_name, pid, spawned_at, request_count, last_completion_at, state)` for the future `kastellan-cli supervisor status` operator surface.

For `single_use` workers, the supervisor's role degenerates to "spawn one process, wait for exit" — identical to today's `tool_host::dispatch` behaviour. No new code path is needed for the shell-exec migration.

## Security model

Per-worker sandbox isolation is unchanged. The new question is: does a long-lived `stateless = true` worker preserve the threat-model invariant?

**Yes, with two caveats:**

1. **Blast radius per single worker compromise is unchanged.** A compromised warm GLiNER-Relex worker has access to its own sandbox, its own argv allowlist, its own filesystem mount set, its own network policy — the same surfaces a single-use spawn would have had. The `--unshare-all` / `--die-with-parent` / etc. bwrap flags from `linux_bwrap.rs` apply identically. The compromise does not reach other workers, the core, the database, or the LLM router.
2. **Persistence of compromise across requests inside one worker is the new attack surface.** If a worker's process is compromised at request N, request N+1 routed to the same process inherits the compromised state. This is real and is the price of warm-keeping. The mitigations:
   - `max_requests` and `max_age_seconds` caps bound the persistence window. Default values (10k requests, 1 day) are not chosen for security — they are operational-hygiene rotation. An operator running a more conservative posture can tighten them.
   - The `stateless = true` contract reviewed at PR time is the primary defence: a worker that correctly scopes per-request state has no opportunity to leak request N's content into request N+1's output.
   - Crash recovery is symmetric: a compromised worker that crashes is replaced by a fresh one. There is no incentive to keep a crashed worker around.

The trade-off is honest. A `stateless = true` warm worker is materially safer than a `stateless = false` warm worker, and materially less safe than a `single_use` worker. The lifecycle declaration in the manifest makes that trade explicit at the source-tree level — not hidden in implementation comments.

For the immediate use cases (GLiNER-Relex, sentiment, embedding, classification, OCR), the stateless contract is natural — the workers are stateless by their underlying ML semantics. We are not stretching to make `stateless = true` true; it is the obvious design.

## Migration plan

- **shell-exec:** declare `lifecycle.kind = "single_use"` in its new manifest. No code change in the worker binary. The supervisor's `single_use` path is bytewise-equivalent to today's `tool_host::dispatch` behaviour. Migration is a one-line manifest add + the supervisor doing the spawn it used to do.
- **All future inference workers (GLiNER-Relex first):** declare `lifecycle.kind = "idle_timeout"` + `contract.stateless = true` + concrete cap values. The supervisor handles the rest.
- **The L0 / L1 / L2 / L3 / L4 memory layers and the LLM router** are not workers; they are core-side modules. Unaffected.
- **The `kastellan-cli` binary** gains a new operator surface — `kastellan-cli supervisor status` — for inspecting warm workers. This is a separate slice from the supervisor's core implementation; the lifecycle work doesn't block on it.

## Open questions (for the planning slice, not this design slice)

1. **Manifest format and discovery.** TOML on disk vs Rust-const declarations next to the worker binary. The codebase precedent is more Rust-const-heavy; TOML buys nothing if operators don't edit it. Probably a Rust struct in each worker crate, registered by name in the supervisor's startup.
2. **Stdio multiplexing.** A warm worker serving multiple requests through one stdin/stdout pair needs JSON-RPC request IDs to demultiplex responses if we ever allow in-process concurrency. The v1 single-threaded contract sidesteps this — but the JSON-RPC layer already supports IDs, so the path is open if we need it.
3. **Restart backoff parameters.** What's the exponential-backoff base and cap for restart-on-crash? Operators want bounded restart rate; conservative defaults like (1 s, 2 s, 4 s, 8 s, capped at 60 s) work but should be revisited if any worker is observed in real restart loops.
4. **Sandbox re-entry on idle restart.** Each spawn re-enters the sandbox from scratch (same `SandboxPolicy`); this is the existing behaviour, and idle teardown + respawn is just two such cycles. Nothing new to design — call it out so the slice author doesn't second-guess.
5. **CUDA / MPS context reuse.** A warm worker keeping a CUDA context open across requests is a feature, not a bug — that's the point of `idle_timeout`. Confirmed by the GLiNER-Relex feasibility study; no special handling needed.

## Next slice

A planning slice consumes this spec and produces an implementation plan with TDD-ordered tasks, mirroring the structure of `docs/superpowers/plans/2026-05-17-l1-promotion-writer.md`. The lifecycle implementation is independent of the GLiNER-Relex worker itself — the supervisor grows the abstraction, shell-exec migrates as a smoke test of `single_use`, and only then does the GLiNER-Relex worker arrive as the first `idle_timeout` consumer.
