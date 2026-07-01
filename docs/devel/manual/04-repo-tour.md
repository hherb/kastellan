# 4 — Repository tour

A quick map of what lives where. You do not need to read every file — just
know where to look when you need something.

---

## Top-level layout

```
core/           The agent brain: scheduler, memory, CASSANDRA, audit, IPC,
                channel bus, egress integration, secret vault, handoff cache
db/             Database crate: migrations, Postgres helpers, secrets
leak-scan/      Pure credential-leak scanner shared by core + egress-proxy
llm-router/     Sole egress for LLM calls (local + frontier, OpenAI shape)
sandbox/        Cross-platform sandbox abstraction (bwrap / Seatbelt / container)
supervisor/     Service supervisor abstraction (systemd --user / launchd)
protocol/       JSON-RPC 2.0 over stdio — the IPC wire format
tests-common/   Shared test helpers (per-test Postgres clusters, etc.)
workers/        One subdirectory per sandboxed tool/channel worker
adapters/       Placeholder dir (empty `signal/`, `telegram/` stubs); the
                Matrix channel lives in workers/matrix + workers/matrix-wire
config/         Runtime policy files and per-worker sandbox profiles
deploy/         Operator deployment templates (e.g. the Matrix homeserver unit)
scripts/        Setup scripts (install bwrap profile, install Postgres, etc.)
docs/           All documentation
prompts/        LLM system-prompt files
seeds/          Seed data (memory L0 meta-rules, entity/relation kinds)
site/           Public website (kastellan.dev) sources
```

The **workspace members** declared in the top-level `Cargo.toml` are:
`core`, `db`, `leak-scan`, `llm-router`, `sandbox`, `supervisor`,
`protocol`, `tests-common`, and the Rust workers `workers/prelude`,
`workers/shell-exec`, `workers/web-common`, `workers/web-fetch`,
`workers/web-search`, `workers/python-exec`, `workers/egress-proxy`,
`workers/matrix-wire`, `workers/matrix`, plus the Firecracker micro-VM
support crates `workers/microvm-run`, `workers/microvm-init`, and
`workers/kv-demo`. The Python workers (`workers/gliner-relex`,
`workers/browser-driver`) live outside the Rust workspace and are built
with `uv`; `workers/mail` is an empty scaffold. See
[The `workers/` directory](#the-workers-directory) below.

---

## The `core/` crate in detail

This is the largest crate and the one most contributors will touch.

```
core/src/
  main.rs                Daemon entry point (probe → connect pool → spawn
                         mirror → block on SIGTERM/SIGINT)
  lib.rs                 Public crate API
  tool_host.rs           THE dispatcher chokepoint — every worker call goes here
  tool_host/             Chokepoint submodules: secret_scrub (python-exec
                         output scrub), egress_provision (dispatch-time
                         secret-hash provisioning), watchdog, lockdown env
  workspace.rs           Per-worker scratch directory lifecycle
  sandbox_health.rs      Boot-time sandbox-backend probe
  classification_inference.rs
                         Heuristic data-classification used by CASSANDRA stage 0

  scheduler/             Tick → plan → review → dispatch → repeat loop
                         (runner, lanes, crash recovery, l3_run routing)
  cassandra/             CASSANDRA review pipeline
    mod.rs               Public re-exports
    types.rs / types/    Plan, PlannedStep, Verdict, DataClass, Severity
    review.rs            ReviewStage trait + ChainReviewStage runner
    constitutional.rs    Real constitutional-principle screen (English phrases)
    deterministic.rs     Data-classification invariants (ceiling/floor/step)
    injection_guard.rs   Worker-output prompt-injection screen (per-tool
    injection_guard/     GuardProfile Strict/Relaxed, ≥ 0.70 block threshold)
                         — wired into tool_host::dispatch after worker.call

  memory/                Recall + memory layers
    mod.rs               Public re-exports / facade
    recall.rs            recall() — RRF fusion across semantic, lexical, graph
    embed.rs             embed_query() via llm-router; MemoryError
    l0_seed.rs           Seed L0 rows from new observations
    l1_promote.rs        Promote L0 rows into L1 memories
    l3_crystallise / l3_approval / l3_invoke / l3_surface
                         Templated L3 skill lifecycle
    l3py_crystallise / l3py_approval / l3py_invoke
                         Agent-authored Python L3 skill lifecycle

  secrets/               Vault (TTL'd in-memory store) + SecretRef opaque
                         newtype + secret:// substitution + value_fingerprint
  channel/               Channel bus: Channel trait, auth/pairing,
                         injection screen, route, Matrix adapter
  egress/                Host side of the egress proxy: sidecar spawn,
                         force-routed net workers, decision audit, leak
                         provisioning
  handoff.rs             In-memory content-addressed large-result cache
  registry_build.rs      Static WORKER_MANIFESTS + build_tool_registry
  worker_manifest.rs     WorkerManifest trait + binary discovery

  entity_extraction/     Entity extraction pipeline (calls gliner-relex worker)
  observation/           Observation phase: turn channel events into L0 rows
  prompt_assembly/       Builds the LLM prompt from recalled context
  recall_assembly/       Assembles recall query parameters from a task
  worker_lifecycle/      Long-lived worker management (SingleUse / IdleTimeout
                         / Composite managers + egress force-routing glue +
                         PersistentWorker: a backend-agnostic supervisor that
                         keeps a worker alive across many calls and respawns
                         it on death — used for the persistent micro-VM path)

  workers/               Host-side manifests + clients for each worker
    shell_exec.rs        ShellExecManifest + entry
    web_fetch.rs / web_search.rs
    python_exec.rs       Net::Deny + WorkerStrict executor manifest
    browser_driver.rs / browser_driver/   Playwright render worker manifest
    gliner_relex.rs / gliner_relex/        torch entity-extraction manifest
    interpreter_deps.rs  Out-of-prefix interpreter-lib auto-bind helper

  audit_mirror.rs        Background JSONL mirror of audit_log rows
  audit_tail.rs          CLI audit log viewer helpers
  cli_audit.rs           Audit write helpers used by CLI subcommands
  bin/kastellan-cli/       All CLI subcommands (ask, audit, tasks, entities,
                         relations, memory l1/l3, secrets, pair, …)
```

The `tool_host.rs` file is especially important. It is the **only** place that
authors a `WorkerCommand` and the only place that writes audit log entries for
tool calls. If you are building a new entry point (a channel, a routine), it
must call through `dispatch()` — never spawn a worker directly. On the way in,
`dispatch` substitutes any `secret://` references into worker params; on the
way out, worker *output* passes back through the same chokepoint where it is
screened by `cassandra::injection_guard` (and, for python-exec, scrubbed of any
materialized-secret fingerprints) before being handed back to the scheduler —
see [chapter 11](./11-cassandra-pipeline.md).

---

## The `sandbox/` crate in detail

```
sandbox/src/
  lib.rs              Public API: SandboxPolicy, SandboxBackend trait,
                      SandboxBackendKind, Net, Profile, PersistentStore
  linux_bwrap.rs      Linux backend (bubblewrap)
  linux_cgroup.rs     cgroup v2 CPU/memory caps via systemd-run
  linux_firecracker/  Linux micro-VM backend (Firecracker, opt-in): plan,
                      probe, images (mkfs.ext4 RO/RW + persistent), mounts
                      (kastellan.mounts share manifest), confine (unprivileged
                      VMM confinement), cleanup (orphan run-dir sweep)
  macos_seatbelt/     macOS backend (sandbox-exec / Seatbelt) — shipped
  macos_container/    macOS micro-VM backend (Apple `container` CLI, opt-in
                      per-worker, Tahoe+) — wired into SandboxBackendKind
sandbox/tests/
  linux_smoke.rs            Negative tests: file denials, net denial, OOM kill
  macos_smoke.rs            Same tests for macOS
  macos_container_smoke.rs  Real Apple `container` tests (opt-in)
```

The key abstraction is `SandboxPolicy` (a plain struct with fields like
`fs_read`, `fs_write`, `net`, `proxy_uds`, `persistent_store`, `mem_mb`,
`profile`) and the `SandboxBackend` trait (`spawn_under_policy`). The Linux
and macOS backends both implement the same trait from the same
`SandboxPolicy` — you only write policy once. `SandboxBackendKind` lets a
worker opt into a specific backend (the Firecracker micro-VM on Linux, the
Apple `container` micro-VM on macOS). `Net` is `Deny` / `Allowlist(hosts)` /
`ProxyEgress`, and `Profile` selects the syscall/Seatbelt cluster
(`WorkerStrict` / `WorkerNetClient` / `WorkerBrowserClient` /
`WorkerMlClient`). The additive `persistent_store` field
(`PersistentStore { host_backing, guest_mount, size_mib }`, `None` by
default so it's byte-identical when unset) gives a long-lived worker a
writable store that survives a micro-VM respawn. See
[chapter 7](./07-sandboxing.md).

---

## The `workers/` directory

Each subdirectory is an independent worker. Rust workers are members of the
Cargo workspace; Python workers are built with `uv` and driven from core over
JSON-RPC.

```
workers/
  prelude/          [RUST] Shared init: Landlock + seccomp lock-down, JSON-RPC
                    serve. Also ships kastellan-worker-lockdown-exec, the
                    lock_down()→execve shim that gives pure-Python venv workers
                    worker-side Linux seccomp + Landlock.
  shell-exec/       [RUST] Runs allow-listed shell commands (no shell
                    interpretation). Argv allowlist via KASTELLAN_SHELL_ALLOWLIST.
  web-common/       [RUST] Shared lib for net-egress workers: HostAllowlist,
                    HttpGet transport, CONNECT-over-UDS proxy connector.
  web-fetch/        [RUST] HTTPS-only web.fetch (HTML readability / PDF / text).
  web-search/       [RUST] web.search against a SearxNG JSON endpoint.
  python-exec/      [RUST] Executes agent-authored Python under the strictest
                    policy (Net::Deny, curated stdlib, no site-packages).
  egress-proxy/     [RUST] Per-worker sandboxed CONNECT proxy: allowlist + SSRF
                    + TLS-intercept MITM + leak scanner + SPKI pinning.
  matrix-wire/      [RUST] Shared serde wire types for the Matrix worker.
  matrix/           [RUST] Matrix channel worker (matrix-rust-sdk behind a seam;
                    hermetic parts compile by default, the live LiveSdk is
                    feature-gated behind `live-matrix`).
  microvm-run/      [RUST] Firecracker launcher Child (pure-std) — boots and
                    supervises the micro-VM for the Linux Firecracker backend.
  microvm-init/     [RUST] Guest PID 1: a vsock↔stdio adapter (Linux-only libc,
                    macOS stub) that bridges the in-VM worker to the host.
  kv-demo/          [RUST] Long-lived Net::Deny key-value worker with a
                    persistent store — the 5b demo + micro-VM integration
                    fixture.

  gliner-relex/     [PYTHON] Named-entity + relation extraction via GLiNER +
                    ReLeX. Built with uv. Host manifest in core/src/workers.
  browser-driver/   [PYTHON] Playwright headless-Chromium read-only render.
                    Built with uv. Host manifest in core/src/workers.
  mail/             [empty scaffold] IMAP/SMTP failover — not yet built.
```

Workers communicate with the core exclusively over stdin/stdout using
JSON-RPC 2.0 (`kastellan-protocol`). They never talk to Postgres. They never
talk to each other. A net-egress worker reaches the network only through its
own egress-proxy sidecar (force-routed on by default) — never directly.

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
  memories.rs / memories/  L0/L1/L3 memory rows + recall helpers; light
                      (embedding-skipping) write path; skill-trust accessors
  graph.rs / graph/   entities + relations graph queries (recursive-CTE walks)
  entities.rs         Entity row CRUD
  entity_kinds.rs     entity_kinds seed table accessors
  entity_name.rs      Canonical-name helpers
  relation_kinds.rs   relation_kinds seed table accessors
  tasks.rs            tasks table CRUD; FOR UPDATE SKIP LOCKED claim
  tool_allowlists.rs  Per-tool egress allowlist rows
  agent_prompts.rs    Versioned system-prompt store
  pairings.rs         Channel DM pairings + single-use pairing codes
  secrets.rs / secrets/  AES-256-GCM-at-rest secret store (crypto, key
                      providers [OS keyring], async DB I/O)
  tests.rs            Cross-module DB integration tests
  bin/                kastellan-db-init and other admin binaries
db/migrations/        Embedded *.sql migrations (0001..0019) via sqlx::migrate!
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
restarts on logout/reboot. Functional: `ServiceSpec` (with `after`/`part_of`
ordering and optional `restart_backoff`), `TargetSpec`, and
`Supervisor::{install,start,stop,uninstall}_target` drive a real
`kastellan.target` bring-up of Postgres + core. `specs::` holds the core,
Postgres, and target specs; service names are screened by
`validate_service_name` before any unit file is written.

---

## The `leak-scan/` crate

`kastellan-leak-scan` is a small, pure (serde/serde_json/sha2 only)
credential-leak scanner — the single source of truth shared by the egress
proxy (which *detects* and blocks leaks mid-stream) and the core (which
*scrubs* python-exec output). It has no async, no I/O: a `RollingMatcher`
(streaming, per-length Rabin rolling pre-filter + SHA-256 confirm) for the
proxy, a `redact()` sibling for core's output scrub, and the
`secret_hashes.json` wire codec. Secret values shorter than 8 bytes are
unscannable by design.

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
