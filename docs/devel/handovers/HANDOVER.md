# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention. Older sessions are compressed
> into "Earlier history" below; full per-session detail lives in the
> [`archive/`](archive/) snapshots.

**Last updated:** 2026-05-29 (Issue [#143](https://github.com/hherb/hhagent/issues/143) — `injection_guard::walk()` recursion-depth guard. Branch `fix/issue-143-walk-recursion-depth-guard`, PR [#155](https://github.com/hherb/hhagent/pull/155), **not yet merged**. Also reconciled the stale header after PR [#152](https://github.com/hherb/hhagent/pull/152)'s merge.)

**Last commit on `main`:** `560d845` (Merge pull request #152 from hherb/fix/issue-144-150-linux-build-clippy-gate). Confirm with `git log --oneline -1`. The #143 fix below lives on branch `fix/issue-143-walk-recursion-depth-guard` pending review/merge.

**Currently on:** `fix/issue-143-walk-recursion-depth-guard` (off `main` at `560d845`). Two commits: docs-reconcile (`97add3a`) + the #143 fix (`d72324c`). Not pushed/merged yet. `main` itself is clean at `560d845` (one untracked, unrelated `docs/essay-medium-draft.md`).

**Session-end verification:** `cargo test --workspace` on macOS (M3 Max, on the fix branch): **1148 passed / 0 failed / 3 ignored** (+3 over `main`'s 1145 — the three new #143 depth-guard tests; skip-as-pass posture without `HHAGENT_PG_BIN_DIR`). `cargo clippy -p hhagent-core --all-targets`: the 10 pre-existing lib warnings unchanged, no new lints from the change. **PR #152 context (now merged):** `cargo test --workspace` on `main` post-merge: 1145/0/3 (unchanged from `main` — skip-as-pass posture without `HHAGENT_PG_BIN_DIR`; with PG live ~640 passed + 2 pre-existing `embedding_recall_e2e`/`gliner_relex_e2e` PG-race flakes identical on `main`). `cargo clippy --workspace --all-targets`: **0 errors** (pre-existing warnings remain; #150's deny-level `uninit_vec` is fixed). **Linux:** **CONFIRMED GREEN via CI** — the new `.github/workflows/linux-check.yml` job on PR [#152](https://github.com/hherb/hhagent/pull/152) ran `cargo check --workspace --all-targets` + `cargo clippy --workspace --all-targets` on `ubuntu-latest` and passed (1m17s). This is the real Linux verification for #144 and confirms there was no *other* Linux-only breakage beyond the `Container` variant. (Full `cargo test --workspace` on the DGX still pending — CI is compile-only; the last DGX test run was `990` at `1abb061`.) **Build gaps now fixed:** [#144](https://github.com/hherb/hhagent/issues/144) (Linux build — `container_mode_entry` macOS-gated) and [#150](https://github.com/hherb/hhagent/issues/150) (`clippy::uninit_vec` in `mem_burner`). Note: #144 was **NOT verifiable on the dev Mac** (cross `cargo check` dies in a C dep needing `aarch64-linux-gnu-gcc`) — hence the CI job.

**Recently merged branches — safe to delete locally** (`git branch -d <name>`): `fix/issue-144-150-linux-build-clippy-gate` (PR #152, just merged), `fix/issue-147-redact-tool-req-plaintext` (PR #151), `feat/opaque-secret-refs-slice-1` (PR #146), `feat/injection-guard-slice-1` (PR #141), `fix/issue-89-tmpfs-per-spawn-invariant-test` (PR #139), `refactor/idle-timeout-release-sibling-lift` (PR #138). Older merged branches are listed in the archive snapshots.

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — high-level diagram, process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — invariant, scenarios in scope, defence-in-depth layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO list with commit hashes for shipped items
4. The design plan (outside the repo) — `~/.claude/plans/i-d-like-to-design-logical-starlight.md`
5. Memory notes (auto-loaded) — see `~/.claude/projects/-home-hherb-src-hhagent/memory/MEMORY.md`
6. Older handovers — `archive/handover_<timestamp>.md` (one snapshot per pruning event; full historical detail lives there). Most recent: [`archive/handover_20260529_pre-prune.md`](archive/handover_20260529_pre-prune.md).

## Working state (what's green right now)

```
hhagent (Rust workspace, 9 crates, AGPL-3.0)
├── core               hhagent-core: lib + 2 bins (`hhagent` daemon + `hhagent-cli` audit-tail viewer). Daemon blocks on SIGTERM/SIGINT via tokio::signal::unix; main.rs runs db::probe::run → connect_runtime_pool → spawn_mirror before wait_for_shutdown (fail-closed startup; mirror failures are logged but non-fatal). lib modules: tool_host (spawn_worker, dispatch chokepoint, lockdown-env derivation, wall-clock watchdog, sealed WorkerCommand, secret-ref substitution on input + injection-guard screen on output), secrets (Vault TTL'd RwLock<HashMap> + SecretRef opaque newtype + substitute_refs_in_params walker), cassandra/injection_guard (22-entry substring catalogue + screen + extract_scannable_text), workspace (per-task scratch with RAII cleanup), audit_mirror (PgListener-driven JSONL writer with daily rotation + fsync per write), audit_tail (`tail -f`-style follower used by `hhagent-cli audit tail`), scheduler/ (audit.rs pure helpers + canonical SCHEDULER_AUDIT_ACTOR; runner.rs spec §7 lifecycle rows; tool_dispatch.rs short-circuit rows; crash_recovery.rs sweep_and_audit), memory/ (mod.rs facade + recall.rs three-lane RRF-fused recall + embed.rs embed_query), worker_lifecycle/ (Lifecycle enum + SingleUse/IdleTimeout/Composite managers; idle_timeout.rs acquire path + idle_timeout/release.rs release path), entity_extraction/ (batch_upsert.rs two-phase unnest + per-row attribution), workers/ (gliner_relex.rs host+container entries)
├── db                 hhagent-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir, pg_bin_dir_candidates_with_env_override) + conn::ConnectSpec + RUNTIME_ROLE/set_role_runtime_statement + probe::run (ensure DB → migrate as superuser → SET ROLE → audit, fail-closed) + graph::{Graph trait, PgGraph; recursive-CTE path() + walk_outbound/inbound_edges + walk_edges_around with DISTINCT ON diamond-dedupe} + audit::{insert, fetch_by_id, fetch_since, truncate_payload} + memories::{insert, semantic/lexical/graph search, link_memory_to_entities} + entity_kinds + relation_kinds lookup caches + pool::{connect_runtime_pool, connect_admin_pool} + MIGRATOR (0001..0017) + memory_entities join table + deleted_memories audit table + secrets (AES-256-GCM at rest + OS keyring) + hhagent-db-init bin
├── llm-router         hhagent-llm-router: sole egress for LLM calls. Router::send + Router::embed over reqwest+rustls; Backend::{Local, Frontier} closed enum; PolicyGate trait (DefaultLocalPolicy always Local — Phase-5 seam). RouterConfig::from_env reads HHAGENT_LLM_* env. Per-OS default URL: vLLM/SGLang on Linux (:8000), Ollama on macOS (:11434). Frontier dispatch returns PolicyDeniedFrontier until Phase 5
├── sandbox            hhagent-sandbox: SandboxPolicy + SandboxBackend trait + SandboxBackendKind (cfg-gated per-OS) + SandboxBackends resolver + LinuxBwrap (wrapped in systemd-run --scope cgroup) + MacosSeatbelt + MacosContainer (Apple `container` micro-VM, macOS-only, opt-in per-worker)
├── supervisor         hhagent-supervisor: SystemdUser (Linux) + LaunchAgents (macOS) + specs::{core_service_spec, postgres_service_spec} + default_probe
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── tests-common       hhagent-tests-common: shared dev-dep crate (publish = false) — PgCluster + bring_up_pg_cluster(+_with_timeout), RAII guards, skip helpers, sandbox factory, binary discovery, macOS launchd serial lock, deterministic SHA-256-seeded embedding seed. Consumed only from [dev-dependencies]; never linked into a runtime binary.
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS) + cross-platform setrlimit(RLIMIT_CPU)
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

**`cargo test --workspace` on macOS (M3 Max): 1148 / 0 / 3** on branch `fix/issue-143-walk-recursion-depth-guard` (Issue #143 — `injection_guard::walk()` depth guard; +3 over `main`'s 1145 at `560d845`). Count measured without `HHAGENT_PG_BIN_DIR` (skip-as-pass posture); with PG live ~640 passed / 2 pre-existing flakes (`embedding_recall_e2e` + `gliner_relex_e2e` PG initdb/pg_notify race, identical on `main`). `secret_vault_e2e` verified 9/9 live against Postgres.app v18. Prior checkpoint: **1137** on `feat/opaque-secret-refs-slice-1` at `19eebd6` (Item 31; +41 over 1096 on `main` at `62905ae`/PR #141). **Linux DGX baseline (most recent known): 990 on `main` at `1abb061`** (Item 22 PR #116) — but see the Linux build gap ([#144](https://github.com/hherb/hhagent/issues/144)) in the header before trusting any Linux number. 3 ignored = explicit doctest markers in `hhagent-core`/`hhagent-sandbox`/`hhagent-worker-prelude`. 4 `[SKIP]` lines on `--nocapture` are GLiNER-Relex real-model tests gated on `HHAGENT_GLINER_RELEX_ENABLE=1`. (Full per-session test-count history is in the archive snapshots.)

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 16 | bwrap argv builder shape (6) + cgroup `systemd-run` argv builder shape: starts with `systemd-run`, uses `--user --scope --quiet --collect`, sets `MemoryMax`+`MemorySwapMax=0`, omits both when `mem_mb=0`, defense-in-depth `CPUQuota=200%` + `TasksMax=64` defaults, ends with `--`, no inner-program leakage, 4 `-p` flags total (10) |
| `sandbox` unit (macos) | 14 | sandbox-exec profile builder shape + path canonicalization + on-host probe + TinyScheme-injection rejection + canonicalize error propagation + strict profile does NOT contain unrestricted `(allow mach-lookup)` (issue #1) |
| `sandbox` integration (`linux_smoke`) | 7 | **real** bwrap+cgroup: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected, mem_burner under `MemoryMax=32M` is OOM-killed, `/tmp` is per-spawn ephemeral tmpfs (#89) |
| `sandbox` integration (`macos_smoke`) | 10 | **real** sandbox-exec: scaffold marker, echo runs jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read paths readable, /dev/disk0 denied, relative-path policy rejected, network unreachable under `Net::Deny`, worker is leader of a fresh session (issue #2), worker cannot `bootstrap_look_up` appleevents (issue #1) |
| `sandbox` integration (`macos_container_smoke`) | 7+ | **real** Apple `container`: argv builder shape, alpine smoke under `--init`, bind-mount-readonly EROFS vs EACCES, strict profile, probe + image-presence skip |
| `core` unit | 60+ | lockdown-env, watchdog loop, workspace RAII, audit_mirror/audit_tail parsers, Option M/N pins, dispatch-result mapping, ToolRegistry, scheduler short-circuit payload pins, Option O embed payload, graph-lane RecallModes pins, injection_guard catalogue + normalize + extract_scannable_text, secrets Vault + SecretRef + substitute_refs_in_params walker (see archive for the full per-area breakdown) |
| `core` integration (`shell_exec_e2e`) | 4 | **cross-platform real** core → bwrap+landlock+seccomp (Linux) / sandbox-exec (macOS) → shell-exec round-trip; every call routes through `tool_host::dispatch` (WorkerCommand seal). Echo; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND; workspace e2e |
| `core` integration (`memory_recall_e2e`) | 1 | **cross-platform real** Phase-1 entry: all three lanes (semantic + lexical + graph) + 1-hop entity expansion + fused RRF + empty-seed degrade |
| `core` integration (`audit_dispatch_e2e`) | 1 | **cross-platform real** dispatcher chokepoint: success + POLICY_DENIED rows; exactly 3 rows total |
| `core` integration (`supervisor_e2e`) | 1 | **cross-platform real** end-to-end smoke: install → start → wait Active → probe-succeeded log → audit_log startup row → JSONL mirror drained → stop → uninstall |
| `core` integration (`scheduler_step_dispatch_e2e`) | 1 | **cross-platform real**: dispatch_step four ways (ok / POLICY_DENIED / UNKNOWN_TOOL+audit row / SPAWN_FAILED+audit row); exactly 5 rows |
| `core` integration (`cli_ask_e2e`) | 2 | **cross-platform real**: full prod chain (CLI → PG → scheduler → LLM → CASSANDRA → dispatch → finalize → CLI exit) against a queued mock LLM; happy path + plan-cap failure |
| `core` integration (`injection_guard_e2e`) | 6 | **PG-required**: placeholder shape, exactly-one policy row, privacy invariant (no scanned body in policy-row payload), SHA shape, benign passthrough, error-path bypass |
| `core` integration (`secret_vault_e2e`) | 9 | **PG-required**: materialize/redeem audit rows, redemption_failed fail-closed, tool-row `req` shows opaque ref not plaintext (#147), policy rows contain no redeemed plaintext |
| `core` integration (`embedding_recall_e2e`) | 4 | **Option O**: embed_query round-trip, audit-row shape, dim-mismatch reject, embed→recall semantic lane |
| `db` unit | 71+ | initdb/auto_conf/bin-dir builders, ConnectSpec, graph field pins, probe SQL pin, RUNTIME_ROLE pins, audit truncate envelope, secrets AES-GCM round-trip + AAD pins, Option N memory pins, entity/relation-kinds validation + description-length cap |
| `db` integration (`postgres_e2e`) | 8+ | probe idempotency + PgGraph happy path; runtime-role REVOKE enforced; audit pool + NOTIFY round-trip; secrets round-trip; memory_entities link + cascade; deleted_memories trigger journalling; kinds runtime-pool list path; walk-edges dedupe |
| `llm-router` unit | 41 | error truncate, ChatRole/ChatResponse decode, Backend serde, config from_env, Option O embedding wire shapes + count-mismatch + pick_embed seam, compose_url, send/pick_backend incl. PolicyDeniedFrontier |
| `llm-router` integration | 8 | hand-rolled TCP mock (no wiremock dep): chat happy/err/decode/chokepoint (4) + embedding happy/err/count-mismatch/chokepoint (4) |
| `prelude` unit + smoke | 21 | env/profile parse, BPF builds, syscall presence; landlock_smoke (4); seccomp_smoke (6) |
| `supervisor` unit | 44 (linux) / 52 (macos) | build_unit_file / build_plist shape, validate_service_name, driver round-trips, specs::* |
| `supervisor` integration | 2 (linux) / 4 (macos) | systemctl --user / launchctl bootstrap round-trips with RAII guards; invalid-name rejection; macOS serialised via static Mutex |
| `core` integration (scheduler_*_e2e) | 8 | inner_loop (4), lanes (1), crash_recovery (2), agent_prompts (1) — cross-platform skip-as-pass without PG |

**Build & test:**
```sh
source "$HOME/.cargo/env"
cargo build --workspace          # produces ./target/debug/hhagent + workers (macOS; see #144 for Linux)
cargo test --workspace           # all green on macOS
./target/debug/hhagent           # runs the core daemon, emits one JSON log line
```

**Required one-time host setup (Ubuntu 24.04+ only):** the AppArmor profile that lets `bwrap` create unprivileged user namespaces is already installed on the user's DGX Spark. Other Linux hosts may need `sudo scripts/linux/install-bwrap-apparmor-profile.sh`. macOS uses `sandbox-exec` (no setup needed).

---

## Recently completed (this session, 2026-05-29 — Issue [#143](https://github.com/hherb/hhagent/issues/143): `injection_guard::walk()` recursion-depth guard, branch `fix/issue-143-walk-recursion-depth-guard`, PR [#155](https://github.com/hherb/hhagent/pull/155), **NOT yet merged**)

Picked from the Next-TODO "injection-guard limits" bucket (item 4). The operator chose to **defer [#142](https://github.com/hherb/hhagent/issues/142)** (chat-template false-positives) per its author's own recommendation — that decision needs real data from a `web-fetch`/`mcp` worker that doesn't exist yet — and ship **#143 only** this session.

**The problem.** [`core/src/cassandra/injection_guard.rs`](../../../core/src/cassandra/injection_guard.rs)'s `walk()` (the recursive body of `extract_scannable_text`) descended once per array/object nesting level with no cap. A pathologically deep worker JSON-RPC result (`{"a":{"a":…}}` 100k deep) would overflow the dispatcher thread's stack before hitting `SCAN_BYTE_CAP`. **Unreachable today** — serde_json's parser rejects nesting past 128 and the protocol frame-size limit bounds wire depth — so this is the defense-in-depth backstop if either upstream limit is ever removed (exactly as the issue framed it).

**The fix (~5 production lines + const).** New `pub const MAX_WALK_DEPTH: usize = 256` (2× serde_json's 128 default, so any worker-parsed `Value` passes well under it; bounds the recursion far below the stack-overflow threshold). Added a `depth: usize` param to `walk` — `extract_scannable_text` seeds it at `0`, each descent into an array/object passes `depth + 1`, and at function entry `if depth >= MAX_WALK_DEPTH { return true; }` bails and signals truncation. **Reuses the existing `truncated` bool channel** rather than threading a new signal: hitting the cap requires adversarial input, for which an audit row marked truncated (and no crash) is the right forensic posture. `extract_scannable_text`'s public `(String, bool)` signature is unchanged.

**Why depth-cap over the issue's alternative "iterative walk with explicit stack".** Both were offered in #143. Depth-cap is the smaller, lower-risk change (it preserves the readable recursive shape and all existing truncation semantics); the iterative rewrite would have reopened the approved design for no extra safety at our scale.

**TDD (RED watched for both behavioural tests).** `walk_stops_at_max_depth` — leaf nested 300 deep is left unscanned + `truncated == true` (RED on unguarded code: walked to 300, `truncated == false`). `walk_captures_content_below_max_depth` — leaf 100 deep still fully scanned (guard doesn't touch normal nesting). `max_walk_depth_is_256` — const drift pin, matching the existing `block_threshold_*` / `scan_byte_cap_*` pins. **No literal 100k-deep overflow test:** any `Value` deep enough to overflow `walk` *also* overflows Rust's recursive `Drop` glue at test teardown (`serde_json::Value` has no iterative `Drop`), and such a `Value` can never reach `walk` in production — so the test would test drop glue on an unconstructable-in-prod input. Overflow-safety is instead a corollary of the ≤256-frame bound that `walk_stops_at_max_depth` pins; documented inline. The crash *was* observed once (SIGABRT) against unguarded code to confirm the vulnerability is real, then the test was removed.

**Verification (macOS M3 Max).** `cargo test --workspace`: **1148 / 0 / 3** (+3 over 1145). `injection_guard` module: 28/28. `cargo clippy -p hhagent-core --all-targets`: no new lints (10 pre-existing lib warnings unchanged). Closes #143 on merge.

**File-size watch.** `injection_guard.rs` 527 → 615 LOC (was already 88 over the 500 cap before this change; now 115 over). The bulk is the module doc + catalogue + inline tests. A **test-module sibling-lift** (`injection_guard/tests.rs`, the established repo pattern) would drop production LOC to ~285 — flagged as a fresh Next-TODO follow-up, kept out of this PR to keep the diff focused.

**Also in this session (separate commit `97add3a`):** reconciled the stale HANDOVER/ROADMAP after PR [#152](https://github.com/hherb/hhagent/pull/152) merged to `main` at `560d845` (header still said "not yet merged"); issues #144 + #150 are closed.

---

## Recently completed (this session, 2026-05-29 — Bugs [#150](https://github.com/hherb/hhagent/issues/150) + [#144](https://github.com/hherb/hhagent/issues/144): clippy gate + Linux build, branch `fix/issue-144-150-linux-build-clippy-gate`, **merged to `main` via PR [#152](https://github.com/hherb/hhagent/pull/152) at `560d845`**)

Both were the "real errors, not enhancements" entries at the top of the previous Next-TODO (rule 5 — fix breakage before features). Picked together because both are small and both unblock a broken gate.

**#150 — `cargo clippy --workspace --all-targets` hard-errored.** [`sandbox/tests/fixtures/mem_burner.rs:46`](../../../sandbox/tests/fixtures/mem_burner.rs) did `Vec::with_capacity(bytes)` then `unsafe { buf.set_len(bytes) }`; `clippy::uninit_vec` is now deny-by-default, so the whole `--all-targets` clippy run failed before reaching any real lint. Fix: `let mut buf: Vec<u8> = vec![0u8; bytes];` and drop the `unsafe`. **Correctness note worth keeping:** the OOM-kill smoke test relies on the page-touch loop *below* the allocation to force residency. `vec![0u8; n]` can hand back demand-zero pages (calloc-style) that don't count against `memory.max` until written — but the loop dirties one byte per page regardless, so residency is still forced and the test's behaviour is unchanged. Rewrote the SAFETY comment accordingly. **Verify:** `cargo clippy --workspace --all-targets` → 0 errors (warnings remain, non-fatal).

**#144 — `hhagent-core` did not compile on Linux** (the primary host is the DGX = Linux, so this was the higher-impact bug). [`core/src/workers/gliner_relex.rs`](../../../core/src/workers/gliner_relex.rs)'s `container_mode_entry` referenced the macOS-only `SandboxBackendKind::Container` variant unconditionally, and `gliner_relex_entry` dispatched to it on every target. Fix mirrors the **established cfg pattern already in [`core/src/sandbox_health.rs`](../../../core/src/sandbox_health.rs)** (`#[cfg(target_os = "macos")]` / `#[cfg(not(...))]` paired):

- `#[cfg(target_os = "macos")]` on `container_mode_entry` + the two container-only consts (`CONTAINER_IMAGE_DEFAULT`, `CONTAINER_BINARY`).
- In `gliner_relex_entry`, the `if env.use_container_backend { return container_mode_entry(env); }` branch is `#[cfg(target_os = "macos")]`; `host_mode_entry` is the only reachable path on Linux.
- In `resolve_env`, `use_container_backend` is computed from the env var on macOS but is a **compile-time `false`** on non-macOS (the env var isn't even read). An operator who sets `HHAGENT_GLINER_RELEX_USE_CONTAINER=1` on Linux silently gets host-mode + bwrap. (bwrap already provides containment on Linux; Apple `container` is the macOS-only memory-enforcement opt-in.)
- Gated the **6 container-specific unit tests** in [`core/src/workers/gliner_relex/tests.rs`](../../../core/src/workers/gliner_relex/tests.rs) to macOS (`entry_container_mode_*` ×2 + `resolve_env_*container*` ×4). Container tests in the *other* crates (`worker_lifecycle/manager/tests.rs`, `gliner_relex_e2e.rs`, `lifecycle_container_routing_e2e.rs`) were already macOS-gated — `gliner_relex.rs` was the sole ungated site.

**Why a CI workflow for the regression guard.** #144 can't be verified on the dev Mac: cross `cargo check -p hhagent-core --target aarch64-unknown-linux-gnu` dies in a C dependency's build (`cc-rs` can't find `aarch64-linux-gnu-gcc`), not in our code. There was no CI in the repo. Added [`.github/workflows/linux-check.yml`](../../../.github/workflows/linux-check.yml): `cargo check --workspace --all-targets` + `cargo clippy --workspace --all-targets` on `ubuntu-latest`. `--all-targets` is deliberate — it type-checks `#[cfg(test)]` code too, so a future un-gated container test reference can't sneak back in. Scope is compile-only (no `cargo test` — integration tests need live PG + the bwrap AppArmor profile; full runs stay DGX/operator-driven). x86_64 is sufficient: the break was `target_os`-conditional, not arch-conditional.

**Verification (macOS M3 Max).** `cargo test --workspace`: **1145 / 0 / 3** (unchanged — the gated tests still run on macOS; #150 only touched a fixture binary). `cargo clippy --workspace --all-targets`: 0 errors. **Linux: CONFIRMED GREEN** — the `linux-check` CI job on PR [#152](https://github.com/hherb/hhagent/pull/152) passed `cargo check` + `cargo clippy` (`--workspace --all-targets`) on `ubuntu-latest` (1m17s), proving `hhagent-core` compiles on Linux again and no other Linux-only breakage exists.

---

## Recently completed (this session, 2026-05-29 — Issue [#147](https://github.com/hherb/hhagent/issues/147): redact secret plaintext in the tool audit row, branch `fix/issue-147-redact-tool-req-plaintext`, **merged to `main` via PR [#151](https://github.com/hherb/hhagent/pull/151) at `54e8885`**)

**The problem.** Opaque secret references slice 1 (Item 31, PR #146) deliberately scoped the secret-plaintext privacy invariant to `actor='policy'` rows only — mirroring the injection-guard slice 1 precedent (commit `45627fd`). The cost: `tool_host::dispatch` snapshotted `req_for_audit` AFTER `substitute_refs_in_params` ran, so the `tool:<name>` row's `payload.req` carried the **redeemed plaintext**. Any operator (or anyone) with read access to `audit_log` could recover every materialized secret by reading the corresponding tool row — a much weaker operational posture than a naive reader of the design spec would assume. Surfaced by `/code-review` on PR #146 and filed as Issue #147.

**The fix (one moved clone).** Snapshot `req_for_audit = params.clone()` BEFORE the substitution block in [`core/src/tool_host.rs`](../../../core/src/tool_host.rs). At that point `params` still carries the opaque `secret://<8-hex>` refs exactly as the planner issued them, so the redeemed plaintext never reaches `audit_log`. The worker still receives the real plaintext via the mutated `params` passed to `WorkerCommand::new` — only the audit snapshot is taken pre-substitution.

**Why snapshot-before over the issue's "substitute back" proposal.** Issue #147 suggested using the `redemption_events` vector to substitute the plaintext back to the ref string in `req_for_audit`. Snapshot-before is strictly better: (1) it's a one-line move, not a reverse-substitution walker; (2) it can't accidentally redact a coincidental plaintext match elsewhere in `params`; (3) it records the request *as the planner issued it* — the most faithful audit shape. The `redemption_events` vector is untouched (still drives the `secret.redeemed` rows).

**Scoped invariant — `payload.result` is out of scope.** A worker that is legitimately handed a secret may echo it in its own output (the `printf %s` test worker does exactly this). That lands in `payload.result`, which is the worker's response, not the request. The invariant is and remains: no *redeemed plaintext* in the request snapshot (`req`) or in any `policy` row. The tool row's `result` is the worker's own data.

**TDD.** Widened test 7 (`policy_rows_contain_no_substring_of_redeemed_plaintext`) in [`core/tests/secret_vault_e2e.rs`](../../../core/tests/secret_vault_e2e.rs): after the policy-row loop it now fetches the `tool:shell-exec` row and asserts `payload.req` (serialized) contains no plaintext marker. Added focused test 9 `tool_row_req_shows_opaque_ref_not_plaintext` — negative (no plaintext) **and** positive (the opaque `secret://<ref>` IS present in `req`, and stdout still carries the plaintext so the worker did receive it). Both confirmed RED on pre-fix code, GREEN after.

**Verification (macOS M3 Max).** `cargo test --workspace` without PG: **1145 / 0 / 3** (1144 → 1145, +1 from new test 9). `secret_vault_e2e` live against Postgres.app v18: **9 / 9 passed**. `cargo clippy -p hhagent-core --all-targets`: 10 warnings, all pre-existing.

**Drive-by filed, not fixed (Issue [#150](https://github.com/hherb/hhagent/issues/150)).** `cargo clippy --workspace --all-targets` now hard-**errors** on `sandbox/tests/fixtures/mem_burner.rs:46` (`set_len()` after `with_capacity` — `clippy::uninit_vec` promoted to deny-by-default). Untouched by this change (different crate); `cargo build`/`cargo test` unaffected. One-line fix (`vec![0u8; bytes]`) kept out of this PR to keep the diff focused.

**File-size watch.** `core/src/tool_host.rs`: net **+0 production LOC**. Still 867 LOC, 367 over the 500-LOC cap — the Items 30+31 sibling-lift bundle remains the tracked follow-up.

---

## Recently completed (this session, 2026-05-29 — ★ Opaque secret references slice 1, branch `feat/opaque-secret-refs-slice-1`, **merged to `main` via PR [#146](https://github.com/hherb/hhagent/pull/146) at `bc36e4c`** + post-review polish `5885e26` + `9348e9c` — closes HANDOVER Item 31)

The planner context is the widest LLM-visible surface in the system. Today a tool calling path that includes an API key, a bearer token, or any named secret passes that value verbatim through `tool_host::dispatch` into the worker's `params`, through the JSON-RPC call, and — if the planner logs it — into the audit log and transcript replay. This slice closes that leak surface for secrets the operator explicitly stages: once a secret is materialized into the in-process `Vault`, the planner sees only an opaque 8-hex ref string (`secret://7c9f2e`), never the plaintext.

The chokepoint pattern mirrors the injection guard (HANDOVER Item 30): substitution runs BEFORE `worker.call` in `tool_host::dispatch`, fail-closed on any miss. The two guards sit at opposite ends of the same dispatch call: injection guard screens worker OUTPUT on the way back; secret substitution resolves opaque refs in worker INPUT on the way in.

**What shipped (4 substantive commits TDD-ordered):**

1. **`b58cd55` — Task 1: module skeleton + types + const pins.** New `core/src/secrets/mod.rs` with `SecretRef` opaque newtype, `RedeemResult` enum (`Hit(String)` / `Expired` / `NotFound`), `SECRET_REF_PREFIX = "secret://"`, `SECRET_REF_HEX_LEN = 8`, `DEFAULT_TTL = 3600s`. +7 unit tests.
2. **`3bc66a3` — Task 2: Vault impl.** New `core/src/secrets/vault.rs` with `Vault` (TTL'd `std::sync::RwLock<HashMap>`, lazy GC, `OsKeyringProvider` trait). `materialize` mints a ref + writes the `policy / secret.materialized` row hard-fail (no materialized-but-unaudited ref can exist). `redeem` returns `RedeemResult` with lazy GC. +9 unit tests.
3. **`7b1e3c5` — Task 3: substitution walker + FakeVault.** New `core/src/secrets/substitute.rs` with `substitute_refs_in_params` — recursive `serde_json::Value` walker; exact-match `SECRET_REF_PREFIX` check; fail-closed on Expired/NotFound. `FakeVault` test seam. +16 unit tests.
4. **`19eebd6` — Task 4: dispatch wiring + scheduler plumbing + main.rs bootstrap + 8 integration tests + classifier test fix.** Substitution wired into `tool_host::dispatch` before `worker.call`: on Ok, one `policy / secret.redeemed` row per ref (best-effort); on Err, `policy / secret.redemption_failed` row fail-closed, worker not called, no tool row. `ToolHostError` widened to `#[non_exhaustive]`; classifier maps the new variant to `POLICY_DENIED`. `main.rs` reads `HHAGENT_BOOTSTRAP_SECRETS` and materializes each via `OsKeyringProvider` at startup.

**Verification (macOS M3 Max):** with PG live **636 passed / 2 pre-existing flakes** (baseline 603 on `main` at `c505b36`); without PG **1137 / 0 / 3** (+41 over 1096 on `main` at `62905ae`). No new clippy warnings.

**Architectural notes worth keeping:**

- **Substitution BEFORE injection guard.** The two dispatch guards are ordered: secret substitution on INPUT, injection guard on OUTPUT. Neither knows about the other; they compose at the chokepoint.
- **Asymmetric audit posture.** Materialize-time audit is hard-fail (no materialized-but-unaudited ref can exist). Redeem-time audit is best-effort (plaintext already in process memory; failing the audit there would discard a successfully-substituted value with no security benefit).
- **Tool row `payload.req`** is allowed to carry the substituted plaintext per the original slice — **corrected by Issue #147 this session** (see entry above; now snapshots the opaque ref pre-substitution). The `policy / secret.redeemed` row carries only `{ref_hash, tool, actor}`, never the value.
- **Exact-match substitution only.** A JSON string that IS `"secret://…"` is replaced entirely; embedded (`"Bearer secret://…"`) is NOT. Deferred to Slice 2.

**File-size watch:** new `secrets/mod.rs` ~130 / `vault.rs` ~260 / `substitute.rs` ~290 LOC (all under cap). `tool_host.rs` 767 → 867 LOC (+100; 367 over cap — bundle the sibling-lift with Item 30).

**Open follow-ups for future slices:** no CLI surface yet (Slice 2 = `hhagent-cli secrets materialize` + CLI↔daemon IPC); per-task vault scoping; embedded substitution; revocation (Slice 3); binary secrets; `tool_host.rs` sibling-lift (bundle Items 30 + 31). Filed: [#148](https://github.com/hherb/hhagent/issues/148) (audit-insert failure paths untested), [#149](https://github.com/hherb/hhagent/issues/149) (`Vault::materialize` ref-collision branch untested — needs RNG injection).

**Post-review polish (`5885e26` + `9348e9c`):** manual `Debug` for `SecretRef` (prints `ref_hash=<sha256>` only, never the ref string; pinned by `secret_ref_debug_never_leaks_ref_string`); `materialize` collision-safety via `Entry::Vacant`-or-`VaultError::RefCollision`; `gliner-relex::Client::extract` shares a `OnceLock<Vault>` static instead of allocating per call; walker partial-walk behaviour promoted from "unspecified" to contract.

---

## Recently completed (this session, 2026-05-28 — ★ Worker-output prompt-injection guard slice 1, **merged to `main` via PR [#141](https://github.com/hherb/hhagent/pull/141) at `62905ae`** — closes HANDOVER Item 30)

Sandboxing contains code; it does not contain text. Today every successful worker result flows verbatim back into the planner's conversation history via `core::tool_host::dispatch`. A poisoned tool output (a hostile file the worker dutifully `cat`'d, a malicious web page once `web-fetch` lands, a coerced MCP response once that lands) can rewrite the planner's instructions on the next turn. The open-loop risk exists **today** via shell-exec reading any FS-allowed file — this is not a hypothetical attack surface.

**Slice scope (deliberately narrow — Slice 1):**

- Two-tier verdict (`Allow` / `Block`) — defer the 0.45–0.70 Review tier per YAGNI; `InjectionDecision` is `#[non_exhaustive]` so it slots in later without breaking callers.
- 22-entry English-substring catalogue across 4 attack classes (instruction_override, role_hijack, secret_exfiltration, unsafe_tool_coercion). Substring matching post-`normalize` (lowercase + zero-width strip). No regex, no leetspeak fold, no multilingual coverage.
- Per-rule weights summed (cap 1.0); `BLOCK_THRESHOLD = 0.70`. Catalogue invariant pinned: every class has at least one entry with weight ≥ `BLOCK_THRESHOLD`.

**What shipped (4 substantive commits TDD-ordered):** `536d23a` Task 1 module skeleton + const pins; `11a0a8e` Task 2 `extract_scannable_text` (recursive `serde_json::Value` walk, UTF-8-aware truncation at `SCAN_BYTE_CAP = 64 KiB`); `eae2a6f` Task 3 `normalize` + 22-entry catalogue + `screen`; `4e32988` Task 4 wire into `tool_host::dispatch` + 6 integration tests. On Block, the worker result is replaced with a placeholder JSON and a SECOND audit row (`actor='policy'`, `action='injection.blocked'`) carries SHA-256 + byte length + truncation flag + score + class codes; the raw scanned body is **never** persisted (pinned by `policy_audit_row_contains_no_substring_of_blocked_body`). Errors are NOT screened.

**Verification (macOS M3 Max):** `cargo test --workspace` **1096 / 0 / 3** (+31 over 1065, re-verified post-merge on `main` at `62905ae`). The 6 integration tests skip-as-pass without PG. No new clippy warnings.

**Architectural notes worth keeping:**

- **The screen runs at the chokepoint, not at the call site** (Option M's sealed `WorkerCommand`); one wiring point, no bypass.
- **The Err path is not screened** — errors are not text-channel content; the scheduler short-circuits on Err.
- **Block returns `Ok(placeholder)`, not `Err`** — the planner sees a tool result it can react to, not a failure that burns the retry budget.
- **Two audit rows on Block** — the `tool:<name>` row carries the placeholder; the `policy / injection.blocked` row carries forensic data; correlate by timestamp/tool/SHA-256.
- **`#[non_exhaustive]` on `InjectionDecision`** is the forward-compat seam for the Review tier.

**File-size watch:** new `cassandra/injection_guard.rs` ~430 LOC (under cap). `tool_host.rs` 708 → 763 LOC (was already over cap; now 263 over). Filed: [#142](https://github.com/hherb/hhagent/issues/142) (chat-template tokens may false-positive on technical docs once web-fetch/MCP land), [#143](https://github.com/hherb/hhagent/issues/143) (`extract_scannable_text` walk has no explicit depth guard; reachable only if upstream serde_json/protocol limits are removed).

**Deferred to future slices:** Review tier; heuristic/combinatorial scoring; multilingual; per-tool policy; `hhagent-cli policy review` operator surface; `tool_host.rs` sibling-lift.

---

## Earlier history (summary)

One bullet per session. Full reasoning lives in the archive snapshots: sessions 2026-05-10 → 2026-05-29 in [`archive/handover_20260529_pre-prune.md`](archive/handover_20260529_pre-prune.md); sessions 2026-05-06 → 2026-05-09 in [`archive/handover_20260510_pre-prune.md`](archive/handover_20260510_pre-prune.md).

- **2026-05-28 — `idle_timeout/release.rs` sibling-lift + Issue #89 `/tmp` tmpfs pin:** pure-mechanical lift of `release_idle_timeout_worker` + `abort/replace_idle_teardown_handle` into a new `worker_lifecycle/idle_timeout/release.rs` (`idle_timeout.rs` 647 → 490 LOC, under cap), merged via PR [#138](https://github.com/hherb/hhagent/pull/138) at `5fc5fee`. Plus a new Linux `linux_smoke` test pinning that `--tmpfs /tmp` is per-spawn ephemeral + writable + listing-real (PR [#139](https://github.com/hherb/hhagent/pull/139) at `504094e`), and docs-only PR [#140](https://github.com/hherb/hhagent/pull/140) at `6fd82bf` importing 7 openhuman design patterns (flagged Items 30 + 31).
- **2026-05-27 — worker_lifecycle hardening + test-infra slices:** debounce pending-acquires queue-depth warn ([#136](https://github.com/hherb/hhagent/issues/136), PR #137 `fed0f21`); hardening trio [#84](https://github.com/hherb/hhagent/issues/84)+[#85](https://github.com/hherb/hhagent/issues/85)+[#86](https://github.com/hherb/hhagent/issues/86) (`#[non_exhaustive]` Lifecycle variant + one-teardown-per-slot abort/respawn + `pending_acquires` AtomicU32 + warn threshold, PR #135 `7f98ee4`); `bring_up_pg_cluster_with_timeout` sibling ([#131](https://github.com/hherb/hhagent/issues/131), PR #133 `8655319`); test-module lift quadruple (PR #132 `162f71f`, −940 LOC off 4 parents); `bring_up_pg_cluster` polling-cap lift 15s→30s + 5-site consolidation −1062 LOC (PR #129 `4e94e42`, filed [#130](https://github.com/hherb/hhagent/issues/130)).
- **2026-05-26 — graph + entity-upsert slices:** `walk_*_edges` diamond-dedupe via `DISTINCT ON` + combined `walk_edges_around` UNION ALL ([#114](https://github.com/hherb/hhagent/issues/114)+[#115](https://github.com/hherb/hhagent/issues/115), PR #128 `bb32cab`); `HHAGENT_PG_BIN_DIR` test-fixture env override (Item 29, PR #126 `7adc582`, filed [#127](https://github.com/hherb/hhagent/issues/127)); entity-upsert Layer B full-batch unnest + per-row attribution fallback ([#95](https://github.com/hherb/hhagent/issues/95), PR #125 `dac0dcd` — new `entity_extraction/batch_upsert.rs`).
- **2026-05-25 — Slice 2.5 + lifts:** Slice 2.5 follow-up triple [#120](https://github.com/hherb/hhagent/issues/120)+[#121](https://github.com/hherb/hhagent/issues/121)+[#122](https://github.com/hherb/hhagent/issues/122) (PR #124 `e93997e`); `gliner_relex.rs` test-module lift 1547 → 811 LOC (PR #123 `920e0c9`); GLiNER-Relex Slice 2.5 Containerfile + macOS image build, `--init` always-on closes [#107](https://github.com/hherb/hhagent/issues/107) (PR #118 `a9e3385`, container e2e empirically PASSED in 12.58s).
- **2026-05-23 — CLI splits + relations show:** Item 23(a) test-module sibling lifts (PR #117 `919882d`); Item 22 kinds-CLI shared lift + over-cap CLI file splits, closes [#111](https://github.com/hherb/hhagent/issues/111)+[#112](https://github.com/hherb/hhagent/issues/112) (PR #116 `1abb061`); `hhagent-cli relations show <id> [--depth N] [--format]` graph-edge introspection (PR #113 `9a46e18`).
- **2026-05-22 — kinds CLIs + container Slice 2:** `entities kinds {add,remove,list}` (PR #110 `a65bb4a`); `relations kinds {add,remove,list}` + `connect_admin_pool` (PR #109 `f234d0c`); `MacosContainerBackend` Slice 2 — `SandboxBackendKind` enum + `SandboxBackends` resolver + per-worker `ToolEntry.sandbox_backend` (PR #108 `1b86f84`). **(NB: the unconditional `Container` reference added here is what breaks the Linux build — [#144](https://github.com/hherb/hhagent/issues/144).)**
- **2026-05-21 — macOS container backend + GLiNER macOS:** `MacosContainer` Slice 1 (PR #106 `cc0b0de`, filed [#107](https://github.com/hherb/hhagent/issues/107)); Issue #55 Apple `container` discovery spike — verdict COMMIT (PR #105 `56456da`); GLiNER-Relex macOS device decision tree (PR #103 `9220f40`); audit_tail tempdir-collision flake fix [#101](https://github.com/hherb/hhagent/issues/101); relation-label vocabulary migration 0017 + `RelationKindsCache` (PR #100 `5bcd060`); defer hhagent-cli tokio runtime ([#97](https://github.com/hherb/hhagent/issues/97), PR #98 `dbee0ac`).
- **2026-05-20 — quarantine CLI + cli split:** `hhagent-cli.rs` 1933 LOC → per-subcommand-module directory ([#66](https://github.com/hherb/hhagent/issues/66), PR #96 `2704468`); entity-upsert Layer A round-trip reduction ([#90](https://github.com/hherb/hhagent/issues/90), PR #94 `3ab94f6`); operator quarantine-review CLI `entities {list,show,approve,reject,merge}` (PR #93 `028e541`).
- **2026-05-19 — entity extraction v2:** memory-write-time `memory_entities` auto-linker (PR #92 `d58ecc9`); Entity Extraction v2 design + plan + full GLiNER-Relex implementation + migration 0016 (PR #91 `f12b460`).
- **2026-05-18 — worker lifecycle + GLiNER worker:** GLiNER-Relex Slice 2 Rust manifest + e2e (PR #88 `715a882`); tech-debt batch (PR #87 `665901d`); GLiNER-Relex Slice 1 Python worker (`dfb1126`); worker-lifecycle slices 1+2 (`SingleUse`/`IdleTimeout`/`Composite` managers, +30 tests); `inner_loop.rs` 1214 → 655 LOC split ([#81](https://github.com/hherb/hhagent/issues/81)); L1 promotion writer (PR #82 `eb6b8a8`); post-merge spec landings.
- **2026-05-17 — recall-lane wiring** (PR #79 `7553404`): recall wired into the production scheduler path.
- **2026-05-16 — memory plumbing:** prompt-assembler L0+L1 `build_system_prompt` (PR #74); L0 seed-data loader (PR #77); runner rejects producer-supplied `agent_raised` provenance ([#71](https://github.com/hherb/hhagent/issues/71)); automatic classification-floor inference (PR #70 `4ddfe3b`).
- **2026-05-15 — first CASSANDRA rules + harness:** L1 memory-layer storage primitive migrations 0013+0014 (PR #68 `b1c63e2`); first real `DeterministicPolicy` rule; first real `ConstitutionalGuard` prompt screen (PR #67 `67d29a0`); rule-iteration harness `observation::replay` + CLI (PR #65 `9c01e30`); audit-payload bump on `agent/plan.formulate` (PR #61 `67f2dac`).
- **2026-05-14 — observation + refusal state:** observation-phase first capture run + `parse_plan_lenient` (PR #60); constitutional refusal `tasks.state='refused'` + migration 0012 ([#23](https://github.com/hherb/hhagent/issues/23), PR #59 `f1fea54`); batch issue cleanup #5/#6/#17/#20/#40/#47/#50 (PR #54 `25c312c`); per-tool argv allowlist DB + `tools allowlist` CLI; producer-cancelled-pending `task.finalize` row; Option G CPU-quota/rlimit enforcement.
- **2026-05-13 — audit rows + seal:** crashed-task `task.finalize` row; observation-phase fixture captures; `WorkerCommand` seal tightened ([#16](https://github.com/hherb/hhagent/issues/16)); CLI `task.submitted` + `task.cancelled` producer rows; graph lane in `memory::recall` (PR #41 `76fe940`).
- **2026-05-12 — Phase 1 foundations:** `hhagent-tests-common` shared dev-dep crate ([#15](https://github.com/hherb/hhagent/issues/15), PR #38 `97f2743`); `task.crashed` crash-recovery sweep row; spec §7 task-lifecycle audit rows; scheduler short-circuit audit rows; split `memory.rs` into submodules ([#30](https://github.com/hherb/hhagent/issues/30)); Option O embedding router + first `actor='llm:router'` row.
- **2026-05-11 — scheduler online:** Task 4.4 `cli_ask_e2e` full-chain pin; Task 3.2.bis wire `ToolHostStepDispatcher` to `tool_host::dispatch`; mock-HTTP coverage for `formulate_plan`; `tasks_lifecycle_e2e` deadlock fix; scheduler/CASSANDRA Phases 2–5 landed.
- **2026-05-10 — chokepoint + recall skeleton:** Options M (sealed `WorkerCommand` + dispatch chokepoint) + N (`memory::recall` pgvector+tsvector RRF skeleton); Option J LLM-router stub; secrets-at-rest AES-256-GCM + OS keyring + migration 0004; Option I dispatcher chokepoint + audit_log NOTIFY trigger + JSONL mirror + `audit tail`; Option L non-superuser runtime role + GRANT split.
- **2026-05-09 — cgroup v2 caps:** `linux_cgroup.rs` wraps every bwrap call in `systemd-run --scope` with `MemoryMax`/`MemorySwapMax=0`/`CPUQuota`/`TasksMax`; `cgroup_probe()` fail-closed; mem_burner OOM-kill test. Plus C2.2 schema + sqlx migrations + Graph trait + probe + e2e.
- **2026-05-08 — supervisors + Phase-0 polish:** Linux `systemd_user` + macOS `LaunchAgents` backends (pure `build_unit_file`/`build_plist` + validate + atomic write); per-task scratch `Workspace` with RAII; wall-clock watchdog + the `kill(-1)` fanout fix.
- **2026-05-06/07 — Phase 0 sandbox core:** `workers/prelude` Landlock + seccomp lock_down; macOS Seatbelt backend (`build_profile` → `.sb`); initial AGPL workspace + Linux bwrap backend + protocol crate + shell-exec worker + tool_host + first e2e. Full detail in the 20260510 archive.

---

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** AGPL project; all third-party deps must be AGPL-compatible (Apache-2.0, MIT, BSD, MPL, LGPL, (A)GPL all fine).
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS (Apple Silicon and Intel). No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python workers.** Rust for core (no eval/dynamic surface); Python only inside sandboxed tool workers. shell-exec is Rust because it's a thin execve wrapper — Python's first appearance will be `python-exec` in Phase 4 (or possibly `web-fetch` earlier).
- **Hybrid LLM with policy routing.** Local-first via OpenAI-compatible HTTP (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS). Frontier (Claude/OpenAI) only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisor.** `systemd --user` (Linux) / `launchd` LaunchAgents (macOS). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Critical workers are human-curated and shipped with the binary. Agent-authored code only runs inside `python-exec`'s strict sandbox; named/persisted skills get an optional human-approve gate (Phase 4).
- **JSON-RPC 2.0 over stdio.** MCP-stdio compatible. Lets us swap in a richer MCP client later without changing the trust boundary.

---

## Next TODO (pick one)

Phase 0 is complete; Phase 1 (memory recall + scheduler loop + end-to-end step dispatch) is on `main` and pinned by `cli_ask_e2e`. The recent slices closed the two openhuman-imported security items (Item 30 injection guard, Item 31 opaque secret references). The list below is an **operator-picks bucket** — sized roughly one session each, with file paths and the verification step.

**Done since last session:** PR [#152](https://github.com/hherb/hhagent/pull/152) (#144 + #150 fixes) merged to `main` at `560d845`; `linux-check` CI green; both issues closed. Still optional/worthwhile (operator action, no code): a full `cargo test --workspace` on the DGX — CI is compile-only, so the Linux *test* baseline is still the stale `990`-at-`1abb061`.

**Secrets / injection-guard follow-ups (continue the Item 30/31 arc):**

3. **[#148](https://github.com/hherb/hhagent/issues/148) + [#149](https://github.com/hherb/hhagent/issues/149) — secret-vault test gaps.** #148: cover the best-effort audit-insert failure paths (`secret.redeemed` loop, `secret.redemption_failed` early-return) — needs an `AuditSink` trait seam in `hhagent_db::audit` for fault injection. #149: exercise `Vault::materialize`'s `Entry::Occupied`/`RefCollision` arm via RNG injection or a `_test_materialize_at_ref` seam.
4. **[#142](https://github.com/hherb/hhagent/issues/142) — injection-guard chat-template false-positives.** **Deferred this session per the issue author's recommendation** (`#143` shipped; see Recently-completed). Chat-template tokens (`<|im_start|>`, `<|system|>`) will false-positive on technical docs once `web-fetch`/MCP workers land — pick a fix (per-tool exemption hook, AND-heuristic, or catalogue tightening) *with real data* once such a worker exists, not before. `#143` (`walk()` depth guard) is **done** (`MAX_WALK_DEPTH = 256`).
4b. **`injection_guard.rs` test-module sibling-lift.** Now 615 LOC, 115 over the 500-LOC cap (the #143 tests pushed it further over). Lift the inline `#[cfg(test)] mod tests` into a sibling `core/src/cassandra/injection_guard/tests.rs` — the established repo pattern (see the 2026-05-18/05-27 lifts) drops production LOC to ~285. Pure-mechanical; existing tests are the regression pin.
5. **`tool_host.rs` sibling-lift (Items 30 + 31 bundle).** Now 867 LOC, 367 over the 500-LOC cap. Lift the injection-guard wiring and/or secret-substitution wiring into sibling submodules. Pure-mechanical; existing tests are the regression pin.

**Test-infra / refactor bucket:**

6. **[#130](https://github.com/hherb/hhagent/issues/130) — parallel-launchd bring-up contention under `HHAGENT_PG_BIN_DIR`.** 5+ tests racing for macOS launchd registration can exceed even the new 30 s cap. Candidate shapes: a serial-mutex around PG bring-up in `tests_common`, or opt-in `--test-threads=1` for the override path. Operator picks the shape.
7. **[#134](https://github.com/hherb/hhagent/issues/134) — `tests-common`: revise the `bring_up_pg_cluster` doc example** or ship a real `_with_timeout` caller (the demonstrative Homebrew-detection caller deferred from #131).
8. **`HHAGENT_GLINER_RELEX_REQUIRE_E2E=1` CI knob** — turn the container e2e's skip-as-pass into a hard fail for any CI runner with PG + container + image + weights staged.
9. **[#81](https://github.com/hherb/hhagent/issues/81) — split `core/src/scheduler/inner_loop.rs`** (still over cap after the 2026-05-18 split); **[#99](https://github.com/hherb/hhagent/issues/99)** migrate `ask.rs`+`observation_replay.rs` to `common::with_runtime`; **`db/src/graph.rs` walk-impl split** (Item 23(b), deferred until a second `WalkedEdge` consumer materialises); **`KindsCli<T>` shared-generic lift** (Item 24, deferred until a third kinds-style CLI appears).

**Engineering pickups (need a spec/design first):**

10. **L3 skill crystallisation — spec.** All pre-reqs in tree; the L1 distillation pattern (`Plan.l1_insight` → `drain_lane` hook → audit row) is the direct precedent. L3 distils multi-step trajectories into parameterised JSON-RPC tool-call templates stored as L3 `memories`.
11. **Worker manifest plumbing — design slice.** Spec open question 1 (TOML files vs Rust consts) is unresolved; slices today ship `Lifecycle` directly on `ToolEntry`.
12. **Production caller wiring for the graph lane.** `RouterAgent::formulate_plan` must populate `seed_entity_ids` from entities extracted from the current task context (the graph lane is a no-op in production until this lands). See "Design notes for parked work" below.

**Operator actions (no code):** recapture observation fixtures against the current daemon (`cargo test -p hhagent-core --test observation_capture -- --ignored --nocapture`); real-model relation-extraction validation (`HHAGENT_GLINER_RELEX_ENABLE=1 cargo test … entity_extraction_e2e`); Linux DGX full `cargo test --workspace` re-verification once #144 is fixed; DGX verification of the #89 `/tmp` tmpfs pin (pass-on-current-code + catches-drift via temporary `--tmpfs`→`--bind-try` mutation).

---

## Design notes for parked work

### Option P — entity↔memory linkage + graph lane (Phase 1 cont.)

The `memory_entities` join table (P1) shipped; the graph lane is wired into `recall`. **Still parked: the production caller wiring** (Next-TODO item 12). For a query carrying `seed_entity_ids`, the lane traverses outbound 1-hop to a candidate entity set, then `SELECT memory_id FROM memory_entities WHERE entity_id = ANY($1)` ranked by neighbour count. The remaining work is an entity-extraction step in `formulate_plan` that populates `seed_entity_ids` from the current task context — until then the lane is a no-op in production. Secondary deferral: `entities.embedding` is NULL for all entities; a populated column would seed an entity-similarity lane. Deferred until observation phase; the `vector(1024)` column already exists.

### Option K — cross-platform exponential restart backoff

Currently `Restart=on-failure RestartSec=5` is a constant 5 s. systemd 252+ supports `RestartSteps`/`RestartMaxDelaySec`; macOS launchd's `KeepAlive=true` has no operator-controllable throttle. Cross-platform shape: extend `ServiceSpec` with `restart_backoff: Option<RestartBackoff>` (max delay + step count); the systemd backend wires it into the unit file, the macOS backend logs a warning at install time and falls back to launchd's default. Filed but parked; no immediate need.

---

## Open follow-up issues (filed but not picked)

Only currently-open issues are listed; closed-issue detail lives in the archive snapshots and git history.

- [#3](https://github.com/hherb/hhagent/issues/3) — drop `SYS_SENDFILE`/`SYS_FADVISE64` shim once libc exposes them on aarch64.
- [#4](https://github.com/hherb/hhagent/issues/4) — bump Last-commit + test-count fields whenever a Recently-completed entry is added (process hygiene).
- [#8](https://github.com/hherb/hhagent/issues/8) — collapse `default_probe`/`default_supervisor` cfg-ladder duplication once a third entry point or backend OS appears.
- [#13](https://github.com/hherb/hhagent/issues/13) — write a migration numbering / rename hygiene checklist (sqlx fingerprints version+slug; a rename on a shipped migration silently breaks startup on existing clusters).
- [#14](https://github.com/hherb/hhagent/issues/14) — replace the brittle `wait_for_log_match("database probe succeeded")` in `supervisor_e2e.rs` with a real readiness signal or a public constant.
- [#20](https://github.com/hherb/hhagent/issues/20) — `agent_prompts` PK on sha256 means renamed prompt files lose their original name *(note: 0011 changed the PK to `(sha256, name)`; issue tracks any residual)*.
- [#21](https://github.com/hherb/hhagent/issues/21) — scheduler per-iteration cancellation poll could be a `watch::Receiver` instead of a DB round-trip.
- [#24](https://github.com/hherb/hhagent/issues/24) — deployment: `HHAGENT_PROMPTS_DIR` has a cwd-relative fallback; production unit files must set it explicitly.
- [#37](https://github.com/hherb/hhagent/issues/37) — scheduler crash-recovery sweep+audit is unoptimised for high crash counts.
- [#39](https://github.com/hherb/hhagent/issues/39) — tests-common optional hardening (PgCluster.sup access, internal self-tests).
- [#40](https://github.com/hherb/hhagent/issues/40) — design: should `RecallParams::new()` default to graph-off until an entity-extraction step lands? *(partially addressed by `with_seeds`; tracks the residual design question.)*
- [#42](https://github.com/hherb/hhagent/issues/42) — `deleted_memories` AFTER DELETE trigger uses `SECURITY INVOKER`; a future role with DELETE-on-memories but no INSERT-on-deleted_memories would silently break DELETE. Deferred until a second DELETE-capable role is proposed.
- [#47](https://github.com/hherb/hhagent/issues/47) — observation/capture: distinguish 'no verdict row' from a real Approve verdict *(SCHEMA_VERSION 2 made `verdict_today` Optional; tracks residual.)*
- [#50](https://github.com/hherb/hhagent/issues/50) — unify finalize-payload provenance signal across crashed/producer-cancelled/runtime emitters *(`provenance` field shipped; tracks residual unification.)*
- [#55](https://github.com/hherb/hhagent/issues/55) — macOS Apple `container` micro-VM backend *(discovery spike shipped + Slices 1/2/2.5 shipped; issue tracks the broader rollout.)*
- [#62](https://github.com/hherb/hhagent/issues/62) — audit-payload truncation can silently nuke `agent/plan.formulate` fields.
- [#63](https://github.com/hherb/hhagent/issues/63) — e2e gap: classification_floor plumbing from `tasks.payload` to the `agent/plan.formulate` audit row.
- [#73](https://github.com/hherb/hhagent/issues/73) — scheduler/runner e2e integration test + TaskContext-construction reminder for producer-side floor-source validation.
- [#76](https://github.com/hherb/hhagent/issues/76) — prompt-assembly: verify PromptAssembly error retry semantics in scheduler.
- [#78](https://github.com/hherb/hhagent/issues/78) — prompt-assembly: global token cap with priority drop for the assembled system prompt.
- [#81](https://github.com/hherb/hhagent/issues/81) — split `core/src/scheduler/inner_loop.rs` (still over cap).
- [#99](https://github.com/hherb/hhagent/issues/99) — migrate `ask.rs` + `observation_replay.rs` to `common::with_runtime` for consistency.
- [#104](https://github.com/hherb/hhagent/issues/104) — audit the pid+nanos tempdir pattern across the workspace (follow-up to #101).
- [#107](https://github.com/hherb/hhagent/issues/107) — `MacosContainer` PID-1 signal-handling posture *(closed in code by always-on `--init` in Slice 2.5; verify end-to-end before long-lived workers migrate).*
- [#127](https://github.com/hherb/hhagent/issues/127) — env-var save/restore RAII helper for the `pg_bin_dir_candidates_with_env_override` tests.
- [#130](https://github.com/hherb/hhagent/issues/130) — parallel-launchd bring-up contention under `HHAGENT_PG_BIN_DIR` override (see Next-TODO item 6).
- [#134](https://github.com/hherb/hhagent/issues/134) — tests-common: revise `bring_up_pg_cluster` doc example or ship a real `_with_timeout` caller.
- [#142](https://github.com/hherb/hhagent/issues/142) — injection_guard: chat-template tokens will false-positive on legitimate technical docs (see Next-TODO item 4).
- ~~[#143](https://github.com/hherb/hhagent/issues/143) — injection_guard `walk()` has no recursion depth limit~~ **fixed on branch `fix/issue-143-walk-recursion-depth-guard` (`MAX_WALK_DEPTH = 256`); closes on merge.**
- [#144](https://github.com/hherb/hhagent/issues/144) — **(`bug`)** `container_mode_entry` breaks the Linux build (see Next-TODO item 1).
- [#148](https://github.com/hherb/hhagent/issues/148) — secrets: cover audit-insert failure paths with fault injection (see Next-TODO item 3).
- [#149](https://github.com/hherb/hhagent/issues/149) — secrets: `Vault::materialize` ref-collision branch is untested (see Next-TODO item 3).
- [#150](https://github.com/hherb/hhagent/issues/150) — `mem_burner` `clippy::uninit_vec` now deny-level, breaks `cargo clippy --all-targets` (see Next-TODO item 2).

---

## Open questions parked for later

(From the design plan, restated here so they're surfaced when relevant.)

1. Embedding model on-device — bge-m3 vs nomic-embed-text vs ColBERT (Phase 1)
2. ~~Channel approval — passcode pairing vs static contact allowlist (Phase 2)~~ **Resolved 2026-05-06:** pairing flow with WebAuthn-or-OTP fallback, modeled on ZeroClaw's `security/{pairing,webauthn,otp}.rs`.
3. ~~Egress proxy as separate worker vs in-process in `tool_host`~~ **Resolved 2026-05-06:** separate worker, with the credential-leak scanner co-located.
4. Skill review workflow for *named* agent-authored Python (Phase 4) — see Phase 4 line items: trust enum + per-level capability ceiling.
5. Worker keep-alive vs spawn-per-call (idle-timeout lifecycle shipped for GLiNER-Relex; revisit for other workers when latency matters).
6. Worker binary discovery in production (currently `target/debug/...` for tests; need a stable install location convention).

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
   - `Last commit on <branch>:` → the hash of the most recent shipped commit.
     Confirm with `git log --oneline -1`.
   - `Session-end verification:` → re-run `cargo test --workspace` and copy
     the **passed / failed / ignored / `[SKIP]`** counts into this line.
   - **Every test-count number embedded elsewhere in the doc that changed
     this session** (e.g. the headline test count, "Test count delta" lines
     in Recently-completed entries). A fresh agent grep-finds them and will
     trust whatever is there.
2. **Move "Next TODO" → "Recently completed (this session)"** if the picked option shipped, with enough detail that the next session can understand the decision (file paths, why-not-X, gotchas, test-count delta).
3. **Write a fresh "Next TODO (pick one)"** with options sized for one session each — include file paths, gotchas, and the verification step.
4. **Refresh "Working state"** — anything new under stubs, anything that became real.
5. **Tick the matching items off in [`../ROADMAP.md`](../ROADMAP.md)** with the commit hash.
6. **Commit both files together** with a `docs(handover): ...` message.

### Pruning convention

The handover should stay focused on **what the next session needs to act on**: the current state, the last 2–3 sessions in detail, and the next TODO. Older session entries get compressed into the "Earlier history" summary or dropped entirely once they're no longer load-bearing.

When HANDOVER.md grows past the point where the next session can absorb it cold (rough rule of thumb: more than a couple of screens of "Recently completed"), prune it:

1. **Snapshot first.** Copy the current HANDOVER.md to `archive/handover_<YYYYMMDD>[_<slug>].md` (e.g. `handover_20260510_pre-prune.md`). The archive is the audit trail — never edited after the fact, never deleted.
2. **Keep verbatim:** the header, "Read these first," "Working state" (current truth), the most recent 1–2 sessions of "Recently completed," "Key design decisions," "Next TODO," "Open follow-up issues," "Open questions," "Inspirations," and this section.
3. **Compress everything else** into a single "Earlier history" section: one bullet per session, naming the slice + the headline change + a pointer to the archive snapshot for full reasoning.
4. **Cross-link** from the compressed bullets to the archive snapshot so anyone who needs the full reasoning can find it.
5. **Commit the prune separately** with `docs(handover): prune older sessions, archive pre-prune snapshot` so the diff is reviewable.

The archive directory is the historical record; HANDOVER.md is the working brief.
