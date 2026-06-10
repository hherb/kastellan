# cli_ask_e2e — End-to-End Integration Test (Task 4.4)

**Date:** 2026-05-11
**Author:** Claude (hherb)
**Status:** Approved 2026-05-11

## Why

Every existing integration test stubs at least one moving part:

| Test                              | What it pins                                   | What it stubs |
| --------------------------------- | ---------------------------------------------- | ------------- |
| `router_agent_mock_e2e`           | `RouterAgent::formulate_plan` against mock HTTP | Scheduler, dispatcher |
| `scheduler_step_dispatch_e2e`     | `ToolHostStepDispatcher` against real sandbox + worker | Scheduler loop, LLM, CLI |
| `scheduler_inner_loop_e2e`        | Inner loop completion / failure / cancel paths | LLM (scripted), dispatcher (scripted) |
| `scheduler_lanes_e2e`             | Concurrent fast+long lane claim                | LLM, dispatcher |
| `scheduler_crash_recovery_e2e`    | `sweep_crashed` lease semantics                | LLM, dispatcher |
| `supervisor_e2e`                  | Daemon bring-up + audit-mirror                 | LLM, scheduler triggering (no `ask`) |

No test exercises **CLI subprocess → PG insert → scheduler claim → LLM call → CASSANDRA review → step dispatch → finalize → CLI exit** end-to-end. That is the chain operators rely on — and the chain a future regression in any layer could silently break.

This slice closes that gap. It is **Task 4.4 from HANDOVER's deferred list**, unblocked 2026-05-11 once Task 3.2.bis wired the real `ToolHostStepDispatcher` to `tool_host::dispatch`.

## What is in scope

Two `#[test]` functions in a new `core/tests/cli_ask_e2e.rs`:

### 1. `ask_subprocess_completes_planned_task_end_to_end` (happy path)

The classic "agent runs one step, then declares completion" flow. Mock LLM serves two plans:

- **Plan A** (returned to call 1): non-terminal — `decision != "task_complete"`, `steps: [{tool: "shell-exec", method: "shell.exec", parameters: {argv: [ECHO_PATH, "<marker>"]}}]`. The inner loop dispatches the step.
- **Plan B** (returned to call 2): terminal — `decision: "task_complete"`, `steps: []`, `result: {kind: "text", body: "<marker>"}`. The inner loop short-circuits to `Outcome::Completed`.

The `<marker>` token (e.g. `marker-<unique-suffix>`) flows from plan B's result through the daemon's `finalize` write into the `tasks.result` column, and from there into the CLI's stdout via `kastellan-cli.rs:307-311`.

**Assertions:**

- CLI process exits with status 0.
- CLI stdout `.trim_end() == "<marker>"`.
- `tasks` row: `state = "completed"`, `plan_count = 2`, `result["body"] == "<marker>"`.
- `audit_log` shape (multiset, not strict order):
  - 1× `core/startup`
  - 2× `agent/plan.formulate`
  - 2× `cassandra:chain/verdict`
  - 1× `tool:shell-exec/shell.exec` (the step in plan A)
  - 1× `scheduler/plan.outcome`

### 2. `ask_subprocess_fails_after_plan_iteration_cap` (failure path)

The mock LLM repeatedly returns the same plan with a non-allowlisted argv. Every step fails worker-side as `POLICY_DENIED`; the inner loop replans until `plan_count >= max_plans` (`DEFAULT_MAX_PLANS_FAST = 3` per `db/src/tasks.rs:50`), then returns `Outcome::Failed("plan_iteration_cap_exceeded (3>=3)")`.

Mock LLM queue: 3 identical responses, each carrying:
- **Plan X:** non-terminal, `steps: [{argv: ["/bin/cat", "/etc/passwd"]}]`.

**Assertions:**

- CLI process exits with status 1.
- CLI stderr contains `"failed"` (matches `kastellan-cli.rs:320 — eprintln!("ask: task ended in state '{state}'")`).
- `tasks` row: `state = "failed"`, `plan_count = 3`.
- `audit_log` shape: 3× `tool:shell-exec/shell.exec` rows whose payload carries `err.code == "POLICY_DENIED"`.

## What is explicitly out of scope

- **Constitutional-block path** — CASSANDRA stages are stubbed to always Approve in this phase (`core::cassandra::review::{ConstitutionalGuard, DeterministicPolicy}` returning `Verdict::Approve`). Real-stage paths get coverage in the observation-phase follow-up.
- **Cancel mid-execution from the CLI side** — `kastellan-cli ask` supports ctrl-C, but reliably planting a SIGINT during inner-loop step execution is a different kind of test (timing-sensitive, would benefit from `BarrierDispatcher`). Filed for later.
- **Long lane** — both tests use `Lane::Fast`. The lane runner abstraction is already pinned by `scheduler_lanes_e2e`.
- **Multiple concurrent CLI invocations** — `scheduler_lanes_e2e` already pins the lane parallel-claim invariant.
- **CLI flag handling regressions** — `tasks list/status/cancel/fail/tail` and `audit tail` have their own coverage (CLI unit tests + `supervisor_e2e` for the mirror integration).

## How

### Test file layout

```
core/tests/cli_ask_e2e.rs    (~600 LOC, mirrors supervisor_e2e.rs + scheduler_step_dispatch_e2e.rs)

1. cfg-gate (linux + macos only)
2. ECHO_PATH per-OS const
3. unique_suffix / unique_temp_root helpers
4. skip helpers: supervisor, sandbox, PG bin dir, worker binary, core binary
5. RAII guards: ServiceGuard, PathGuard
6. wait_for_status, wait_for_socket, wait_for_log_match helpers
7. bring_up_pg_cluster helper (verbatim from supervisor_e2e.rs)
8. MockLlm queued helper (~120 LOC)
9. macOS-only serial_lock (matches supervisor_e2e.rs)
10. Common bring_up_daemon helper (PG + core service spec + env wiring)
11. #[test] ask_subprocess_completes_planned_task_end_to_end
12. #[test] ask_subprocess_fails_after_plan_iteration_cap
```

The helpers (1)–(7) are duplicated from `supervisor_e2e.rs` / `scheduler_step_dispatch_e2e.rs`. Issue #15 already tracks the workspace-level `tests-common` refactor; **this is the seventh duplication site**.

### Mock LLM helper shape

```rust
/// Multi-shot HTTP mock for the LLM router. Serves responses from a
/// queue in FIFO order; returns HTTP 503 once exhausted so an unexpected
/// extra call surfaces as `RouterError::HttpStatus` in the daemon log
/// rather than as a silent hang.
struct MockLlm {
    base_url: String,
    /// Captured request bodies in arrival order. Useful for asserting
    /// the daemon dialed N times.
    requests: Arc<Mutex<Vec<String>>>,
    join: tokio::task::JoinHandle<()>,
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.join.abort();
    }
}

async fn spawn_queued_mock(responses: Vec<String>) -> MockLlm;
```

Backed by a single `tokio::net::TcpListener` and a `Vec<String>` queue inside an `Arc<Mutex<>>`. Each accepted connection reads the request, captures the body, pops the next response, writes it, and shuts the socket down (same HTTP/1.1 buffer-up-to-Content-Length pattern as `router_agent_mock_e2e.rs`'s `spawn_one_shot_mock`). When the queue is exhausted, return a `503 Service Unavailable` with an empty JSON body.

### Daemon env wiring

| Env var                          | Value                                                       | Purpose                                                                  |
| -------------------------------- | ----------------------------------------------------------- | ------------------------------------------------------------------------ |
| `KASTELLAN_DATA_DIR`               | per-test PG data dir                                        | Daemon points at our per-test cluster, not user's installed cluster.     |
| `KASTELLAN_STATE_DIR`              | per-test temp dir                                           | Audit-mirror JSONL writes here, away from `~/.local/state/`.             |
| `KASTELLAN_PROMPTS_DIR`            | `<workspace>/prompts/`                                      | Prompt loader is fail-closed if missing.                                 |
| `KASTELLAN_LLM_LOCAL_URL`          | `format!("{}/v1", mock.base_url)`                           | `compose_url` will append `/chat/completions`.                           |
| `KASTELLAN_LLM_LOCAL_MODEL`        | `"test-local-model"`                                        | Echoed in `FormulationMeta`.                                             |
| `KASTELLAN_LLM_TIMEOUT_MS`         | `2000`                                                      | Tight timeout — mock failures surface fast.                              |
| `KASTELLAN_SHELL_EXEC_BIN`         | `<workspace>/target/debug/kastellan-worker-shell-exec`        | Registers shell-exec in the tool registry.                               |
| `KASTELLAN_SHELL_EXEC_ALLOWLIST`   | `ECHO_PATH` only                                            | Echo allowed; `/bin/cat` deliberately denied for the failure path.       |
| `USER`                           | inherited from test process                                 | Required by `ConnectSpec::default_for` (peer auth identity).             |

The CLI subprocess inherits its env from the test process and only needs `KASTELLAN_DATA_DIR` propagated (it shares the cluster with the daemon).

### CLI invocation pattern

```rust
let output = Command::new(&cli_binary)
    .arg("ask")
    .arg(format!("say {marker}"))
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("KASTELLAN_DATA_DIR", &data_dir)
    .env("USER", &user)
    .output()
    .expect("spawn kastellan-cli ask");
```

Use `output()` (blocking, captures stdout/stderr) rather than `spawn()` so we have a single witness for exit code + streams. The CLI's `ask` path waits for the completion NOTIFY internally — no external sleep needed.

### Audit-log assertion pattern

```rust
let rows: Vec<(String, String)> = sqlx::query_as(
    "SELECT actor, action FROM audit_log ORDER BY id",
).fetch_all(&pool).await.unwrap();

let multiset: std::collections::HashMap<(String, String), usize> =
    rows.into_iter().fold(HashMap::new(), |mut m, k| {
        *m.entry(k).or_insert(0) += 1; m
    });

assert_eq!(multiset.get(&("core".into(), "startup".into())), Some(&1));
assert_eq!(multiset.get(&("agent".into(), "plan.formulate".into())), Some(&2));
// etc.
```

Multiset-based, not order-based — so adding a new intra-iter audit row in the future (e.g. one of the Phase-1 follow-up `actor='scheduler', action='task.<state>'` rows in HANDOVER's open list) does not break this test.

### Skip behaviour

Identical to `scheduler_step_dispatch_e2e.rs`:

- `[SKIP] supervisor probe failed` — no `systemd --user` / no `loginctl enable-linger` on Linux; SSH-only macOS.
- `[SKIP] no Postgres install found` — no PGDG / Homebrew binaries.
- `[SKIP] sandbox probe failed` — bwrap user-namespace not enabled.
- `[SKIP] kastellan / kastellan-worker-shell-exec binary not found` — workspace not built.

All skips print to stderr via `eprintln!`; `cargo test -- --nocapture` to see them.

## Risks and mitigations

### CLI's LISTEN-BEFORE-INSERT race

`kastellan-cli ask` does `PgListener::connect` → `listen("tasks_completed")` → `insert_pending` (`kastellan-cli.rs:257-275`). The daemon's scheduler claims the task only after the INSERT; the NOTIFY fires after `finalize`. The ordering is safe because the listener is established before the INSERT — the same pattern that survives in production.

### Mock LLM HTTP timing

Tight `KASTELLAN_LLM_TIMEOUT_MS=2000`. A slow CI machine could conceivably take longer than 2 s between connect and full response. Mitigation: the mock responds inline on the same accepted socket — there's no buffered work. If we observe flakes, bump to 5 s.

### Multi-thread tokio runtime

Required because `tool_host::dispatch` uses `block_in_place`. The CLI subprocess runs in its own process so its runtime choice is independent; the test body itself uses `Builder::new_multi_thread().worker_threads(1).enable_all()` matching the precedent.

### Subprocess output capture

`Command::output()` buffers full stdout/stderr in-process. The CLI's output is tiny (< 1 KiB for both cases), so no risk of pipe-fill deadlock.

### macOS launchd domain serialization

The static-mutex `serial_lock` pattern from `supervisor_e2e.rs` is the existing convention. Hold it for the test body so `cargo test --workspace` doesn't race two launchd-touching tests.

## Test-count delta

`core` integration test count: **+2** (both `#[test]` in a single new file).
Workspace total: **297 → 299**.

## Files

- **New:** `core/tests/cli_ask_e2e.rs` (~600 LOC).
- **No production-code changes** are planned. If a regression surfaces while writing this test, it is fixed inline (with a separate commit explaining the fix).

## Acceptance

- `cargo test --workspace` green; zero `[SKIP]` lines on the DGX (real bwrap + real systemd-user + real PG installed).
- Both new tests pass deterministically across 5 consecutive runs (`for i in {1..5}; do cargo test -p kastellan-core --test cli_ask_e2e; done`).
- HANDOVER + ROADMAP updated with the slice description + the new test count.
- A summary committed alongside the test, with `Co-Authored-By: Claude Opus 4.7`.
