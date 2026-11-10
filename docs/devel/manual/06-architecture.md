# 6 — Architecture primer

This chapter explains how the pieces fit together. You do not need to
understand every line of code; you need to know the overall shape so you can
find the right place to make a change.

---

## The big picture

```
 Your message (Matrix, or the gated email fallback)
      │
      ▼
 Channel worker (sandboxed; JSON-RPC over its stdin/stdout)
      │
      ▼
 Channel bus (in core): NUL check → peer auth → injection screen → audit
      │  enqueue a task row
      ▼
 AGENT CORE scheduler ───── Postgres (tasks, memory, audit, secrets)
      │
      │ formulate plan (LLM call via llm-router)
      ▼
 CASSANDRA plan review (in the scheduler, before anything is spawned)
      │ approve / block / escalate to the operator
      ▼
 tool_host::dispatch()   ◄── the only door into worker processes
      │  worker (spawned under its SandboxPolicy by the lifecycle
      │  manager); secret refs substituted just before the call
      ▼
 Worker process (shell-exec / python-exec / web-fetch / mail / …)
      │  sandboxed by bwrap (+ cgroup) or Seatbelt, optionally a micro-VM
      │  self-sandboxed again by Landlock + seccomp (Linux)
      ▼
 Result back over stdin/stdout (JSON-RPC)
      │  secret scrub → injection screen (catalogue, optional guard model)
      ▼
 Scheduler renders the result for the planner and loops
      │
      ▼
 Final answer → channel bus → reply on the same channel
```

---

## Processes and their roles

### The supervised services

In a real deployment `kastellan-cli install` writes OS-native units
(`systemd --user` on Linux, launchd agents on macOS) from one set of
specs in `supervisor/src/specs.rs`:

| Unit | What it is |
|------|------------|
| `kastellan.target` | The bundle that brings the whole agent up and down (on launchd it names the member set). |
| `kastellan-postgres` | A per-user Postgres cluster. Unix socket only (`listen_addresses=''`), peer auth, no remote access. |
| `kastellan-core` | The `kastellan` daemon. Starts `After=` Postgres; restarts on failure with a backoff. |
| `kastellan-guard` | **Optional.** A `llama-server` serving the guard model on `127.0.0.1`, added only when the operator configures one; core then starts after it. |

The LLM server itself is **not** a member: it is an operator-managed
dependency the core health-checks at boot. Workers are not members either —
the core spawns them on demand.

### Agent core (`kastellan` binary)

The only process that touches Postgres or makes LLM calls (through the
`llm-router` crate). It holds all state, runs the scheduler, reviews plans,
runs the channel bus, and manages every worker's lifetime.

### Worker processes

Each tool call runs in a worker: its own OS process in its own sandbox. Workers
never talk to Postgres, never talk to each other, and never call the LLM. They
speak JSON-RPC over their own stdin/stdout.

A worker's **lifecycle** is declared on its registry entry: single-use (spawn
per call, the default), idle-timeout (kept warm between calls, retired when
idle), or persistent (long-lived and respawned on death by the
`PersistentWorker` supervisor — the Matrix channel worker is the main
example).

Each worker is sandboxed by bwrap (Linux) or Seatbelt (macOS); a worker can
also opt into a **micro-VM backend** (Firecracker on Linux, Apple `container`
on macOS). See [chapter 7](./07-sandboxing.md).

### Sidecars

- **Egress proxy.** A worker that needs the network gets its own sandboxed
  egress-proxy sidecar. Force-routing is on by default in the supervised
  deployment: the worker runs in a private network namespace whose only route
  out is the proxy's Unix socket, and the proxy enforces the host:port
  allowlist, an SSRF guard, a credential-leak scan and certificate pins.
  Workers never reach the network directly.
- **Brokers.** The trusted `embed-broker` and `search-broker` sidecars let a
  jailed worker reach an embedding or search backend through a bound socket
  without widening its egress. They are the deliberate worker-side exceptions
  to "only the core talks to backends".

---

## The scheduler loop

Tasks are rows in the Postgres `tasks` table. There are two lanes, **fast**
and **long**, each with its own runner that waits on `LISTEN/NOTIFY`
(`tasks_inserted`, `tasks_cancelled`, `tasks_resumed`) with a 30-second
catch-up poll in case a notification was lost.

```
claim the oldest pending task on this lane
  (UPDATE … FOR UPDATE SKIP LOCKED, with a lease)
  → loop until the task is terminal:
      check the limits (plan count; fast lane: a 15-minute wall-clock ceiling)
      formulate a plan (LLM call via llm-router)
      CASSANDRA review of the plan
         approve   → dispatch each step via the StepDispatcher
                       → tool_host::dispatch() per step
         block     → the reason goes back to the planner, which replans
         constitutional block → the task ends blocked
         escalate  → an operator ask is raised; the task waits in
                     awaiting_operator (answered over the channel or with
                     `kastellan-cli inbox`)
      render each step's result for the next plan
  → write the terminal state + audit rows, purge the handoff cache
```

The plan-count caps default to 20 (fast) and 25 (long). On the fast lane the
binding limit is time; the count is a backstop. When a limit is hit and
something useful was gathered, the planner gets one forced-synthesis turn to
answer from what it has.

**Crash safety.** At startup the daemon sweeps every `running` task whose lease
has expired to `crashed` and writes a `task.crashed` audit row for each. A
crashed task is not silently retried.

### The planner loop: the task notebook

The planner keeps its own working memory in a per-task notebook
(`task_notebook` table): it writes a step list, works one step per turn, and
ticks each step off with the facts it found. A `sift` step runs one read over
every item in a list and keeps the matches. (The older `classic` loop, which
re-read a growing history of its plans every turn, retired in #993;
`KASTELLAN_PLANNER_MODE=classic` stops the daemon at boot.)

Every call goes through one review-and-dispatch path
(`scheduler/inner_loop/execute.rs`), so CASSANDRA, the audit rows and the
dispatcher chokepoint apply to every step. See [chapter 15](./15-planner.md)
for the planner in depth.

### What the planner sees of a result

Two mechanisms keep tool results useful without letting them swamp the
prompt (and the notebook shows only the last call's result each turn):

- **`result_view`** (`scheduler/inner_loop/result_view.rs`) — prunes a JSON
  result to size while keeping its structure: object keys, numbers and
  booleans survive, and short identifier-like strings (ids, URLs, paths) are
  shown whole or not at all, never cut in half. It is a formatting step, not
  a security control — what it produces is screened separately.
- **The handoff cache** (`core/src/handoff.rs`) — a result over 64 KiB is
  stashed in memory, per task, keyed by its SHA-256, and replaced by a small
  placeholder carrying a screened 1 KiB head. The planner pulls slices back
  with the `handoff.fetch` built-in. The cache is purged when the task ends
  and does not survive a daemon restart.

---

## IPC: JSON-RPC 2.0 over stdio

Workers and the core communicate using the
[Model Context Protocol (MCP)](https://modelcontextprotocol.io) wire format:
newline-delimited JSON-RPC 2.0 messages over stdin/stdout. This is the only
IPC mechanism between the core and its workers.

The `kastellan-protocol` crate provides typed `Client` and `Server` objects.
Rust workers import `kastellan-worker-prelude`, whose `serve_stdio` installs
the worker-side self-sandbox and then starts serving JSON-RPC requests.

You do not need to understand the wire protocol to add a feature; just use
the existing helper types.

---

## The channel bus

The channel bus (`core/src/channel/`) is the transport-agnostic boundary
between a messaging channel and the task queue. Each channel is a `Channel`
implementation in core driving a sandboxed worker:

- **Matrix** (primary) — the `matrix` worker runs `matrix-rust-sdk`
  (self-hosted homeserver, single user, federation off, end-to-end
  encrypted). Live.
- **Email** (fallback) — the `email-in` worker polls a local mail archive.
  Config-gated and off by default. A message becomes a task only with a DMARC
  verdict from the operator's own MX **and** a per-pairing token in the body.
  **Inbound only:** replies to email are routed and audited but not yet
  delivered (no SMTP sending yet).

Every inbound message passes the same steps before it can become a task:
a NUL check (a NUL in a channel, peer or conversation id refuses the message;
one in the body is escaped), fail-closed peer authentication (an unpaired peer
is dropped and audited; pairing uses single-use codes issued by the operator),
and the same prompt-injection screen that worker output gets. Every received,
rejected, enqueued and replied message is audited. When a task completes, a
pump routes the answer back to the channel and conversation it came from.

See [chapter 16](./16-channels.md) for the channels in depth.

---

## Memory and recall

Memories live in the `memories` table in five layers (`MemoryLayer`):

- **L0 — meta-rules.** Hard constraints from an operator-edited TOML file,
  seeded by `memory::l0_seed` and placed in every system prompt. The agent
  cannot write L0.
- **L1 — insight index.** Small routing pointers; the newest ones (up to
  32 rows / 4 KiB) go into every system prompt.
- **L2 — stable facts.** The default layer.
- **L3 — skills.** Successful trajectories crystallise into reusable
  tool-call templates (or agent-authored Python). They start `untrusted` and
  reach the prompt only after operator approval. See chapter 12.
- **L4 — session digests.** Reserved; nothing writes it yet.

`core::memory::recall()` runs up to four lanes — **semantic** (pgvector over
256-dimension embeddings), **lexical** (`tsvector` + `ts_rank`), **graph**
(entity neighbour walk) and **entity-similarity** — and fuses them with
Reciprocal Rank Fusion. Recall returns L1, L2 and L4 only: L0 and L3 reach the
prompt through their own gated blocks, never through recall. See
[chapter 12](./12-memory-and-recall.md).

---

## The audit log

Tool calls, plan formulations and verdicts, memory writes, egress decisions,
secret redemptions, channel messages and task-state transitions all write
rows to `audit_log`. Every write goes through one function,
`kastellan_db::audit::insert`.

- **Append-only, enforced by the database.** The runtime Postgres role has
  `INSERT` and `SELECT`; `UPDATE`, `DELETE` and `TRUNCATE` are revoked.
- **Bounded.** Payloads are capped at 4 KiB. An oversized payload is replaced
  by a SHA-256 fingerprint and its length, keeping a short summary of the
  request and a few preserved keys (such as the task id and the outcome of a
  control that ran).
- **NUL-safe.** Postgres cannot store U+0000. NULs in a payload, actor or
  action are escaped to `␀` before anything else, and the row records how many
  were escaped — so a hostile string cannot make its own audit row fail.
- **Losses are reported, but thinned.** If a row cannot be written (shed under
  load, or a failed insert) the daemon says so on stderr with an
  `[audit-lost]` marker or a tracing error. In a flood it reports the first
  16 of a burst, then only every 2^k-th, each line carrying the running count;
  a new cause of failure always gets its own line, and shutdown reports what
  went unsaid (`core/src/audit_thinning.rs`).

A background task mirrors every committed row to a date-named JSONL file,
`~/.local/state/kastellan/audit-YYYY-MM-DD.jsonl`. `kastellan-cli audit tail`
reads those files, so it works even when Postgres is down.

---

## CASSANDRA — screens on both sides of the worker call

CASSANDRA sits at two points:

1. **Plan review (before anything is spawned).** In the scheduler, the
   constitutional and deterministic (data-classification) stages inspect each
   formulated plan. A block or an escalation stops the plan before any worker
   is spawned.
2. **Output screen (after the call).** Inside `tool_host::dispatch()`, after
   the worker returns, `injection_guard` scores the result body against a
   substring catalogue. A score at or above `BLOCK_THRESHOLD` (0.70) replaces
   the result with a redacted placeholder and writes a `policy /
   injection.blocked` audit row carrying the SHA-256 of the body, never the
   raw bytes. When the operator has configured a guard model
   (`kastellan-guard`), results the catalogue allowed are also scored by it;
   it can only turn an allow into a block, never the reverse.

The planner-facing summary is screened once more before it reaches the
prompt. The review stages are deterministic rules today; the LLM-backed review
stages are not built yet. See [chapter 11](./11-cassandra-pipeline.md) for the
full pipeline.

---

## Where to add a new feature

| Type of feature | Where to start |
|-----------------|---------------|
| New tool worker (Rust) | New crate under `workers/`; add to `[workspace.members]`; implement `WorkerManifest` in `core/src/workers/<name>.rs`; add one line to `WORKER_MANIFESTS` in `core/src/registry_build.rs` — no `tool_host.rs` edit |
| New CLI subcommand | New file under `core/src/bin/kastellan-cli/`; register in `main.rs` |
| New DB table | New migration in `db/migrations/`; new helpers in `db/src/` |
| New CASSANDRA plan-review rule | Extend `core/src/cassandra/constitutional.rs` or `deterministic.rs` |
| New injection pattern | Add an entry to the catalogue in `core/src/cassandra/injection_guard.rs` |
| New recall lane | New lane in `core/src/memory/recall.rs` and wire into `recall()` |
| New LLM backend | Implement in `llm-router/src/backend.rs`; gate selection in `policy.rs` |
| New channel | Worker crate under `workers/` + a `Channel` impl in `core/src/channel/`; wire into the `ChannelBus` |
