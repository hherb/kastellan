# kastellan — Development Roadmap

Sequenced feature list. Items are added whenever a feature decision is made
and ticked off as they ship. Order reflects expected build sequence — earlier
items unlock later ones.

> **How to update.** When we agree on a new feature, append it to the most
> appropriate phase (or a new phase) at the position that respects
> dependencies. When a feature ships, change `[ ]` → `[x]` and condense its
> entry to a **terse one line** with the commit/PR hash — enough to document
> the build sequence, no more. Pending `[ ]` items keep their full design
> context. **Pure refactors, test-module lifts, file splits, clippy/CI gates,
> flake fixes, and isolated bug fixes are NOT recorded here** unless they're
> load-bearing for remaining work — git history and the handover archives
> (`handovers/archive/`) are the durable record for those.

---

## Phase 0 — Skeleton & First Sandboxed Worker (Linux)

- [x] Cargo workspace, AGPL-3.0 license, README, .gitignore — `140eec5`
- [x] `kastellan-core` (bin+lib stub) — `140eec5`
- [x] `kastellan-sandbox` crate skeleton (trait + policy struct) — `140eec5`
- [x] `kastellan-supervisor` crate skeleton — `140eec5`
- [x] Architecture & threat-model doc skeletons — `140eec5`
- [x] Linux bwrap backend (`linux_bwrap.rs`): unshare-all, FS bind, --clearenv, --setenv, die-with-parent, new-session, as-pid-1 — `eae3df4`, `f2411ec`
- [x] AppArmor `unprivileged_userns` workaround: `scripts/linux/install-bwrap-apparmor-profile.sh` + `LinuxBwrap::probe()` — `eae3df4`
- [x] Sandbox negative tests (/etc/passwd + /home invisible, listed paths visible, net unreachable, relative paths rejected) — `eae3df4`
- [x] `kastellan-protocol` crate: JSON-RPC 2.0 server/client over stdio (MCP-stdio compatible) — `f2411ec`
- [x] `workers/shell-exec`: argv allowlist, no shell interpretation (`KASTELLAN_SHELL_ALLOWLIST`) — `f2411ec`
- [x] `core::tool_host::spawn_worker`: spawn worker under sandbox, return connected protocol Client — `f2411ec`
- [x] End-to-end test: core → bwrap → shell-exec → JSON-RPC echo + POLICY_DENIED + METHOD_NOT_FOUND — `f2411ec`

## Phase 0 hardening — Defence in depth (Linux)

- [x] Landlock LSM as second FS-allowlist layer in the worker (ABI v6) — `3210f70`, `97d4465`
- [x] seccomp-bpf syscall filter — per-profile allow-list (`Strict` kills `socket()`, `NetClient` permits) — `3210f70`, `97d4465`
- [x] Worker prelude crate (`workers/prelude`): `serve_stdio` calls `lock_down()` before serving — `3210f70`
- [x] `tool_host` derives lockdown env (`KASTELLAN_LANDLOCK_RW` / `KASTELLAN_SECCOMP_PROFILE`) so callers can't skip worker-side layers — `3210f70`
- [x] cgroup v2 CPU/memory caps via `systemd-run --user --scope` (MemoryMax + MemorySwapMax=0 + CPUQuota + TasksMax); probe fails closed without a live `systemd --user` — `3cea642`
- [x] Policy-driven `cpu_quota_pct` / `tasks_max` + `setrlimit(RLIMIT_CPU)` `cpu_ms` enforcement (cross-platform `prelude/rlimit.rs`) — closes #6, 2026-05-14
- [x] Per-task `Workspace` RAII type (`<root>/<task_id>/{in,out,tmp}`, single owner, `extend_policy` wiring) — `9333311`
- [x] Spawn timeout / wall-clock kill (`WorkerSpec.wall_clock_ms`, watchdog thread, `kill(-1)`-fanout guard) — `57edfb2`

## Phase 0b — macOS Port (Seatbelt)

> Done before adding more workers, to stop Linux-isms leaking through the codebase.

- [x] `macos_seatbelt.rs`: SandboxPolicy → `.sb` (TinyScheme) generator; strict profile denies unrestricted mach-lookup (#1) — `2fa46a2`
- [x] `sandbox-exec` invocation + `setsid` fresh-session isolation (#2) — `2fa46a2`
- [x] setrlimit CPU via shared `prelude::rlimit` (mem/wallclock deferred to container backend / parent watchdog) — 2026-05-14
- [x] Network containment via `(deny network*)` + allowlist — `2fa46a2`
- [x] All sandbox containment + e2e tests mirrored green on macOS — `2fa46a2`

## Phase 0 cont. — Service supervisor

- [x] Linux `systemd --user` unit generator + `systemctl --user` driver (`supervisor/src/systemd_user.rs`) — 2026-05-10
- [x] macOS LaunchAgent plist generator + `launchctl bootstrap` driver (`supervisor/src/launchd_agents.rs`) — 2026-05-08
- [x] Core daemon `ServiceSpec` (`specs::core_service_spec`) + cross-OS `default_probe()` + e2e against the real binary — 2026-05-09
- [x] `kastellan.target` that brings up Postgres + core — native systemd `.target` (Linux) / readiness-based bundle (macOS); inference is an external health-checked dependency, workers are core-owned (spawned on demand). `TargetSpec` + `Supervisor::{install,start,stop,uninstall}_target` + `specs::kastellan_target_spec()`; `ServiceSpec.after`/`part_of` ordering fields; gated `target_smoke` e2e ran live against `systemctl --user` — branch `feat/kastellan-target-bring-up`, 2026-06-06
- [x] Auto-restart with backoff on worker crash (Option K). `ServiceSpec.restart_backoff: Option<RestartBackoff { max_delay_sec, steps }>` (additive, `#[serde(default)]`, `None` = old constant-`RestartSec=5` behaviour). systemd backend emits `RestartSteps`/`RestartMaxDelaySec` (252+; older systemd warns-but-loads) inside the `keep_alive` block; macOS launchd warns-and-ignores at install (no equivalent knob — same posture as `after`/`part_of`). core+postgres specs wired with a 5s→300s/8-step curve. Builder test modules lifted to siblings to stay under cap — branch `feat/restart-backoff`, 2026-06-07

## Phase 0 cont. — Postgres bring-up

- [x] Local Postgres via PGDG apt + user-level supervisor unit (`scripts/linux/install-postgres.sh`, PG 18; macOS via Homebrew) — 2026-05-09
- [x] Localhost-only UDS, peer auth, dedicated `kastellan` role, locked-down `initdb` (`kastellan-db-init`, idempotent) — 2026-05-09
- [x] `pgvector` extension; full-text search via native `tsvector`+GIN; graph storage via relational `entities`/`relations` behind a `Graph` trait — 2026-05-09 (closes #9/#10 won't-fix)
- [x] `db/migrations/` skeleton (`memories`/`tasks`/`entities`/`relations`/`audit_log`/`secrets`); `vector(1024)` (bge-m3 dim) — 2026-05-09
- [x] `sqlx` embedded `MIGRATOR` run at core startup, fail-closed — 2026-05-09
- [x] Secrets at rest: AES-256-GCM + OS keyring (`db::secrets`, AAD-bound, `Zeroizing`); migration 0004 — closes #12, 2026-05-10

## Phase 0 cont. — Audit log

- [x] Non-superuser `kastellan_runtime` role + DB-layer REVOKE on `audit_log` (append-only enforced by Postgres); migration 0002 — 2026-05-10
- [x] Append-only audit writer at the `tool_host::dispatch` chokepoint; migration 0003 NOTIFY trigger; runtime-pool `SET ROLE` on every connection — closes #11, 2026-05-10
- [x] JSONL on-disk mirror under `~/.local/state/kastellan/` (`audit_mirror::spawn_mirror`, daily rotation, fsync per write) — 2026-05-10
- [x] CLI viewer: `kastellan-cli audit tail` (no DB connection required) — 2026-05-10

## Phase 0 cont. — LLM router stub

- [x] OpenAI-compatible HTTP client (`kastellan-llm-router`, `Router::send`, reqwest + rustls) — Option J, 2026-05-10
- [x] Local backend pointer (vLLM/SGLang :8000 Linux, Ollama :11434 macOS; `KASTELLAN_LLM_*` env) — 2026-05-10
- [x] Frontier backend pointer — unwired (`PolicyDeniedFrontier`) until the Phase-5 policy gate; key sourced from `db::secrets`, never env — 2026-05-10

## Phase 1 — Memory & Loop

- [x] **Dispatcher chokepoint invariant** — every tool/channel/routine action enters core through `tool_host::dispatch()`; `WorkerCommand` newtype seals direct worker calls (module-private), pinned by a `compile_fail` doctest — Option M, 2026-05-10 (seal tightened #16, 2026-05-13)
- [x] **`memory::recall` — semantic + lexical lanes** via pgvector + `tsvector`/GIN, fused with Reciprocal Rank Fusion — Option N, 2026-05-10
- [x] **Graph lane in `memory::recall`** — `memory_entities` join table (migration 0007) + 1-hop `Graph::neighbors` expansion fused alongside semantic/lexical; `GRAPH_FANOUT_CAP_PER_SEED=32` — Option P, PR #41 (`76fe940`), 2026-05-13
- [x] **Embedding router** — `Router::embed` + `core::memory::embed_query`; OpenAI-compat `/embeddings`, dim-validated against `EMBEDDING_DIM` — Option O, 2026-05-12
- [x] **Scheduler (CASSANDRA Phase 1)** — tick → claim task → LLM plan → CASSANDRA review → step dispatch loop; lanes/leases/`FOR UPDATE SKIP LOCKED`/NOTIFY triggers, `agent_prompts` SHA-256 ledger; migrations 0005/0006 — merged `93da413`, 2026-05-11 (stub ConstitutionalGuard + DeterministicPolicy held for the observation phase)
- [x] **`ToolHostStepDispatcher`** — `ToolRegistry`/`ToolEntry` host-side allowlist, spawn-per-step through `tool_host::dispatch`, deny-by-default `shell_exec_entry` — 2026-05-11
- [x] **Scheduler audit-row coverage** — spec §7 task-lifecycle rows (`task.running`/`task.<state>`/`task.finalize`), step short-circuit rows (`step.unknown_tool`/`step.spawn_failed`), crash-recovery `task.crashed`+finalize, producer-side `task.submitted`/`task.cancelled`/`task.finalize` from the CLI — 2026-05-12 → 2026-05-14
- [x] **`cli_ask_e2e` full-chain pin** — real CLI → daemon → sandboxed worker → Postgres, mock LLM only; the canonical end-to-end regression for the whole loop — 2026-05-11
- [x] **First real `ConstitutionalGuard` rule** — substring screen over the 5 constitutional principles → `Verdict::ConstitutionalBlock` — PR #67 (`67d29a0`), 2026-05-15
- [x] **First real `DeterministicPolicy` rule** — data-classification invariants (ceiling≥floor / step≥floor / step≤ceiling) → `Verdict::Block`; paired `ask --classification-floor` — 2026-05-15
- [x] **Observation / rule-iteration harness** — `observation::capture` fixture captures (SCHEMA_VERSION), `plan.formulate` carries full Plan, `observation replay` re-runs the CASSANDRA chain offline with verdict deltas, lenient plan parser for fenced LLM output — 2026-05-13 → 2026-05-15
- [x] **Constitutional refusal state** — `Plan.refused` + `Outcome::Refused` + terminal `tasks.state='refused'`; migration 0012 — closes #23, PR #59 (`f1fea54`), 2026-05-14
- [x] **Automatic classification-floor inference** — CLI keyword classifier + agent raise-only `Plan.floor_request`; `ClassificationFloorSource` provenance, inner-loop `max(producer, agent)`; runner rejects forged `agent_raised` (#71) — PR #70 (`4ddfe3b`), 2026-05-16
- [x] **Memory layers — L1 always-in-context index** — `MemoryLayer{Meta..Digest}` + `layer` column (migrations 0013/0014); `load_l1` row/byte caps; L0-writer policy enforced in code — PR #69 (`eb8e4bd`), 2026-05-15
- [x] **L0 seed data loader** — TOML meta-rules → validated L0 rows, idempotent seeding, `load_l0_active`; starter `seeds/memory/l0_meta_rules.toml` — 2026-05-16
- [x] **Prompt assembler (L0 + L1 + base)** — `assemble_system_prompt` + `SystemPromptBuilder` trait wired into every plan iteration; drift-detection keys in `plan.formulate` — 2026-05-16
- [x] **Recall-lane wiring** — `RecallBuilder` composes `embed_query` + `recall` into the assembled prompt (degrade-and-warn); first production consumer of `Router::embed` + `recall` — PR #79 (`7553404`), 2026-05-17
- [x] **L1 promotion writer** — operator `memory l1 {add,list,remove}` + agent `Plan.l1_insight`; validator + source-agnostic dedup — PR #82 (`eb6b8a8`), 2026-05-18
- [x] **Worker lifecycle policy** — `Lifecycle::{SingleUse, IdleTimeout}` (warm-keep, post-completion-only caps, `stateless` contract); `WorkerLifecycleManager` trait + Single/Idle/Composite managers; passive crash detection + restart backoff — spec + slices 1–2, PR #83 (`b7dba3a`), 2026-05-18 (hardening #84/#85/#86 followed)
  - [x] **Worker manifest plumbing** (item 11; resolves lifecycle spec open question 1 — Rust consts, not on-disk TOML) — `WorkerManifest` trait + per-worker impl (`ShellExecManifest`/`GlinerRelexManifest`) + static `WORKER_MANIFESTS` driving a pure `assemble_registry`; `build_tool_registry` reduced to allowlist-prefetch + `ResolveCtx`; `current_exe()`-relative sibling binary discovery (env override wins, set-but-invalid override fails closed, gliner exempt). Behaviour-preserving — PR #187 at `2e3d0c5`, 2026-06-05
  - [ ] **Slice 3 (operator surface + SIGTERM grace)** — `kastellan-cli supervisor status` for warm workers + cap state; formal SIGTERM-grace-then-SIGKILL teardown via `grace_period_seconds`; proactive SIGCHLD crash detection. Low priority until a worker needs one of these.
- [x] **GLiNER-Relex worker** (first `IdleTimeout` consumer) — Python package + Rust manifest/typed client; CPU/CUDA/MPS device resolution; Apple-`container` backend variant — slices 1–2.5 + macOS slice, PRs #88/#103/#118, 2026-05-18 → 2026-05-25
  - [ ] **operator-CLI macOS validation** (operator action): install Postgres locally (`brew install postgresql@17 && brew services start postgresql@17`) and rerun `KASTELLAN_GLINER_RELEX_ENABLE=1 cargo test -p kastellan-core --test gliner_relex_e2e -- --nocapture` to exercise the full PG-backed lifecycle path on darwin. Python `_resolve_device` is already cross-validated; this is the lifecycle-manager validation. Half-hour once PG is installed.
- [x] **Entity extraction v2** — single-pass GLiNER-Relex call; `EntityExtractor` trait, quarantine-by-default (`entities.quarantine`/`name_norm`, migrations 0015/0016), extraction runs before recall — PR #91 (`f12b460`), 2026-05-19. (v1 `HybridEntityExtractor` was superseded; design preserved at `docs/superpowers/specs/2026-05-18-entity-extraction-graph-lane-design.md`.)
- [x] **Memory-write-time entity auto-linker** — `link_memory_entities` threaded through L0/L1 writers; one shared extractor across query- and write-time — PR #92 (`d58ecc9`), 2026-05-19
- [x] **Operator quarantine-review CLI** — `entities {list,show,approve,reject,merge}`; approving an entity makes `recall(GRAPH_ONLY)` return its memories — PR #93 (`028e541`), 2026-05-20. Completes the graph-lane chain end-to-end (v2 extractor → auto-linker → review CLI).
- [x] **Relation-label vocabulary** — `relation_kinds` (migration 0017, 19 seeds) fed to the worker so triples are no longer silently dropped; `RelationKindsCache` — PR #100 (`5bcd060`), 2026-05-21
- [x] **Vocabulary management CLIs** — `relations kinds {add,remove,list}` (PR #109) + `entities kinds {add,remove,list}` (PR #110) over the lookup tables via `connect_admin_pool` — 2026-05-22
- [x] **`relations show <entity-id>`** — outbound + inbound edge walk (recursive CTE, depth-capped, diamond-deduped), text/JSON output — PR #113 (`9a46e18`), 2026-05-23
- [x] **macOS Apple `container` micro-VM backend** — `SandboxBackendKind` per-worker selection; closes the macOS memory-enforcement gap Seatbelt can't cover (GLiNER-Relex now enforces `mem_mb` on darwin) — spike + slices 1/2/2.5, PRs #105/#106/#108/#118, issue #55, 2026-05-21 → 2026-05-25
- [ ] **`context_manager`**: token-budget + task-completion + wall-clock reset triggers
- [ ] **Reset snapshot writer** (compact context → memory before reset)
- [x] **Worker-output prompt-injection guard (slice 1)** — `cassandra::injection_guard` 22-entry catalogue + `screen`; on Block, `tool_host::dispatch` substitutes a redacted placeholder and writes a `policy/injection.blocked` audit row carrying only SHA-256 — closes Item 30, PR #141 (`62905ae`), 2026-05-28. (#142 chat-template false-positives deferred until a `web-fetch`/MCP worker exists.) **Slice 2 candidates (deferred per YAGNI):** Review tier (0.45–0.70 band), `kastellan-cli policy review` surface, heuristic/combinatorial scoring, multilingual catalogue, per-tool policy.
- [x] **Opaque secret references (slice 1)** — `Vault` (TTL'd `RwLock<HashMap>`) + `SecretRef` opaque newtype + `substitute_refs_in_params`; substitution in `tool_host::dispatch` is fail-closed, plaintext never in audit rows (#147); `KASTELLAN_BOOTSTRAP_SECRETS` daemon-startup materialization — closes Item 31, PR #146 (`bc36e4c`), 2026-05-29. Pre-req for the Phase-5 frontier gate. **Slice 2 (deferred):** CLI↔daemon IPC + `kastellan-cli secrets materialize`; per-task vault scoping; embedded substitution.
- [x] **L3 skill arc** (crystallise → approve → pin → invoke) — the GenericAgent skill import: distil a successful trajectory into a parameterised JSON-RPC tool-call template stored as an L3 `memories` row, recalled and re-invoked on the next similar task. **Complete end-to-end on `main`:**
  - Crystallisation writer — agent emits `Plan.l3_skill`; validated, SHA-256-deduped, stored `layer=3 trust:untrusted` with a `dispatch_count >= 1` grounding gate; `memory l3 {list,remove}` — PR #173 (`6eb966e`), 2026-05-31
  - Trust enum + approval gate — `SkillTrust{Untrusted|UserApproved|Pinned}` (fail-safe parse); pure `evaluate_approval` (re-validate + `secret://` scan + tool-existence vs the `registry.loaded` snapshot, fail-closed); `memory l3 {approve,revoke}` — PR #176 (`bbcc7b3`), 2026-05-31
  - Recall surfacing — `<skills>` planner block for `UserApproved`/`Pinned` only (reference, no invoke), SQL trust push-down — PR #177 (`4b978d8`), 2026-06-01
  - Operator invocation — `memory l3 run` substitutes `{{params}}` → live-registry re-validation → sandboxed dispatch; dry-run by default, no CASSANDRA review on the operator path — PR #178 (`d862e6e`), 2026-06-03
  - Autonomous door — agent `Plan.invoke_skill` expanded before CASSANDRA review; gated on a new `pinned` tier (strict subset of operator-runnable); `memory l3 pin` — PR #181 (`6e10a81`), 2026-06-04
  - Daemon reroute (#179) — `memory l3 run` enqueues an `l3_run` task executed daemon-side against the single live `ToolRegistry` (the Postgres `tasks` queue + `LISTEN/NOTIFY` IS the operator→daemon command channel), retiring the in-process path and its env-divergence cliff — PR #186 (`67bc474`), 2026-06-05, **#179 CLOSED**
- [x] **Developer onboarding manual** — `docs/devel/manual/` (10 chapters + index, ≤2 A4 pages each) — PR #119 (`99bbfab`), 2026-05-25
- [x] **Large-tool-result handoff cache** — in-memory per-task content-addressed `HandoffCache` (`core/src/handoff.rs`). Oversized `Ok` results (serialized JSON > `DEFAULT_RESULT_BYTE_CAP` = 64 KiB) are stashed in the dispatcher layer (`ToolHostStepDispatcher::dispatch_step`, after `tool_host::dispatch` returns — the sealed chokepoint is untouched) and replaced with a `{handoff_ref, byte_len, summary_head}` placeholder. A reserved `handoff`/`fetch` built-in, intercepted before registry lookup (no worker spawn), returns clamped slices (`MAX_FETCH_BYTES` = 256 KiB). `task_id` threaded through `StepDispatcher`; entries purged at task terminal in the lane runner; per-task byte budget + global `MAX_TRACKED_TASKS` backstop bound memory. Blocked injection outputs are never stashed (they arrive as the tiny `injection_blocked` placeholder, under cap); operator `memory l3 run` path (`task_id <= 0`) passes through verbatim. In-memory (not the unwired `Workspace` scratch) per the design; `web-fetch`'s 100 KiB text cap is the realistic worst case. — branch `feat/handoff-cache`, 2026-06-08. Review follow-ups (2026-06-09): the stash branch now has real-worker dispatcher coverage (`scheduler_step_dispatch_e2e::dispatcher_stashes_oversized_ok_result_only_for_positive_task_id` — shell-exec echo > 64 KiB, asserts placeholder + cache round-trip + purge + the `task_id = 0` passthrough gate + the `handoff.stashed` audit row), closing [#198](https://github.com/hherb/kastellan/issues/198); the global backstop now `warn!`s when it evicts a bucket; the fetch intercept documents why it (unlike stash) is ungated on `task_id`. Planner-surface follow-up (2026-06-09, PR [#200](https://github.com/hherb/kastellan/pull/200)): `assemble_system_prompt` now emits an always-present, drift-proofed `<handoff>` block (`render_handoff_block()` in `core/src/prompt_assembly/assemble.rs`, interpolating the source-of-truth `HANDOFF_TOOL`/`HANDOFF_METHOD_FETCH` constants plus the byte caps `SUMMARY_HEAD_BYTES`/`MAX_FETCH_BYTES`, with a unit test cross-checking the placeholder fields, the real `fetch(...)` return shape, the fetch params, and both caps) teaching the planner the placeholder shape + the `fetch` step protocol — the mechanism is no longer inert. Deferred: per-tool `result_byte_cap` override (YAGNI); on-disk store.
- [x] **Memory two-tier write path: `put_doc()` vs `put_doc_light()`** — `db::memories::insert_memory_light(executor, body, metadata, layer)`: a thin named delegate to `insert_memory_at_layer` with `embedding = None` (no new SQL, no migration), inheriting the L0 `PolicyViolation` guard. Documents the degradation contract (lexical + `metadata @>` work; semantic + graph degrade gracefully — `semantic_search` already filters `WHERE embedding IS NOT NULL`). PR [#195](https://github.com/hherb/kastellan/pull/195) (`39a036a`), 2026-06-07. **Deferred follow-ups:** core-side caller wiring; per-namespace caps + oldest-eviction (openhuman quotes "max 50 KV entries, max 200 docs") — fits on `memories.metadata` as the namespace selector with no schema change, but does not block this surface; a graph-lane degradation test ([#196](https://github.com/hherb/kastellan/issues/196)).

## Phase 2 — Channels (read-only)

> **Primary channel decided 2026-06-12 (operator brainstorm):** **Matrix, self-hosted,
> single-user, federation OFF** (E2E via `matrix-rust-sdk` + `vodozemac`, vendor-neutral, zero
> marginal cost, all platforms via Element). **Email is the cross-transport fallback** (separate
> failure domain), used for low-trust async notifications, never commands. Signal (`presage`
> fragility + ban risk) and Telegram (no bot E2E, centralized) rejected as primary. Homeserver
> runs as a supervised **conduwuit** unit; hosting is operator-selectable, fail-down: Tier A
> dedicated VPS (preferred) → Tier B existing WireGuard VPS (co-host = shared blast radius with
> network ingress) → Tier C the kastellan host itself ("poor man's" default). Matrix has **no
> single-user homeserver failover** — redundancy is the cross-transport email fallback, not a
> second homeserver. Full design + co-hosting security analysis + slice decomposition:
> `docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`.

- [x] **Channel-bus abstraction (build first)** — `core/src/channel`: `Channel` trait (inbound
  `IncomingMessage` stream + outbound `send`, dyn-safe) + pure security core — fail-closed
  `PeerAuthorizer`/`StaticPairings` (`auth.rs`), `classify_inbound` (authorize→`injection_guard`
  screen under `GuardProfile::Strict`→`tasks` payload, `ingest.rs`), `reply_for_completed_task`
  (finalized task→user reply, `route.rs`) — plus the `ChannelBus` runtime (`bus.rs`) over four
  seams (`Channel`/`PeerAuthorizer`/`ChannelEvents`/`CompletedTasks`; real `PgChannelEvents` +
  `PgCompletedTasks` over the `tasks` queue + `tasks_completed` NOTIFY, no new IPC). Channel tasks
  mirror the `ask` payload so the scheduler is unchanged; unpaired peers + injection are dropped +
  audited (hash only). 18 unit tests + hermetic `FakeChannel` full-loop e2e + PG-gated real-queue
  e2e; clippy `-D warnings` clean. No live transport / no `main.rs` wiring (slice #2). Branch
  `claude/zen-bell-6bn2ze`, 2026-06-12. Plan: `docs/superpowers/plans/2026-06-12-channel-bus-abstraction.md`.
- [~] **Matrix inbound** (`MatrixChannel`, `matrix-rust-sdk`, E2E) — net allowlist scoped to the
  homeserver host:port only, force-routed through the egress proxy; single-user homeserver
  bring-up. (Slice #2.) **Phases A–C+E done** (branch `claude/zen-bell-6bn2ze`, 2026-06-12,
  hermetic + verified anywhere): `kastellan-matrix-wire` + sandboxed-worker JSON-RPC surface
  (`matrix.init/poll/send` over the SDK seam), core `MatrixChannel` (blocking driver thread
  bridging the synchronous protocol Client to the async `Channel` trait — keeps the protocol pure
  request/response), `build_matrix_policy` (pure), config-gated `main.rs` hook (byte-identical when
  unset), and `matrix_channel_e2e` (full loop against a real fake-worker process; paired round-trip
  + unpaired-dropped). **Phase D pending (DGX):** the real `matrix-rust-sdk` worker impl + egress
  coupling + persistent encrypted E2E store + restart supervision + `#[ignore]` live test (gated on
  the `live-matrix` feature; the matrix-rust-sdk-through-MITM-egress-proxy spike is the top risk).
  Spec/plan: `docs/superpowers/{specs,plans}/2026-06-12-matrix-inbound-sandboxed-worker*`.
- [x] **Homeserver supervisor unit + hardening** — conduwuit (federation OFF, loopback bind,
  token-gated→closed registration): hardened **system** systemd unit template + config template +
  `setup-conduwuit.sh` (dev/Tier-C) + `check-conduwuit-config.sh` (security-invariant verifier,
  `--self-test` green) + `docs/deploy/matrix-homeserver.md` (Tier A/B/C + co-hosting blast-radius +
  operator steps). Deliberately **not** a kastellan `ServiceSpec`: the user-level supervisor can't run
  conduwuit as the dedicated unprivileged `matrix` user, so the homeserver is a root/system unit (or
  a separate host), installed independently. Branch `claude/zen-bell-6bn2ze`, 2026-06-12. (Slice #6.)
- [ ] IMAP inbound worker (fallback channel; sandbox: net allowlist = configured IMAP server only). Low-trust async notifications only; require SPF/DKIM/DMARC pass + per-pairing in-body token before surfacing. (Slice #5.)
- [x] DM pairing flow: short-lived single-use pairing code issued out-of-band (`kastellan-cli pair issue`, hash-only storage); the new peer presents it in-channel; binding recorded in `pairings` (migration 0018), revocable, audited. Static contact allowlists rejected (forgeable) — `DbPeerAuthorizer` gates the bus on active `(channel,peer)` rows; the pairing carve-out is compare-only + operator-gated + single-use (`claim_code` atomic). **WebAuthn deferred** (no browser/CLI client surface yet). Daemon wiring (swap `StaticPairings`→`DbPeerAuthorizer` + pass `DbPairingService`) rides slice #2 Phase D. Branch `claude/zen-bell-6bn2ze`, 2026-06-12. Spec/plan: `docs/superpowers/{specs,plans}/2026-06-12-channel-pairing*`. (Pattern: ZeroClaw `security/{pairing,otp}.rs`.) (Slice #3.)
- [ ] ~~Telegram inbound adapter (`grammers`, Rust)~~ — **rejected as primary 2026-06-12** (no bot E2E, centralized, ban risk). Could return later as an additional `Channel` impl if a need arises.

## Phase 3 — Channels outbound + browser + web

- [ ] Egress proxy (per-worker host allowlist, TLS pinning, audit logging) — **decomposed into 4 slices; slice #1 shipped.**
  - [x] **Slice #1 — boundary host-allowlist enforcement + SSRF/IP defense** — new `workers/egress-proxy` crate (sandboxed per-worker CONNECT proxy over a UDS: reuses `web-common::HostAllowlist`, resolves DNS itself, rejects private/loopback/link-local/ULA/CGNAT/multicast resolved IPs with a literal-IP carve-out for an operator-allowlisted address, pins + dials the surviving IP, tunnels). `Net::ProxyEgress` sandbox variant across bwrap/seatbelt/container. `core/src/egress` (`spawn_sidecar`/`SidecarHandle` + pure `decision_to_audit`; proxy never touches PG — decisions flow proxy→core stdout→`audit_log`). Proven by an e2e test CONNECT client against the real sandboxed sidecar (allow/block/audit) + `#[ignore]` real-net + PG-gated audit-insert. **Does NOT route real workers yet** (mechanism only). Commits `df51c5c`..`29240eb`, branch `feat/egress-proxy-boundary`, 2026-06-10. Design+plan: `docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md`.
  - [x] **Slice #2 — unbypassable force-routing — COMPLETE, DGX-accepted, ON by default 2026-06-11** (branch `feat/egress-proxy-slice2-impl`). Built + Mac-verified (1521/0/7 workspace, clippy clean): the `web-common` CONNECT-over-UDS connector (`ProxyConnectGet` hyper+tokio-rustls + env-selected `make_get`; `web-fetch`/`web-search` swapped onto it); OS force-routing — Linux `bwrap` private netns + UDS bind, macOS Seatbelt deny-outbound-except-UDS (gating probe **confirms AF_INET denied** on the dev Mac, else `MacosContainer` fallback) + new additive `SandboxPolicy.proxy_uds`; **port-scoping the allowlist (closes [#241](https://github.com/hherb/kastellan/issues/241))** with a distinct audit flag for bare-host grants; the coupled host-side spawn (`core::egress::spawn_net_worker` — sidecar-first **fail-closed**, pure `rewrite_worker_policy`, 1:1 teardown via additive `SupervisedWorker.egress`, decision-ingest → `audit_log`). DGX kernel-barrier probe written (`sandbox/tests/linux_force_routing.rs`). **Task 4.4 — the live auto-flip — SHIPPED 2026-06-11** (branch `feat/egress-force-routing-autoflip`): `core/src/worker_lifecycle/force_route.rs` (pure `policy_net_is_force_routable`/`resolve_force_routing`/`spawn_worker_maybe_forced` + env-glue `from_env`) wires force-routing into both cold-spawn sites (`SingleUseLifecycle::acquire` + `idle_timeout::acquire_impl`) behind the opt-in **`KASTELLAN_EGRESS_FORCE_ROUTING` (default OFF ⇒ byte-identical legacy)**; `egress::net_worker::spawn_forced_net_worker` owns a per-worker scratch dir (RAII-cleaned via `EgressSidecar.scratch`); `main.rs` builds the config fail-closed (enabled-but-no-proxy-binary aborts startup). +16 Mac unit tests (1537/0/7, incl. a PR #250 review-hardening pass: UDS path-length guard, proxy-bin discovery tests, leak-not-remove on the unreachable no-bundle arm). **DGX acceptance + flip-on 2026-06-11** (native aarch64 over WireGuard SSH, branch `feat/egress-slice2-dgx-acceptance`): the kernel-barrier probe `force_routed_allowlist_worker_has_no_direct_route` passes with **real containment** (no `[SKIP]`); new `core/tests/egress_force_routing_e2e.rs` exercises the live coupling end to end (allow round-trip / off-allowlist 403 / on_decision ingest / 1:1 teardown / no-direct-route-from-the-jail / live `pg_decision_sink`→`audit_log`); **#243** confirmed (the `NetClient` seccomp profile allows AF_UNIX `bind`/`listen`/`accept`/`connect`); full workspace **1523/0/9**, clippy `-D warnings` clean. **Flipped ON by default**: `core_service_spec` injects `KASTELLAN_EGRESS_FORCE_ROUTING=1` (**fail-closed** — the daemon won't start without the `kastellan-worker-egress-proxy` binary beside it). Fixed a cross-platform default that would have broken macOS: `from_env`'s scratch root defaulted to `$TMPDIR`, too deep for macOS `sockaddr_un.sun_path` once nested — now `/tmp` on macOS. Deferred: stale-scratch crash-sweep [#251](https://github.com/hherb/kastellan/issues/251). Plan/spec: `docs/superpowers/{plans,specs}/2026-06-10-egress-proxy-slice2-force-routing*.md`.
  - [x] **Slice #3a — TLS interception (MITM) mechanism — COMPLETE 2026-06-12** (PR [#259](https://github.com/hherb/kastellan/pull/259), `e2a7b2b`). The per-worker proxy MITM-terminates each worker's TLS (in-proxy **ephemeral per-instance CA** via `rcgen`; private key never leaves the sandbox, public `ca.pem` exported beside the UDS) and re-originates a **webpki-validated** TLS session to the pinned origin — surfacing nothing new but an additive `tls_intercepted` audit flag (so slice #3b can scan the plaintext). New egress-proxy modules `ca.rs`/`leaf_cache.rs`/`mitm.rs` (`looks_like_tls` + async `intercept` — `tokio-rustls` `TlsAcceptor`/`TlsConnector` + `copy_bidirectional` on a per-connection current-thread runtime; accept-loop + `decide()` stay sync); `handle_conn` restructured connect→200→peek(`recv MSG_PEEK`)→branch (`0x16`→MITM, else transparent pass-through). Worker side: `web-common::ProxyConnectGet` trusts **only** the per-instance CA when `KASTELLAN_EGRESS_PROXY_CA` is set (fail-closed). Host side: `spawn_sidecar` gates readiness on `ca.pem`; `rewrite_worker_policy` binds it into the jail + sets the env. Verified: hermetic in-crate MITM round-trip (real two-leg TLS, Mac+DGX); live `real_mitm_fetch_through_sidecar` → **200 from example.com through the MITM** (macOS); DGX **under real bwrap** `forced_coupling`/`no_direct_route`/`pg_decision_sink` green (the rcgen-keygen + `MSG_PEEK` survive the `NetClient` seccomp profile). Mac `1555/0/8`, DGX `1538/0/10`, clippy `-D warnings` clean. Spec/plan: `docs/superpowers/{specs,plans}/2026-06-11-egress-proxy-slice3-tls-intercept*`.
  - [ ] **Slice #3b — credential-leak scanner** co-located on #3a's now-visible plaintext (line below).
  - [ ] **Slice #4 — TLS pinning** for the frontier/LLM egress path.
- [ ] **Credential-leak scanner co-located in the egress proxy** (egress-proxy slice **#3b**; #3a above already MITM-terminates so the proxy now sees plaintext) — every outbound request body and inbound response body scanned for the SHA-256 (prefix) of every secret currently materialized for the calling worker; hits are blocked and audited (hash + offset only, never plaintext). Scanning happens at the trust boundary, not inside the worker (which may itself be compromised). **Needs a new host→proxy provisioning path for secret-*value* hashes** (the Vault exposes none today; the audit log carries only the ref-string hash, not the value hash) — see the 3a spec's "Follow-up — slice #3b" section. (Pattern: IronClaw `safety::leak_detector`, ZeroClaw `security/leak_detector.rs`.)
- [~] **Matrix outbound** (agent → user replies over the E2E `MatrixChannel`) — primary outbound path (decision 2026-06-12; slice #4). **Reply mapping shipped** (2026-06-12): `route::reply_body` surfaces the agent's real completion `plan.result` (`{"kind":"text","body":...}` → the body; `message` alias; compact fallback) and maps `error`/`blocked`/`refused` to safe sentences — fixing the slice-#1 stub that mis-handled the real shape. Live delivery rides slice #2 Phase D. (~~Telegram/Signal outbound~~ rejected as primary — see Phase 2 note.)
- [ ] SMTP outbound in mail worker (`lettre`) — fallback-channel outbound; low-trust notifications, never the primary command path (slice #5)
- [x] `web-fetch` worker: HTTPS-only, host allowlist (self-enforced per redirect hop) + `Net::Allowlist` policy data for the egress proxy, 5 MiB body cap, 5-redirect cap, extracted readable text (HTML readability via `dom_smoothie`/`pdf-extract`/text+JSON), `Profile::WorkerNetClient` + `reqwest::blocking`+rustls — branch `feat/web-fetch-worker`, 2026-06-08. Deferred: egress-proxy enforcement (its consumer is now this worker); `web-search`; hermetic TLS happy-path e2e (waits on the proxy test-CA).
- [x] `web-search` worker (SearxNG default) — new crate `workers/web-search` exposing `web.search` (query → ranked `{title,url,snippet,engine}` hits from a SearxNG `/search?format=json` endpoint; web-search finds, web-fetch reads). Operator-configures `KASTELLAN_WEB_SEARCH_ENDPOINT`; the LLM supplies only the query (no URL-injection surface), so `http://` is allowed for loopback only, `https://` mandatory elsewhere. `Net::Allowlist` derived from the endpoint host:port; `cpu_ms=5_000`/`mem_mb=256`/`SingleUse`; fail-closed `from_env`. Carries the **shared `workers/web-common` crate** extracted from web-fetch (`HostAllowlist` + `HttpGet`/`ReqwestGet` transport + feature-gated `FakeGet`) — single source of truth for the security-critical allowlist matcher; web-fetch re-pointed, behaviour byte-preserved (its strict HTTPS-only rule unchanged). `scripts/web-search/setup-searxng.sh` stands up a local SearxNG with the JSON format enabled. — branch `feat/web-search-worker`, 2026-06-09. Deferred: category/language/engine params; pagination; hermetic SearxNG mock e2e (real round-trip stays `#[ignore]`); egress-proxy enforcement (shared with web-fetch).
- [x] **injection-guard per-tool profiles ([#142](https://github.com/hherb/kastellan/issues/142))** — chat-template tokens (`<|im_start|>`/`<|system|>`) no longer false-positive on fetched documentation. `GuardProfile { Strict (default, fail-closed) | Relaxed }` chosen per worker via `GuardProfile::for_tool` at the dispatch chokepoint; only `web-fetch`/`web-search` relax. Strict is byte-for-byte the Slice-1 algorithm; Relaxed collapses all chat-template matches to a single capped 0.40 sub-threshold contribution (handles the two-token tutorial case) so a lone token Allows but corroboration still Blocks. Committed benign/attack fixtures + full `extract_scannable_text`→`screen_with_profile` pipeline pin; `#[ignore]` live HuggingFace spot-check. — branch `feat/injection-guard-per-tool-profiles`, 2026-06-09. Deferred: Review tier; manifest-declared profiles; the catalogue-completeness evasion (Slice-1 limitation, documented).
- [ ] `browser-driver` worker (Playwright headless, dedicated profile, scratch FS) — ROADMAP:147
  - [x] **Spike + slice #1 scaffold — 2026-06-12** (branch `feat/browser-driver-worker`). **Spike GREEN both platforms**: headless Chromium (`chromium-headless-shell`) renders inside the real jail on macOS Seatbelt **and** the DGX (aarch64, bwrap) with `--no-sandbox --disable-dev-shm-usage`; no `/dev/shm`, no `--single-process`, no Firefox fallback. Findings (design spec §3.1): Seatbelt needs `ipc-posix-shm*`+`iokit-open/get-properties`+`mach-lookup/register` (a deliberate mach-lookup re-grant to narrow later); DGX seccomp needs 9 additions over `net_client` (`fallocate ftruncate getresgid getresuid inotify_add_watch inotify_init1 memfd_create pidfd_open restart_syscall`) + an `io_uring` `Errno(EPERM)` carve-out. **Slice #1 scaffold (spike-independent, TDD):** `workers/browser-driver` Playwright-Python package (`browser.render` stdio JSON-RPC: post-JS readable text + final HTML; readability-lxml; https-only, timeout-clamp, wait_until validation; 14 py tests) + `core/src/workers/browser_driver.rs` host manifest (pure `resolve_env`+skip reasons, `WorkerNetClient`+legacy direct-net `Net::Allowlist`, `KASTELLAN_BROWSER_DRIVER_ENABLE` opt-in, registered) + `GuardProfile::Relaxed` join. Workspace 1562/0/8, clippy clean. Spec/plan: `docs/superpowers/{specs,plans}/2026-06-12-browser-driver-worker*`.
  - [ ] **Phase 2** — real Playwright render in `render.py` (spike flags) + `Profile::BrowserClient` seccomp profile (the 9 adds + io_uring `Errno`) + Seatbelt browser-profile extension + self-contained-venv install script + TMPDIR→scratch wiring + `core/tests/browser_driver_e2e.rs` (hermetic deny + `#[ignore]` real loopback render).
  - [ ] **Slice #2** — egress-proxy integration (loopback-TCP↔UDS shim + in-browser per-instance-CA trust) so the browser force-routes through the proxy; broadened subresource allowlist policy.
- [ ] **MCP onboarding: discover → boot-spawn → validate → persist** — when kastellan grows third-party MCP-server support (any of the registries openhuman taps: Smithery, `modelcontextprotocol/registry`), naive "spawn it with the operator's intended policy" is a foothold attack: a malicious MCP server gets its production sandbox on first run. Adopt openhuman's pattern (`docs/MCP_SETUP_AGENT.md` — "boot-spawn for this one server... spawns the candidate subprocess in a scratch workspace"): every newly-discovered MCP server is first booted under a **maximally restrictive** `SandboxPolicy` (`Net::Deny`, `fs_read=[]`, `fs_write=[scratch]`, `Profile::Strict`, `cpu_ms=5000`, `mem_mb=128`), driven through `initialize` + `tools/list` over our existing `kastellan-protocol` stdio JSON-RPC, recording the declared tool surface to `db::mcp_servers` (new migration) only on success. Only then does the operator promote the server to its intended runtime policy via a separate explicit step that lands one `actor='cli' action='mcp.promoted'` audit row carrying the SHA-256 of the policy that was approved. Production runs refuse to spawn an MCP server whose policy hash has changed since promotion (mirror of the `tool_allowlists` SHA-256 drift detection from PR #51). Cross-platform "free" via `SandboxBackend` — same flow on bwrap, Seatbelt, and the new `MacosContainer` backend (Issue #55).

## Phase 4 — python-exec & agent-authored skills

- [ ] `python-exec` worker: scratch FS only, no net, hard CPU/mem/wallclock; curated stdlib bind
  - [x] **Slice #1 — worker + manifest + containment pins — 2026-06-12** (branch `claude/compassionate-shannon-3jeh31`).
    New Rust crate `workers/python-exec` mirroring shell-exec: `python.exec` pipes the source over stdin to
    `<python> -I -S -B -` (child env cleared; a Python exception returns as `exit_code` + traceback, **not** an RPC
    error, so the planner can iterate on its own code), 256 KiB code/capture caps (char-boundary-safe truncation +
    flags). Host manifest `core/src/workers/python_exec.rs`: **opt-in `KASTELLAN_PYTHON_EXEC_ENABLE=1`** (the
    deny-by-default posture moves to registration — there is no argv-allowlist equivalent for arbitrary code),
    interpreter discovery via `KASTELLAN_PYTHON_EXEC_PYTHON` override (set-but-invalid **fails closed**) → candidate
    cascade (`/usr/bin`, `/usr/local/bin`, `/opt/homebrew/bin`), registered in `WORKER_MANIFESTS`. Strictest policy of
    any worker: `Net::Deny`, `Profile::WorkerStrict` (the seccomp filter survives `execve` into the CPython child —
    pinned empirically by a new `coreutils_smoke::python3_survives_strict` case), **`fs_write = []`** — scratch is the
    jail's per-spawn ephemeral `/tmp` tmpfs (#89) granted through Landlock by an explicit
    `KASTELLAN_LANDLOCK_RW=["/tmp"]` in `policy.env` (never a host bind), cpu 10 s / mem 512 MiB / wall 30 s,
    `SingleUse`; "curated stdlib" = `-I -S` (no site-/dist-packages — determinism measure, the jail is the security
    boundary). Tests: 10 worker unit + 7 real-interpreter integration + 7 manifest unit + 3-test
    `core/tests/python_exec_e2e.rs` (production-policy jailed round-trip, socket-containment negative, scratch
    round-trip; skip-as-pass without PG/sandbox). Spec:
    `docs/superpowers/specs/2026-06-12-python-exec-worker-design.md`.
  - [ ] **Follow-ups:** DGX (bwrap) + Mac (Seatbelt) acceptance run of `python_exec_e2e` (built on a Linux container
    where sandbox suites skip); macOS writable scratch (Seatbelt deny-default leaves slice #1 with none — tighter, not
    looser; shares the per-spawn scratch wiring browser-driver Phase 2 needs); curated-wheels RO dir if/when the skill
    catalog demands packages; planner-prompt surfacing (parity note: the net workers have none either).
- [ ] Skill catalog (named/persisted Python skills) with optional human-approve gate
- [ ] **Skill trust enum** — `Untrusted | UserApproved | Pinned`, each level mapping to an explicit capability ceiling (which workers it may invoke, which net allowlists, which fs paths). Authorship and approval recorded in `audit_log`; promotion requires re-approval. (Pattern: IronClaw skill trust model — user-placed vs registry-installed. The L3 templated-skill arc above is the first concrete implementation of this shape.)
- [ ] Optional micro-VM backend for `python-exec` (Firecracker on Linux, Apple `container` on macOS — discovery spike completed 2026-05-21, verdict COMMIT; see [`docs/superpowers/specs/2026-05-21-macos-container-spike-notes.md`](../superpowers/specs/2026-05-21-macos-container-spike-notes.md))
- [ ] **Tiered delegation policy with hard no-recursion ceiling** — when the scheduler grows subagent delegation (today everything is one inner loop), borrow openhuman's `docs/DELEGATION_POLICY.md` four-tier shape: Tier 1 reply-directly (no tools), Tier 2 direct tool, Tier 3 inline subagent (≤5 turns, no new thread), Tier 4 dedicated worker thread (>5 turns). **The structural constraint that matters: workers do not spawn workers.** Encode it in `tool_host` as a compile-time check (`SubagentContext: Sealed` newtype that can only be constructed from the root scheduler) so the spawn tree is provably finite and the audit log has bounded fan-out per task. Maps cleanly onto the existing `Lifecycle::{SingleUse, IdleTimeout}` shape: tier-3 inline subagents are `SingleUse`, tier-4 dedicated threads piggyback on `IdleTimeoutLifecycle`. Pre-req for any meaningful agent-authored-skills work; defines the budget per skill invocation.
- [ ] **Stability-scored preference learning** — when the agent starts inferring user preferences (style, vetoes, tooling, timezone, identity facts), a naive "remember whatever the latest message said" path is vulnerable: a single injected message in any channel permanently flips a long-standing preference. Adopt openhuman's `docs/AGENT_SELF_LEARNING.md` scoring shape: `stability(class, key, value) = base × cue × user_state`, evidence weighted by source (explicit user statement 1.0, structural data 0.9, behavioural heuristic 0.7, recurrence 0.6), only "Active" at stability ≥1.5 (requires corroboration). Storage: new `user_profile_facets` table behind `db::profile`, runtime-role can INSERT bounded candidate rows but only the explicit "operator pin" CLI surface (`kastellan-cli profile pin <class> <key>=<value>`) can promote a facet to Active or override automatic scoring. Keeps `memory access is core-only` invariant intact — workers never write profile state. Pre-req for any prompt-assembly surface that injects a `UserProfileSection` (current `assemble_system_prompt` ships an `L0`/`L1` block but no profile facets yet).

## Phase 5 — Frontier escalation, hardening, audit UI

- [ ] Policy gate: per-tool, per-task, per-data-classification routing decision
- [ ] Frontier escalation through egress proxy (Anthropic / OpenAI)
- [ ] Read-only audit log viewer (CLI complete; web optional)
- [ ] 7-day adversarial soak test (prompt-injected channel content; no escapes in audit log)

---

## Cross-cutting / continuous

- [ ] Threat-model doc kept in sync with shipped backends
- [ ] Architecture doc kept in sync with shipped components
- [ ] License audit on every new dependency (AGPL-compatible only)
- [ ] CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace` — both Linux and macOS. (Clippy `-D warnings` gate live on `linux-check`, #153; `cargo fmt` still TODO.)
- [x] **Public website `kastellan.dev`** — landing + roadmap + security + contributing pages, hand-rolled static `site/` (no build step) served by Cloudflare Pages free tier; kastellan-branded SVG diagrams; `scripts/site/check-site.sh` verification suite; curated content kept fresh via HANDOVER checklist item 7; CF-side wiring is an operator step in `site/README.md` — branch `claude/relaxed-davinci-ddf2ed`, 2026-06-11
- [x] **Shared `kastellan-tests-common` dev-dep crate** — `PgCluster` + `bring_up_pg_cluster`, RAII guards, skip helpers, sandbox factory, binary discovery, macOS launchd serial lock, deterministic SHA-256-seeded embeddings — closes #15, 2026-05-12
- [x] **Memory deletion audit infrastructure** — `deleted_memories` table + AFTER DELETE trigger on `memories` (migration 0008), append-only by GRANT; preserves body/metadata/embedding/timestamps. Preventive infra for a future GDPR-style forgetting path — 2026-05-12
