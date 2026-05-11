# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention.

**Last updated:** 2026-05-12
**Last commit:** `2f98eb9` (`refactor(core/memory): split into recall.rs + embed.rs submodules`). Pure structural refactor closing [issue #30](https://github.com/hherb/hhagent/issues/30): `core/src/memory.rs` (602 LOC, 102 over the 500-LOC soft cap in CLAUDE.md) split into `core/src/memory/{mod.rs, recall.rs, embed.rs}` (55 + 384 + 219 LOC respectively, each under the cap). Public API preserved exactly via re-exports in `mod.rs`: `memory::{recall, reciprocal_rank_fusion, RecallModes, RecallParams, RRF_K_CONSTANT, embed_query, MemoryError}`. `build_embed_audit_payload` tightened from `pub(crate)` to module-private (only called inside `embed.rs` and its tests). All 327 tests still pass; 0 failures; one dead-code warning in `core/tests/embedding_recall_e2e.rs::ServedRequest` is pre-existing (introduced in PR #29, unrelated to this slice).

Branch lineage: PR #29 (Option O — embedding router + first `actor='llm:router'` audit row) merged 2026-05-11 at `d39023b`; follow-up code-review fixups in `7538132` on `feat/embedding-router` were carried in via the merge (audit `backend` tag now sourced from `pick_embed_backend(&req).as_tag()` instead of hardcoded `"local"` — Phase-5-correct under `DefaultLocalPolicy` which always returns `Local`; `compose_url` straightened to `if/else`; `latency_ms` cast as `try_into().unwrap_or(u64::MAX)`).
**Branch:** `refactor/split-core-memory` (off `main` at `d39023b`). Working tree clean.

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — high-level diagram, process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — invariant, scenarios in scope, defence-in-depth layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO list with commit hashes for shipped items
4. The design plan (outside the repo) — `~/.claude/plans/i-d-like-to-design-logical-starlight.md`
5. Memory notes (auto-loaded) — see `~/.claude/projects/-home-hherb-src-hhagent/memory/MEMORY.md`
6. Older handovers — `archive/handover_<timestamp>.md` (one snapshot per pruning event; full historical detail lives there).

## Working state (what's green right now)

```
hhagent (Rust workspace, 8 crates, AGPL-3.0)
├── core               hhagent-core: lib + 2 bins (`hhagent` daemon + `hhagent-cli` audit-tail viewer). Daemon blocks on SIGTERM/SIGINT via tokio::signal::unix; main.rs runs db::probe::run → connect_runtime_pool → spawn_mirror before wait_for_shutdown (fail-closed startup; mirror failures are logged but non-fatal). lib modules: tool_host (spawn_worker, dispatch chokepoint, lockdown-env derivation, wall-clock watchdog, sealed WorkerCommand), workspace (per-task scratch with RAII cleanup), audit_mirror (PgListener-driven JSONL writer with daily rotation + fsync per write), audit_tail (`tail -f`-style follower used by `hhagent-cli audit tail`), memory/ (split 2026-05-12 into `mod.rs` facade + `recall.rs` + `embed.rs` to stay under the 500-LOC soft cap; flat public surface preserved): `recall.rs` carries Phase-1 `recall(pool, params)` (fans out to `db::memories` semantic + lexical lanes, fuses via Reciprocal Rank Fusion, hydrates top-k bodies in one round-trip), pure `reciprocal_rank_fusion(lists, k)` helper, `RecallModes::{ALL, SEMANTIC_ONLY, LEXICAL_ONLY}`, `RRF_K_CONSTANT = 60.0`; graph lane deferred — needs entity↔memory linkage that the schema doesn't carry yet (Option P). `embed.rs` carries `embed_query(pool, router, text) -> Result<Vec<f32>, MemoryError>` (Option O — validates dim against `EMBEDDING_DIM`, writes first `actor='llm:router' action='embed'` audit row with payload `{model, n_texts, dim, backend, latency_ms}`), `MemoryError` (covers dim mismatch + DB + router error paths), and module-private `build_embed_audit_payload`
├── db                 hhagent-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir) + conn::ConnectSpec (UDS PgConnectOptions builder) + RUNTIME_ROLE/set_role_runtime_statement + probe::run (ensure DB → migrate as superuser → SET ROLE hhagent_runtime → audit row, fail-closed) + graph::{Graph trait, PgGraph} (relational entities/relations + recursive-CTE path()) + audit::{insert, fetch_by_id, fetch_since, truncate_payload} (4 KiB SHA-256 envelope) + memories::{insert_memory, semantic_search, lexical_search, fetch_by_ids, vector_literal} (pgvector text-cast bind for `vector(1024)`; `<=>` cosine via sequential scan; `to_tsvector('simple')` + `ts_rank` paired with the schema's GENERATED `tsv` column) + pool::connect_runtime_pool (PgPool with `after_connect` SET ROLE hhagent_runtime hook) + MIGRATOR (sqlx::migrate!() over 0001_init.sql + 0002_runtime_role.sql + 0003_audit_log_notify.sql + 0004_secrets_aad_nonempty.sql) + secrets::{Router-shaped AES-256-GCM at-rest with OS keyring KeyProvider} + hhagent-db-init bin
├── llm-router         hhagent-llm-router: sole egress for LLM calls. `Router::send(&ChatRequest) -> Result<ChatResponse, RouterError>` and `Router::embed(&EmbeddingRequest) -> Result<EmbeddingResponse, RouterError>` over reqwest+rustls; `Backend::{Local, Frontier}` closed enum; `PolicyGate` trait with `DefaultLocalPolicy` always picking `Local` (Phase-5 seam) and `pick_embed` default method (Phase-5 seam for embedding routing). `RouterConfig::from_env` reads `HHAGENT_LLM_LOCAL_URL` / `HHAGENT_LLM_LOCAL_MODEL` / `HHAGENT_LLM_FRONTIER_URL` / `HHAGENT_LLM_FRONTIER_MODEL` / `HHAGENT_LLM_TIMEOUT_MS` / `HHAGENT_LLM_EMBEDDING_URL` (falls back to `HHAGENT_LLM_LOCAL_URL`) / `HHAGENT_LLM_EMBEDDING_MODEL` (defaults to `"embedding-default"` which vLLM rejects to surface misconfig loudly). Per-OS default URL: vLLM/SGLang on Linux (:8000), Ollama on macOS (:11434). `EmbeddingRequest`/`EmbeddingData`/`EmbeddingResponse` wire shapes in `embeddings.rs`. `RouterError::EmbeddingCountMismatch` validates that the response contains the expected number of embedding vectors. Frontier dispatch returns `RouterError::PolicyDeniedFrontier` until Phase 5
├── sandbox            hhagent-sandbox: SandboxPolicy + LinuxBwrap (wrapped in systemd-run --scope cgroup) + MacosSeatbelt
├── supervisor         hhagent-supervisor: SystemdUser (Linux) + LaunchAgents (macOS) + specs::{core_service_spec, postgres_service_spec} + default_probe (per-OS supervisor probe)
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS)
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

**`cargo test --workspace` on Linux: 327 tests passed, 0 failed, 0 `[SKIP]` lines** on branch `refactor/split-core-memory` (this commit, off `main` at `d39023b`). Baseline on `main` after Task 4.4 (`cli_ask_e2e`) was 299; Option O added **+28**; this split is pure structural (test count unchanged). One pre-existing dead-code warning in `core/tests/embedding_recall_e2e.rs::ServedRequest` (introduced in PR #29) is unrelated to this slice — could be cleaned up in any future touch of that file (`#[allow(dead_code)]` on the struct, or read the fields in an assertion). Two pre-existing doctests in `hhagent-sandbox` and `hhagent-worker-prelude` are `ignored` (explicit markers).
**macOS (main):** 299 all pass on macOS (skip-as-pass for PG-dependent tests); Option O additions not yet verified on macOS (embedding TCP mock tests are cross-platform clean; the `embedding_recall_e2e` skip-as-pass path is expected).

**Known flake fixed this session:** `tasks_lifecycle_e2e` (in `db/tests/postgres_e2e.rs`) had a structural deadlock — `pool.close().await` blocks until all `max_connections` permits are released, but two `PgListener`s were still in scope when close() was called. The multi-thread tokio runtime exposed it reliably (90 s+ hang) while the single-thread runtime variant in `audit_helpers_pool_and_notify_round_trip` (same pattern, one listener) had been passing on timing. Fix: explicitly `drop(listener)` before `pool.close().await`. Applied preemptively to `audit_helpers_pool_and_notify_round_trip` too so the latent flake there is closed out as well.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 16 | bwrap argv builder shape (6) + cgroup `systemd-run` argv builder shape: starts with `systemd-run`, uses `--user --scope --quiet --collect`, sets `MemoryMax`+`MemorySwapMax=0`, omits both when `mem_mb=0`, defense-in-depth `CPUQuota=200%` + `TasksMax=64` defaults, ends with `--`, no inner-program leakage, 4 `-p` flags total (10) |
| `sandbox` unit (macos) | 14 | sandbox-exec profile builder shape + path canonicalization + on-host probe + TinyScheme-injection rejection + canonicalize error propagation + strict profile does NOT contain unrestricted `(allow mach-lookup)` (issue #1) |
| `sandbox` integration (`linux_smoke`) | 7 | **real** bwrap+cgroup: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected, mem_burner allocating 256 MiB under `MemoryMax=32M` is OOM-killed |
| `sandbox` integration (`macos_smoke`) | 10 | **real** sandbox-exec: scaffold marker, echo runs jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read paths readable, /dev/disk0 denied, relative-path policy rejected, network unreachable under `Net::Deny`, worker is the leader of a fresh session — sid == pid via setsid (issue #2), worker cannot `bootstrap_look_up` `com.apple.coreservices.appleevents` (issue #1) |
| `core` unit | 56 | `derive_lockdown_env` (4); watchdog loop honours cancel/deadline/early-exit (4); `is_valid_target_pid` rejects 0/1/u32::MAX/`i32::MAX+1` (1); workspace creates layout, drops wipes tree, `fs_write_paths` order, `extend_policy` appends, task-id validation, root auto-create, pre-existing dir refused (7). `audit_mirror::audit_log_path_for` zero-pads month/day + handles 4-digit year (2), `format_jsonl_line` ends with single \n + serialises every AuditRow field (2), `default_state_dir` resolves under `$HOME/.local/state/hhagent` (1). `audit_tail::parse_audit_filename` accepts canonical shape + rejects every off-shape (2), `find_audit_files` ascending + ignores non-matching + handles missing dirs (2), `tail_loop` from-start mode (1). **Option M (2):** `WorkerCommand::new` carries method+params verbatim; accepts `&str` and owned `String`. **Option N (12):** `reciprocal_rank_fusion` algorithm pins (7); `RecallModes` shape pins (4); `RRF_K_CONSTANT` pinned at exactly `60.0` (1). **Task 3.2.bis (13):** `rpc_code_name` mapping (2 — every known JSON-RPC code + unknown fallback to `RPC_ERROR`); `map_dispatch_result` Ok/POLICY_DENIED/unknown-RPC-code/non-Rpc Protocol/Io buckets (5); `ToolRegistry` empty/insert/lookup/replace (3); `shell_exec_entry` carries allowlist + invariants (Net::Deny, WorkerStrict, fs_read binary, empty fs_write) + empty-list = deny-all (2); `dispatch_step` unknown-tool branch (1). **Option O (3):** `build_embed_audit_payload` shape pins (3 — model/n_texts/dim/backend/latency_ms fields; omits input texts + output vectors + HTTP failure context) |
| `core` integration (`shell_exec_e2e`) | 4 | **cross-platform real** core → bwrap+landlock+seccomp (Linux) / sandbox-exec (macOS) → shell-exec round-trip — rewritten 2026-05-10 (Option M) to route every call through `tool_host::dispatch` since the `WorkerCommand` seal forecloses out-of-crate `worker.call(...)`. Each test brings up its own per-test PG cluster; `[SKIP]`s cleanly without PG / supervisor / sandbox / worker binary. Echo round-trip; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND; workspace e2e (cp from in/ to out/, host reads back, Drop wipes tree). Per-test PG cost: ~3 s × 4 = ~12 s |
| `core` integration (`memory_recall_e2e`) | 1 | **cross-platform real** Phase-1 entry. Per-test PG cluster, probe applies 0001+0002+0003+0004, runtime-role pool, seeds 3 memories with hermetic SHA-256-seeded 1024-dim L2-normalised embeddings (same text → distance 0; different texts → ~orthogonal). Asserts `semantic_search(emb_a)` ranks A first, `lexical_search("alpha")` returns only A, `recall(SEMANTIC_ONLY)`/`recall(LEXICAL_ONLY)`/`recall(ALL)` all return A as top-1, ALL also includes B+C below A (proves RRF fuses). ~1.9 s |
| `core` integration (`audit_dispatch_e2e`) | 1 | **cross-platform real** dispatcher chokepoint. Per-test PG cluster, probe, `pool::connect_runtime_pool` (auto SET ROLE), spawn shell-exec, exercise `tool_host::dispatch` twice: success (`echo dispatch-ok` → audit row payload `{req, result, ms}`); POLICY_DENIED (`/bin/cat /etc/passwd` → audit row payload `{req, err, ms}`). Final assertion: exactly 3 rows in `audit_log` (bring-up + 2 dispatches). Multi-thread tokio runtime mandatory (dispatch uses `block_in_place`) |
| `core` integration (`supervisor_e2e`) | 1 | **cross-platform real** end-to-end smoke. Brings up per-test PG cluster + `core_service_spec` for the freshly-built `hhagent` binary with `HHAGENT_DATA_DIR` + `HHAGENT_STATE_DIR` + `USER` injected. Install → start → wait Active → 500 ms stable-Active recheck → poll redirected stdout for `"database probe succeeded"` → `psql -d hhagent` asserts `audit_log` has at least one `(actor='core', action='startup')` row → poll per-test state dir for an `audit-YYYY-MM-DD.jsonl` containing the bring-up row within ≤ 5 s (proves audit-mirror task drained + fsynced) → stop → wait Inactive → uninstall |
| `db` unit | 71 | `build_initdb_argv` (8) + `build_postgresql_auto_conf` (7) + `find_pg_bin_dir` (3) + `is_data_dir_initialized` (2) + `require_absolute` / `default_data_dir` / `default_socket_dir` (5). C2.2: `conn::ConnectSpec` (9), `graph::{Entity, Relation}` field-shape pins (2), `probe::ensure_database_exists` SQL shape pin (1). **Option L (2):** `RUNTIME_ROLE`/`set_role_runtime_statement()` pins. **Option I (6):** `audit::truncate_payload` pass-through, boundary, oversize envelope, deterministic, distinct fingerprints. **Secrets at rest (18):** AES-GCM round-trip + tampering paths (5); fresh-nonce no determinism leak (1); `MAX_PLAINTEXT_LEN` (1); AAD shape pins (3); `validate_name` rejects (5) + accepts typical names (1); `MapKeyProvider` (2); constants pinned (1). **Option N (9):** `EMBEDDING_DIM = 1024` (1), `DEFAULT_RECALL_K ≥ 1` (1), `vector_literal` shape (4), `check_embedding_dim` rejects/accepts with call-site label (2), `limit_as_i64` saturates (1) |
| `db` integration (`postgres_e2e`) | 5 | `postgres_install_start_select_one_uninstall` (existing); `probe_runs_migrations_and_graph_happy_path` (C2.2 — probe idempotency + `PgGraph` upsert/get/neighbors/path); `runtime_role_audit_log_revoke_is_enforced` (Option L — `pg_roles` shape pins, INSERT ok, UPDATE/DELETE on `audit_log` denied, full CRUD on `memories` ok); `audit_helpers_pool_and_notify_round_trip` (Option I — pool's auto-SET-ROLE proven by UPDATE-denied negative path; `PgListener` on `audit_log_inserted` round-trip + `fetch_by_id` byte-for-byte + 8 KiB payload triggers `_truncated` envelope); `secrets_put_get_list_delete_round_trip` (secrets — 7 assertions: round-trip, list metadata-only, UPSERT, idempotent delete, AAD-mismatch on rename, GCM-auth-tag failure on tamper, 0004 CHECK constraint rejects empty AAD) |
| `llm-router` unit | 41 | `error::truncate_for_error` (3); `messages::ChatRole` lowercase + closed-enum (2), constructors (1), `skip_serializing_if` pin (1), `ChatResponse` decodes vLLM full-envelope + minimal Ollama (2); `Backend` serde + `as_tag()` round-trip (3); `config::default_local_url_for_os()` Linux/macOS (1), `DEFAULT_LOCAL_MODEL`/`DEFAULT_TIMEOUT_MS` (1), `RouterConfig::default()` (1) + `from_env` (5); **Option O additions (7):** `HHAGENT_LLM_EMBEDDING_URL` fallback + override semantics (2); `HHAGENT_LLM_EMBEDDING_MODEL` default (1); `EmbeddingRequest`/`EmbeddingData`/`EmbeddingResponse` wire shapes (2); `RouterError::EmbeddingCountMismatch` (1); `PolicyGate::pick_embed` default (1); `Router::pick_embed_backend` proxy delegation (1); `router_embed_rejects_frontier_choice_in_phase_0` frontier-rejection pin (1); `policy::DefaultLocalPolicy` always picks Local (1) + Send+Sync (1); `lib::compose_url` (2), `CHAT_COMPLETIONS_PATH` (1), `Router::new`/`pick_backend`/`send` (3 incl. `PolicyDeniedFrontier`) |
| `llm-router` integration (`local_backend_e2e`) | 4 | hand-rolled `tokio::net::TcpListener` mock (no `wiremock`/`httpmock` dev-dep). `happy_path_round_trips_request_and_response` proves `skip_serializing_if = Option::is_none` survives round-trip; `http_error_status_is_surfaced_with_truncated_body` → 500 with operator-readable body capped at 1 KiB; `decode_error_is_surfaced_when_response_is_not_chat_response` → 200 + bad JSON; `router_send_routes_to_pick_backend_choice` — `AlwaysFrontier` test policy → no HTTP request reaches the mock (defends chokepoint) |
| `llm-router` integration (`embedding_backend_e2e`) | 4 | **Option O (new file).** hand-rolled TCP mock, same style as `local_backend_e2e`. `embed_happy_path_round_trips_request_and_response` (full `EmbeddingRequest` → `EmbeddingResponse` shape + `skip_serializing_if`); `embed_http_error_status_is_surfaced` (500 → `RouterError::HttpStatus`); `embed_count_mismatch_is_rejected` (`EmbeddingCountMismatch` when response has fewer vectors than requested); `embed_rejects_frontier_choice_in_phase_0` (`AlwaysFrontierEmbed` stub → no mock hit, proves `pick_embed` chokepoint) |
| `prelude` unit | 11 | env-var parsing, profile parsing, BPF program builds (Strict + NetClient), unshare/mount/ptrace/bpf absent under both profiles, socket present *only* in NetClient, essential syscalls present in BASE_ALLOW |
| `prelude` integration (`landlock_smoke`) | 4 | write-to-non-allowlisted denied with EACCES; allowlisted scratch write works; `/usr` reads still work; v6 ABI yields `FullyEnforced` |
| `prelude` integration (`seccomp_smoke`) | 6 | `unshare(CLONE_NEWUSER)` and `mount(...)` killed with SIGSYS under both profiles; `socket(AF_INET, SOCK_STREAM)` killed under Strict, survives under NetClient; `getpid()` survives |
| `supervisor` unit (linux) | 44 | `build_unit_file` shape (14); `validate_service_name` (6); driver against custom units_dir (7); `specs::core_service_spec` (8); `specs::postgres_service_spec` (8); `canonical_service_names_are_distinct` (1) |
| `supervisor` unit (macos) | 52 | `build_plist` shape (14); `validate_service_name` (6); helpers (7); driver against custom agents_dir (8); `specs::*` (17 — same `specs.rs` runs on both OSes since no platform deps) |
| `supervisor` integration (`systemd_user_smoke`, linux) | 2 | `systemctl --user` round-trip with RAII guard; invalid name rejected before any systemctl call |
| `supervisor` integration (`launchd_agents_smoke`, macos) | 4 | `launchctl bootstrap gui/<uid>` round-trip; idempotent start/stop; invalid name rejected; serialised with static `Mutex` (GUI domain is shared global) |
| `core` integration (`scheduler_inner_loop_e2e`) | 4 | **cross-platform skip-as-pass** (no PG on macOS). Four scenarios against scripted stub router: happy path (Completed), tool-fail-then-recover (Completed), plan-iteration-cap exhausted (Failed), cancel mid-execution (Cancelled). Per-test PG cluster bring-up |
| `core` integration (`scheduler_lanes_e2e`) | 1 | **cross-platform skip-as-pass.** Concurrent fast+long lane claim with timing assertion; verifies lane-default lease constants |
| `core` integration (`scheduler_crash_recovery_e2e`) | 1 | **cross-platform skip-as-pass.** Back-dated lease → `sweep_crashed` marks task as crashed; daemon restart safety invariant |
| `core` integration (`agent_prompts_e2e`) | 1 | **cross-platform skip-as-pass.** `load_prompts_from_dir` writes SHA-256 into `agent_prompts` ledger; cache entry round-trip; both v1 and v2 of an edited prompt persist (append-only by GRANT, migration 0006) |
| `core` integration (`scheduler_step_dispatch_e2e`) | 1 | **cross-platform real** (skips on hosts without PG/supervisor/sandbox/worker). Task 3.2.bis regression pin. Per-test PG cluster + probe + runtime-role pool + `ToolRegistry` with shell-exec entry (ECHO_PATH allowlisted). Exercises `ToolHostStepDispatcher::dispatch_step` three ways: (1) happy path → `StepOutcome::Ok` with `exit_code=0` and `stdout="step-ok"`, (2) non-allowlisted argv → `StepOutcome::Err { code: "POLICY_DENIED" }`, (3) unknown tool (`web-fetch`) → `StepOutcome::Err { code: "UNKNOWN_TOOL" }` *without* writing an audit row. Final assertion: audit_log has exactly 3 rows (bring-up + ok + denied) — confirms UNKNOWN_TOOL short-circuits before the chokepoint, as designed |
| `core` integration (`cli_ask_e2e`) | 2 | **cross-platform real** (skips on hosts without PG/supervisor/sandbox/worker). Task 4.4 regression pin: the *full* prod chain (CLI subprocess → PG insert → scheduler claim → LLM call → CASSANDRA review → step dispatch → finalize → CLI exit) end-to-end against a queued multi-shot mock LLM. (1) Happy path: mock serves `[non-terminal echo-step plan, terminal text plan]`; CLI exits 0; stdout `= marker`; `tasks.state="completed"`, `plan_count=2`; audit multiset `{core/startup ×1, agent/plan.formulate ×2, cassandra:chain/verdict ×2, tool:shell-exec/shell.exec ×1, scheduler/plan.outcome ×1}`. (2) Plan-cap failure: mock serves 3× same non-terminal plan with `/bin/cat /etc/passwd` (not allowlisted); CLI exits 1, stderr contains `"failed"`; `tasks.state="failed"`, `plan_count=3`; 3× tool:shell-exec rows whose payload carries the JSON-RPC `-32001` POLICY_DENIED code in `err`. Per-test PG cluster + per-test mock LLM (FIFO Vec<String> queue, 503 once exhausted so overruns surface loudly). 5/5 deterministic runs on the DGX in ~5.4 s each |
| `core` integration (`embedding_recall_e2e`) | 4 | **Option O (new file).** cross-platform real (skips cleanly without PG). Per-test PG cluster + hand-rolled TCP mock for `/embeddings`. `embed_query_returns_vector_from_mock_backend` — round-trip through `embed_query`, dim validated, vector returned; `embed_query_writes_llm_router_audit_row` — confirms the audit_log row has `actor='llm:router' action='embed'` with the expected payload shape (model/n_texts/dim/backend/latency_ms; no input texts, no vectors); `embed_query_fails_on_dim_mismatch` — mock returns wrong dim → `MemoryError::EmbeddingDimMismatch`; `embed_query_then_recall_semantic_lane` — full compose: embed_query → recall(SEMANTIC_ONLY) → asserts seeded memory is rank-1. 5/5 deterministic local runs. |

**Build & test:**
```sh
source "$HOME/.cargo/env"
cargo build --workspace          # produces ./target/debug/hhagent + workers
cargo test --workspace           # all green
./target/debug/hhagent           # runs the (skeleton) core daemon, emits one JSON log line
```

**Required one-time host setup (Ubuntu 24.04+ only):** the AppArmor profile that lets `bwrap` create unprivileged user namespaces is already installed on the user's DGX Spark. Other Linux hosts may need `sudo scripts/linux/install-bwrap-apparmor-profile.sh`. macOS uses `sandbox-exec` (no setup needed).

---

## Recently completed (this session, 2026-05-12 — split `core/src/memory.rs` into submodules)

Branch: `refactor/split-core-memory` (off `main` at `d39023b`). Closes [issue #30](https://github.com/hherb/hhagent/issues/30).

**Why this slice now.** The Option O slice (shipped earlier today, merged via PR #29) grew `core/src/memory.rs` from 489 LOC to 602 LOC — 102 over the 500-LOC soft cap in CLAUDE.md. The file had two natural halves: pure retrieval (`recall` + `reciprocal_rank_fusion` + `RecallModes` + `RecallParams` + `RRF_K_CONSTANT` + `LANE_FANOUT`) which has zero dependencies beyond `hhagent-db`, and the LLM-router-touching helper (`embed_query` + `MemoryError` + `build_embed_audit_payload`) which depends on `hhagent-llm-router` + the audit module. Splitting them tightens the dependency surface of each file and keeps both well under the cap, with no behaviour change and no public-API change.

**Shape.** One module became three:

- `core/src/memory/mod.rs` (55 LOC) — facade. Module-level docstring describes the role; submodule decls (`mod recall; mod embed;`); flat re-exports preserve the external API: `pub use recall::{recall, reciprocal_rank_fusion, RecallModes, RecallParams, RRF_K_CONSTANT}; pub use embed::{embed_query, MemoryError};`.
- `core/src/memory/recall.rs` (384 LOC) — retrieval surface + RRF. Carries `recall` (async, runs configured lanes + fuses + hydrates), `reciprocal_rank_fusion` (pure), `RecallModes`/`RecallParams`/`RRF_K_CONSTANT`/`LANE_FANOUT`. Imports only from `hhagent_db::memories` + `hhagent_db::DbError` + `sqlx::PgPool` — no LLM-router dependency. All RRF + RecallModes unit tests (11 tests) live in `recall.rs::tests`.
- `core/src/memory/embed.rs` (219 LOC) — embedding query helper + audit row. Carries `embed_query` (async, validates dim, writes the `actor='llm:router' action='embed'` audit row), `MemoryError` enum, and the module-private `build_embed_audit_payload` (tightened from `pub(crate)` since no out-of-module caller exists). Three audit-payload-shape unit tests live in `embed.rs::tests`.

**API surface preserved.** The two integration tests that import the module (`core/tests/memory_recall_e2e.rs` and `core/tests/embedding_recall_e2e.rs`) needed zero changes — they use the flat `hhagent_core::memory::{recall, RecallModes, RecallParams, embed_query, MemoryError}` paths, which the `mod.rs` re-exports satisfy.

**Visibility tightening.** `build_embed_audit_payload` went from `pub(crate)` to module-private (no `pub` keyword at all). Pre-split, the rest of the `hhagent_core` crate *could* have called it; post-split, only `embed.rs` and its tests can. The audit + dispatcher chokepoint pattern in HANDOVER and CLAUDE.md says payload builders are internal helpers — the new visibility makes that contract structural rather than conventional.

**Test count delta:** 327 → 327 (no change). Workspace builds clean; `cargo test --workspace` is 0 failed, 0 `[SKIP]` lines.

**What this slice deliberately does NOT do.**
- No new functionality. Strict structural split.
- No public-surface change. `embed_query`, `recall`, `MemoryError`, `RecallModes`, `RecallParams`, `reciprocal_rank_fusion`, `RRF_K_CONSTANT` all reachable at the same `hhagent_core::memory::{...}` paths.
- No fix to the pre-existing dead-code warning in `core/tests/embedding_recall_e2e.rs` (introduced in PR #29, not this slice).

**Verification.** Per CLAUDE.md rule #6, all tests pass before commit: full `cargo test --workspace` is green at 327. Per rule #4, each new file is well under the 500-LOC soft cap (the largest is `recall.rs` at 384). Per rule #3, every new symbol carries a docstring explaining its role and the why-not-X (the module-level docs in `mod.rs`, `recall.rs`, and `embed.rs` each justify the split shape).

---

## Recently completed (previous session, 2026-05-12 — Option O: embedding router + first actor='llm:router' audit row)

Branch: `feat/embedding-router` (off `main` at `9fe45d6`, the plan commit; spec at `docs/superpowers/specs/2026-05-11-embedding-router-design.md`, plan at `docs/superpowers/plans/2026-05-11-embedding-router.md`). 7 implementation commits + 1 docs commit.

**Why this slice now.** `core::memory::recall` (Option N, 2026-05-10) ships three lanes but the semantic lane requires a pre-computed `query_embedding`. There was no production path that turned a free-text query into that embedding. Every test seeded vectors with a deterministic SHA-256-seeded helper. This slice closes the gap: callers compose `embed_query(pool, router, text)` then `recall(pool, &params)`, and the embedding HTTP call writes the system's first `actor='llm:router' action='embed'` audit row.

**Design decision (recorded in the spec).** HANDOVER's Option O brief mixed two designs (a new sandboxed `embedding-worker` crate vs a `Router::embed` method). The brainstorming pass chose `Router::embed` in core for symmetry with the existing `Router::send` precedent (`RouterAgent::formulate_plan` already makes HTTPS calls from core with no worker in front). A future "all net egress in sandboxed workers" Phase-3 slice would migrate both `send` and `embed` together; doing it for embed alone now would create an asymmetric oddity. Lower latency (no spawn-per-call), smaller surface area, and threat-model invariant preserved.

**Shape.** 5 modules touched, 2 new test files:
- `llm-router/src/embeddings.rs` (NEW) — `EmbeddingRequest`/`EmbeddingData`/`EmbeddingResponse` wire shapes
- `llm-router/src/lib.rs` — `Router::embed`, `Router::pick_embed_backend`, `EMBEDDINGS_PATH`, re-exports
- `llm-router/src/config.rs` — `embedding_url`/`embedding_model` fields + `HHAGENT_LLM_EMBEDDING_URL`/`HHAGENT_LLM_EMBEDDING_MODEL` env vars
- `llm-router/src/policy.rs` — `PolicyGate::pick_embed` default trait method
- `llm-router/src/error.rs` — `RouterError::EmbeddingCountMismatch`
- `core/src/memory.rs` — `MemoryError`, `embed_query`, `build_embed_audit_payload`
- `llm-router/tests/embedding_backend_e2e.rs` (NEW) — 4 router-layer integration tests vs hand-rolled TCP mock
- `core/tests/embedding_recall_e2e.rs` (NEW) — 4 e2e tests vs per-test PG cluster + TCP mock

**Audit row exact shape (the headline):** `{actor: "llm:router", action: "embed", payload: {model, n_texts, dim, backend: "local", latency_ms}}`. Deliberately omits the input texts (privacy), the output embeddings (size), and HTTP failure context (failures don't write the row — matches `Router::send` and `tool_host::dispatch` precedent). Pinned end-to-end by `core/tests/embedding_recall_e2e.rs::embed_query_writes_llm_router_audit_row`.

**Spec deviation accepted during implementation:** dropped `MemoryError::AuditSqlx(#[from] sqlx::Error)` because `DbError` already implements `From<sqlx::Error>`, which would cause a conflicting `From` impl via thiserror. `audit::insert` returns `Result<i64, DbError>` (not raw `sqlx::Error`), so `Db(#[from] DbError)` covers all audit-failure paths. The deviation makes the implementation strictly correct.

**Review-driven extra tests beyond the plan (+4):**
- Task 3 (config) — code-quality reviewer flagged that the fallback contract (LOCAL_URL drives EMBEDDING_URL when unset; EMBEDDING_URL wins when both set) was asserted only by code-reading. Added 2 fallback-semantic tests.
- Task 5 (Router::embed) — code-quality reviewer flagged the `Router::send` frontier-rejection pin (`router_send_rejects_frontier_choice_in_phase_0`) had no symmetric pin for embed, and `Router::pick_backend` had no symmetric proxy test for `pick_embed_backend`. Added 2 tests.

**What this slice deliberately does NOT do:**
- No new sandboxed worker (see design decision above)
- No change to `recall`'s signature (callers compose `embed_query` then `recall`; pure-function principle)
- No batch helper (`Vec<String>` wire support is there but the single-text helper is the only public path; a batch indexer is a Phase-1 cont. follow-up)
- No frontier embed support (Phase 5; `pick_embed` is the seam)
- No graph lane in `recall` (Option P — needs entity↔memory linkage)

**Test count delta:** 299 → **327** (+28; the plan projected +24, the +4 extras came from review feedback above). 0 failed, 0 warnings. 5/5 deterministic local runs of `embedding_recall_e2e`.

**Open follow-up surfaced by this slice:**
- `core/src/memory.rs` is now **585 LOC** (was 489), **85 LOC over the 500-LOC soft limit** in CLAUDE.md. Natural split: `recall` / `reciprocal_rank_fusion` / `RecallParams` / `RecallModes` / `RRF_K_CONSTANT` / `LANE_FANOUT` → `memory/recall.rs` (pure retrieval); `embed_query` / `MemoryError` / `build_embed_audit_payload` → `memory/embed.rs` (LLM-router + audit). Should be a separate cleanup slice.

**Commits (in order):** Task 1 `70c76e4`, Task 2 `111b949`, Task 3 `7c03d56`, Task 4 `c80bd11`, Task 5 `64c7b2d`, Task 6 `dca1604`, Task 7 `a1256cd`. Task 8 (this commit) follows.

---

## Recently completed (this session, 2026-05-11 — Task 4.4: `cli_ask_e2e` end-to-end integration test)

Branch: `main`, off `e6e282f`.

**Why this slice now.** Every existing integration test stubbed at least one moving part: `router_agent_mock_e2e` stubs the scheduler+dispatcher, `scheduler_step_dispatch_e2e` calls the dispatcher in-process without the LLM, `scheduler_inner_loop_e2e` scripts both the formulator and the dispatcher, and `supervisor_e2e` doesn't exercise `ask` at all. Nothing pinned the production chain end-to-end. Task 4.4 (HANDOVER's deferred-list item) closed that gap, unblocked yesterday by Task 3.2.bis wiring the real `ToolHostStepDispatcher`.

**Shape.** Single new file `core/tests/cli_ask_e2e.rs` (~840 LOC). Two `#[test]` functions, each owning its per-test PG cluster + per-test mock LLM. Design spec (committed earlier in `e6e282f`): [`docs/superpowers/specs/2026-05-11-cli-ask-e2e-design.md`](../../superpowers/specs/2026-05-11-cli-ask-e2e-design.md).

- **`ask_subprocess_completes_planned_task_end_to_end` (happy path):**
  * Per-test PG cluster + per-test mock LLM bound to ephemeral 127.0.0.1 port. Mock queue: `[plan A (non-terminal, one echo step), plan B (terminal, kind=text body=marker)]` wrapped in OpenAI-compatible chat-completion envelopes.
  * Bring up the real `hhagent` daemon under `systemd --user` (Linux) / `launchctl` (macOS) with env wiring: `HHAGENT_DATA_DIR`, `HHAGENT_STATE_DIR`, `HHAGENT_PROMPTS_DIR` → workspace `prompts/`, `HHAGENT_LLM_LOCAL_URL` → mock `/v1`, `HHAGENT_LLM_LOCAL_MODEL` → `test-local-model`, `HHAGENT_LLM_TIMEOUT_MS=5000`, `HHAGENT_SHELL_EXEC_BIN` → workspace `hhagent-worker-shell-exec`, `HHAGENT_SHELL_EXEC_ALLOWLIST` → `ECHO_PATH` (per-OS).
  * Wait for the daemon's `"scheduler spawned"` log line (signals scheduler ready to claim).
  * Spawn the real `hhagent-cli ask "say <marker>"` subprocess via `std::process::Command::output()`.
  * Assertions: CLI exits 0; stdout `.trim_end() == marker`; `tasks` row ends `state="completed"`, `plan_count=2`, `result.body=marker`; audit multiset matches the expected 6-event shape (1× core/startup, 2× agent/plan.formulate, 2× cassandra:chain/verdict, 1× tool:shell-exec/shell.exec, 1× scheduler/plan.outcome — `plan.outcome` fires only on non-terminal plans whose steps ran, so plan B doesn't add one); mock was dialed exactly 2×.

- **`ask_subprocess_fails_after_plan_iteration_cap` (failure path):**
  * Same bring-up, except the mock queue is 3× the same non-terminal plan with `/bin/cat /etc/passwd` as the argv (deliberately not in the allowlist).
  * The worker returns POLICY_DENIED on every step (`-32001` from the `argv[0] not in allowlist` check). Inner loop replans, hits `DEFAULT_MAX_PLANS_FAST = 3` from `db/src/tasks.rs:50` on what would have been iter 4, returns `Outcome::Failed("plan_iteration_cap_exceeded (3>=3)")`.
  * CLI's `ask_async` (`hhagent-cli.rs:319-322`) sees `state != "completed"`, prints `"ask: task ended in state 'failed'"` to stderr, and exits 1.
  * Assertions: CLI exits non-zero; stderr contains `"failed"`; `tasks.state="failed"`, `plan_count=3`; 3× `tool:shell-exec/shell.exec` rows whose payload carries `"-32001"` in the `err` string (the dispatcher chokepoint writes errors as a string, not a structured object — the rpc_code → mnemonic mapping happens one layer up in `ToolHostStepDispatcher`); audit multiset has `agent/plan.formulate ×3` + `scheduler/plan.outcome ×3`; mock was dialed exactly 3×.

**Queued multi-shot mock LLM (~110 LOC).** New helper inside the test file. Hand-rolled `tokio::net::TcpListener` mock matching `router_agent_mock_e2e.rs`'s style; no `wiremock`/`httpmock` dev-dep. Background tokio task loops `accept().await`, reads each request body (cap 1 MiB), captures it for later assertions, FIFO-pops from a `Vec<String>` queue under `std::sync::Mutex`, writes the canned 200-OK response, and shuts the socket. Once exhausted, every subsequent request gets a `503 Service Unavailable` — so an unexpected extra dial surfaces as `RouterError::HttpStatus` in the daemon log AND as a `tasks.state="failed"` row in the test's assertion. Loud, not silent. Mock's `Drop` aborts the accept task so the ephemeral port releases cleanly.

**What this slice deliberately does NOT do:**
- No constitutional-block coverage. CASSANDRA stages still stub-Approve in this phase (`ConstitutionalGuard` + `DeterministicPolicy` both return `Verdict::Approve`); real-stage paths get coverage in the observation-phase follow-up.
- No cancellation-mid-step test. Reliably planting a SIGINT during inner-loop step execution from a subprocess is timing-sensitive and would benefit from a `BarrierDispatcher`-style hook in the daemon (separate slice).
- No long-lane test. Both cases use `Lane::Fast`. `scheduler_lanes_e2e` already pins the lane abstraction.
- No `tests-common` refactor. Issue #15 already tracks the workspace-level hoist; this file is now the **seventh** duplication site for the per-test PG cluster bring-up. Each new e2e test that needs PG makes the issue more compelling.

**Five-runs determinism check.** `for i in 1 2 3 4 5; do cargo test -p hhagent-core --test cli_ask_e2e; done` passed clean: ~5.4 s per run, both tests green every time, zero `[SKIP]` lines.

**Test count delta:** 297 (post-`e524959` main) → **299** (+2 integration). 0 failed, 0 warnings.

**Files added/modified this session:**
- New: `core/tests/cli_ask_e2e.rs` (~840 LOC, 2 #[test]).
- No production-code changes. The CLI, daemon, scheduler, dispatcher, worker, sandbox, and mock LLM all worked end-to-end on the first build — the only test-iteration was a wrong audit-payload shape assertion (`err` is a JSON string with the JSON-RPC error text, not a structured object). Fixed inline before committing.

---

## Recently completed (previous session, 2026-05-11 — Task 3.2.bis: wire `ToolHostStepDispatcher` to `tool_host::dispatch`)

Branch: `feat/tool-host-step-dispatcher`, off `main` at `ea7556a`. **Merged to `main` via PR #28 at `db0197c`; follow-up `/review` nits in `e524959`** (see header summary).

**Why this slice now.** Phase 1 scheduler shipped without step execution (Task 3.2.bis was the last deferred item). The daemon would accept tasks via `hhagent-cli ask`, formulate plans via the LLM, run them through CASSANDRA review — and then every `PlannedStep` hit a `NOT_IMPLEMENTED` placeholder in `core::scheduler::runner::ToolHostStepDispatcher`. Operators got an audit-log `plan.outcome` row with `terminal_kind: "err"` and no information about *why*. This slice replaces the placeholder with a real spawn-per-step path through `tool_host::dispatch`.

**Shape:**

- **New module `core/src/scheduler/tool_dispatch.rs` (~330 lines + 13 unit tests):** ownership of the production dispatcher moved out of `runner.rs` into its own file. Contains:
  * `pub struct ToolEntry { binary, policy, wall_clock_ms }` — one row in the tool registry.
  * `pub struct ToolRegistry` — `HashMap<String, ToolEntry>` with `new`/`insert`/`lookup`/`is_empty`/`len`. The dispatcher takes an `Arc<ToolRegistry>` so the daemon owns the canonical instance and the inner loop sees a cheap clone.
  * `pub fn shell_exec_entry(binary, allowlist) -> ToolEntry` — canonical recipe for the shell-exec worker: `Net::Deny`, `Profile::WorkerStrict`, `cpu_ms = 5_000`, `mem_mb = 256`, `wall_clock_ms = Some(30_000)`, `HHAGENT_SHELL_ALLOWLIST` env carrying the argv allowlist.
  * `pub fn rpc_code_name(code: i32) -> &'static str` — pure mapping from JSON-RPC numeric codes (`-32001`, `-32601`, …) to the mnemonic strings the inner loop and audit consumers see (`"POLICY_DENIED"`, `"METHOD_NOT_FOUND"`, …). Unknown code → `"RPC_ERROR"`.
  * `pub fn map_dispatch_result(Result<Value, ToolHostError>) -> StepOutcome` — pure translation from the chokepoint's typed error surface to the inner loop's `StepOutcome::{Ok, Err{code, detail}}`. Five buckets: `Ok`, `Sandbox` → `SPAWN_FAILED`, `Io` → `IO_ERROR`, `Protocol(Rpc)` → named via `rpc_code_name`, `Protocol(non-Rpc)` → `PROTOCOL_ERROR`.
  * `pub struct ToolHostStepDispatcher { pool, sandbox, registry }` — `#[async_trait] impl StepDispatcher`. `dispatch_step`: lookup → spawn → call `tool_host::dispatch` → drop worker → `map_dispatch_result`. Unknown tools short-circuit before spawn (no audit row), surfaced loudly via `tracing::warn!`. Spawn failures surface as `SPAWN_FAILED` *without* an audit row — also a gap, flagged in the module doc comment.

- **`core/src/scheduler/runner.rs` slimmed down:** the placeholder `ToolHostStepDispatcher` removed. The unused `_workspace_root: PathBuf` parameter dropped from `spawn_scheduler` (it was only kept so the placeholder didn't break `main.rs` call sites — now obsolete). The `PathBuf` import also dropped. Net: ~50 lines deleted.

- **`core/src/main.rs` rewiring:**
  * New helper `build_tool_registry()` reads `HHAGENT_SHELL_EXEC_BIN` and `HHAGENT_SHELL_EXEC_ALLOWLIST` (colon-separated) from env. If `HHAGENT_SHELL_EXEC_BIN` is unset or the binary doesn't exist, shell-exec is simply *not registered* — plans that name it will fall through to `UNKNOWN_TOOL`. **Deny-by-default**: empty/unset `HHAGENT_SHELL_EXEC_ALLOWLIST` means no programs are allowlisted, every shell-exec step returns `POLICY_DENIED`. The daemon admin opts programs in explicitly. This is the same posture used in the Phase 3 egress proxy plan.
  * Workspace-root computation removed entirely. `Workspace::new` reads `HHAGENT_WORKSPACE_ROOT` directly, so the env seam still exists; nothing in the scheduler currently uses per-step workspaces. When a tool that needs writable scratch lands, the `Workspace` integration will go *inside* `dispatch_step` (or its trait sig will grow `task_id`).

- **`core/tests/scheduler_step_dispatch_e2e.rs` (~420 lines):** the regression pin for the wiring. Per-test PG cluster (sixth duplication site, issue #15 still open). Multi-thread tokio runtime mandatory (the chokepoint uses `block_in_place`). Three assertions:
  1. **Happy path** — `PlannedStep { tool: "shell-exec", method: "shell.exec", parameters: { argv: [ECHO_PATH, "step-ok"] } }` → `StepOutcome::Ok(value)` where `value["exit_code"] == 0` and `value["stdout"].trim_end() == "step-ok"`.
  2. **Worker policy denial** — `argv = ["/bin/cat", "/etc/passwd"]` (not allowlisted) → `StepOutcome::Err { code: "POLICY_DENIED", detail: non-empty }`.
  3. **Unknown tool** — `step.tool = "web-fetch"` → `StepOutcome::Err { code: "UNKNOWN_TOOL", detail: contains "web-fetch" }`.
  Final audit_log assertion: exactly 3 rows (bring-up + ok + denied — UNKNOWN_TOOL is *deliberately* not audited because the spawn never happened and the chokepoint was never reached). Cleanly skips on hosts without PG/supervisor/sandbox/worker binary.

- **`core/tests/scheduler_lanes_e2e.rs`:** updated to drop the `workspace_root` arg from the `spawn_scheduler` call (now redundant after the param removal).

**Why deny-by-default for shell-exec allowlist.** The planner LLM supplies `step.parameters` (the argv); if the host-side allowlist came from the LLM-supplied params, a prompt-injected channel would directly control which programs ran inside the jail — defeating the whole point of the allowlist. The allowlist must come from a source the LLM cannot influence: daemon-admin env vars. Empty allowlist + worker-side `POLICY_DENIED` is the safest starting position; operators opt programs in by setting `HHAGENT_SHELL_EXEC_ALLOWLIST=/usr/bin/echo:/bin/cat:...` at daemon start.

**What this slice deliberately does NOT do:**
- No per-step `Workspace` integration. Shell-exec doesn't need writable scratch for the canonical `echo` test case. When `python-exec` or any tool needing scratch lands, the trait sig grows a `task_id: i64` parameter (the inner loop already has it in `TaskContext.task_id`).
- No long-lived worker pooling. Spawn-per-step matches the existing "spawn-per-call" mode in `tool_host`; revisit when scheduler-latency profiling shows it's a bottleneck (HANDOVER §"Open questions" #5).
- No `actor='scheduler', action='task.<state>'` lifecycle audit rows from the scheduler. Spec §7 expected them; still deferred (see existing ROADMAP Phase 1 follow-up). The `tool:shell-exec` row from `tool_host::dispatch` is one row per *step*, not per *task*.
- No new audit row for `UNKNOWN_TOOL` or `SPAWN_FAILED`. Spawn-side failures never reach the chokepoint, so today they appear only in the daemon log. Flagged in the module doc — could be tightened in Phase 1 once the failure-shape contract is decided.

**Test count delta:** 284 (post-PR-#26-and-#27 main) → **297** (+13: 12 unit + 1 integration). 0 failed, 0 warnings.

**Post-merge follow-up (`e524959`).** A `/review` pass on the merged slice surfaced four small nits, all applied in one commit:
- The tautological `dispatch_step_unknown_tool_returns_unknown_tool_err` unit test constructed a `PlannedStep`, discarded it (`let _ = step;`), and asserted on a hand-rolled `expected` value — never invoked the dispatcher. Deleted; the unknown-tool branch is covered end-to-end by `scheduler_step_dispatch_e2e.rs`, and `tool_registry_starts_empty` pins the underlying registry-miss contract.
- `build_tool_registry` now filters empty entries out of the colon-split `HHAGENT_SHELL_EXEC_ALLOWLIST`. An operator typo like `:` or `/usr/bin/echo::/bin/echo` was silently shipping an empty argv[0] to the worker, surfacing as a less-obvious `POLICY_DENIED` at a different layer than the misconfiguration.
- Dropped the redundant `info!("tool registry built")` summary in `main.rs`. `build_tool_registry` already emits a per-tool `info!` line on registration.
- Narrowed the `scheduler::mod` re-exports to drop `map_dispatch_result` and `rpc_code_name` — internal helpers used only by `dispatch_step`. Public surface stays at `{shell_exec_entry, ToolEntry, ToolHostStepDispatcher, ToolRegistry}`.

Net change: 298 → 297 tests passing (the tautology); zero behavioural change.

---

## Recently completed (previous session, 2026-05-11 — post-merge follow-ups, mock HTTP tests, deadlock fix)

The Phase 1 scheduler work that was on `worktree-scheduler-phase1` has now landed on `main`. This session bundled three follow-up slices on top of that merge.

### Merge `worktree-scheduler-phase1` → `main` (commit `93da413`)

The scheduler-phase1 branch (commit range `71e144f`–`40d7719`, 15 commits + 3 doc commits) was merged via fast-forward equivalent (actually a merge commit). Everything described in the older "Recently completed (this session, 2026-05-11 — scheduler / CASSANDRA Phases 2–5)" section below is now in `main`. Detailed resume state is still in [`HANDOVER_CASSANDRA.md`](HANDOVER_CASSANDRA.md).

### Post-merge code review follow-ups (PR #25, merged at `ec007d7`)

Branch `fix/scheduler-phase1-followups`, commit `aff0621`. Two **real bugs** fixed and several reviewable nits cleaned up.

**Real bugs:**

- **Lane runner startup race** in `core::scheduler::runner::lane_loop`. The loop subscribed to `tasks_inserted` and then waited on the PgListener — but PG does *not* queue NOTIFY for late subscribers. A task inserted before LISTEN sat for one full HEARTBEAT (30 s) before being claimed. Fix: an initial drain after LISTEN, factored into `drain_lane`. Unblocks `two_lanes_run_concurrently` on fast hardware where insert-then-spawn-then-wait was hitting the gap.
- **`cancel_mid_execution_returns_cancelled` was timing-racy** on DGX-class hardware where iter 1 + iter 2 finish before the 150 ms sleep. Replaced with a `BarrierDispatcher` so the cancellation is planted while the step is provably mid-flight.

**Reviewable nits** (each in its own audit-grep-able comment):

- `hhagent-cli tasks list`: char-based truncation (was `&instr[..60]` — UTF-8 panic on multi-byte input); rejects unknown flags consistently with `run_ask`; replaced `std::process::exit(2)` with `ExitCode::from(2)` to keep the pool-drop path correct.
- `hhagent-cli tasks tail`: JSON-aware filter (was substring-matching `"task_id":N` which false-positives on `parent_task_id`). Pure `line_matches_task` helper with unit tests.
- `core::scheduler::runner`: `max_plans` payload override uses `try_into::<u32>()` so a producer-supplied 2^33 doesn't roll over.
- `core::scheduler::runner`: `ToolHostStepDispatcher` placeholder logs at `tracing::error!` before returning `NOT_IMPLEMENTED` — operators running `hhagent-cli ask` today get pointed at Task 3.2.bis from the journal.
- `core::scheduler::inner_loop`: dead `is_transient` helper removed (both arms returned `Outcome::Failed`); `tasks::increment_plan_count` errors now `tracing::warn!`; `Verdict::Escalate → Block` degradation emits a `tracing::warn!` and pinned `TODO(channel-bus)` for the Phase-2 follow-up.
- `core::scheduler::prompts::load_prompts_from_dir`: skips non-conforming filenames (vim swap files, dotfiles) with a warn rather than aborting daemon startup.
- `supervisor_e2e`: sets `HHAGENT_PROMPTS_DIR` pointing at the workspace `prompts/` so the daemon under systemd doesn't fail prompt-load on a `prompts/` cwd-relative miss.
- `prompts/agent_planner.md`: documents the JSON input shape the inner loop sends each iteration.

Five follow-up issues filed: [#20](https://github.com/hherb/hhagent/issues/20), [#21](https://github.com/hherb/hhagent/issues/21), [#22](https://github.com/hherb/hhagent/issues/22), [#23](https://github.com/hherb/hhagent/issues/23), [#24](https://github.com/hherb/hhagent/issues/24).

### Mock-HTTP coverage for `RouterAgent::formulate_plan` (PR #26, **OPEN — not yet merged**)

Branch `fix/router-agent-mock-http-tests`. Commits `2e2657c` (initial) + `44d42c3` (review nits). Closes [#22](https://github.com/hherb/hhagent/issues/22).

Before this PR, `core::scheduler::agent::RouterAgent::formulate_plan` — the only production path that turns a `TaskContext` into a `Plan` — was exercised only by the type system. Every scheduler test (`scheduler_inner_loop_e2e`, `scheduler_lanes_e2e`, `scheduler_crash_recovery_e2e`) swaps in a scripted `PlanFormulator`, so regressions in the JSON-decode path or the `FormulationMeta` field wiring would not have surfaced.

`core/tests/router_agent_mock_e2e.rs` (~367 lines) pins three cases against a hand-rolled tokio `TcpListener` mock (matching `llm-router/tests/local_backend_e2e.rs`'s style — no `wiremock`/`httpmock` dev-dep):

1. **`happy_path_decodes_plan_and_populates_meta`** — backend returns a valid Plan JSON envelope; `formulate_plan` returns `Ok((plan, meta))` with `plan.is_terminal() == true` and `FormulationMeta` carrying `prompt_name=agent_planner`, `prompt_sha256`, `llm_model`, `llm_backend="local"`. Also pins that the cached system prompt is sent verbatim on the wire.
2. **`decode_error_when_assistant_content_is_not_a_plan`** — backend returns a chat envelope whose content is plain text; the agent must surface `AgentError::Decode { detail, raw }` with the raw body preserved for triage. A silent default or panic here would corrupt the audit trail.
3. **`prompt_missing_short_circuits_before_dialing_backend`** — empty `PromptCache` → `AgentError::PromptMissing` without dialing the backend (witness: the mock's `served_rx` oneshot never fires).

Mock helpers (`spawn_one_shot_mock`, `find_double_crlf`, `header_content_length`) are duplicated from `local_backend_e2e.rs` rather than hoisted; issue #15 tracks the broader test-fixture refactor. No production-code changes, no new dependencies.

### `tasks_lifecycle_e2e` deadlock fix (this branch, commit `5d7a6ee`)

A `cargo test --workspace` run early this session hung for 33 minutes on `db::tests::postgres_e2e::tasks_lifecycle_e2e` — no output, all threads in `futex_do_wait`. The test had been added in `b125e46` (part of the scheduler-phase1 merge) and PR #25's pre-merge verification was `cargo test -p hhagent-core`, so this `hhagent-db`-integration test had never been observed running cleanly on this DGX.

**Root cause:** `PgListener::connect_with(&pool)` checks out a `PoolConnection` and *holds* it for the listener's lifetime (sqlx 0.8.6 source: stores it as `Some(connection)`, only releases on `Drop` or when an active `recv()` observes `Pool::close_event`). `pool.close().await` loops in `sqlx-core/src/pool/inner.rs::close()` acquiring all `max_connections` permits — which blocks until the listener-held connections are released. The two listeners in `tasks_lifecycle_e2e` were `let mut`-bindings in the test function, so they did not drop until end-of-scope — *after* the explicit `pool.close().await`. Deadlock.

**Why it's intermittent in practice:** the workspace run on `main` happened to pass `tasks_lifecycle_e2e` in 4.97 s, but three isolated focused runs reliably hung past 60–90 s before the fix. The multi-thread tokio runtime (`#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`) exposes it more reliably than the single-thread runtime used in the sibling `audit_helpers_pool_and_notify_round_trip` (which has the same structural pattern with one listener and has not been observed to hang).

**Fix:** explicit `drop(inserted_listener); drop(completed_listener);` before `pool.close().await`. PgListener's `Drop` impl spawns an async task that runs `UNLISTEN *` and `return_to_pool` (sqlx 0.8.6 line 357–373) — once both permits release, `pool.close()` proceeds. Verified by 3 consecutive focused runs (2 s each) and a full workspace run.

### Test-count delta (this session)

281 on this branch (was 267 in the previous handover snapshot). `+14` from the scheduler-phase1 merge and PR #25 / agent_prompts changes; PR #26 would add `+3` (the three `router_agent_mock_e2e` cases) when merged.

---

## Recently completed (previous session, 2026-05-11 — scheduler / CASSANDRA Phases 2–5)

All work on branch `worktree-scheduler-phase1` (worktree at `.claude/worktrees/scheduler-phase1`). Commit range `71e144f`–`40d7719` (15 commits + 3 doc commits). **Merged to `main` at `93da413` earlier today.** Detailed resume state in [`HANDOVER_CASSANDRA.md`](HANDOVER_CASSANDRA.md).

### What shipped

- **Migrations:** `0005_tasks_scheduler.sql` (lanes, lease, 3 NOTIFY triggers, GRANT shape with REVOKE DELETE), `0006_agent_prompts.sql` (append-only prompt ledger).
- **`db::tasks`:** Lane enum, lease constants, full CRUD: `insert_pending`, `claim_one` (FOR UPDATE SKIP LOCKED), `finalize`, `observe_state`, `mark_cancelled`, `mark_failed_running`, `sweep_crashed`, `increment_plan_count`, `get`, `list`. NOTIFY triggers on insert + state transitions.
- **`db::agent_prompts`:** `hash_content` (SHA-256 hex, 64 chars), `upsert_prompt` (idempotent on existing sha256), `get_by_hash`.
- **`core::cassandra::types`:** `DataClass` + `Severity` (with Ord/PartialOrd), `PlannedStep`, `Plan` (with `is_terminal()`, `skip_serializing_if` on `result`), `Verdict` (5-variant), `DECISION_TERMINAL` constant.
- **`core::cassandra::review`:** `ReviewStage` trait, `ChainReviewStage` (first-non-Approve short-circuit), `ConstitutionalGuard` + `DeterministicPolicy` + `NoopReviewStage` stubs (all return `Approve` — **deliberate**; observation phase before real rules). Stage names are audit-log contract (`"stage--1"`, `"stage-0"`, `"chain"`, `"noop"`).
- **`core::scheduler::prompts`:** `PromptCache`, `PromptEntry`, `load_prompts_from_dir` — reads `.md` files, SHA-256 hashes, upserts into `agent_prompts`, returns `Arc<PromptCache>`.
- **`core::scheduler::agent`:** `PlanFormulator` trait, `TaskContext`, `FormulationMeta`, `AgentError`.
- **`core::scheduler::inner_loop`:** `run_to_terminal`, `Outcome` (Completed/Failed/Cancelled), `StepDispatcher` trait, `StepOutcome`. Plan-iteration cap = 10.
- **`core::scheduler::runner`:** `LaneRunner` (per-lane PgListener-wake loop with `claim_one` → inner loop → finalize), `spawn_scheduler` (starts both lane runners under tokio tasks).
- **`core/src/main.rs` wiring:** `spawn_scheduler` called at daemon startup; crash sweep + prompt load + `ChainReviewStage`. **`ToolHostStepDispatcher` is a NOT_IMPLEMENTED placeholder** (returns `StepOutcome::Err` with code `NOT_IMPLEMENTED` for every step) — see deferrals below.
- **`hhagent-cli` subcommands:** `ask` (LISTEN-before-INSERT for completion, ctrl-C cancel), `tasks list`, `tasks status`, `tasks cancel`, `tasks fail`, `tasks tail`.
- **Integration tests (all skip-as-pass on macOS without PG):** `tasks_lifecycle_e2e` (db) + `scheduler_inner_loop_e2e` (4 scenarios) + `scheduler_lanes_e2e` + `scheduler_crash_recovery_e2e` + `agent_prompts_e2e`.

### Deferrals (explicit — not forgotten)

Two items from the original plan were deliberately deferred when Phase-1 scheduler shipped. **Both have since landed:**

1. ~~**Task 3.2.bis — `ToolHostStepDispatcher` wiring to `tool_host::dispatch`:**~~ **Shipped 2026-05-11** on branch `feat/tool-host-step-dispatcher`, merged via PR #28 at `db0197c` (post-merge `/review` follow-ups in `e524959`). See the Task 3.2.bis section earlier in this handover.
2. ~~**Task 4.4 — `cli_ask_e2e` integration test:**~~ **Shipped 2026-05-11 (this session)** on `main` — see the "Recently completed (this session)" section near the top of this handover.

### Test-count delta

249 → **267** (+18: 15 scheduler/db/cli tests + 3 doc/ROADMAP commits touched no test files).

## Recently completed (this session, 2026-05-10)

> **Note:** the 2026-05-10 working day landed seven slices in succession; before this prune they were each described in full detail. The pre-prune snapshot lives in [`archive/handover_20260510_pre-prune.md`](archive/handover_20260510_pre-prune.md) — read that for the full reasoning behind every decision below.

### Code-review follow-up to Options M + N (commit `52bc4ef`)

A `/review` pass on Options M+N surfaced four nits and two design discussions.

- **`db::memories::check_embedding_dim(label, v)` extracted** as a shared helper used by both `insert_memory` (label `"insert"`) and `semantic_search` (label `"query"`). Same change for `db::memories::limit_as_i64(k)` — saturates at `i64::MAX` rather than wrapping to negative.
- **`db::memories::fetch_by_ids` doc clarifies dedupe behaviour** — internal `HashMap::remove` returns `None` on later occurrences of duplicate ids; future arbitrary-id callers must not rely on `fetch_by_ids` to expand them.
- **`vector_literal` doc-comment correction** — `f32::Display` emits shortest round-trippable form (decimal for human-scale, scientific for very small/large); pgvector accepts both. Doc was overstating "standard decimal."
- **Two design discussions filed as GitHub issues:**
  * **Issue #16** — `tool_host`: `WorkerCommand` seal has an in-crate hole. Sibling modules inside `hhagent_core` can construct one and reach `SupervisedWorker::call` directly. Three candidate fixes filed.
  * **Issue #17** — `memory::recall`: warn-and-degrade on missing input may mask caller bugs. Three options on the issue (status quo / `RecallError::MissingInput` / hybrid).

Test count: 247 → **249** (-1 inline-mirror test, +3 real-helper tests).

### Phase 1 entry (Option N — `memory::recall` skeleton: pgvector + tsvector lanes fused via RRF, commit `48dfeee`)

Phase 1's scheduler asks "what does the agent already know that's relevant to this query?" and the answer goes through `core::memory::recall(pool, params)`.

- **`db/src/memories.rs` (~470 lines, 7 unit tests):** canonical chokepoint for every read/write of the `memories` table. Surface: `insert_memory`, `semantic_search`, `lexical_search`, `fetch_by_ids` (caller-order preserving). Constants: `EMBEDDING_DIM = 1024` (bge-m3 dim), `DEFAULT_RECALL_K = 10`. Pure helper `vector_literal(&[f32]) -> String` formats the canonical pgvector text representation; bound and cast in SQL via `$1::vector` so we avoid the `pgvector` Rust crate dep.
- **`core/src/memory.rs` (~420 lines, 12 unit tests):** the public recall surface. `RecallParams { query_text, query_embedding, k, modes }`. `RecallModes` selects which lanes to run (`ALL` / `SEMANTIC_ONLY` / `LEXICAL_ONLY`). `recall(pool, params)` runs each enabled lane (per-lane fanout `k * 4`), fuses via RRF, and hydrates the top-k bodies in one round-trip. Pure `reciprocal_rank_fusion(lists, k)` does the fusion: `score(d) = Σ_lanes 1 / (k + rank)` over 1-based positions, sorted descending with ties broken on smaller id. `RRF_K_CONSTANT = 60.0` matches Cormack/Clarke/Buettcher 2009.
- **`core/tests/memory_recall_e2e.rs` (~490 lines, 1 integration test):** per-test PG cluster, seeds 3 memories with hermetic SHA-256-seeded 1024-dim L2-normalised embeddings (same text → cosine 1.0; different → ~orthogonal). Five assertions across `semantic_search`, `lexical_search`, and `recall(SEMANTIC_ONLY/LEXICAL_ONLY/ALL)`. The `ALL` lane proves RRF *fuses* rather than intersects (A is rank-1 but B+C also appear).
- **What this slice deliberately does NOT do (and why):** no graph lane (schema has no entity↔memory linkage yet — filed as Option P); no `actor='llm:router'` audit row (embedding worker doesn't exist yet — filed as Option O); `recall` does not write to `audit_log` (reads aren't actions; the *consumer's* decision row is the canonical record).

Test count: 227 → 247 (+7 db unit + 12 core unit + 1 integration).

### Phase 1 entry (Option M — sealed `WorkerCommand` + `tool_host::dispatch` chokepoint compile-time pin, commit `3279c6d`)

The threat-model invariant says *every tool/channel/routine action enters core through `tool_host::dispatch()`*. Until this slice that was policy, not enforcement: any contributor with a `&mut SupervisedWorker` could call `worker.call(method, params)` directly and silently bypass the audit-log row.

- **`core/src/tool_host.rs::WorkerCommand` (new public type):** newtype `WorkerCommand { pub(crate) method: String, pub(crate) params: serde_json::Value }` with `pub(crate) fn new(method: impl Into<String>, params: serde_json::Value) -> Self`. The `pub(crate)` visibility means an out-of-crate caller — including each doctest harness — cannot construct one. `SupervisedWorker::call`'s signature changed from `(method: &str, params: serde_json::Value)` to `(cmd: WorkerCommand)`.
- **`compile_fail` doctest is the regression pin:** doc comment carries a `compile_fail` block invoking `WorkerCommand::new` from outside the crate. If a future refactor widens `new` to `pub`, the doctest fails with "compile_fail block compiled successfully."
- **Why the newtype seal and not `pub(crate)` rename of `call` itself:** keeping `call` public lets `core/tests/audit_dispatch_e2e.rs` hold a `&mut SupervisedWorker` and pass it to `dispatch(...)` — the intended architecture (long-lived workers; per-call dispatch rows). A `pub(crate) fn call` would have forced every test that holds a worker handle to also be in-crate, which integration tests cannot be.
- **`core/tests/shell_exec_e2e.rs` rewritten (302 → 640 lines):** the four sandbox-layer integration tests previously called `client.call(method, params)` directly. Post-seal, that no longer compiles. Each test now brings up its own per-test PG cluster (issue #15 has a 4th duplication site to hoist), runs the probe, opens `pool::connect_runtime_pool`, spawns the worker, and calls `dispatch(...)` instead. Per-test cluster cost: ~3 s × 4 = ~12 s acceptable for the chokepoint pin.

Test count: 224 → 227 (+2 unit + 1 doctest). The four migrated `shell_exec_e2e` tests are unchanged in count — the seal repointed them at `dispatch`, didn't add new tests.

### Phase 0 cont. (Option J — LLM router stub, commit before Option M)

The last application-layer plumbing required before Phase 1: every future model call goes through `hhagent_llm_router::Router::send(&ChatRequest) -> Result<ChatResponse, RouterError>`.

- **New top-level workspace crate `llm-router` (`hhagent-llm-router`, member #3):** ~960 lines + ~340 lines integration test, 32 tests (28 unit + 4 integration). The user explicitly chose the new-crate boundary (vs `core::llm_router`) because the router is a self-contained subsystem with a stable typed surface and the Phase-5 grow-out adds a real policy gate that will read state from `db::secrets`, emit telemetry, and gain its own integration test surface.
- **Modules:** `messages.rs` (OpenAI-compatible wire shapes; `ChatRole` is closed enum with serde lowercase; `skip_serializing_if = Option::is_none` on optional fields so older llama.cpp builds don't reject `null`); `backend.rs` (`Backend::{Local, Frontier}` closed enum with `as_tag()` for audit-log payloads); `config.rs` (`RouterConfig::from_env` reads `HHAGENT_LLM_*`; per-OS default URL — Linux vLLM/SGLang :8000, macOS Ollama :11434; **API keys NOT read from env** by design, they belong in `db::secrets`); `policy.rs` (`PolicyGate` trait + `DefaultLocalPolicy`); `error.rs` (truncated body capture at 1 KiB); `lib.rs` (`Router::new` + `Router::with_policy`; `Router::send` calls `policy.pick(&request)` then dispatches or returns `PolicyDeniedFrontier`).
- **Integration tests:** hand-rolled `tokio::net::TcpListener` mock (no `wiremock`/`httpmock` dev-dep). Four tests including `router_send_routes_to_pick_backend_choice` which uses an `AlwaysFrontier` test policy and asserts no HTTP request reaches the mock — defends the chokepoint against a future refactor that bypasses `policy.pick`.
- **New deps (workspace):** `reqwest` with `default-features = false, features = ["rustls-tls", "json"]`. Pure-Rust TLS, no `libssl-dev` system-package dep at build time.
- **Why we did NOT integrate `Router::send` into `tool_host::dispatch` in this slice:** wiring the dispatcher to fire an `actor='llm:router'` audit row is a Phase-1 step that requires a concrete first consumer (memory recall is the most likely candidate) to validate the integration shape. Filed as Option O.

Test count: 192 → 224 (+28 unit + 4 integration).

### Phase 0 cont. (secrets at rest — AES-256-GCM + OS-keyring wrapping key + `db::secrets` runtime + 0004 migration)

Plaintext for an API token, IMAP password, or signing key now lives only in agent-process memory and inside the OS keyring; the Postgres row carries AES-256-GCM ciphertext + 12-byte nonce + AAD-bound row identity + a `key_id` pointer back to the keyring entry.

- **`db/src/secrets.rs` (~520 lines, 18 unit tests):** pure crypto helpers (`encrypt`, `decrypt`, `compute_aad`, `validate_name`) decoupled from any I/O. AAD layout: `b"hhagent-secrets-v1" || 0x00 || name.as_bytes() || 0x00 || optional_extra` — domain-separated, NUL-delimited, name-bound. Gives row-rename detection: `UPDATE secrets SET name = …` leaves the stored AAD pointing at the old name, so `get` either fails the prefix-match check (`AadMismatch`) or, if an attacker UPDATEs the AAD column too, fails the GCM auth tag (`DecryptFailed`) because the tag was computed under the original AAD. Public secret-getter returns `Zeroizing<Vec<u8>>` so a panic-unwind cannot leave plaintext on the stack. Soft caps: `MAX_NAME_LEN = 256`, `MAX_PLAINTEXT_LEN = 64 KiB`.
- **`KeyProvider` trait + two impls:** `MapKeyProvider` is the test seam; `OsKeyringProvider::ensure_initialized()` opens the `(hhagent, secrets-v1)` entry on first use (generates 32-byte random key if absent). Cached `key_bytes` means the keyring lookup happens once at startup.
- **Async DB I/O (~150 lines):** `put`, `get`, `list`, `delete` all generic over `sqlx::Executor`. `put` UPSERTs by name. `get` does a recompute-then-compare on AAD before passing to GCM, catching the swap case as `AadMismatch` distinctly from `DecryptFailed`. `list` selects only metadata columns — debug-dump leaks nothing cryptographic. `delete` is idempotent.
- **`db/migrations/0004_secrets_aad_nonempty.sql`:** drops the provisional `aad BYTEA NOT NULL DEFAULT ''::bytea` and adds `CHECK (octet_length(aad) > 0)`. Closes [#12](https://github.com/hherb/hhagent/issues/12). Belt-and-braces — the application layer is structurally incapable of producing an empty AAD, but the DB-layer CHECK catches a rogue `INSERT INTO secrets …` that bypassed `db::secrets::put`.
- **New deps (workspace):** `aes-gcm 0.10` (pure-Rust RustCrypto AEAD; `zeroize` feature wires key state to wipe on drop), `zeroize 1`. **Per-target keyring deps:** Linux uses `keyring 3` with `async-secret-service` + `crypto-rust` features (pure-Rust D-Bus via `zbus`, no `libdbus-1-dev` system-package requirement); macOS uses `apple-native` (Security.framework). All Apache-2.0/MIT.

Test count: 172 → 191 (+18 unit + 1 integration).

### Phase 0 cont. (Option I — dispatcher chokepoint + audit_log NOTIFY trigger + JSONL mirror + `hhagent-cli audit tail`)

Every Phase 0+ tool call now goes through a single `tool_host::dispatch` chokepoint that writes one `audit_log` row per call. A long-lived `audit_mirror` task replicates committed rows to `~/.local/state/hhagent/audit-YYYY-MM-DD.jsonl` with fsync per write and daily UTC rotation; `hhagent-cli audit tail` reads those files with no DB connection.

- **`db/migrations/0003_audit_log_notify.sql`:** AFTER INSERT trigger calls `pg_notify('audit_log_inserted', NEW.id::text)`. Per-row trigger (Phase 0 throughput is one INSERT per tool call). Payload = `id::text` not full row (Postgres caps NOTIFY payloads at 8000 bytes; the listener is in-process so the extra SELECT is a sub-ms UDS round-trip).
- **`db/src/audit.rs` (~280 lines, 6 unit tests):** `truncate_payload(value)` is the pure 4 KiB cap — oversize JSON replaced with `{"_truncated": true, "sha256": "<64 hex>", "len": <bytes>}`. SHA-256 via new workspace dep `sha2 0.10`. Async I/O: `insert(executor, actor, action, payload) -> i64`, `fetch_by_id`, `fetch_since`. Generic over `sqlx::Executor`.
- **`db/src/pool.rs` (~110 lines):** `connect_runtime_pool(spec)` opens a `PgPool` with `PgPoolOptions::after_connect` running `set_role_runtime_statement()` on every dialed connection. Closes [issue #11](https://github.com/hherb/hhagent/issues/11) ahead of schedule. Defaults: `max_connections = 4`, `acquire_timeout = 10 s`, `idle_timeout = 5 min`.
- **`core/src/tool_host.rs::dispatch`:** the new chokepoint. Snapshots `params` for the audit row, wraps the synchronous `Client::call` in `tokio::task::block_in_place`, measures elapsed ms, then **best-effort** writes one row (failures `tracing::error!` but do not mask the worker's actual result — silently turning success into error because we couldn't log would be a strictly worse failure mode). Phase 1 may flip this once the scheduler has a concrete contract for audit-row durability.
- **`core/src/audit_mirror.rs` (~370 lines, 5 unit tests):** `spawn_mirror(pool, state_dir)` opens a `PgListener` on its own dedicated connection, does an initial `fetch_since(0)` drain, then enters a `tokio::select!` racing NOTIFY arrivals + 5 s catch-up timer + cancellation watch. Daily UTC rotation keyed on `row.ts.date()`. Every line is followed by `File::sync_all`. NOTIFY drops are tolerated because the catch-up SELECT is the canonical fetch path.
- **`core/src/audit_tail.rs` (~190 lines, 5 unit tests):** `tail -f`-style follower. Pure helpers `parse_audit_filename` + `find_audit_files`. Async `tail_loop(cfg, writer)` supports `from_start` (replay) and live (anchor at end). Polls every 250 ms. Date roll-over flushes the previous file's tail before switching.
- **`core/src/bin/hhagent-cli.rs` (~140 lines):** new operator CLI binary. Today: `hhagent-cli audit tail [--from-start] [--no-follow] [--state-dir PATH]`. Hand-rolled argv (no `clap` dep). State-dir resolution: `--state-dir` → `$HHAGENT_STATE_DIR` → `$HOME/.local/state/hhagent`.
- **`core/src/main.rs` rewrite:** after `probe::run`, daemon now calls `connect_runtime_pool` (fail-closed) and `spawn_mirror` (best-effort). On SIGTERM/SIGINT, shuts down mirror *before* closing the pool so the mirror's final `sync_all` observes an alive pool. New env-var seam `HHAGENT_STATE_DIR` (parallel to `HHAGENT_DATA_DIR`).

Test count: 154 → 172 (+18 across db unit, db integration, core unit, core integration; supervisor_e2e gained an audit-mirror assertion).

### Phase 0 cont. (Option L — non-superuser runtime role + audit-log GRANT split, earlier 2026-05-10)

The audit_log table picked up its long-promised `REVOKE UPDATE, DELETE` guarantee, and the daemon now drops privileges before every application-level write.

- **`db/migrations/0002_runtime_role.sql` (~140 lines):** creates `hhagent_runtime` with `NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOINHERIT`, grants the OS user membership via `EXECUTE format('GRANT hhagent_runtime TO %I', current_user)`, then carves the GRANT/REVOKE shape: `GRANT SELECT, INSERT ON audit_log` paired with `REVOKE UPDATE, DELETE, TRUNCATE`. Other five tables get bulk `GRANT SELECT, INSERT, UPDATE, DELETE`. Sequences get explicit `GRANT USAGE`. `ALTER DEFAULT PRIVILEGES` covers future migrations' tables. `CREATE ROLE` wrapped in `DO $$ IF NOT EXISTS … END $$` (Postgres has no `CREATE ROLE IF NOT EXISTS`).
- **`db/src/conn.rs` additions:** `pub const RUNTIME_ROLE: &str = "hhagent_runtime"` and `pub fn set_role_runtime_statement() -> String` returning `SET ROLE "hhagent_runtime"` (identifier-quoted via existing `quote_ident`).
- **`db/src/probe.rs` change:** between `MIGRATOR.run` and the `audit_log` INSERT, the probe executes `set_role_runtime_statement()` on the same connection. Module docstring updated (5 → 6 steps).
- **`db/tests/postgres_e2e.rs::runtime_role_audit_log_revoke_is_enforced`:** full bring-up + role-shape pin + membership pin + negative path (UPDATE/DELETE on audit_log denied) + positive path (full CRUD on memories ok) + final invariant (audit_log row count exactly 2).
- **Why `SET ROLE` instead of `pg_ident.conf` mapping:** SET ROLE is pure SQL and lives entirely in a sqlx migration; runtime role's privileges are bounded by the GRANTs regardless of how the role was entered, so threat-model story is identical. Cost (one extra SET ROLE round-trip per connection) is invisible against a UDS round-trip we'd be paying anyway.
- **Why probe migrations as superuser, application writes as runtime:** `MIGRATOR.run` includes `CREATE EXTENSION` (superuser-only) and `CREATE ROLE` (superuser-only). Connecting as runtime for *migrations* would deadlock the schema. Clean split: bootstrap identity (= OS user under peer auth) for migrations, runtime role for everything afterwards.
- **Why we did not split per-worker roles yet:** today there's exactly one application path — the daemon's audit_log INSERT — making per-worker split premature. Per-worker carving belongs in the migration that introduces the first worker that needs *less* than full CRUD (likely the embedding worker).

Test count: 151 → 154 (+2 db unit + 1 db integration).

---

## Recently completed (previous session, 2026-05-09)

### Phase 0 cont. (Option C2.2 — schema + sqlx migrations + Graph trait + core probe + e2e)

The C2 foundation (private per-user PG cluster on a UDS) gained a schema, a migration runner integrated into the daemon's startup, a typed graph abstraction, and a single fail-closed probe path: connect → ensure DB → migrate → emit a bring-up `audit_log` row.

- **`db/migrations/0001_init.sql` (~150 lines):** six tables + `vector` extension. `audit_log` (append-only landing zone for the dispatcher chokepoint, monotonic `id BIGSERIAL`, `(actor, ts)` index — the `REVOKE UPDATE, DELETE` shipped in Option L), `tasks` (scheduler queue, state machine via CHECK constraint not ENUM), `memories` (recall corpus; `embedding vector(1024)` bge-m3 dim; HNSW deferred to Phase 1's first batch ingest), `entities`/`relations` (graph; `UNIQUE (kind, name)` natural key; `ON DELETE CASCADE`), `secrets` (column shape pin for AES-256-GCM ciphertext + nonce + AAD + key_id; runtime shipped later this session).
- **`db/src/conn.rs` (~240 lines, 9 unit tests):** `ConnectSpec::default_for(&data_dir)` reads `$USER` for peer-auth identity, fails closed with `EnvVarMissing("USER")` when `$USER` is unset/empty. `for_maintenance_db()` swaps the DB field for the brief CREATE-DATABASE roundtrip. `quote_ident` is the canonical defense for future DDL.
- **`db/src/probe.rs` (~150 lines):** `probe::run` is the single entry point: connect to maintenance DB → check `pg_database` → CREATE DATABASE if absent → reconnect → `MIGRATOR.run(&mut conn)` → INSERT into `audit_log`. Fail-closed via `?` propagation. `ensure_database_exists` split out as pub helper for isolation testing.
- **`db/src/graph.rs` (~340 lines):** `Graph` trait + `PgGraph` impl. Async-fn-in-trait (Rust 1.75+) directly rather than `async-trait` to avoid `Box<Pin<…>>` allocations. `upsert_entity` (`ON CONFLICT (kind, name) DO UPDATE` so re-upsert is id-stable), `upsert_relation` (multi-edges allowed), `get_entity`, `neighbors` (filtered + unfiltered SQL paths), `path` (recursive CTE with visited-set, `ORDER BY depth ASC LIMIT 1`).
- **`MIGRATOR` static:** `sqlx::migrate!("./migrations")` embeds at compile time (no source tree on disk for binary install). sqlx tracks applied migrations in `_sqlx_migrations`.
- **`core::main::bring_up_database`:** wired into `main.rs` before `wait_for_shutdown`. Reads `HHAGENT_DATA_DIR` env (test override; production uses `default_data_dir()`), constructs `ConnectSpec` from `$USER`, calls `probe::run` with `actor="core" action="startup"`.
- **sqlx feature picks:** `runtime-tokio` (no TLS — UDS only), `postgres`, `migrate`, `macros`, `json`, `time`. Specifically *not* enabled: `query!`/`query_as!` (compile-time SQL validation requires `DATABASE_URL` at build, would tie CI to a running cluster).
- **`core/tests/supervisor_e2e.rs` rewrite:** test renamed to `core_starts_runs_db_probe_writes_audit_row_and_shuts_down_cleanly`. Brings up a per-test PG cluster before installing the `hhagent` core service. Forwards `HHAGENT_DATA_DIR` and `USER` via `spec.env`.
- **`db/tests/postgres_e2e.rs` extension:** `probe_runs_migrations_and_graph_happy_path` exercises probe idempotency + the `Graph` trait happy path against a real cluster.

**Why the probe lives in `hhagent-db` rather than `hhagent-core`:** the probe's logic (connect → ensure DB → migrate → audit row) is pure database orchestration with zero `core`-specific shape. Future memory worker (Phase 1) can call the same function for its own bring-up without dragging core in.

**Why peer auth, role = OS user, application DB = `hhagent`:** smallest containment story. Peer auth on a UDS → remote auth structurally impossible. Role = OS user → different OS users on the same host literally cannot connect. Application DB = `hhagent` keeps `postgres`/`template0`/`template1` for maintenance.

**Why `sqlx` over `refinery` and over a hand-rolled runner:** Phase 1 will need `sqlx::query` for memory recall regardless, so adding sqlx now and piggybacking the migration runner on the same crate is one tool instead of two.

**Pre-existing Linux build break, fixed inline:** `sandbox/tests/fixtures/mach_probe.rs` (added 2026-05-07 for issue #1) used `extern { static bootstrap_port; fn bootstrap_look_up; }` — both libSystem-only. `cargo build --workspace` failed on Linux at the linker stage. Fix gates the body with `#[cfg(target_os = "macos")]` and provides a non-macOS stub `fn main()`.

Test count: 138 → 151. Post-review follow-ups (same session): `graph::path` collapsed to a single SQL statement (closed a tiny race between two-query path-then-expand under concurrent DELETE), `graph::decode_entity` helper de-duplicated, `db::env_lock` for unit tests that mutate `$USER`/`$HOME`, `probe::run` close-error logging. Filed parking issues [#11](https://github.com/hherb/hhagent/issues/11), [#12](https://github.com/hherb/hhagent/issues/12), [#13](https://github.com/hherb/hhagent/issues/13), [#14](https://github.com/hherb/hhagent/issues/14).

### Other 2026-05-09 work (in summary)

- **Option C2 (Postgres bring-up, foundation slice):** `scripts/linux/install-postgres.sh` (idempotent PGDG setup; disables auto-created system-wide `postgresql@18-main.service`). New `hhagent-db` crate with pure helpers (`build_initdb_argv`, `build_postgresql_auto_conf`, `find_pg_bin_dir`) and `hhagent-db-init` bin. New `supervisor::specs::postgres_service_spec`. New `db/tests/postgres_e2e.rs::postgres_install_start_select_one_uninstall` (full real-world UDS round-trip). Both extension-deferral issues dropped won't-fix ([#9](https://github.com/hherb/hhagent/issues/9) Apache AGE, [#10](https://github.com/hherb/hhagent/issues/10) ParadeDB pg_search). Test count: 105 → 138.
- **Option H (long-running daemon + `keep_alive=true`):** `core/src/main.rs` rewrite — `wait_for_shutdown()` blocks on `tokio::signal::unix::signal(SignalKind::terminate())` and `SignalKind::interrupt()` in `tokio::select!`. `supervisor/src/specs.rs::core_service_spec` flipped `keep_alive` `false` → `true`. `core/tests/supervisor_e2e.rs` contract upgrade: install → assert Inactive → start → wait Active → 500 ms stable-Active recheck → stop → wait Inactive ≤ 5 s → uninstall. Closes [#7](https://github.com/hherb/hhagent/issues/7). Test count: 105 → 105.
- **Option C4 (wire core into the supervisor):** New `supervisor/src/specs.rs` with pure `core_service_spec(binary, log_dir) -> ServiceSpec`. New `supervisor::default_probe()` cross-OS probe. New `core/tests/supervisor_e2e.rs` (~190 lines, 1 test). Test count: 96 → 105.
- **macOS Seatbelt hardening (issues #1 + #2):** `setpgid(0,0)` → `setsid()` via `pre_exec` hook (worker is now session leader, no controlling terminal — `/dev/tty` opens fail with `ENXIO` regardless of profile). Empirical finding: none of our shipping workers need `(allow mach-lookup)` on macOS 26.4 ARM64; rule removed from `build_profile`. New tests `worker_runs_in_its_own_session` (`sid == pid`) and `worker_cannot_look_up_arbitrary_mach_services` (uses Apple Events broker `com.apple.coreservices.appleevents` as canary).

---

## Earlier history (summary)

Full reasoning for these slices lives in [`archive/handover_20260510_pre-prune.md`](archive/handover_20260510_pre-prune.md).

- **2026-05-08 — Linux supervisor scaffold (`hhagent-supervisor::systemd_user`):** pure `build_unit_file(spec)` + `validate_service_name`, `SystemdUser` driver with atomic write (write-to-tmp + fsync + rename), `daemon-reload` only for canonical dir, `probe()` via `systemctl --user show-environment`. 27 unit + 2 smoke tests. Test count 67 → 96.
- **2026-05-08 — macOS LaunchAgent supervisor backend:** pure `build_plist(spec)` + `validate_service_name` (same character class as Linux for portability), `LaunchAgents` driver, idempotent `start` via status-first check (not error-string parsing — Apple's launchctl error messages are version-unstable), serial mutex around tests because GUI launchd domain is a shared global. 35 unit + 4 smoke tests. Test count 96 → 83 on macOS (full delta visible only on macOS).
- **2026-05-08 — Phase 0 polish:** per-task scratch workspace `core::workspace::Workspace` with RAII cleanup; wall-clock watchdog `SupervisedWorker` with injectable `kill: fn(u32)` for tests + the **`kill(-1)` fanout fix** (`u32::MAX as i32 == -1` had been signalling every process the user could signal — explained the long-standing "DGX display blackout" attributed to NVIDIA driver; was actually us); workspace+worker e2e in `core/tests/shell_exec_e2e.rs`. Three new syscalls in `BASE_ALLOW` for `cp` (`copy_file_range`, `sendfile`, `fadvise64`).
- **2026-05-09 — cgroup v2 caps:** new `sandbox/src/linux_cgroup.rs` wraps every bwrap invocation in `systemd-run --user --scope --quiet --collect -p MemoryMax=Nm -p MemorySwapMax=0 -p CPUQuota=200% -p TasksMax=64 -- bwrap ...`. Discovered `MemorySwapMax=0` is mandatory: without it the kernel pages overruns to swap rather than killing the cgroup. New `cgroup_probe()` tightens `LinuxBwrap::probe()` to fail-closed when *any* containment layer is missing. New `mem_burner` fixture + OOM-kill test. Test count 56 → 67.
- **2026-05-08 — Phase 0 hardening stage 2 (Linux):** seccomp deny-list → per-profile allow-list (`BASE_ALLOW` ~110 syscalls common to x86_64+aarch64; `Profile::Strict` vs `Profile::NetClient` separation; default action `KillProcess`; catastrophic syscalls killed by *not* being in the list). Landlock ABI v1 → v6 (Refer/Truncate/IoctlDev/Scope rights). `add_path_rule` bug fix: `stat`s the path and intersects with `AccessFs::from_file(V6)` for files (kernel rejects directory-only rights on file PathBeneath rules; the crate silently strips, downgrading to `PartiallyEnforced`). Test count after: 43 on Linux.
- **2026-05-07 — Phase 0b macOS Seatbelt sandbox:** new `sandbox/src/macos_seatbelt.rs` with pure `build_profile(policy)` returning a TinyScheme `.sb` profile, `MacosSeatbelt::probe()`, `spawn_under_policy()` with absolute-path validation, path canonicalization (`/etc/...` → `/private/etc/...`), `env_clear()` + per-policy env, `process_group(0)`. 11 unit + 8 smoke tests. Two empirical broadenings vs the design doc: needed `(allow file-read* (literal "/"))` and `(allow mach-lookup)` to launch real binaries on macOS 26.4 ARM64 (the latter was tightened back out 2026-05-09 as issue #1). `default_backend()` returns `MacosSeatbelt` on `cfg(target_os = "macos")`. `core/tests/shell_exec_e2e.rs` made cross-platform.
- **2026-05-06 — Phase 0 hardening stage 1:** new `workers/prelude` crate (Linux-only Landlock + seccomp lock_down with `serve_stdio` drop-in around `hhagent_protocol::server::serve_stdio`). `core::tool_host::derive_lockdown_env()` injects `HHAGENT_LANDLOCK_RW` + `HHAGENT_SECCOMP_PROFILE`. **bwrap probe bug fix:** `LinuxBwrap::probe()` was launching `bwrap` without the `/lib*` symlinks so `execvp /usr/bin/true` returned ENOENT → probe failed-closed → integration tests `[SKIP]`'d silently → previous handover's "0 skipped" was wrong.
- **Earlier scaffold:** initial workspace + AGPL-3.0 (`140eec5`); Linux bwrap backend with AppArmor probe (`eae3df4`); protocol crate + shell-exec worker + tool_host + first e2e (`f2411ec`); roadmap and handover convention created; convergent prior art studied (ZeroClaw, IronClaw — see Inspirations section below).

---

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** AGPL project; all third-party deps must be AGPL-compatible (Apache-2.0, MIT, BSD, MPL, LGPL, (A)GPL all fine).
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS (Apple Silicon and Intel). No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python workers.** Rust for core (no eval/dynamic surface); Python only inside sandboxed tool workers. shell-exec is Rust because it's a thin execve wrapper — Python's first appearance will be `python-exec` in Phase 4 (or possibly `web-fetch` earlier).
- **Hybrid LLM with policy routing.** Local-first via OpenAI-compatible HTTP (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS). Frontier (Claude/OpenAI) only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisor.** `systemd --user` (Linux) / `launchd` LaunchAgents (macOS). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Critical workers are human-curated and shipped with the binary. Agent-authored code only runs inside `python-exec`'s strict sandbox; named/persisted skills get an optional human-approve gate (Phase 4).
- **JSON-RPC 2.0 over stdio.** MCP-stdio compatible. Lets us swap in a richer MCP client later without changing the trust boundary.

## Next TODO (pick one)

**Phase 0 is complete. Phase 1 — memory recall + the scheduler loop, including end-to-end step dispatch — is on `main`, and the production chain is now pinned end-to-end by `cli_ask_e2e` (shipped this session, Task 4.4).** The agent-core daemon comes up fail-closed, runs crash recovery, loads prompts, builds a tool registry from env vars, starts two lane runners, accepts tasks via `hhagent-cli ask`, executes shell-exec steps under sandbox, finalises the task, and the CLI prints the result — every layer verified by either `scheduler_step_dispatch_e2e` (dispatcher-only), `supervisor_e2e` (daemon bring-up), or `cli_ask_e2e` (the whole chain).

**Immediate next pickups, in priority order:**

- **Observation phase** (spec §9) — now that real CLI-driven plans execute, the scheduler can be driven with real tasks to collect failure modes before designing the real `ConstitutionalGuard` + `DeterministicPolicy` rules. Do not skip this phase or the real stage rules will be guesses. Practical step: build a small fixture set of "real-ish" instructions (5–10 prompts spanning safe + edge-case + clearly-blockable), run them through the CLI, dump the audit log, look at which plans CASSANDRA *would* have wanted to block under each candidate rule, iterate the rule set against the dump rather than against speculation.
- **Audit rows for spawn-side failures + UNKNOWN_TOOL** — today these surface only in the daemon log. `cli_ask_e2e`'s failure-path test confirmed the existing audit shape works for POLICY_DENIED (chokepoint write fires), but spawn-side failures (`SPAWN_FAILED`) and `UNKNOWN_TOOL` don't reach the chokepoint, so they leave no row at all. If the channel-bus operator wants to react to "the agent tried web-fetch but it's not registered," the row needs to exist. Possible shape: `actor='scheduler', action='step.unknown_tool' | 'step.spawn_failed'`, payload `{tool, method, error_or_none}` — written by `ToolHostStepDispatcher::dispatch_step` before it returns the Err variant.
- **Per-tool argv allowlist hygiene** — the deny-by-default `HHAGENT_SHELL_EXEC_ALLOWLIST` env is acceptable for now, but production deployment needs a versioned per-host config (or `db::secrets`-stored allowlist) so a host restart can't accidentally widen it. Filed as a follow-up issue when the first non-test deployment lands.
- **Issue #15 — hoist tests-common dev-dep:** now **eight** duplication sites for the per-test PG cluster bring-up (`cli_ask_e2e` was #7; `embedding_recall_e2e` is #8). Pure mechanical refactor; not blocking but increasingly cheap to do.

**Existing Phase 1 cont. pickups (updated priority):**

- **Option P — entity↔memory linkage + graph lane in `recall`:** the third lane mentioned in Option N's brief. Now that `embed_query` exists (Option O shipped) and the module is split (issue #30), Option P is the next natural recall extension.
- ~~**Refactor `core/src/memory.rs` into `memory/recall.rs` + `memory/embed.rs`:**~~ **Shipped 2026-05-12 (this session)** — see "Recently completed" entry at the top. Closes issue #30.
- ~~**Option O — embedding worker (Phase 1 cont.):**~~ **Shipped 2026-05-12** as `Router::embed` in core (worker-process design rejected during brainstorming; see the older "Recently completed" section and the spec). Branch: `feat/embedding-router` (merged via PR #29 at `d39023b`).
- **Issue #16 — close the in-crate hole in the `WorkerCommand` seal:** sibling modules inside `hhagent_core` can construct one and reach `SupervisedWorker::call` directly. Three candidate fixes filed.
- **Issue #17 — `memory::recall` warn-and-degrade on missing input may mask caller bugs:** tighten before Phase 1's scheduler is the production caller.
- **Option K — cross-platform exponential restart backoff:** filed but parked; no immediate need.

### ~~Option O — embedding worker (Phase 1 cont.)~~ SHIPPED 2026-05-12

**Design changed from "worker" to `Router::embed` in core** during the brainstorming pass (see spec `docs/superpowers/specs/2026-05-11-embedding-router-design.md`). Worker-process design rejected for symmetry with the existing `Router::send` precedent. A future "all net egress in sandboxed workers" Phase-3 slice migrates both `send` and `embed` together.

What shipped: `llm-router/src/embeddings.rs` (wire shapes), `Router::embed` + `Router::pick_embed_backend` + `EMBEDDINGS_PATH` + `PolicyGate::pick_embed` (Phase-5 seam), `RouterError::EmbeddingCountMismatch`, `RouterConfig::embedding_url`/`embedding_model`, `core::memory::embed_query` + `MemoryError` + `build_embed_audit_payload`. Branch `feat/embedding-router` (range `9fe45d6..a1256cd`). +28 tests (299 → 327).

### Option P — entity↔memory linkage + graph lane (Phase 1 cont.)

The original Option N brief named three lanes; this slice ships the third. Requires picking the linkage shape:

- **Option P1: `memory_entities` join table.** New migration: `(memory_id BIGINT REFERENCES memories(id) ON DELETE CASCADE, entity_id BIGINT REFERENCES entities(id) ON DELETE CASCADE, PRIMARY KEY (memory_id, entity_id))`. Cleaner separation; richer query semantics; requires explicit `INSERT INTO memory_entities` at memory-write time.
- **Option P2: `metadata->'entities'` JSONB array on `memories`.** No new table; uses the existing `metadata` GIN index. `metadata->'entities' ?| array['<id>']` is the query. Less code; tighter coupling between memory shape and graph linkage.

Recommendation: **P1**. The memory store will accumulate linkage data over time; a dedicated table makes the query shape (and any future "find memories that mention any descendant of entity X" recursive walk) cleaner.

- **Graph lane shape:** for a query carrying `seed_entity_ids: &[i64]`, traverse outbound 1-hop (or via `Graph::path` with `max_hops = 2`) to get a candidate entity set, then `SELECT memory_id FROM memory_entities WHERE entity_id = ANY($1)` returns the ranked id-list. Rank = # of seed-entity neighbours that connect to the memory.
- **Verification:** integration test seeds entities + memories + linkage rows, queries with one entity as seed, asserts the connected memories rank above unconnected ones, and asserts the fused `recall(ALL)` over all three lanes surfaces the most-relevant memory at top-1.

### Option K — cross-platform exponential restart backoff

Currently `Restart=on-failure RestartSec=5` is a constant 5 s. systemd 252+ supports `RestartSteps` / `RestartMaxDelaySec` for true exponential backoff. macOS launchd's `KeepAlive=true` has no operator-controllable throttle. Cross-platform shape: extend `ServiceSpec` with `restart_backoff: Option<RestartBackoff>` (max delay + step count); the systemd backend wires it into the unit file, the macOS backend logs a warning at install time and falls back to launchd's default. Filed but parked.

### Option G — make `cpu_quota_pct`/`tasks_max` policy-driven + setrlimit-based `cpu_ms` enforcement ([#6](https://github.com/hherb/hhagent/issues/6))

Smaller follow-up to Option E. Today the cgroup layer hardcodes `CPUQuota=200%` and `TasksMax=64`; `policy.cpu_ms` is documented but unenforced.

- Extend `SandboxPolicy` with `cpu_quota_pct: Option<u32>` and `tasks_max: Option<u64>` (both `#[serde(default)]`). Add a `Default` impl for `SandboxPolicy` first to avoid test-fixture churn.
- Plumb through `linux_cgroup::build_systemd_run_argv`.
- For `cpu_ms`, the natural enforcement is `setrlimit(RLIMIT_CPU)` from the worker prelude before `exec(2)` — cgroup v2 has no direct CPU-budget primitive. Add `apply_rlimits(policy)` and call from `serve_stdio` before Landlock/seccomp lock_down.
- macOS parity: same `setrlimit` approach (POSIX). The cgroup-shaped `mem_mb` cap on macOS still requires the future micro-VM backend or `RLIMIT_AS` (which has known false-positive risks for malloc-heavy workers).

---

## Open follow-up issues (filed but not picked)

- [#1](https://github.com/hherb/hhagent/issues/1) — narrow macOS `(allow mach-lookup)` to a `global-name` allowlist  *(closed in code 2026-05-09; rule removed entirely from `build_profile`)*
- [#2](https://github.com/hherb/hhagent/issues/2) — evaluate `setpgid` → `setsid` for stronger session isolation on macOS  *(closed in code 2026-05-09; `pre_exec` hook calls `libc::setsid()`)*
- [#3](https://github.com/hherb/hhagent/issues/3) — drop `SYS_SENDFILE`/`SYS_FADVISE64` shim once libc exposes them on aarch64
- [#4](https://github.com/hherb/hhagent/issues/4) — bump Last-commit + test-count fields whenever a Recently-completed entry is added
- [#5](https://github.com/hherb/hhagent/issues/5) — audit `BASE_ALLOW` against a fixture of common worker binaries
- [#6](https://github.com/hherb/hhagent/issues/6) — tunable `cpu_quota_pct`/`tasks_max` policy fields + `setrlimit`-based `cpu_ms` enforcement (Option G above)
- [#8](https://github.com/hherb/hhagent/issues/8) — collapse `default_probe` / `default_supervisor` cfg-ladder duplication once a third entry point or backend OS appears
- ~~[#11](https://github.com/hherb/hhagent/issues/11) — daemon-scoped `PgPool`~~ **closed 2026-05-10** by Option I's `pool::connect_runtime_pool`
- ~~[#12](https://github.com/hherb/hhagent/issues/12) — reject empty `secrets.aad`~~ **closed 2026-05-10** — `db::secrets::put` always populates AAD via `compute_aad(name, _)`; migration `0004_secrets_aad_nonempty.sql` adds `CHECK (octet_length(aad) > 0)`
- [#13](https://github.com/hherb/hhagent/issues/13) — write a migration numbering / rename hygiene checklist; sqlx fingerprints version+slug, so a rename or edit on a shipped migration silently breaks startup on existing clusters
- [#14](https://github.com/hherb/hhagent/issues/14) — replace the brittle `wait_for_log_match("database probe succeeded")` in `core/tests/supervisor_e2e.rs` with a constant in `hhagent-core`'s public API or a real readiness signal
- [#15](https://github.com/hherb/hhagent/issues/15) — hoist the duplicated PG bring-up boilerplate into a workspace-level `tests-common` dev-dep crate; **eight duplication sites today** (`db/tests/postgres_e2e.rs`, `core/tests/audit_dispatch_e2e.rs`, `core/tests/supervisor_e2e.rs`, `core/tests/shell_exec_e2e.rs`, `core/tests/memory_recall_e2e.rs`, `core/tests/router_agent_mock_e2e.rs`, `core/tests/cli_ask_e2e.rs`, `core/tests/embedding_recall_e2e.rs`)
- [#16](https://github.com/hherb/hhagent/issues/16) — close the in-crate hole in the `WorkerCommand` seal (filed 2026-05-10)
- [#17](https://github.com/hherb/hhagent/issues/17) — tighten `memory::recall` behaviour when input is missing (filed 2026-05-10)
- [#20](https://github.com/hherb/hhagent/issues/20) — `agent_prompts` schema: PK on sha256 means renamed prompt files lose their original name (filed 2026-05-10 from PR #25 review)
- [#21](https://github.com/hherb/hhagent/issues/21) — `core::scheduler::runner` per-iteration cancellation poll could be a `watch::Receiver` instead of a DB round-trip (filed 2026-05-10 from PR #25 review)
- ~~[#22](https://github.com/hherb/hhagent/issues/22) — `RouterAgent::formulate_plan` has no mock-HTTP test coverage~~ **addressed by PR #26 (open)**
- [#23](https://github.com/hherb/hhagent/issues/23) — scheduler: constitutional refusals are recorded as `state='completed'`, not `'blocked'` — design discussion before CASSANDRA real impls (filed 2026-05-10 from PR #25 review)
- [#24](https://github.com/hherb/hhagent/issues/24) — deployment: `HHAGENT_PROMPTS_DIR` has a cwd-relative fallback; production unit files must set it explicitly (filed 2026-05-10 from PR #25 review)
- ~~[#30](https://github.com/hherb/hhagent/issues/30) — split `core/src/memory.rs` into `recall.rs` + `embed.rs` submodules~~ **closed 2026-05-12 by this slice** (`core/src/memory/{mod.rs, recall.rs, embed.rs}`, all under the 500-LOC soft cap)
- ~~**Deferred — Task 3.2.bis:** wire `ToolHostStepDispatcher` to `tool_host::dispatch`~~ **shipped 2026-05-11** on branch `feat/tool-host-step-dispatcher`. See older "Recently completed" entry.
- ~~**Deferred — Task 4.4:** `cli_ask_e2e` integration test~~ **shipped 2026-05-11** on `main` (see older "Recently completed" entry).

(Closed won't-fix: [#9](https://github.com/hherb/hhagent/issues/9) Apache AGE, [#10](https://github.com/hherb/hhagent/issues/10) ParadeDB pg_search — both 2026-05-09 after review. Closed in earlier 2026-05-09: [#7](https://github.com/hherb/hhagent/issues/7) — daemon log-line substring is now precise after `(skeleton)` was dropped from the startup line.)

---

## Open questions parked for later

(From the design plan, restated here so they're surfaced when relevant.)

1. Embedding model on-device — bge-m3 vs nomic-embed-text vs ColBERT (Phase 1)
2. ~~Channel approval — passcode pairing vs static contact allowlist (Phase 2)~~ **Resolved 2026-05-06:** pairing flow with WebAuthn-or-OTP fallback, modeled on ZeroClaw's `security/{pairing,webauthn,otp}.rs`.
3. ~~Egress proxy as separate worker vs in-process in `tool_host`~~ **Resolved 2026-05-06:** separate worker, with the credential-leak scanner co-located.
4. Skill review workflow for *named* agent-authored Python (Phase 4) — see Phase 4 line items: trust enum + per-level capability ceiling.
5. Worker keep-alive vs spawn-per-call (currently spawn-per-call; revisit when latency matters)
6. Worker binary discovery in production (currently `target/debug/...` for tests; need a stable install location convention)

## Inspirations / things to read before each milestone

Two adjacent OpenClaw-derived projects ship code we can read (Apache-2.0/MIT, AGPL-compatible) before each new milestone — convergent prior art saves design time:

- **ZeroClaw** ([`zeroclaw-labs/zeroclaw`](https://github.com/zeroclaw-labs/zeroclaw), 100% Rust): read [`crates/zeroclaw-runtime/src/security/`](https://github.com/zeroclaw-labs/zeroclaw/tree/main/crates/zeroclaw-runtime/src/security) — has working `bubblewrap.rs`, `landlock.rs`, `seatbelt.rs`, `firejail.rs`, `pairing.rs`, `webauthn.rs`, `leak_detector.rs`, `workspace_boundary.rs`. Architectural drawback vs us: tools run as in-process Rust traits, OS sandbox wraps the runtime — weaker boundary than our process-per-worker. Don't copy the in-process tool model.
- **IronClaw** ([`nearai/ironclaw`](https://github.com/nearai/ironclaw)): read its dispatcher chokepoint pattern (`ToolDispatcher::dispatch()` is the single audit/safety-validation funnel for *every* action, regardless of caller). Drawbacks: WASM-as-boundary is software-only containment; Postgres+libSQL dual backend is overkill at our stage.

The *defining* architectural difference: hhagent enforces **one OS process + one bwrap/Seatbelt jail per worker**. Both reference projects retreated from that. Don't.

---

## How to update this document at session end

1. **Bump header fields** — `Last updated`, `Last commit`, `Branch` at the top.
2. **Move "Next TODO" → "Recently completed (this session)"** if the picked option shipped, with enough detail that the next session can understand the decision (file paths, why-not-X, gotchas, test-count delta).
3. **Write a fresh "Next TODO (pick one)"** with options sized for one session each — include file paths, gotchas, and the verification step.
4. **Refresh "Working state"** — green-test count, anything new under stubs, anything that became real.
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
