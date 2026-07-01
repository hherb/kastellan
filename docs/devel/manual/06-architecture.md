# 6 — Architecture primer

This chapter explains how the pieces fit together. You do not need to
understand every line of code; you need to know the overall shape so you can
find the right place to make a change.

---

## The big picture

```
 Your message
      │
      ▼
 Channel adapter (Matrix E2E · email failover)
      │  JSON-RPC over Unix socket
      ▼
 AGENT CORE  ───── Postgres (memory, tasks, audit, secrets)
      │
      │ formulate plan
      ▼
 CASSANDRA review pipeline
      │ approve / block
      ▼
 tool_host::dispatch()   ◄── the only door into worker processes
      │
      │ spawn worker, pass WorkerCommand
      ▼
 Worker process (shell-exec / python-exec / web-fetch / …)
      │  sandboxed by bwrap or sandbox-exec
      │  self-sandboxed again by Landlock + seccomp
      ▼
 Result returned over stdin/stdout (JSON-RPC)
      │
      ▼
 Agent core continues the plan loop
```

---

## Processes and their roles

**Agent core** (`kastellan` binary)  
The only process that touches Postgres or makes LLM calls. It holds all state,
schedules work, reviews plans, and manages the lifetime of worker processes.

**Worker processes** (one per tool invocation)  
Short-lived processes that execute a single tool call and exit. They never
talk to Postgres. They never talk to each other. They speak JSON-RPC over
their own stdin/stdout. Each is sandboxed by bwrap (Linux) or Seatbelt
(macOS); a worker can also opt into a **micro-VM backend** (Firecracker on
Linux, Apple `container` on macOS) for hardware-level isolation, and a
long-lived worker can be kept alive across calls and respawned on death by the
`PersistentWorker` supervisor. A worker that needs network egress gets its own
sandboxed **egress-proxy sidecar** (force-routed on by default): the worker
runs in a private network namespace whose only route out is the proxy's Unix
socket, which enforces the host:port allowlist + SSRF guard. Workers never
reach the network directly.

**Postgres** (long-lived, local, Unix socket only)  
Stores memories, tasks, the audit log, entity/relation graph, and secrets.
No remote access. Peer auth only.

**LLM server** (local or frontier, via HTTP)  
The core calls it via the `llm-router` crate. Workers never call it.

---

## The scheduler loop

```
tick
  → claim next task from DB (FOR UPDATE SKIP LOCKED)
  → formulate plan (LLM call via llm-router)
  → CASSANDRA pre-spawn review:
       Stage -1 constitutional → Stage 0 deterministic → …
  → if approved: dispatch each step via tool_host::dispatch()
       inside dispatch: spawn under SandboxPolicy
                     → worker.call()
                     → injection_guard::screen(result)   ← post-call check
                     → on Block: redacted placeholder + audit row
                     → on Allow: hand result back to scheduler
  → if blocked at any stage: mark task failed, write reason to audit log
  → advance task state → loop
```

Multiple lanes run in parallel (each lane is a Postgres LISTEN/NOTIFY loop
watching its own task queue). A crash mid-plan is safe: the lease on the task
expires and another scheduler tick reclaims it.

---

## IPC: JSON-RPC 2.0 over stdio

Workers and the core communicate using the
[Model Context Protocol (MCP)](https://modelcontextprotocol.io) wire format:
newline-delimited JSON-RPC 2.0 messages over stdin/stdout.

The `kastellan-protocol` crate provides typed `Client` and `Server` objects.
Workers import `kastellan-worker-prelude` which wraps `serve_stdio` — the
single function that installs the self-sandbox and then starts serving
JSON-RPC requests.

You do not need to understand the wire protocol to add a feature; just use
the existing helper types.

---

## Memory and recall

Memories live in layers under `core::memory`:

- **L0 — raw observations.** Every incoming channel event, every tool
  result the scheduler decides to remember, is seeded into L0 by
  `memory::l0_seed`.
- **L1 — promoted memories.** `memory::l1_promote` distills L0 rows into
  longer-lived L1 memories with embeddings + tsvector + entity links.
  Recall queries hit L1.
- **L3 — skills.** Successful trajectories crystallise into reusable
  JSON-RPC tool-call templates (or verbatim agent-authored Python),
  promoted through a trust lifecycle (untrusted → user-approved → pinned)
  before they can be recalled and re-invoked. See chapter 12.

L1 memories are addressable via three lanes:

1. **Semantic** — pgvector ANN search on the 256-dimension embedding
   produced by `memory::embed_query` (which Matryoshka-truncates the
   model's native output to 256 and routes through `llm-router`).
2. **Lexical** — native Postgres `tsvector` full-text search with `ts_rank`.
3. **Graph** — entity-keyed neighbour walk over the `entities`/`relations`
   tables, seeded from entities mentioned in the query.

`core::memory::recall()` runs the requested lanes, then fuses the ranked
lists with Reciprocal Rank Fusion (RRF) to produce a single ordered set
of relevant memories. See [chapter 12](./12-memory-and-recall.md) for the
fusion algorithm and lane-specific tuning knobs.

---

## The audit log

Every tool call, LLM call, memory write, and task-state transition writes one
row to `audit_log`. This table has `INSERT` and `SELECT` granted to the runtime
Postgres role but `UPDATE`, `DELETE`, and `TRUNCATE` are revoked — rows are
immutable once written, enforced by the database.

A background task mirrors every row to a date-named JSONL file under
`~/.local/state/kastellan/audit-YYYY-MM-DD.jsonl`. You can tail this file
directly without a running daemon.

---

## CASSANDRA — two screens, not one

CASSANDRA has historically been described as a pre-spawn review of agent
plans. As of the worker-output prompt-injection guard slice
(`core::cassandra::injection_guard`, shipped May 2026) it now operates at
**two points** in the dispatcher chokepoint:

1. **Pre-spawn (plan review)** — the constitutional and deterministic
   stages inspect each `PlannedStep` before a worker is spawned. A
   `Verdict::Block` short-circuits the task.
2. **Post-call (output screen)** — after `worker.call()` returns Ok,
   `injection_guard::screen` runs a substring catalogue over the result
   body. A score ≥ `BLOCK_THRESHOLD` (0.70) replaces the result with a
   redacted placeholder and writes a second audit row with the SHA-256
   of the scanned body — never the raw bytes.

Both screens live inside `tool_host::dispatch()`. There is no path that
skips them. See [chapter 11](./11-cassandra-pipeline.md) for the full
pipeline.

---

## Where to add a new feature

| Type of feature | Where to start |
|-----------------|---------------|
| New tool worker (Rust) | New crate under `workers/`; add to `[workspace.members]`; spawn from `core/src/tool_host.rs` |
| New CLI subcommand | New file under `core/src/bin/kastellan-cli/`; register in `main.rs` |
| New DB table | New migration in `db/migrations/`; new helpers in `db/src/` |
| New CASSANDRA pre-spawn rule | Extend `core/src/cassandra/constitutional.rs` or `deterministic.rs` |
| New injection pattern | Add an entry to the catalogue in `core/src/cassandra/injection_guard.rs` |
| New recall lane | New lane in `core/src/memory/recall.rs` and wire into `recall()` |
| New LLM backend | Implement in `llm-router/src/backend.rs`; gate selection in `policy.rs` |
| New channel | Worker crate under `workers/` + a `Channel` impl in `core/src/channel/`; wire into the `ChannelBus` |
