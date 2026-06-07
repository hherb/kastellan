# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention. Older sessions are compressed
> into "Earlier history" below; full per-session detail lives in the
> [`archive/`](archive/) snapshots.

**Last updated:** 2026-06-07 (Memory two-tier write path — `insert_memory_light`, ROADMAP:130, branch `feat/memory-light-write-path`, PR [#195](https://github.com/hherb/hhagent/pull/195) **OPEN**; on macOS).

**Current state.** `main` is at `3c9f70d` (PR [#194](https://github.com/hherb/hhagent/pull/194) Option K restart
backoff **MERGED**; PR [#193](https://github.com/hherb/hhagent/pull/193) three clean test-lifts **MERGED**).
This session is on branch **`feat/memory-light-write-path`** at the docs commit below, PR
[#195](https://github.com/hherb/hhagent/pull/195) **OPEN**. Dev box on **macOS**. This session shipped
**ROADMAP:130 — `insert_memory_light`**, the "light" half of the two-tier memory write path:
- `db::memories::insert_memory_light(executor, body, metadata, layer)` — a thin named delegate to
  `insert_memory_at_layer` with `embedding = None`; no new SQL, no migration. Inherits the L0
  (`MemoryLayer::Meta`) `PolicyViolation` guard for free.
- Documents the recall **degradation contract**: lexical lane + `metadata @>` containment work
  normally; semantic lane silently skips the row (`semantic_search` filters `WHERE embedding IS NOT
  NULL`); graph lane never surfaces it (no `memory_entities` links).
- Two PG-required tests (round-trip + L0 rejection; cross-lane degradation pin) verified **passing
  against live PG** (Postgres.app v18).
- **Review-fix:** added a PG-free unit test (`insert_memory_light_rejects_l0_without_pg`, in
  `write.rs`) pinning the L0 `PolicyViolation` guard via a lazy pool that never connects — the guard
  short-circuits before any SQL, so it now has coverage on every dev machine, not only where live PG
  is configured.

**Deferred (per spec):** core-side caller wiring; per-namespace caps + oldest-eviction. Graph-lane
degradation is asserted by construction but not yet exercised in a test (heavier — needs
`link_memory_to_entities` + `graph_search`); tracked as
[#196](https://github.com/hherb/hhagent/issues/196).

**Prior session shipped** Option K — cross-platform exponential restart backoff
(ROADMAP:61, ticked, PR #194 MERGED):
- New `ServiceSpec.restart_backoff: Option<RestartBackoff { max_delay_sec, steps }>`
  in `hhagent-supervisor` — additive, `#[serde(default)]`, `None` reproduces today's
  constant-`RestartSec=5` output byte-for-byte (same precedent as `after`/`part_of`).
- **systemd** backend emits `RestartSteps`/`RestartMaxDelaySec` (252+; older systemd
  warns-but-loads) inside the `keep_alive` block only.
- **macOS launchd** warns-and-ignores at install (`tracing::warn!`) — launchd has no
  operator-controllable backoff; `build_plist` unchanged, pinned by a regression guard.
- `core_service_spec` + `postgres_service_spec` wired with a **5s→300s/8-step** curve.
- Two builder test modules lifted to siblings to stay under the 500-LOC cap
  (`systemd_user/builder.rs` 524→259; `launchd_agents/builders.rs` 508→234).

**Residual (deferred per the documented ≤27-over policy):**
`supervisor/src/launchd_agents.rs` is **508 LOC** (+8; tests already external, so a
fix needs a real prod-split — disproportionate for 8 lines).

Recent merged history: three clean test-lifts (PR #193); `macos_seatbelt.rs` test-lift
(PR #192); `systemd_user.rs` prod-split (PR #191); Phase-0 `hhagent.target` bring-up
(PR #190); L3 invocation arc COMPLETE (PR #186, #179 CLOSED); worker manifest plumbing
item 11 (PR #187). Full detail in Earlier history + archive snapshots.

**Session-end verification (macOS, on `feat/memory-light-write-path`):**
`cargo clippy -p hhagent-db --all-targets --locked -- -D warnings` exit 0;
`db/src/memories/write.rs` 373 LOC (under cap, +40 for the review-fix unit-test module). All
three light-path tests run **against live PG** (Postgres.app v18 via session-local
`HHAGENT_PG_BIN_DIR=/Applications/Postgres 2.app/...`): the new PG-free
`insert_memory_light_rejects_l0_without_pg` unit test passes, and the two e2e tests
(`insert_memory_light_round_trip_and_rejects_l0`,
`insert_memory_light_degrades_gracefully_across_lanes`) pass in 2.11s of real cluster bring-up. **Known macOS test-infra gotcha (not a
regression):** a *full-workspace* run under `HHAGENT_PG_BIN_DIR` flakes 4 tests in
`core/tests/embedding_recall_e2e.rs` at PG bring-up (`tests-common/src/pg.rs:249/314`) —
parallel `initdb`/launchd churn (issue #130 territory); they pass single-threaded and in
isolation. Use skip-as-pass for the whole workspace on the Mac; run live-PG suites
individually or on the DGX.

**Recently merged (safe to `git branch -d` if still local):**
`refactor/gliner-relex-prod-split` (PR #189), `refactor/recall-test-module-lift`
(PR #188), `feat/worker-manifest-plumbing` (PR #187).

**Recently merged (safe to `git branch -d` if still local; but see the
`fix/issue-179-...` caveat above — it has an unmerged skills commit):**
`fix/issue-179-l3-run-daemon-reroute` (PR #186),
`refactor/capture-test-module-lift` (PR #185), `refactor/l0-seed-test-module-lift`
(PR #183), `refactor/l3-over-cap-splits` (PR #182), `feat/l3-skill-autonomous-door`
(PR #181), `fix/issue-179-run-registry-divergence-diagnostic` (PR #180),
`feat/l3-skill-invocation` (PR #178), `feat/l3-skill-recall-surfacing` (PR #177),
`feat/l3-skill-approval-gate` (PR #176). Older merged branches are in the archive
snapshots.

**Toolchain note (standing).** Dev box + CI are on rustc **1.96.0**
(`dtolnay/rust-toolchain@stable`). On the dev **Mac**, `core` cannot be
cross-`cargo test`/`check`'d for Linux (its `ring` C dep needs
`x86_64-linux-gnu-gcc`, the #144 cross-compile wall) — `core`'s Linux path is
CI-verified, and the `linux-check` CI is **compile + clippy only** (no
`cargo test`). On the **DGX Spark** (aarch64), `core` compiles/tests/clippies
**natively**, so a full native-Linux `cargo test --workspace` +
`cargo clippy --workspace --all-targets -D warnings` are both runnable there.
The current native-Linux test baseline is **1327 / 0 / 4**
(`feat/hhagent-target-bring-up`; was 1311 on `main` at `cdadea1`).

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — high-level diagram, process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — invariant, scenarios in scope, defence-in-depth layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO list with commit hashes for shipped items
4. The design plan (outside the repo) — `~/.claude/plans/i-d-like-to-design-logical-starlight.md`
5. Memory notes (auto-loaded) — see `~/.claude/projects/-home-hherb-src-hhagent/memory/MEMORY.md`
6. Older handovers — `archive/handover_<timestamp>.md` (one snapshot per pruning event; full historical detail lives there). Most recent: [`archive/handover_20260605_pre-prune.md`](archive/handover_20260605_pre-prune.md).

## Working state (what's green right now)

```
hhagent (Rust workspace, 9 crates, AGPL-3.0)
├── core               hhagent-core: lib + 2 bins (`hhagent` daemon + `hhagent-cli` audit-tail viewer). Daemon blocks on SIGTERM/SIGINT via tokio::signal::unix; main.rs runs db::probe::run → connect_runtime_pool → spawn_mirror before wait_for_shutdown (fail-closed startup; mirror failures are logged but non-fatal). lib modules: tool_host (spawn_worker, dispatch chokepoint, lockdown-env derivation, wall-clock watchdog, sealed WorkerCommand, secret-ref substitution on input + injection-guard screen on output), secrets (Vault TTL'd RwLock<HashMap> + SecretRef opaque newtype + substitute_refs_in_params walker), cassandra/injection_guard (22-entry substring catalogue + screen + extract_scannable_text), workspace (per-task scratch with RAII cleanup), audit_mirror (PgListener-driven JSONL writer with daily rotation + fsync per write), audit_tail (`tail -f`-style follower used by `hhagent-cli audit tail`), scheduler/ (audit.rs pure helpers + canonical SCHEDULER_AUDIT_ACTOR; runner.rs spec §7 lifecycle rows + l3_run routing; tool_dispatch.rs short-circuit rows; crash_recovery.rs sweep_and_audit; l3_run.rs daemon-side L3 skill execution), memory/ (mod.rs facade + recall.rs three-lane RRF-fused recall + embed.rs embed_query + l0_seed/l1_promote/l3_crystallise/l3_approval/l3_invoke/l3_surface), worker_lifecycle/ (Lifecycle enum + SingleUse/IdleTimeout/Composite managers; idle_timeout.rs acquire path + idle_timeout/release.rs release path), entity_extraction/ (batch_upsert.rs two-phase unnest + per-row attribution), worker_manifest (WorkerManifest trait + Resolution + ResolveCtx + discover_binary — the uniform self-description each worker registers behind), workers/ (shell_exec.rs ShellExecManifest + shell_exec_entry; gliner_relex/ facade re-exporting wire.rs serde shapes + resolve.rs GlinerRelexEnv/resolve_env + entry.rs gliner_relex_entry/host+container builders + client.rs Client + manifest.rs GlinerRelexManifest), registry_build (static WORKER_MANIFESTS + pure assemble_registry + async build_tool_registry(pool, exe_dir))
├── db                 hhagent-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir, pg_bin_dir_candidates_with_env_override) + conn::ConnectSpec + RUNTIME_ROLE/set_role_runtime_statement + probe::run (ensure DB → migrate as superuser → SET ROLE → audit, fail-closed) + graph::{Graph trait, PgGraph; recursive-CTE path() + walk_outbound/inbound_edges + walk_edges_around with DISTINCT ON diamond-dedupe} + audit::{insert, fetch_by_id, fetch_since, truncate_payload} + memories::{insert, insert_memory_at_layer, insert_memory_light (embedding-skipping light write path), semantic/lexical/graph search, link_memory_to_entities, set_skill_trust, load_layer_by_trust} + entity_kinds + relation_kinds lookup caches + pool::{connect_runtime_pool, connect_admin_pool} + MIGRATOR (0001..0017) + memory_entities join table + deleted_memories audit table + secrets (AES-256-GCM at rest + OS keyring) + hhagent-db-init bin
├── llm-router         hhagent-llm-router: sole egress for LLM calls. Router::send + Router::embed over reqwest+rustls; Backend::{Local, Frontier} closed enum; PolicyGate trait (DefaultLocalPolicy always Local — Phase-5 seam). RouterConfig::from_env reads HHAGENT_LLM_* env. Per-OS default URL: vLLM/SGLang on Linux (:8000), Ollama on macOS (:11434). Frontier dispatch returns PolicyDeniedFrontier until Phase 5
├── sandbox            hhagent-sandbox: SandboxPolicy + SandboxBackend trait + SandboxBackendKind (cfg-gated per-OS) + SandboxBackends resolver + LinuxBwrap (wrapped in systemd-run --scope cgroup) + MacosSeatbelt + MacosContainer (Apple `container` micro-VM, macOS-only, opt-in per-worker)
├── supervisor         hhagent-supervisor: SystemdUser (Linux; driver in systemd_user.rs + pure builders re-exported from systemd_user/builder.rs) + LaunchAgents (macOS) + specs::{core_service_spec, postgres_service_spec, hhagent_target_spec} + default_probe. ServiceSpec carries after/part_of ordering + optional restart_backoff (RestartBackoff{max_delay_sec,steps}: systemd → RestartSteps/RestartMaxDelaySec, launchd → warn-and-ignore); TargetSpec + Supervisor::{install,start,stop,uninstall}_target (default = generic bundle for launchd; SystemdUser overrides with a native hhagent.target unit). Names screened by validate_service_name before unit-file write
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── tests-common       hhagent-tests-common: shared dev-dep crate (publish = false) — PgCluster + bring_up_pg_cluster(+_with_timeout), RAII guards, skip helpers, sandbox factory, binary discovery, macOS launchd serial lock (reentrant), deterministic SHA-256-seeded embedding seed. Consumed only from [dev-dependencies]; never linked into a runtime binary.
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS) + cross-platform setrlimit(RLIMIT_CPU)
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

**Test baselines.** Native-Linux (DGX, PG 18.4 live, rustc 1.96.0): **1327 / 0 / 4**
on `feat/hhagent-target-bring-up` (+16 over the `cdadea1` baseline of 1311).
macOS skip-as-pass posture (no `HHAGENT_PG_BIN_DIR`): **1319 / 0 / 3** on `main`
at `f695a46` (L3 over-cap-split baseline). 3–4 ignored = explicit doctest markers;
`[SKIP]` lines on `--nocapture` are GLiNER-Relex real-model tests gated on
`HHAGENT_GLINER_RELEX_ENABLE=1`. (Full per-session test-count history is in the
archive snapshots; the suite table below lists what each integration suite verifies.)

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 16 | bwrap argv builder shape (6) + cgroup `systemd-run` argv builder shape (10) |
| `sandbox` unit (macos) | 14 | sandbox-exec profile builder + path canonicalization + on-host probe + TinyScheme-injection rejection + strict-profile mach-lookup guard (issue #1) |
| `sandbox` integration (`linux_smoke`) | 7 | **real** bwrap+cgroup: jailed echo, fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, `/tmp` per-spawn ephemeral tmpfs (#89) |
| `sandbox` integration (`macos_smoke`) | 10 | **real** sandbox-exec: jailed echo, fs invisibility, fs_read readable, net deny, fresh session leader (#2), no appleevents bootstrap (#1) |
| `sandbox` integration (`macos_container_smoke`) | 7+ | **real** Apple `container`: argv shape, alpine smoke under `--init`, bind-mount-readonly, strict profile, probe skip |
| `core` unit | 60+ | lockdown-env, watchdog, workspace RAII, audit parsers, dispatch-result mapping, ToolRegistry, injection_guard catalogue, secrets Vault + SecretRef, L3 crystallise/approval/invoke/surface units (see archive for full breakdown) |
| `core` integration (`shell_exec_e2e`) | 4 | **cross-platform real** core → sandbox → shell-exec round-trip; every call routes through `tool_host::dispatch` |
| `core` integration (`memory_recall_e2e`) | 1 | **real** Phase-1 entry: all three lanes + 1-hop entity expansion + fused RRF + empty-seed degrade |
| `core` integration (`cli_ask_e2e`) | 2 | **real** full prod chain (CLI → PG → scheduler → LLM → CASSANDRA → dispatch → finalize) against a queued mock LLM |
| `core` integration (`injection_guard_e2e`) | 6 | **PG-required**: placeholder shape, one policy row, privacy invariant, SHA shape, benign passthrough, error-path bypass |
| `core` integration (`secret_vault_e2e`) | 9 | **PG-required**: materialize/redeem rows, fail-closed redemption, opaque-ref-not-plaintext (#147), no plaintext in policy rows |
| `core` integration (`cli_memory_l3_run_daemon_e2e`) | 2 | **PG + real daemon**: `--execute` succeeds against the daemon registry with `env_clear()` + NO `HHAGENT_SHELL_EXEC_BIN` (the #179 regression pin) + no-daemon cancels & errors |
| `core` integration (`cli_memory_l3_e2e` / `_run_e2e`) | 10 / 5 | **PG-required**: L3 list/remove/approve/revoke/pin + operator `run` (dry-run / execute / refuse paths) |
| `db` unit | 71+ | initdb/auto_conf/bin-dir builders, ConnectSpec, graph pins, probe SQL pin, RUNTIME_ROLE pins, audit truncate, secrets AES-GCM, memory pins, kinds validation |
| `db` integration (`postgres_e2e`) | 8+ | probe idempotency, PgGraph, runtime-role REVOKE, audit NOTIFY, secrets, memory_entities cascade, deleted_memories journalling, walk-edges dedupe |
| `llm-router` unit + integration | 41 + 8 | error truncate, decode, config from_env, embedding wire shapes, compose_url, pick_backend; hand-rolled TCP mock chat+embed chokepoints |
| `prelude` unit + smoke | 21 | env/profile parse, BPF builds, syscall presence; landlock_smoke (4); seccomp_smoke (6) |
| `supervisor` unit + integration | 44–52 + 2–4 | build_unit_file/build_plist, validate_service_name, driver round-trips, specs; systemctl/launchctl bootstrap (macOS serialised via reentrant Mutex) |
| `core` integration (scheduler_*_e2e) | 8+ | inner_loop, lanes, crash_recovery, agent_prompts — cross-platform skip-as-pass without PG |

**Build & test:**
```sh
source "$HOME/.cargo/env"
cargo build --workspace          # produces ./target/debug/hhagent + workers (macOS; see #144 for Linux)
cargo test --workspace           # all green on macOS (skip-as-pass) / DGX (live PG)
./target/debug/hhagent           # runs the core daemon, emits one JSON log line
```

**Required one-time host setup (Ubuntu 24.04+ only):** the AppArmor profile that lets `bwrap` create unprivileged user namespaces is already installed on the user's DGX Spark. Other Linux hosts may need `sudo scripts/linux/install-bwrap-apparmor-profile.sh`. macOS uses `sandbox-exec` (no setup needed).

---

## Recently completed (2026-06-07 — `insert_memory_light`, ROADMAP:130, branch `feat/memory-light-write-path`, PR [#195](https://github.com/hherb/hhagent/pull/195) OPEN, on macOS)

**What & why.** The "light" half of openhuman's two-tier memory write path
(`put_doc` vs `put_doc_light`). Today every `memories` row is written with a
caller-computed embedding; for *future* high-frequency ephemeral writers
(channel inbound, browser observations, screen capture) that would never be
useful semantic-search targets, embedding every row wastes the expensive embed
call. This adds the deliberately-named embedding-skipping writer.
Design + plan: [`docs/devel/specs/2026-06-07-memory-light-write-path-design.md`](../specs/2026-06-07-memory-light-write-path-design.md),
[`docs/devel/plans/2026-06-07-memory-light-write-path.md`](../plans/2026-06-07-memory-light-write-path.md).

**What shipped (3 code commits `39a036a`..`6e7eb13`, all in `hhagent-db`):**
- **`db::memories::insert_memory_light(executor, body, metadata, layer)`**
  (`write.rs`) — a thin named delegate to `insert_memory_at_layer` with
  `embedding = None`. No `embedding` parameter (skipping it is the point); no new
  SQL, no migration. Inherits the L0 (`MemoryLayer::Meta`) `PolicyViolation` guard
  for free, preserving the "grep `seed_meta_memory` = every L0 write" invariant.
  Re-exported from the parent `memories` module.
- **Documented degradation contract** on the function: lexical lane + `metadata @>`
  containment work normally; semantic lane silently skips the row (`semantic_search`
  filters `WHERE embedding IS NOT NULL`); graph lane never surfaces it (no
  `memory_entities` links). Graceful degradation, not breakage.
- **Two PG-required tests** (`db/tests/postgres_e2e.rs`):
  `insert_memory_light_round_trip_and_rejects_l0` (NULL embedding + correct layer;
  L0 rejected; no L0 leak) and `insert_memory_light_degrades_gracefully_across_lanes`
  (light row absent from `semantic_search`, present via lexical + `metadata @>`, with
  an embedded control row proving the semantic lane is live). Verified **passing
  against live PG**.
- **One PG-free unit test** (`write.rs`, review-fix): `insert_memory_light_rejects_l0_without_pg`
  pins the L0 `PolicyViolation` guard with a lazy pool that never connects — the guard
  short-circuits before any SQL, so the policy now has coverage on every dev machine.

**Reviews:** spec-compliance ✅ (exact signature, thin body, 3 files, no scope creep);
code-quality "approved with minor fixes" — applied the test-cluster-label rename
(`mlight-*`/`mdegrad-*`, commit `6e7eb13`) for readability; kept the degradation-contract
rustdoc (deliberate, spec-mandated API doc). **`/review` follow-up:** added the PG-free L0
unit test above (review flagged the guard had no coverage under macOS skip-as-pass); graph-lane
test gap lodged as [#196](https://github.com/hherb/hhagent/issues/196).

**Deferred (per spec):** core-side caller wiring; per-namespace caps + oldest-eviction;
a graph-lane degradation test.

---

## Recently completed (2026-06-07 — Option K: cross-platform exponential restart backoff, ROADMAP:61, branch `feat/restart-backoff`, on macOS)

**What & why.** Until now every keep-alive `ServiceSpec` restarted on a constant
5 s (`Restart=on-failure RestartSec=5` / `KeepAlive=true`); a crash-looping daemon
hammered the host forever. Option K adds an optional exponential ramp, wired
honestly across both OSes. Design + plan: [`docs/devel/specs/2026-06-07-restart-backoff-design.md`](../specs/2026-06-07-restart-backoff-design.md), [`docs/devel/plans/2026-06-07-restart-backoff.md`](../plans/2026-06-07-restart-backoff.md).

**What shipped (6 commits, `03b54b5`..`ee9099f`, all in `hhagent-supervisor`):**
- **`RestartBackoff { max_delay_sec: u32, steps: u32 }`** + `ServiceSpec.restart_backoff:
  Option<RestartBackoff>` (`#[serde(default)]`). `None` reproduces today's output
  byte-for-byte — additive, exactly like the `after`/`part_of` precedent. (`lib.rs`)
- **systemd** (`systemd_user/builder.rs`): inside the `keep_alive` block only, when
  `Some`, emits `RestartSteps=<steps>` + `RestartMaxDelaySec=<max>`. Needs systemd
  252+; older systemd logs an "unknown directive" warning but still loads (safe degrade).
- **launchd** (`launchd_agents.rs`): launchd has no operator-controllable backoff
  (`ThrottleInterval` is a constant floor, not a ramp), so `install` emits one
  `tracing::warn!` and writes today's plist **unchanged** — pinned by a
  `build_plist_identical_with_and_without_backoff` regression guard. Same
  "degrade-with-a-visible-warning" posture as `after`/`part_of` on launchd.
- **Canonical specs** (`specs.rs`): `core_service_spec` + `postgres_service_spec` carry
  `RestartBackoff { max_delay_sec: 300, steps: 8 }` — a crash loop ramps 5 s → ~5 min.
- **Cap hygiene:** the two builder test modules were lifted to siblings
  (`systemd_user/builder/tests.rs`, `launchd_agents/builders/tests.rs`); parents
  524→259 and 508→234, production regions byte-identical (modulo one doc-align nit).

**Residual (deferred, documented ≤27-over policy):** `launchd_agents.rs` is 508 LOC
(+8); tests are already external so a fix needs a real prod-split — disproportionate now.

**Verification (macOS, `ee9099f`):** `cargo test --workspace` all `ok` / 0 failed
(skip-as-pass; supervisor 65 unit + the new tests); `cargo clippy --workspace
--all-targets --locked -- -D warnings` exit 0. Linux-gated systemd code + its lifted
tests **compile + clippy-clean** under `--target aarch64-unknown-linux-gnu` (pure-Rust
crate cross-`check`s on the Mac); the systemd tests only *run* on Linux (DGX/CI).
Final holistic review: **ready to merge**.

**Post-review polish (PR #194, same session):** addressed three minor `/review`
findings — all non-behavioural. (1) `RestartBackoff` doc now states the value
constraints (`steps ≥ 1` or systemd disables the ramp; `max_delay_sec` should
exceed the 5s `RestartSec` floor) — left unenforced since specs are
code-constructed, flagged for any future external/JSON source. (2) the design
doc's launchd warn example reconciled to the actual structured-field form.
(3) import-style nit in `launchd_agents/builders/tests.rs` (`use crate::RestartBackoff;`
to match the systemd sibling). Supervisor 65 unit + smoke green; clippy native +
`aarch64-unknown-linux-gnu` cross-target both exit 0.

---

## Recently completed (2026-06-07 — three clean test-lifts batch, item 9b-a, branch `refactor/clean-test-lifts-batch`, PR [#193](https://github.com/hherb/hhagent/pull/193) MERGED, on macOS)

**What & why.** A fresh `wc -l` census found **three clean over-cap test-lifts
the prior handover's bucket-(a) had never tracked** (it had declared clean
test-lifts exhausted). Same precedent as `macos_seatbelt.rs`: lift the inline
`#[cfg(test)] mod tests` block alone and the parent lands under the 500-LOC cap.

**What shipped (3 parents edited, 3 new siblings; commit `92dcfa1`):** for each
file the inline test block moved verbatim into a new sibling `<stem>/tests.rs`
(de-indented one level, `//!` header); the parent declares `#[cfg(test)] mod
tests;`. A scripted lift with a **round-trip byte-identity assertion**
(re-indenting the lifted body must reproduce the original file exactly) ran
before any write, so production regions are guaranteed byte-identical to HEAD.
- `core/src/cassandra/types.rs` **897 → 336** + new `cassandra/types/tests.rs` (568)
- `core/src/scheduler/inner_loop_audit.rs` **655 → 304** + new `inner_loop_audit/tests.rs` (357)
- `core/src/entity_extraction/gliner_relex.rs` **570 → 386** + new `gliner_relex/tests.rs` (190)

**Residual flagged:** `cassandra/types/tests.rs` (568) is now an over-cap
**test** file (bucket-c, lower priority).

**Verification (macOS, sandbox-exec live):** `cargo test --workspace`
**1350 / 0 / 3** (unchanged baseline; real `macos_smoke`/`macos_container_smoke`
ran live); `cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
exit 0; `git diff` confirms each parent hunk removes only the test body and adds
`mod tests;` (production context lines unchanged).

---

---

## Earlier history (summary)

One bullet per session, newest first. Full reasoning lives in the archive snapshots:
the L3 arc + 2026-05-29 → 2026-06-04 sessions in
[`archive/handover_20260605_pre-prune.md`](archive/handover_20260605_pre-prune.md);
sessions 2026-05-10 → 2026-05-29 in
[`archive/handover_20260529_pre-prune.md`](archive/handover_20260529_pre-prune.md);
sessions 2026-05-06 → 2026-05-09 in
[`archive/handover_20260510_pre-prune.md`](archive/handover_20260510_pre-prune.md).

- **2026-06-07 — `macos_seatbelt.rs` test-lift (item 9b-a, PR [#192](https://github.com/hherb/hhagent/pull/192) MERGED):** inline `#[cfg(test)] mod tests` → sibling `macos_seatbelt/tests.rs`; parent 604 → 332 LOC, production byte-identical, 16 unit tests pass from the new location.
- **2026-06-06 — `systemd_user.rs` production split (item 9b-b, PR [#191](https://github.com/hherb/hhagent/pull/191) MERGED):** the most over-cap file (1069 LOC after the `hhagent.target` slice) → 427-LOC `systemctl --user` driver parent + `systemd_user/builder.rs` (478, pure builders+tests, re-exported via `pub use`) + `systemd_user/tests.rs` (216, driver tests); mirrors the `launchd_agents.rs` precedent. Behaviour-preserving (workspace 1327/0/4).
- **2026-06-06 — `gliner_relex.rs` production split (item 9b, PR [#189](https://github.com/hherb/hhagent/pull/189) MERGED):** 921-LOC monolith → 51-LOC re-export facade + five cohesive siblings (`wire`/`resolve`/`entry`/`client`/`manifest`, all under cap); public API byte-identical via `pub use`. Reconciled same session: `recall.rs` test-lift (PR [#188](https://github.com/hherb/hhagent/pull/188), 622→406). Residual: `workers/gliner_relex/tests.rs` 851 (bucket-c).
- **2026-06-05 — worker manifest plumbing (item 11, PR [#187](https://github.com/hherb/hhagent/pull/187) MERGED at `2e3d0c5`):** `trait WorkerManifest` + `Resolution` enum + `ResolveCtx` + pure `discover_binary` — each worker self-describes; `registry_build.rs` reduced to `assemble_registry(manifests, ctx)`. Plain workers resolve as a sibling of the `hhagent` binary (`current_exe()`-relative; `HHAGENT_*_BIN` override wins, fail-closed if set-but-invalid; gliner exempt). Every produced `ToolEntry` byte-identical; containment shape stays compiled-in. Workspace 1311/0/4.
- **2026-06-05 — #179 Opt-3 daemon reroute of `memory l3 run` (PR [#186](https://github.com/hherb/hhagent/pull/186) at `67bc474`, #179 CLOSED):** `run` now enqueues an `l3_run` task the daemon executes against its single live `ToolRegistry` (the Postgres `tasks` queue + `LISTEN/NOTIFY` IS the operator→daemon command channel — `ask`'s second user, zero new IPC). New `scheduler/l3_run.rs`; `drain_lane` routing; CLI rewrite waits on `tasks_completed` with busy-vs-absent daemon detection (`tasks::any_live_worker`, pending-only cancel). Deleted the interim `diagnose_registry_divergence` (PR #180). TOCTOU re-validation now strictly stronger (live registry); all 7 security invariants PASS. Workspace 1297/0/4.
- **2026-06-04 — `capture.rs` test-lift + `secret_vault_e2e` `sun_path` fix (PR [#185](https://github.com/hherb/hhagent/pull/185) at `ef01ae3`):** clean over-cap test-lift → `observation/capture/tests.rs`; parent 715 → 373 LOC, production L1–371 byte-identical. Bundled: dropped the redundant doubled `{suffix}` from `secret_vault_e2e` data/log labels (108-byte `sun_path` overflow under the harness `TMPDIR`; #104 systemic sweep stays open). First DGX native-Linux verification in a while; toolchain bumped 1.95→1.96 to match CI; workspace 1290/0/4.
- **2026-06-04 — `l0_seed.rs` test-lift (PR [#183](https://github.com/hherb/hhagent/pull/183) at `305b927`):** clean over-cap test-lift → `l0_seed/tests.rs`; parent 730 → 462 LOC, behaviour-preserving (production L1–459 byte-identical; 19 unit tests pass from new location).
- **2026-06-04 — L3 over-cap file splits, the #181 follow-up (PR [#182](https://github.com/hherb/hhagent/pull/182) at `f695a46`):** production-split `l3_invoke.rs` (569 → 38-line facade + `pure`/`operator`/`agent` siblings) and `memory_l3.rs` (692 → 52-line dispatcher + per-subcommand siblings + `shared.rs` approve/pin DRY); all L3 files under the 500-LOC cap, behaviour-preserving (workspace 1319/0/3 unchanged; live PG L3 suites green).
- **2026-06-03 — #179 interim diagnostic, Approach C (PR [#180](https://github.com/hherb/hhagent/pull/180) at `fdfd0a8`):** pure `diagnose_registry_divergence` classifier + actionable CLI `hint:` for the `Refused` arm (since DELETED by this session's Opt-3 reroute). #179 re-scoped to the Opt-3 structural fix.
- **2026-06-03 — L3 operator-triggered invocation, "the DOOR" (PR [#178](https://github.com/hherb/hhagent/pull/178) at `d862e6e`):** `hhagent-cli memory l3 run <id>` executes an approved skill — substitute `{{params}}` → live `ToolRegistry` re-validation → sandboxed dispatch → audit; dry-run by default. Filed #179 (the registry-parity question this session resolved).
- **2026-06-04 — L3 autonomous door, agent-path (PR [#181](https://github.com/hherb/hhagent/pull/181) at `6e10a81`):** `Plan.invoke_skill` directive the inner loop expands (pinned-only; reuses `prepare_invocation` live re-validation; CASSANDRA review on the agent path) + the `pin` command (real `Pinned`-vs-`UserApproved`). Completes the L3 arc bar #179's IPC reroute.
- **2026-06-01 — L3 recall surfacing, the `<skills>` block (PR [#177](https://github.com/hherb/hhagent/pull/177) at `4b978d8`):** new `core/src/memory/l3_surface.rs` surfaces only `UserApproved`/`Pinned` skills to the planner (L0 → L1 → skills → recalled → base); `skill_count` threaded + audited. Surfacing-only, no invocation. Carries SQL trust push-down `load_layer_by_trust` (`a53b4bc`).
- **2026-05-31 — L3 skill trust enum + approval gate (PR [#176](https://github.com/hherb/hhagent/pull/176) at `bbcc7b3`):** `SkillTrust{Untrusted|UserApproved|Pinned}` (fail-safe parse); pure `evaluate_approval` (re-validate + `secret://` scan + tool-existence vs the `registry.loaded` snapshot, fail-closed); `set_skill_trust` db helper; `memory l3 {approve,revoke}` + audit rows. Trust flips → `user_approved` ONLY on `Approve`. No execution.
- **2026-05-31 — `l3_crystallise.rs` test-lift (PR [#175](https://github.com/hherb/hhagent/pull/175) at `55b212e`):** inline `mod tests` → sibling; 676 → 467 LOC.
- **2026-05-31 — L3 skill crystallisation writer (PR [#173](https://github.com/hherb/hhagent/pull/173) at `6eb966e`):** first writer for `MemoryLayer::Skill` (L3) — agent emits `Plan.l3_skill` template → `drain_lane` validates → canonical-SHA-256 dedup → stores `layer=3 trust:"untrusted"`; `dispatch_count >= 1` grounding gate; `memory l3 {list,remove}`. Writer-only, non-executable. New `core/src/memory/l3_crystallise.rs`.
- **2026-05-31 — `inner_loop.rs` test-lift, closes [#81](https://github.com/hherb/hhagent/issues/81) (PR [#172](https://github.com/hherb/hhagent/pull/172) at `98a5be0`):** 655 → 438 LOC.
- **2026-05-30 — `replay.rs` test-lift (PR [#171](https://github.com/hherb/hhagent/pull/171) at `30aa52e`):** 804 → 422 LOC.
- **2026-05-30 — `tool_dispatch.rs` split (PR [#170](https://github.com/hherb/hhagent/pull/170) at `4e401cc`):** test-lift + re-exported `result_mapping.rs` seam; 828 → 442 LOC.
- **2026-05-30 — `db/memories.rs` split (PR [#169](https://github.com/hherb/hhagent/pull/169) at `e1be537`):** real prod split into re-exported `write.rs` + `search.rs`; 961 → 360 LOC.
- **2026-05-30 — `launchd_agents.rs` split (PR [#168](https://github.com/hherb/hhagent/pull/168) at `5bf010b`):** `builders.rs` + `tests.rs` siblings; 1093 → 485 LOC.
- **2026-05-30 — `scheduler/audit.rs` split (PR [#167](https://github.com/hherb/hhagent/pull/167) at `79fcc27`):** `extract_entities.rs` + `tests.rs` siblings; 1106 → 500 LOC.
- **2026-05-30 — #99 CLI `with_runtime` migration (PR [#166](https://github.com/hherb/hhagent/pull/166) at `75e9039`):** all six `hhagent-cli` dispatchers share one idiom; #99 CLOSED.
- **2026-05-30 — `macos_container.rs` test-lift (PR [#165](https://github.com/hherb/hhagent/pull/165) at `48c0396`):** 983 → 491 LOC.
- **2026-05-30 — #130 launchd bring-up serialization + #163 `sun_path` fix (PR [#164](https://github.com/hherb/hhagent/pull/164) at `091e53d`):** reentrant `serial_lock` around the macOS launchd window; bundled `injection_guard_e2e` label shorten + `check_socket_path_fits` guard. Both CLOSED.
- **2026-05-30 — #162 graph-lane seed-thread regression test (PR [#162](https://github.com/hherb/hhagent/pull/162) at `a83be4a`):** found item-12 wiring already shipped (Slice F, 2026-05-19); reconciled + pinned the seed thread; zero production change.
- **2026-05-30 — #153 clippy `-D warnings` gate (PR [#161](https://github.com/hherb/hhagent/pull/161) at `12b080c`):** cleared the whole workspace, flipped `linux-check` to `-D warnings`. CLOSED.
- **2026-05-29 — #5 `tool_host.rs` sibling-lift (PR [#160](https://github.com/hherb/hhagent/pull/160) at `fd7dd7a`):** watchdog + lockdown_env + seal tests → child modules; 911 → 519 LOC (trust-boundary residual).
- **2026-05-29 — #4b `injection_guard.rs` test-lift (PR [#159](https://github.com/hherb/hhagent/pull/159) at `1106145`):** 667 → 338 LOC.
- **2026-05-29 — #156 `walk()` sibling-continue (PR [#158](https://github.com/hherb/hhagent/pull/158) at `f3c380f`):** depth-skip now continues siblings. CLOSED.
- **2026-05-29 — #148/#149 secret-vault test gaps (PR [#157](https://github.com/hherb/hhagent/pull/157) at `53e68ed`):** `AuditSink` seam + `insert_fresh` extraction. Both CLOSED.
- **2026-05-29 — #143 `walk()` recursion-depth guard (PR [#155](https://github.com/hherb/hhagent/pull/155) at `6e82252`):** `MAX_WALK_DEPTH = 256`. CLOSED.
- **2026-05-29 — #144/#150 Linux build + clippy gate (PR [#152](https://github.com/hherb/hhagent/pull/152) at `560d845`):** `linux-check` CI green.
- **2026-05-29 — #147 redact secret plaintext in tool audit row (PR [#151](https://github.com/hherb/hhagent/pull/151) at `54e8885`).**
- **2026-05-29 — ★ Opaque secret references slice 1 (PR [#146](https://github.com/hherb/hhagent/pull/146) at `bc36e4c`):** `SecretRef` opaque newtype + `substitute_refs_in_params` walker + Vault. Closes openhuman Item 31.
- **2026-05-28 — ★ Worker-output prompt-injection guard slice 1 (PR [#141](https://github.com/hherb/hhagent/pull/141) at `62905ae`):** 22-entry substring catalogue + screen + `extract_scannable_text`. Closes openhuman Item 30.
- **2026-05-28 — `idle_timeout/release.rs` sibling-lift + #89 `/tmp` tmpfs pin** (PRs [#138](https://github.com/hherb/hhagent/pull/138)/[#139](https://github.com/hherb/hhagent/pull/139)/[#140](https://github.com/hherb/hhagent/pull/140)).
- **2026-05-27 — worker_lifecycle hardening (#84/#85/#86) + test-infra slices** (PRs #137/#135/#133/#132/#129; filed #130).
- **2026-05-26 — graph diamond-dedupe (#114/#115) + `HHAGENT_PG_BIN_DIR` override + entity-upsert Layer B** (PRs #128/#126/#125).
- **2026-05-25 — Slice 2.5 follow-ups (#120/#121/#122) + `gliner_relex.rs` test-lift + GLiNER-Relex container** (PRs #124/#123/#118).
- **2026-05-23 — Item 23(a) test-lifts + Item 22 CLI splits (#111/#112) + `relations show`** (PRs #117/#116/#113).
- **2026-05-22 — kinds CLIs + `MacosContainer` Slice 2** (PRs #110/#109/#108; NB: the unconditional `Container` ref here is what broke the Linux build, #144).
- **2026-05-21 — macOS container backend Slice 1 + Apple `container` spike + GLiNER macOS device tree** (PRs #106/#105/#103/#100/#98).
- **2026-05-20 — quarantine review CLI + `hhagent-cli` split (#66) + entity-upsert Layer A** (PRs #96/#94/#93).
- **2026-05-19 — entity extraction v2: `memory_entities` auto-linker + GLiNER-Relex + migration 0016** (PRs #92/#91).
- **2026-05-18 — worker lifecycle managers + GLiNER worker + `inner_loop.rs` split (#81) + L1 promotion writer** (PRs #88/#87/#82).
- **2026-05-17 — recall-lane wiring into the production scheduler** (PR #79).
- **2026-05-16 — prompt-assembler L0+L1 + L0 seed loader + classification-floor inference** (PRs #74/#77/#70).
- **2026-05-15 — first CASSANDRA rules + replay harness + L1 storage migrations 0013/0014** (PRs #68/#67/#65/#61).
- **2026-05-14 — observation capture + constitutional refusal state (#23) + per-tool argv allowlist + CPU/rlimit** (PRs #60/#59/#54).
- **2026-05-13 — task-lifecycle audit rows + `WorkerCommand` seal (#16) + graph lane in recall** (PR #41).
- **2026-05-12 — `tests-common` crate (#15) + crash-recovery sweep + Option O embedding router** (PR #38).
- **2026-05-11 — scheduler online: `cli_ask_e2e` full-chain pin + CASSANDRA Phases 2–5.**
- **2026-05-10 — chokepoint + recall skeleton (Options M/N) + secrets-at-rest + audit NOTIFY/mirror + non-superuser role.**
- **2026-05-09 — cgroup v2 caps: `systemd-run --scope` MemoryMax/CPUQuota/TasksMax + C2.2 schema + Graph trait.**
- **2026-05-08 — Linux/macOS supervisors + per-task `Workspace` RAII + watchdog `kill(-1)` fix.**
- **2026-05-06/07 — Phase 0 sandbox core: Landlock+seccomp prelude + macOS Seatbelt + AGPL workspace + bwrap backend + shell-exec + first e2e.** Full detail in the 20260510 archive.

---

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** AGPL project; all third-party deps must be AGPL-compatible (Apache-2.0, MIT, BSD, MPL, LGPL, (A)GPL all fine).
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS (Apple Silicon and Intel). No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python workers.** Rust for core (no eval/dynamic surface); Python only inside sandboxed tool workers. shell-exec is Rust because it's a thin execve wrapper — Python's first appearance will be `python-exec` in Phase 4 (or possibly `web-fetch` earlier).
- **Hybrid LLM with policy routing.** Local-first via OpenAI-compatible HTTP (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS). Frontier (Claude/OpenAI) only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisor.** `systemd --user` (Linux) / `launchd` LaunchAgents (macOS). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Critical workers are human-curated and shipped with the binary. Agent-authored code only runs inside `python-exec`'s strict sandbox; named/persisted skills get an optional human-approve gate (the L3 skill arc).
- **JSON-RPC 2.0 over stdio.** MCP-stdio compatible. Lets us swap in a richer MCP client later without changing the trust boundary.
- **Operator→daemon command channel = the Postgres `tasks` queue + `LISTEN/NOTIFY`** (not a new IPC socket). `ask` and `memory l3 run` both ride it; daemon-side execution against the single live `ToolRegistry` is the canonical pattern (#179 Opt-3).

---

## Next TODO (pick one)

Phase 0 is complete; Phase 1 is on `main` and pinned by `cli_ask_e2e`. **The L3 invocation arc is COMPLETE on `main`** (PR #186, #179 CLOSED). **Worker manifest plumbing (item 11) MERGED** (PR #187). **`hhagent.target` bring-up (ROADMAP:60) MERGED** (PR #190). **Option K — restart backoff (ROADMAP:61) MERGED** (PR #194). **Memory two-tier write path (ROADMAP:130 — `insert_memory_light`) shipped** this session (branch `feat/memory-light-write-path`, PR [#195](https://github.com/hherb/hhagent/pull/195)). The list below is an **operator-picks bucket** — sized roughly one session each, with file paths and the verification step.

**Natural follow-ups to this session (ROADMAP:130):** core-side caller wiring for `insert_memory_light` (lands when the first high-frequency writer does — Phase 2 channels / Phase 3 browser); per-namespace caps + oldest-eviction on `memories.metadata` (no schema change); a graph-lane degradation test (`link_memory_to_entities` + `graph_search` to exercise the now-documented-but-untested graph degradation — tracked as [#196](https://github.com/hherb/hhagent/issues/196)).

**Refactor bucket — over-cap file splits (item 9b).** Re-census the exact split (`wc -l`) before picking — the numbers below drift each session:

- **(a) Clean test-lifts** (lifting the inline `mod tests` block alone lands the parent under cap): **none meaningfully remaining.** The substantial ones are done — `cassandra/types.rs`, `inner_loop_audit.rs`, `entity_extraction/gliner_relex.rs` (2026-06-07 batch); `macos_seatbelt.rs` (PR #192); `recall.rs`/`l0_seed.rs`/`capture.rs`/`inner_loop.rs`/`replay.rs` (Earlier history). A fresh census shows only files sitting **1–27 LOC over cap** still carry a liftable block (`core/src/main.rs` 527, `db/src/lib.rs` 525, `core/src/bin/hhagent-cli/memory_l3/run.rs` 519, `core/src/tool_host.rs` 519, `core/src/cassandra/constitutional.rs` 502, `core/src/memory/l1_promote.rs` 501) — a lift would save little; defer unless one grows.
- **(b) Need a real prod split or a re-exported pure-helper seam** (a test-lift alone leaves the parent over cap): `core/src/cli_audit.rs` (958, the most over-cap production file), `db/graph.rs` (926, the design-gated Item 23b walk-impl split — deferred until a 2nd `WalkedEdge` consumer materialises), `db/secrets.rs` (848, a clean prod-split candidate), `core/src/scheduler/runner.rs` (773), `core/src/scheduler/audit.rs` (701, tests already lifted), `db/src/entities.rs` (653), `workers/prelude/src/seccomp_lock.rs` (650), `core/src/scheduler/inner_loop.rs` (566, tests already lifted). (`systemd_user.rs`/`gliner_relex.rs` done — see history.)
  Also `supervisor/src/launchd_agents.rs` (508, +8) — pushed over by Option K's install-time warn; tests already external, so a fix needs a real prod-split (disproportionate for 8 lines; deferred per this same policy).
- **(c) Over-cap *test* files** (lower priority — not production code, but rule 4 still applies): `core/src/workers/gliner_relex/tests.rs` (851), `core/src/cassandra/types/tests.rs` (568).

**Engineering pickups (need a spec/design first):**

- **[#142](https://github.com/hherb/hhagent/issues/142) — injection-guard chat-template false-positives.** Deferred per the issue author: chat-template tokens (`<|im_start|>`) will false-positive on technical docs once `web-fetch`/MCP workers land — pick a fix *with real data* once such a worker exists, not before.

**Test-infra / smaller picks:**

- **[#134](https://github.com/hherb/hhagent/issues/134)** — revise the `bring_up_pg_cluster` doc example or ship a real `_with_timeout` caller.
- **[#104](https://github.com/hherb/hhagent/issues/104)** — systemic de-doubling of the `pid+nanos` tempdir suffix across all e2e callers (the `secret_vault_e2e` instance was fixed last session; this tracks the broader sweep).
- **`HHAGENT_GLINER_RELEX_REQUIRE_E2E=1` CI knob** — turn the container e2e's skip-as-pass into a hard fail for any runner with PG + container + image + weights staged.

**Operator actions (no code):** recapture observation fixtures against the current daemon (`cargo test -p hhagent-core --test observation_capture -- --ignored --nocapture`); real-model relation-extraction validation (`HHAGENT_GLINER_RELEX_ENABLE=1 cargo test … entity_extraction_e2e`).

---

## Design notes for parked work

### Option P — entity↔memory linkage + graph lane (Phase 1 cont.)

The `memory_entities` join table (P1) shipped; the graph lane is wired into `recall` and the **production caller wiring is DONE** (2026-05-19 Slice F, PR #91): `RouterAgent::formulate_plan` populates `seed_entity_ids` from `entity_extractor.extract(&ctx.instruction)` each iteration; `main.rs` wires the real `GlinerRelexExtractor`. For a query carrying `seed_entity_ids`, the lane traverses outbound 1-hop then `SELECT memory_id FROM memory_entities WHERE entity_id = ANY($1)` ranked by neighbour count. **Remaining parked work is the quarantine review gate, not the wiring:** freshly-extracted entities default `quarantine=TRUE` and `graph_search` filters `quarantine=FALSE`, so seed entities surface no memories until an operator un-quarantines them ([#40](https://github.com/hherb/hhagent/issues/40) tracks the graph-default policy question). Secondary deferral: `entities.embedding` is NULL for all entities; a populated column would seed an entity-similarity lane (the `vector(1024)` column already exists).

---

## Open follow-up issues (filed but not picked)

Only currently-open issues are listed; closed-issue detail lives in the archive snapshots and git history.

- [#3](https://github.com/hherb/hhagent/issues/3) — drop `SYS_SENDFILE`/`SYS_FADVISE64` shim once libc exposes them on aarch64.
- [#4](https://github.com/hherb/hhagent/issues/4) — bump Last-commit + test-count fields whenever a Recently-completed entry is added (process hygiene).
- [#8](https://github.com/hherb/hhagent/issues/8) — collapse `default_probe`/`default_supervisor` cfg-ladder duplication once a third entry point or backend OS appears.
- [#13](https://github.com/hherb/hhagent/issues/13) — write a migration numbering / rename hygiene checklist (sqlx fingerprints version+slug; a rename on a shipped migration silently breaks startup).
- [#14](https://github.com/hherb/hhagent/issues/14) — replace the brittle `wait_for_log_match("database probe succeeded")` in `supervisor_e2e.rs` with a real readiness signal.
- [#20](https://github.com/hherb/hhagent/issues/20) — `agent_prompts` PK on sha256 means renamed prompt files lose their original name *(0011 changed the PK to `(sha256, name)`; tracks any residual)*.
- [#21](https://github.com/hherb/hhagent/issues/21) — scheduler per-iteration cancellation poll could be a `watch::Receiver` instead of a DB round-trip.
- [#24](https://github.com/hherb/hhagent/issues/24) — deployment: `HHAGENT_PROMPTS_DIR` has a cwd-relative fallback; production unit files must set it explicitly.
- [#37](https://github.com/hherb/hhagent/issues/37) — scheduler crash-recovery sweep+audit is unoptimised for high crash counts.
- [#39](https://github.com/hherb/hhagent/issues/39) — tests-common optional hardening (PgCluster.sup access, internal self-tests).
- [#40](https://github.com/hherb/hhagent/issues/40) — design: should `RecallParams::new()` default to graph-off until an entity-extraction step lands? *(partially addressed by `with_seeds`.)*
- [#42](https://github.com/hherb/hhagent/issues/42) — `deleted_memories` AFTER DELETE trigger uses `SECURITY INVOKER`; deferred until a second DELETE-capable role is proposed.
- [#47](https://github.com/hherb/hhagent/issues/47) — observation/capture: distinguish 'no verdict row' from a real Approve verdict *(SCHEMA_VERSION 2 made `verdict_today` Optional; tracks residual.)*
- [#50](https://github.com/hherb/hhagent/issues/50) — unify finalize-payload provenance signal across crashed/producer-cancelled/runtime emitters.
- [#55](https://github.com/hherb/hhagent/issues/55) — macOS Apple `container` micro-VM backend *(spike + Slices 1/2/2.5 shipped; tracks the broader rollout.)*
- [#62](https://github.com/hherb/hhagent/issues/62) — audit-payload truncation can silently nuke `agent/plan.formulate` fields.
- [#63](https://github.com/hherb/hhagent/issues/63) — e2e gap: classification_floor plumbing from `tasks.payload` to the `agent/plan.formulate` audit row.
- [#73](https://github.com/hherb/hhagent/issues/73) — scheduler/runner e2e integration test + TaskContext-construction reminder for producer-side floor-source validation.
- [#76](https://github.com/hherb/hhagent/issues/76) — prompt-assembly: verify PromptAssembly error retry semantics in scheduler.
- [#78](https://github.com/hherb/hhagent/issues/78) — prompt-assembly: global token cap with priority drop for the assembled system prompt.
- [#104](https://github.com/hherb/hhagent/issues/104) — audit the pid+nanos tempdir pattern across the workspace (follow-up to #101; `secret_vault_e2e` instance fixed 2026-06-04).
- [#107](https://github.com/hherb/hhagent/issues/107) — `MacosContainer` PID-1 signal-handling posture *(closed in code by always-on `--init`; verify end-to-end before long-lived workers migrate).*
- [#127](https://github.com/hherb/hhagent/issues/127) — env-var save/restore RAII helper for the `pg_bin_dir_candidates_with_env_override` tests.
- [#134](https://github.com/hherb/hhagent/issues/134) — tests-common: revise `bring_up_pg_cluster` doc example or ship a real `_with_timeout` caller.
- [#142](https://github.com/hherb/hhagent/issues/142) — injection_guard: chat-template tokens will false-positive on legitimate technical docs (see Next-TODO).

---

## Open questions parked for later

(From the design plan, restated here so they're surfaced when relevant.)

1. Embedding model on-device — bge-m3 vs nomic-embed-text vs ColBERT (Phase 1)
2. ~~Channel approval — passcode pairing vs static contact allowlist (Phase 2)~~ **Resolved 2026-05-06:** pairing flow with WebAuthn-or-OTP fallback, modeled on ZeroClaw's `security/{pairing,webauthn,otp}.rs`.
3. ~~Egress proxy as separate worker vs in-process in `tool_host`~~ **Resolved 2026-05-06:** separate worker, with the credential-leak scanner co-located.
4. Skill review workflow for *named* agent-authored Python (Phase 4) — see Phase 4 line items: trust enum + per-level capability ceiling. *(The L3 skill arc — crystallise → approve → pin → invoke — is the first concrete implementation of this for templated tool-call skills.)*
5. Worker keep-alive vs spawn-per-call (idle-timeout lifecycle shipped for GLiNER-Relex; revisit for other workers when latency matters).
6. ~~Worker binary discovery in production~~ **Advanced 2026-06-05 (item 11):** plain compiled workers default to a sibling of the `hhagent` binary (`current_exe()`-relative; `HHAGENT_*_BIN` override wins; gliner exempt — keeps venv/weights env resolution). Residual: FHS `libexec` layout if/when packaging wants it.

## Inspirations / things to read before each milestone

Two adjacent OpenClaw-derived projects ship code we can read (Apache-2.0/MIT, AGPL-compatible) before each new milestone — convergent prior art saves design time:

- **ZeroClaw** ([`zeroclaw-labs/zeroclaw`](https://github.com/zeroclaw-labs/zeroclaw), 100% Rust): read [`crates/zeroclaw-runtime/src/security/`](https://github.com/zeroclaw-labs/zeroclaw/tree/main/crates/zeroclaw-runtime/src/security) — has working `bubblewrap.rs`, `landlock.rs`, `seatbelt.rs`, `firejail.rs`, `pairing.rs`, `webauthn.rs`, `leak_detector.rs`, `workspace_boundary.rs`. Architectural drawback vs us: tools run as in-process Rust traits, OS sandbox wraps the runtime — weaker boundary than our process-per-worker. Don't copy the in-process tool model.
- **IronClaw** ([`nearai/ironclaw`](https://github.com/nearai/ironclaw)): read its dispatcher chokepoint pattern (`ToolDispatcher::dispatch()` is the single audit/safety-validation funnel for *every* action, regardless of caller). Drawbacks: WASM-as-boundary is software-only containment; Postgres+libSQL dual backend is overkill at our stage.

The *defining* architectural difference: hhagent enforces **one OS process + one bwrap/Seatbelt jail per worker**. Both reference projects retreated from that. Don't.

---

## How to update this document at session end

**Header first, prose last.** The header is what the next session reads first
and treats as authoritative; stale header fields silently mislead future
sessions even when the prose is correct. Follow the steps in this order:

1. **Bump header fields at the top — before writing any prose:**
   - `Last updated:` → today's date.
   - **Current state / Last commit** → the hash of the most recent shipped commit. Confirm with `git log --oneline -1`.
   - `Session-end verification:` → re-run `cargo test --workspace` and copy the **passed / failed / ignored / `[SKIP]`** counts into this line.
   - **Every test-count number embedded elsewhere in the doc that changed this session** — a fresh agent grep-finds them and will trust whatever is there.
2. **Move "Next TODO" → "Recently completed (this session)"** if the picked option shipped, with enough detail (file paths, why-not-X, gotchas, test-count delta) that the next session can start cold.
3. **Write a fresh "Next TODO (pick one)"** with options sized for one session each — include file paths, gotchas, and the verification step.
4. **Refresh "Working state"** — anything new under stubs, anything that became real.
5. **Tick the matching items off in [`../ROADMAP.md`](../ROADMAP.md)** with the commit hash.
6. **Commit both files together** with a `docs(handover): ...` message.

### Pruning convention

The handover should stay focused on **what the next session needs to act on**: the current state, the last 2–3 sessions in detail, and the next TODO. Older session entries get compressed into the "Earlier history" summary or dropped entirely once they're no longer load-bearing.

When HANDOVER.md grows past the point where the next session can absorb it cold (rough rule of thumb: more than a couple of screens of "Recently completed"), prune it:

1. **Snapshot first.** Copy the current HANDOVER.md to `archive/handover_<YYYYMMDD>[_<slug>].md` (e.g. `handover_20260605_pre-prune.md`). The archive is the audit trail — never edited after the fact, never deleted.
2. **Keep verbatim:** the header, "Read these first," "Working state" (current truth), the most recent 1–2 sessions of "Recently completed," "Key design decisions," "Next TODO," "Open follow-up issues," "Open questions," "Inspirations," and this section.
3. **Compress everything else** into a single "Earlier history" section: one bullet per session, naming the slice + the headline change + a pointer to the archive snapshot for full reasoning.
4. **Cross-link** from the compressed bullets to the archive snapshot so anyone who needs the full reasoning can find it.
5. **Commit the prune separately** with `docs(handover): prune older sessions, archive pre-prune snapshot` so the diff is reviewable.

The archive directory is the historical record; HANDOVER.md is the working brief.
