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
 Channel adapter (Telegram / Signal / IMAP)
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

**Agent core** (`hhagent` binary)  
The only process that touches Postgres or makes LLM calls. It holds all state,
schedules work, reviews plans, and manages the lifetime of worker processes.

**Worker processes** (one per tool invocation)  
Short-lived processes that execute a single tool call and exit. They never
talk to Postgres. They never talk to each other. They speak JSON-RPC over
their own stdin/stdout.

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
  → formulate plan (LLM call)
  → CASSANDRA chain: Stage -1 (constitutional) → Stage 0 (deterministic) → …
  → if approved: dispatch each step via tool_host::dispatch()
  → if blocked: mark task failed, write reason to audit log
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

The `hhagent-protocol` crate provides typed `Client` and `Server` objects.
Workers import `hhagent-worker-prelude` which wraps `serve_stdio` — the
single function that installs the self-sandbox and then starts serving
JSON-RPC requests.

You do not need to understand the wire protocol to add a feature; just use
the existing helper types.

---

## Memory and recall

The agent stores every memory as a Postgres row with three retrieval paths:

1. **Semantic** — pgvector ANN search on the 1024-dimension embedding.
2. **Lexical** — native Postgres `tsvector` full-text search with `ts_rank`.
3. **Graph** — 1-hop outbound neighbour walk over the `entities`/`relations`
   tables.

`core::memory::recall()` runs all three, then fuses the ranked lists with
Reciprocal Rank Fusion (RRF) to produce a single ordered set of relevant
memories.

---

## The audit log

Every tool call, LLM call, memory write, and task-state transition writes one
row to `audit_log`. This table has `INSERT` and `SELECT` granted to the runtime
Postgres role but `UPDATE`, `DELETE`, and `TRUNCATE` are revoked — rows are
immutable once written, enforced by the database.

A background task mirrors every row to a date-named JSONL file under
`~/.local/state/hhagent/audit-YYYY-MM-DD.jsonl`. You can tail this file
directly without a running daemon.

---

## Where to add a new feature

| Type of feature | Where to start |
|-----------------|---------------|
| New tool worker | New crate under `workers/`; add entry to `core/src/scheduler/tool_dispatch.rs` |
| New CLI subcommand | New file under `core/src/bin/hhagent-cli/`; register in `main.rs` |
| New DB table | New migration in `db/migrations/`; new helpers in `db/src/` |
| New CASSANDRA rule | New function in `core/src/cassandra/constitutional.rs` or `deterministic.rs` |
| New recall lane | New lane in `core/src/memory/` and wire into `recall()` |
| New channel adapter | New crate under `adapters/`; connect via JSON-RPC to core |
