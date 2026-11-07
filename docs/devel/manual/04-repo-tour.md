# 4 — Repository tour

A quick map of what lives where. You do not need to read every file — just
know where to look when you need something.

---

## Top-level layout

```
core/           The agent brain: scheduler + planner loops, memory, CASSANDRA,
                audit, tool-host chokepoint, channel bus, egress integration,
                secret vault, handoff cache, installer; the `kastellan` daemon
                and `kastellan-cli`
db/             Database crate: embedded migrations, Postgres helpers, secrets
leak-scan/      Pure credential-leak scanner shared by core + egress-proxy
llm-router/     Sole core-side LLM egress (local / guard / frontier, OpenAI shape)
net-classify/   Pure IP-range classifier: the SSRF / DNS-rebinding deny
                predicate the egress proxy enforces at connect time
sandbox/        Cross-platform sandbox abstraction (bwrap / Seatbelt /
                Firecracker / Apple container)
supervisor/     Service supervisor (systemd --user / launchd): kastellan.target
protocol/       JSON-RPC 2.0 over stdio — the IPC wire format
tests-common/   Shared test harness (per-test Postgres, REQUIRE knobs, markers)
workers/        One subdirectory per sandboxed tool/channel worker or sidecar
adapters/       Placeholder dir (empty `signal/`, `telegram/` stubs); the
                Matrix channel lives in workers/matrix + workers/matrix-wire
config/         Example runtime policy + per-worker sandbox profile examples
deploy/         Operator deployment templates (the Matrix homeserver unit)
scripts/        Host setup, release build, e2e gate, image/rootfs builds
prompts/        LLM system-prompt files, loaded into the DB at daemon start
seeds/          Seed data (the L0 memory meta-rules)
tests/          Data fixtures: the guard-calibration corpus and observation
                captures (the Rust integration tests live in each crate)
exploration/    Out-of-tree experiments (not built)
site/           Public website (kastellan.dev) sources
docs/           All documentation
```

The **workspace members** declared in the top-level `Cargo.toml` are 27
crates: `core`, `db`, `leak-scan`, `llm-router`, `net-classify`,
`sandbox`, `supervisor`, `protocol`, `tests-common`, and 18 Rust workers —
`workers/prelude`, `workers/mail`, `workers/email-in`,
`workers/python-exec`, `workers/shell-exec`, `workers/web-common`,
`workers/web-fetch`, `workers/web-research`, `workers/web-search`,
`workers/egress-proxy`, `workers/embed-broker`, `workers/search-broker`,
`workers/matrix-wire`, `workers/matrix`, the demo workers
`workers/kv-demo` and `workers/net-demo`, and the Firecracker micro-VM
support crates `workers/microvm-run` and `workers/microvm-init`. The Python
workers (`workers/gliner-relex`, `workers/browser-driver`) live outside the
Rust workspace. See [The `workers/` directory](#the-workers-directory) below.

---

## The `core/` crate in detail

This is the largest crate and the one most contributors will touch. Large
modules are split into a `foo.rs` plus a `foo/` directory of submodules (and
usually a `foo/tests.rs`); the tree below lists the top level and the
submodules worth knowing.

```
core/src/
  main.rs                Daemon entry point (database probe + migrations, tool
                         registry, channels, scheduler; blocks on SIGTERM/SIGINT)
  main/                  Daemon bootstrap split: bootstrap.rs, the config-gated
                         matrix_boot.rs / email_boot.rs, the audit sink, the
                         guard boot-refusal row
  lib.rs                 Public crate API

  tool_host.rs           THE dispatcher chokepoint — every worker call goes here
  tool_host/             Chokepoint submodules: spawn_invocation, post_process
                         (output screening after worker.call), secret_scrub
                         (python-exec output scrub), egress_provision
                         (dispatch-time secret-hash provisioning), audit_sink,
                         scratch, watchdog, lockdown_env

  scheduler/             The agent loop
    runner.rs, runner/   Per-lane runner loop + spawn_scheduler; task execution
    agent.rs, agent/     LLM adapter: TaskContext → Plan
    inner_loop.rs, inner_loop/
                         The classic per-task replanning loop: execute, the
                         data-class ceiling/floor, result views, the
                         over-budget summary, LLM-failure handling
    notebook/            The task-notebook planner loop (the default;
                         KASTELLAN_PLANNER_MODE=classic opts out): state, render,
                         reply, formulator, run, repeat, thinking
      sift/              The `sift` step: go through every item of a list,
                         keep the matches in the task's scrapbook, render the
                         answer's table by code
    conversation.rs, conversation/
                         Conversational continuity for channel tasks
    asks/                The operator-ask (escalation) path
    tool_dispatch.rs     Production StepDispatcher: PlannedStep → worker call
    plan_parser.rs       Lenient parse of an LLM reply into a Plan
    prompts.rs           Prompt loading + the agent_prompts ledger
    crash_recovery.rs    Startup sweep for tasks a crash left running
    l3_run.rs            Operator-submitted skill runs

  cassandra/             CASSANDRA review pipeline
    types.rs / types/    Plan, PlannedStep, Verdict, DataClass, Severity
    review.rs            ReviewStage trait + ChainReviewStage runner
    constitutional.rs    Constitutional-principle screen
    deterministic.rs     Data-classification invariants (ceiling/floor/step)
    data_ceiling.rs      Resolving an absent data ceiling
    plan_digest.rs       What an operator approval binds to
    injection_guard.rs, injection_guard/
                         Worker-output prompt-injection screen (per-tool
                         GuardProfile), run from tool_host::post_process
    guard_model/         Optional model-based guard tier (Shieldstral):
                         escalate-only, fail-open, weights/context pinned
  guard_calibration/     Offline corpus scoring for the guard tier
                         (`kastellan-cli guard calibrate`)
  observation/           Capture + replay of plans for offline rule iteration
  classification_inference.rs
                         CLI-side automatic classification-floor inference

  memory/                Memory layers + recall
    recall.rs, recall/   recall() — RRF fusion across the recall lanes
    layers.rs            L1 always-in-context index loader
    l0_seed.rs           Seed L0 meta-rules from seeds/memory/l0_meta_rules.toml
    l1_promote.rs        L1 writer
    embed.rs, embedder.rs, *reembed.rs
                         Embedding via llm-router; backfill workflows
    entity_link.rs       Memory-write-time entity auto-linker
    l3_* / l3py_*        Templated and agent-authored-Python L3 skill
                         lifecycles (crystallise, approve, surface, invoke)
  prompt_assembly/       Builds the planner's system message (L0, L1, skills)
  recall_assembly/       Runs per-query recall and packages it for the prompt
  entity_extraction/     Query-time NER for the graph lane (gliner-relex)
  handoff.rs             In-memory content-addressed large-result cache

  channel/               Channel bus: Channel trait, auth/pairing, inbound
                         screening (ingest), routing, the bus runtime
                         (bus*.rs), boot_supervisor (supervised bring-up),
                         polled_driver (generic long-poll driver), outbox,
                         reply catch-up, ask messages, pg_events (the DB
                         seam), matrix/ and email/ (config, DMARC + token
                         gate, policy, wire)
  egress/                Host side of the egress proxy: sidecar spawn,
                         force-routed net workers, decision audit, leak
                         provisioning, cert pins, operator extra CA
  broker/                Trusted broker sidecars (embed / search)
  secrets/               Vault (TTL'd in-memory store) + SecretRef opaque
                         newtype + secret:// substitution
  worker_lifecycle/      Long-lived worker management (SingleUse /
                         IdleTimeout / Composite managers, force-route glue,
                         PersistentWorker — keeps a worker alive across many
                         calls and respawns it on death)
  worker_manifest.rs     WorkerManifest trait + current_exe()-relative
                         binary discovery
  registry_build.rs      Static WORKER_MANIFESTS + build_tool_registry
  workers/               Host-side manifests for each worker (shell_exec,
                         web_fetch, web_search, web_research, mail,
                         python_exec, browser_driver, gliner_relex) plus
                         interpreter_deps and endpoint_guard
  worker_stderr/         Drains a worker's stderr; the death report a
                         failing test sees
  workspace.rs           Per-task scratch directory lifecycle
  sandbox_health.rs      Boot-time sandbox-backend probe
  sidecar_ready.rs       Bounded readiness wait for sidecars
  untrusted_text.rs      One definition of dangerous characters in untrusted
                         text (prompt escaping + log neutralising)

  audit_mirror.rs        Background JSONL mirror of audit_log rows
  audit_thinning.rs      Thinning of repeated reports about refused/lost
                         audit rows (counted per burst, never silent)
  audit_tail.rs          `audit tail` viewer helpers
  cli_audit.rs, cli_audit/
                         Audit rows written by CLI subcommands
  install/               `kastellan-cli install`: pure planning (plan.rs,
                         env_diff, guard_backend) + IO orchestration (run.rs)
  bin/kastellan-cli/     All CLI subcommands
```

The CLI's top-level subcommands are `ask`, `tasks`, `inbox`, `audit tail`,
`memory`, `entities`, `relations`, `tools allowlist`, `guard calibrate`,
`observation replay`, `pair`, `matrix`, `secret`, `install` and `uninstall`;
`kastellan-cli --help` prints the details.

The `tool_host.rs` file is especially important. It is the **only** place that
authors a `WorkerCommand` and the only place that writes audit log entries for
tool calls. If you are building a new entry point (a channel, a routine), it
must call through `dispatch()` — never spawn a worker directly. On the way in,
`dispatch` substitutes any `secret://` references into worker params; on the
way out, worker *output* passes back through the same chokepoint where it is
screened by `cassandra::injection_guard` (and, for python-exec, scrubbed of any
materialized-secret fingerprints) before being handed back to the scheduler —
see [chapter 11](./11-cassandra-pipeline.md). Both planner loops, classic and
notebook, review and dispatch every step through the same code.

---

## The `sandbox/` crate in detail

```
sandbox/src/
  lib.rs              Public API: SandboxPolicy, SandboxBackend trait,
                      SandboxBackendKind, Net, Profile, PersistentStore
  linux_bwrap.rs      Linux backend (bubblewrap); build_argv() is a pure
                      SandboxPolicy → Vec<String> function
  linux_cgroup.rs     cgroup v2 CPU/memory caps via systemd-run --user --scope
  linux_firecracker/  Linux micro-VM backend (Firecracker, opt-in): plan,
                      probe, images (RO/RW + persistent ext4), mounts (host
                      directory sharing), confine (unprivileged VMM jail),
                      cleanup (orphan run-dir sweep)
  guest_kernel_pin.rs sha256-pin verification of the micro-VM guest kernel,
                      run fail-closed at every VM boot
  macos_seatbelt/     macOS backend (sandbox-exec / Seatbelt)
  macos_container/    macOS micro-VM backend (Apple `container` CLI, opt-in
                      per worker, macOS 26+)
  bounded_command/    Run a helper subprocess under a wall-clock budget
  private_dir.rs      Owner-private per-spawn dirs/files under a shared root
  pid.rs              Cross-platform pid-liveness check for orphan sweeps
sandbox/tests/
  linux_smoke.rs            Negative tests: file denials, net denial, OOM kill
  linux_force_routing.rs    Force-routed worker has no direct route: private
                            netns whose only egress is the bound proxy UDS
  macos_smoke.rs            Same tests for macOS
  seatbelt_uds_probe.rs     macOS twin: Seatbelt denies AF_INET, permits the
                            proxy UDS
  macos_container_smoke.rs  Real Apple `container` tests (opt-in)
```

The key abstraction is `SandboxPolicy` (a plain struct with fields like
`fs_read`, `fs_write`, `net`, `proxy_uds`, `persistent_store`, `mem_mb`,
`profile`, `broker_uds`) and the `SandboxBackend` trait (`spawn_under_policy`). The Linux
and macOS backends both implement the same trait from the same
`SandboxPolicy` — you only write policy once. `SandboxBackendKind` lets a
worker opt into a specific backend (`Bwrap` / `FirecrackerVm` on Linux,
`Seatbelt` / `Container` on macOS — each variant exists only on its own OS). `Net` is `Deny` / `Allowlist(hosts)` /
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
Cargo workspace; the two Python workers are set up by their own scripts under
`scripts/workers/` and driven from core over the same JSON-RPC.

```
workers/
  prelude/          [RUST] Shared init: Landlock + seccomp lock-down, JSON-RPC
                    serve. Also ships kastellan-worker-lockdown-exec, the
                    lock_down()→execve shim that gives pure-Python venv workers
                    worker-side Linux seccomp + Landlock.
  shell-exec/       [RUST] Runs allow-listed commands via execve (no shell
                    interpretation). The argv allowlist comes from the
                    tool_allowlists table (`kastellan-cli tools allowlist`).
  web-common/       [RUST] Shared lib for net-egress workers: HostAllowlist,
                    HttpGet transport, CONNECT-over-UDS proxy connector.
  web-fetch/        [RUST] HTTPS-only web.fetch (HTML readability / PDF / text).
  web-search/       [RUST] web.search / web.search_batch against SearxNG.
  web-research/     [RUST] Composite web.research: search → fetch top-N
                    allowlisted pages → chunk → BM25-rank passages, one call.
  python-exec/      [RUST] python.exec / python.evaluate: agent-authored
                    Python under the strictest policy (Net::Deny, ephemeral
                    scratch, curated stdlib, no site-packages). Opt-in.
  egress-proxy/     [RUST] Per-worker sandboxed CONNECT proxy: allowlist + SSRF
                    + TLS-intercept MITM + leak scanner + SPKI pinning.
  embed-broker/     [RUST] Trusted embedding-broker sidecar: bridges a jailed
                    worker's UDS to the operator's embedding backend, so the
                    worker needs no embed egress.
  search-broker/    [RUST] Trusted search-broker sidecar: the same bridge
                    shape, UDS → SearxNG.
  mail/             [RUST] Six read-only mail.* tools against localmail's /v1
                    (search, get_message, list_messages, list_accounts,
                    get_attachment_text, get_attachment → a durable per-task
                    out dir). Needs a current localmail; see
                    docs/workers/mail.md.
  email-in/         [RUST] Sandboxed inbound-email poller (email.init / poll /
                    ack) for the email failover channel; the DMARC+token gate
                    stays core-side.
  matrix-wire/      [RUST] Shared serde wire types for the Matrix worker.
  matrix/           [RUST] Matrix channel worker (matrix-rust-sdk behind a seam;
                    hermetic parts compile by default, the live LiveSdk is
                    feature-gated behind `live-matrix`).
  microvm-run/      [RUST] Firecracker launcher Child (pure-std) — boots and
                    supervises the micro-VM for the Linux Firecracker backend.
  microvm-init/     [RUST] Guest PID 1: a vsock↔stdio adapter (Linux-only libc,
                    macOS stub) that bridges the in-VM worker to the host.
  kv-demo/          [RUST] Long-lived Net::Deny key-value worker with a
                    persistent store — a reference worker and micro-VM
                    integration fixture.
  net-demo/         [RUST] Long-lived Net::Allowlist demo worker: end-to-end
                    TLS through a transparent-tunnel egress sidecar (network
                    egress inside a persistent VM).

  gliner-relex/     [PYTHON] Named-entity + relation extraction via GLiNER +
                    ReLeX. uv-managed venv + ~1.3 GB of weights. Host
                    manifest in core/src/workers.
  browser-driver/   [PYTHON] browser.render: Playwright headless-Chromium
                    read-only render. Opt-in. System-python venv (not uv).
                    Host manifest in core/src/workers.
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
  lib.rs              Re-exports; PgPool wiring; migration runner; Postgres
                      bin-dir discovery (KASTELLAN_PG_BIN_DIR override)
  pool.rs / conn.rs   Per-user Unix-socket pool; connection options (peer
                      auth, the `kastellan` application database)
  probe.rs            Boot probe: create the database if needed, migrate
  boot_wait.rs        Bounded wait for Postgres to accept connections at boot
  audit.rs / audit/   audit_log writes; SHA-256 fingerprint for oversized rows
  nul.rs / nul/       The NUL rule: text Postgres would refuse is handled,
                      never silently dropped
  memories.rs / memories/  L0–L4 memory rows + recall helpers
  graph.rs / graph/   entities + relations graph queries (recursive-CTE walks)
  entities.rs, entity_*.rs, relation_kinds.rs
                      Entity rows, embeddings, curated kind vocabularies
  tasks.rs / tasks/   tasks table CRUD; FOR UPDATE SKIP LOCKED claim; the
                      per-task turn record for conversational continuity
  asks.rs             Operator asks (the escalation path)
  notebook.rs         task_notebook: the notebook planner's append-only log
  scrapbook.rs        task_scrapbook: the items a sift kept
  tool_allowlists.rs  Per-tool argv/egress allowlist rows
  agent_prompts.rs    Versioned system-prompt store
  pairings.rs         Channel DM pairings + single-use pairing codes
  secrets.rs / secrets/  AES-256-GCM-at-rest secret store (crypto, key
                      providers [OS keyring], async DB I/O)
  bin/                kastellan-db-init (idempotent per-user cluster setup)
db/migrations/        29 embedded *.sql migrations (0001..0029) via sqlx::migrate!
db/tests/             DB-level e2e suites (per-test Postgres cluster)
```

Migrations live under `db/migrations/` and are embedded into the binary at
compile time. The daemon runs pending migrations on startup; you don't run
`psql` by hand. **Never edit a migration that has shipped** — deployed
databases have already applied it, and sqlx refuses a changed checksum. Add
a new numbered file instead.

---

## The `llm-router/` crate

Sole core-side egress for LLM calls. Every model request the agent core makes
goes through the `Router` (`send` for chat, `embed` for embeddings), so there
is exactly one outbound model client to audit.

```
llm-router/src/
  lib.rs              Router type: send / embed / props
  backend.rs          Backend enum (Local / Frontier) and as_tag for audit
  config.rs, config/  Endpoint + model config (env-driven): local,
                      embedding, guard and frontier endpoints
  messages.rs, messages/
                      ChatRequest / ChatResponse / ChatMessage wire shape
  embeddings.rs       Embedding request/response
  thinking.rs, thinking/
                      Asking a reasoning model not to think, in the
                      backend's own dialect
  logprob_score.rs    Renormalising yes/no logprobs into a calibrated score
                      (the guard tier)
  completion_stats.rs What one completion cost, shaped for an audit row
  policy.rs           PolicyGate — today always picks Backend::Local
  error.rs            RouterError surface
```

The router speaks the OpenAI-compatible HTTP shape and today only dispatches
to a local backend (Ollama, vLLM/SGLang or llama.cpp on Linux; oMLX,
llama.cpp or Ollama on macOS). The optional guard model is a second, separate
local endpoint. The frontier path is wired but the policy gate refuses it
until Phase 5. See [chapter 13](./13-llm-router.md) for details.

---

## The `supervisor/` crate

Cross-platform service supervisor. Generates and installs `systemd --user`
units on Linux and `launchd` plists on macOS, behind one `Supervisor` trait
(`install` / `start` / `stop` / `uninstall` / `status`, plus the `*_target`
variants), so the agent comes back after logout or reboot.

`kastellan-cli install` drives it to bring up **`kastellan.target`**, which
groups:

- `kastellan-postgres` — the per-user cluster;
- `kastellan-guard` — only when a guard backend is configured: llama.cpp's
  `llama-server` serving the guard model on loopback, started **before** the
  core (the daemon refuses to boot when a configured guard is unreachable);
- `kastellan-core` — the daemon, ordered after the other two.

`specs.rs` holds those service specs (with `after` / `part_of` ordering and a
`restart_backoff`); `systemd_user/` and `launchd_agents/` are the two
backends; `env_file.rs` implements systemd's `EnvironmentFile=` grammar so the
macOS backend reads the operator overlay identically; `status.rs` is the
backend-neutral `ServiceStatus`. Service names are screened by
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

## The `net-classify/` crate

`kastellan-net-classify` is a single pure function family: given an IP
address, is it in a denied range (loopback, private, link-local, CGNAT,
multicast, the IPv4-mapped forms of those, …)? The egress proxy calls it
after resolving a hostname itself, so a DNS answer pointing at an internal
address (SSRF, DNS rebinding) is refused at connect time. Keeping it in its
own dependency-free crate makes it trivially testable and lets core reuse
the same predicate.

---

## The `tests-common/` crate

`kastellan-tests-common` is a dev-dependency shared by every crate's
integration tests. The pieces you will meet first:

- **`pg`** — per-test Postgres clusters: each test that touches the DB gets
  its own freshly initialised data directory and shuts it down at the end.
- **`skip`** — the evidence markers. `skip_line`, `warn_line` and `e2e_line`
  build the `[SKIP]`, `[WARN]` and `[E2E] <tier>: <detail>` lines (each
  starts with a newline and must be printed in one write, so it can never
  land mid-line where the gate script would miss it). Also the Postgres
  precondition helpers such as `pg_bin_dir_or_skip`.
- **`require`** — the one **REQUIRE-knob** contract. A tier is declared once
  as `const … = RequireKnob::new("KASTELLAN_<TIER>_REQUIRE_E2E", "<tier>")`
  beside the tests it gates; when its knob is set, a missing precondition
  fails instead of skipping, and a met one is announced as an `[E2E]` line.
  Every knob is listed in `require::KNOBS`.
- **`panic_hook`** — a panic hook that neutralises the panic message, so a
  panic cannot forge an evidence line; `own_line` is its marker builder.
- **`gate_script_tests`** — tests that pin `scripts/run-e2e-gate.sh` against
  `KNOBS` in both directions (every knob the script names exists, every knob
  that exists is demanded by some profile) and run its verdict logic against
  a fake `cargo`.
- **`sandbox`**, **`microvm`** (Firecracker and Apple `container`
  preconditions, image freshness checks), **`scripted_llm`** (a queued mock
  OpenAI-compatible server), **`mock_localmail`** / **`live_localmail`**,
  **`daemon`** (real-daemon bring-up for CLI e2e tests), and smaller helpers.

[Chapter 5](./05-build-test-run.md#gated-e2e-tiers-skip-as-pass-and-the-gate-script)
explains how these fit together when you run a tier as evidence.

---

## `scripts/`, `config/`, `prompts/`, `seeds/`

```
scripts/
  build-release.sh       THE way to build target/release/ (see chapter 5)
  run-e2e-gate.sh        Run a gated e2e tier as evidence (see chapter 5)
  upgrade_from_git.sh    Operator upgrade: pull, release build, reinstall
  linux/                 One-time privileged host setup: bwrap AppArmor
                         profile, Postgres 18 install, Firecracker vsock +
                         image dir + guest kernel
  workers/               Python-worker installers (gliner-relex,
                         browser-driver), Apple container image builds
                         (python-exec, gliner-relex), Firecracker binary +
                         guest-kernel fetch + rootfs builds (microvm/,
                         kv-demo/)
  matrix/                Local conduwuit homeserver setup + dev bootstrap;
                         vps/ for a hosted homeserver
  mail/                  live-shape-gate.sh: the live localmail gate
  web-search/            SearxNG setup
  eval/, bench/, spikes/ Measurement scripts (guard model, planner, prefill)
  site/                  Website + manual-site build and checks
config/
  policy.yaml.example    Example runtime policy
  sandbox/               Example per-worker sandbox profile (shell-exec)
prompts/
  agent_planner.md       The classic planner's system prompt
  agent_notebook.md      The notebook planner's system prompt
  sift_sorter.md         The sift sorter's per-item prompt
seeds/memory/
  l0_meta_rules.toml     The L0 meta-rules, seeded into memory at daemon start
```

The daemon reads `prompts/` and `seeds/` relative to its working directory
unless `KASTELLAN_PROMPTS_DIR` / `KASTELLAN_L0_RULES_FILE` say otherwise —
which is why you run a dev daemon from the repo root, and why the installer
copies both into `~/.local/share/kastellan/`. Prompts are hashed and
versioned in the `agent_prompts` table, so a change to a prompt file is a
reviewed, audited change like any other.

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
| Host setup scripts | `scripts/linux/` |
| Worker installers, image and rootfs builds | `scripts/workers/` |
| Release build / e2e gate | `scripts/build-release.sh`, `scripts/run-e2e-gate.sh` |
| REQUIRE knobs and evidence markers | `tests-common/src/require.rs`, `tests-common/src/skip.rs` |
| Planner prompts | `prompts/` |
| Operator environment settings | `docs/deploy/operator-env.md` |
| Worker-output injection guard | `core/src/cassandra/injection_guard.rs` |
| LLM router (sole egress) | `llm-router/src/` |
