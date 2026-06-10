# 4 — Repository tour

A quick map of what lives where. You do not need to read every file — just
know where to look when you need something.

---

## Top-level layout

```
core/           The agent brain: scheduler, memory, CASSANDRA, audit, IPC
db/             Database crate: migrations, Postgres helpers, secrets
llm-router/     Sole egress for LLM calls (local + frontier, OpenAI shape)
sandbox/        Cross-platform sandbox abstraction (bwrap / Seatbelt)
supervisor/     Service supervisor abstraction (systemd --user / launchd)
protocol/       JSON-RPC 2.0 over stdio — the IPC wire format
tests-common/   Shared test helpers (per-test Postgres clusters, etc.)
workers/        One subdirectory per sandboxed tool worker
adapters/       Placeholder for channel adapters (Telegram, Signal) — not
                yet scaffolded as crates; the dirs hold .gitkeep only
config/         Runtime policy files and per-worker sandbox profiles
scripts/        Setup scripts (install bwrap profile, install Postgres, etc.)
docs/           All documentation
prompts/        LLM system-prompt files
seeds/          SQL seed data for kinds and other static tables
```

The **workspace members** declared in the top-level `Cargo.toml` are:
`core`, `db`, `llm-router`, `sandbox`, `supervisor`, `protocol`,
`tests-common`, `workers/prelude`, `workers/shell-exec`. Other directories
either live outside the Rust workspace (Python `gliner-relex` is built
with `uv`) or are scaffolds not yet ready to compile (see
[The `workers/` directory](#the-workers-directory) below).

---

## The `core/` crate in detail

This is the largest crate and the one most contributors will touch.

```
core/src/
  main.rs                Daemon entry point
  lib.rs                 Public crate API
  tool_host.rs           THE dispatcher chokepoint — every worker call goes here
  workspace.rs           Per-worker scratch directory lifecycle
  sandbox_health.rs      Boot-time sandbox-backend probe
  classification_inference.rs
                         Heuristic data-classification used by CASSANDRA stage 0

  scheduler/             Tick → plan → review → dispatch → repeat loop
  cassandra/             CASSANDRA review pipeline
    mod.rs               Public re-exports
    types.rs             Plan, PlannedStep, Verdict, DataClass, Severity
    review.rs            ReviewStage trait, ChainReviewStage,
                         ConstitutionalGuard, DeterministicPolicy stubs
    constitutional.rs    Five hard-coded constitutional constraints
    deterministic.rs     Data-classification invariants
    injection_guard.rs   Worker-output prompt-injection screen (catalogue
                         scan, ≥ 0.70 block threshold) — wired into
                         tool_host::dispatch after worker.call returns

  memory/                Recall + memory layers
    mod.rs               Public re-exports
    recall.rs            recall() — RRF fusion across semantic, lexical, graph
    embed.rs             embed_query() via llm-router; MemoryError
    layers.rs            L0 (raw observation) and L1 (promoted) layer types
    l0_seed.rs           Seeding L0 rows from new observations
    l1_promote.rs        Promotion logic: which L0 rows become L1 memories
    entity_link.rs       Entity ↔ memory linkage helpers

  entity_extraction/     Entity extraction pipeline (calls gliner-relex worker)
    batch_upsert/        Batched upsert into entities + relations tables
  observation/           Observation phase: turn channel events into L0 rows
  prompt_assembly/       Builds the LLM prompt from recalled context
  recall_assembly/       Assembles recall query parameters from a task

  worker_lifecycle/      Long-lived worker management (gliner-relex etc.)
    types.rs             WorkerHandle, lifecycle states
    manager.rs           Spawn / restart / health-check loop
    manager/             Submodules of manager (oversized → split per soft
                         500-LOC cap)
    idle_timeout.rs      Reap idle workers after a quiet window
    idle_timeout/        Submodules of idle_timeout (release path lifted
                         into a sibling per recent refactor)
    composite.rs         Composite policy combining manager + idle_timeout

  workers/               Adapters from core to specific worker crates
    gliner_relex/        Spawn + JSON-RPC contract for the GLiNER/ReLeX worker

  audit_mirror.rs        Background JSONL mirror of audit_log rows
  audit_tail.rs          CLI audit log viewer helpers
  cli_audit.rs           Audit write helpers used by CLI subcommands
  bin/kastellan-cli/       All CLI subcommands (ask, audit, tasks, entities, …)
```

The `tool_host.rs` file is especially important. It is the **only** place that
authors a `WorkerCommand` and the only place that writes audit log entries for
tool calls. If you are building a new entry point (a channel, a routine), it
must call through `dispatch()` — never spawn a worker directly. Worker
*output* also passes through the same chokepoint, where it is screened by
`cassandra::injection_guard::screen` before being handed back to the
scheduler — see [chapter 11](./11-cassandra-pipeline.md).

---

## The `sandbox/` crate in detail

```
sandbox/src/
  lib.rs              Public API: SandboxPolicy, SandboxBackend trait,
                      SandboxBackendKind
  linux_bwrap.rs      Linux backend (bubblewrap)
  linux_cgroup.rs     cgroup v2 CPU/memory caps via systemd-run
  macos_seatbelt.rs   macOS backend (sandbox-exec / Seatbelt) — shipped
  macos_container.rs  macOS micro-VM backend (Apple container CLI, optional,
                      Tahoe+) — file exists; not yet wired into the
                      SandboxBackendKind enum
sandbox/tests/
  linux_smoke.rs      Negative tests: file denials, network denial, OOM kill
  macos_smoke.rs      Same tests for macOS
```

The key abstraction is `SandboxPolicy` (a plain struct with fields like
`fs_read`, `fs_write`, `net`, `mem_mb`) and the `SandboxBackend` trait
(`spawn_under_policy`). The Linux and macOS backends both implement the same
trait from the same `SandboxPolicy` — you only write policy once.

---

## The `workers/` directory

Each subdirectory is intended to become an independent binary crate. Today
only two are in the Rust workspace:

```
workers/
  prelude/          [IN WORKSPACE] Shared init: Landlock + seccomp lock-down,
                    JSON-RPC serve. Every Rust worker imports this.
  shell-exec/       [IN WORKSPACE] Runs allow-listed shell commands (no shell
                    interpretation). Argv allowlist via env var.

  gliner-relex/     [scaffolded, Python] Named-entity extraction via GLiNER
                    + ReLeX. Built with uv, not cargo. Driven from core via
                    JSON-RPC.
  python-exec/      [scaffolded, in progress] General Python execution
  web-fetch/        [scaffolded, in progress] HTTP fetcher
  browser-driver/   [scaffolded, in progress] Playwright-based browser
  mail/             [scaffolded, in progress] IMAP/SMTP worker
```

Aspirational workers that aren't yet members of the workspace will not be
built by `cargo build --workspace`. When you make one ready, add it to
`[workspace.members]` in the top-level `Cargo.toml` and update this list.

Workers communicate with the core exclusively over stdin/stdout using
JSON-RPC 2.0 (`kastellan-protocol`). They never talk to Postgres. They
never talk to each other.

---

## The `db/` crate

The single owner of Postgres access. The agent core depends on `db`; nothing
else does. Workers never link to it.

```
db/src/
  lib.rs              Re-exports; PgPool wiring; migration runner
  pool.rs             Per-user Unix-socket pool builder
  conn.rs             Connection options (peer auth, search_path, etc.)
  probe.rs            Liveness / readiness probe
  audit.rs            audit_log writes; SHA-256 fingerprint for oversized rows
  memories.rs         L0/L1 memory rows + recall helpers (semantic + lexical)
  graph.rs / graph/   entities + relations graph queries
  entities.rs         Entity row CRUD
  entity_kinds.rs     entity_kinds seed table accessors
  entity_name.rs      Canonical-name helpers
  relation_kinds.rs   relation_kinds seed table accessors
  tasks.rs            tasks table CRUD; FOR UPDATE SKIP LOCKED claim
  tool_allowlists.rs  Per-tool egress allowlist rows
  agent_prompts.rs    Versioned system-prompt store
  secrets.rs          AES-256-GCM-at-rest secret store (decrypt on demand)
  tests.rs            Cross-module DB integration tests
  bin/                kastellan-db-init and other admin binaries
db/migrations/        Embedded *.sql migrations baked in via sqlx::migrate!
```

Migrations live under `db/migrations/` and are embedded into the binary at
compile time. The daemon runs pending migrations on startup; you don't run
`psql` by hand.

---

## The `llm-router/` crate

Sole egress for LLM calls. Every model request the agent core makes goes
through `Router::send` so there's exactly one outbound HTTP client for the
future egress proxy to see.

```
llm-router/src/
  lib.rs        Router type and high-level send entry point
  backend.rs    Backend enum (Local / Frontier) and as_tag for audit
  config.rs     Endpoint + model config (env-driven)
  messages.rs   ChatRequest / ChatResponse / ChatMessage typed wire shape
  embeddings.rs embed() helper used by memory::embed_query
  policy.rs    PolicyGate — Phase 0 always picks Backend::Local
  error.rs     RouterError surface
```

In Phase 0 the router speaks the OpenAI-compatible HTTP shape and only
dispatches to a local backend (vLLM/SGLang on Linux, llama.cpp/Ollama on
macOS). The frontier path is wired but the policy gate refuses it until
Phase 5. See [chapter 13](./13-llm-router.md) for details.

---

## The `supervisor/` crate

Cross-platform service supervisor abstraction. Generates and installs a
`systemd --user` unit on Linux and a `launchd` plist on macOS so the agent
restarts on logout/reboot. Skeleton today — the public trait and the
per-platform implementations are sketched but not yet driving production
boot.

---

## The `protocol/` crate

The JSON-RPC 2.0 line-delimited wire format used between the core and every
worker. Provides typed `Client` and `Server` types and is MCP-stdio
compatible — a future MCP-based tool can plug in without re-implementing the
transport. Workers don't import this directly; they import
`kastellan-worker-prelude` which wraps it.

---

## The `tests-common/` crate

Shared helpers for integration tests, including per-test Postgres cluster
spin-up (each test that touches the DB gets its own freshly-initialised
data directory and shuts it down at the end). Pulling these helpers into
their own crate keeps the test pattern uniform across `core/tests/`,
`db/tests/`, and others.

---

## Where to find things

| What you need | Where to look |
|---------------|---------------|
| Current TODO and recent changes | `docs/devel/handovers/HANDOVER.md` |
| Phased feature roadmap | `docs/devel/ROADMAP.md` |
| Architecture invariants | `docs/architecture.md` |
| Threat model | `docs/threat-model.md` |
| CASSANDRA design plan | `docs/cassandra_design_plan.md` |
| Postgres migrations | `db/migrations/` |
| CLI subcommand implementations | `core/src/bin/kastellan-cli/` |
| Integration tests (per-test PG cluster) | `core/tests/` and `db/tests/` |
| Setup scripts | `scripts/linux/` |
| Worker-output injection guard | `core/src/cassandra/injection_guard.rs` |
| LLM router (sole egress) | `llm-router/src/` |
