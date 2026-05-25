# 4 — Repository tour

A quick map of what lives where. You do not need to read every file — just
know where to look when you need something.

---

## Top-level layout

```
core/           The agent brain: scheduler, memory, CASSANDRA, audit, IPC
db/             Database crate: migrations, Postgres helpers, secrets
llm-router/     Thin HTTP client for local and frontier LLMs
sandbox/        Cross-platform sandbox abstraction (bwrap / Seatbelt)
supervisor/     Service supervisor abstraction (systemd --user / launchd)
protocol/       JSON-RPC 2.0 over stdio — the IPC wire format
tests-common/   Shared test helpers (per-test Postgres clusters, etc.)
workers/        One subdirectory per sandboxed tool worker
adapters/       Channel adapters (Telegram, Signal — in progress)
config/         Runtime policy files and per-worker sandbox profiles
scripts/        Setup scripts (install bwrap profile, install Postgres, etc.)
docs/           All documentation
prompts/        LLM system-prompt files
seeds/          SQL seed data for kinds and other static tables
```

---

## The `core/` crate in detail

This is the largest crate and the one most contributors will touch.

```
core/src/
  main.rs               Daemon entry point
  lib.rs                Public crate API
  tool_host.rs          THE dispatcher chokepoint — every worker call goes here
  workspace.rs          Per-worker scratch directory lifecycle
  scheduler/            Tick → plan → review → dispatch → repeat loop
  cassandra/            CASSANDRA review pipeline
    constitutional.rs   Five hard-coded constitutional constraints
    deterministic.rs    Data-classification invariants
  memory/               Recall (semantic + lexical + graph lanes), RRF fusion
  audit_mirror.rs       JSONL on-disk mirror of audit_log rows
  audit_tail.rs         CLI audit log viewer helpers
  cli_audit.rs          Audit write helpers used by CLI subcommands
  bin/hhagent-cli/      All CLI subcommands (ask, audit, tasks, entities, …)
```

The `tool_host.rs` file is especially important. It is the **only** place that
authors a `WorkerCommand` and the only place that writes audit log entries for
tool calls. If you are building a new entry point (a channel, a routine), it
must call through `dispatch()` — never spawn a worker directly.

---

## The `sandbox/` crate in detail

```
sandbox/src/
  lib.rs            Public API: SandboxPolicy, SandboxBackend trait
  linux_bwrap.rs    Linux backend (bubblewrap)
  linux_cgroup.rs   cgroup v2 CPU/memory caps via systemd-run
  macos_seatbelt.rs macOS backend (sandbox-exec / Seatbelt)
  macos_container.rs macOS micro-VM backend (Apple container CLI, optional)
sandbox/tests/
  linux_smoke.rs    Negative tests: file denials, network denial, OOM kill
  macos_smoke.rs    Same tests for macOS
```

The key abstraction is `SandboxPolicy` (a plain struct with fields like
`fs_read`, `fs_write`, `net`, `mem_mb`) and the `SandboxBackend` trait
(`spawn_under_policy`). The Linux and macOS backends both implement the same
trait from the same `SandboxPolicy` — you only write policy once.

---

## The `workers/` directory

Each subdirectory is an independent binary crate:

```
workers/
  prelude/        Shared init code: Landlock + seccomp lock-down, JSON-RPC serve
  shell-exec/     Runs allow-listed shell commands (no shell interpretation)
  gliner-relex/   Named-entity extraction (Python, uv, GLiNER + ReLeX)
  python-exec/    General Python execution (in progress)
  web-fetch/      HTTP fetcher (in progress)
  browser-driver/ Playwright-based browser (in progress)
  mail/           IMAP/SMTP worker (in progress)
```

Workers communicate with the core exclusively over stdin/stdout using
JSON-RPC 2.0 (`hhagent-protocol`). They never talk to Postgres. They
never talk to each other.

---

## Where to find things

| What you need | Where to look |
|---------------|---------------|
| Current TODO and recent changes | `docs/devel/handovers/HANDOVER.md` |
| Phased feature roadmap | `docs/devel/ROADMAP.md` |
| Architecture invariants | `docs/architecture.md` |
| Threat model | `docs/threat-model.md` |
| Postgres migrations | `db/migrations/` |
| CLI subcommand implementations | `core/src/bin/hhagent-cli/` |
| Integration tests (per-test PG cluster) | `core/tests/` and `db/tests/` |
| Setup scripts | `scripts/linux/` |
