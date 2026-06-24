# Plan: Matrix worker respawn stability + death observability ([#348](https://github.com/hherb/kastellan/issues/348))

Spec: `docs/superpowers/specs/2026-06-24-matrix-worker-respawn-stability-design.md`. TDD throughout.

## Task 1 — pure sync-retry policy (item 2)

New non-gated module `workers/matrix/src/sync_retry.rs` (declared `mod sync_retry;`
in `main.rs`, unconditional → tested in the default build).

- `SyncOutcome { Backoff(Duration), GiveUp }`.
- `update_consecutive(prev: u32, ran_for: Duration, healthy: Duration) -> u32` —
  `0` if `ran_for >= healthy` (healthy run, reset), else `prev + 1`.
- `next_action(consecutive: u32, max_consecutive: u32, base: Duration, max: Duration)
  -> SyncOutcome` — `GiveUp` when `consecutive >= max_consecutive`, else
  `Backoff(capped exponential = min(max, base * 2^(consecutive-1)))`.
- Constants (in `sdk_live.rs` where used): `HEALTHY_RUN_THRESHOLD` (e.g. 60s),
  `SYNC_BACKOFF_BASE` (1s), `SYNC_BACKOFF_MAX` (30s), `SYNC_MAX_CONSECUTIVE` (e.g. 10).

Tests: reset on healthy run; increment on fast fail; backoff escalates then caps;
GiveUp at threshold.

## Task 2 — wire retry into the sync task (item 2)

`sdk_live.rs::connect`: replace the `match sync().await { … } process::exit(1)` with
a loop that times each `sync()`, updates the counter via `update_consecutive`, then
`Backoff(d) → sleep(d); continue` or `GiveUp → eprintln! + process::exit(1)`.
`live-matrix`-gated; covered by the pure tests above + manual reasoning.

## Task 3 — reusable stderr drain + tail (item 1a)

New `core/src/worker_stderr.rs`:

- `StderrTail` (`Arc<Mutex<VecDeque<String>>>` + cap) — `new(cap)`, internal `push`
  (bounded, oldest-evicted), `snapshot() -> Vec<String>`.
- `drain_reader<R: Read>(pid: u32, reader: R, tail: Option<&StderrTail>)` — raw-byte
  read loop logging each chunk at `debug` (preserves tool_host behavior); when a tail
  is given, also splits complete lines into it. Pure-ish: tested with a `Cursor`.
- `spawn_drain(pid, stderr)` + `spawn_drain_with_tail(pid, stderr) -> StderrTail` —
  thread wrappers.
- `format_death_report(status: Option<ExitStatus>, tail: &[String]) -> String`.

Refactor `tool_host::drain_worker_stderr` → delegate to `worker_stderr::spawn_drain`
(behavior-preserving).

Tests: bounded tail evicts oldest; `drain_reader` populates tail from a Cursor;
`format_death_report` None + tail-join branches (+ a real `ExitStatus` from `false`).

## Task 4 — death report in the matrix driver (item 1b)

- `kastellan-protocol`: add `Client::try_wait(&mut self) -> io::Result<Option<ExitStatus>>`.
- `WorkerClient` trait: add `fn death_report(&mut self) -> Option<String> { None }`
  (default keeps the `FakeWorker` + unsupervised tests unchanged).
- `ProtocolWorkerClient`: hold `Option<StderrTail>`; keep `new(client)` (tail `None`,
  used by `matrix_channel_e2e`) + add `with_stderr(client, tail)`. `death_report`
  bounded-reaps via `try_wait` (a few 50ms tries) + snapshots the tail →
  `format_death_report`.
- `spawn_worker_client`: `child.stderr.take()` → `spawn_drain_with_tail` →
  `ProtocolWorkerClient::with_stderr`.
- Driver loop: at the top of the `if worker_dead` block, log
  `client.death_report()` (when `Some`) at `warn`.

Tests: existing `matrix_channel_e2e` (real worker pipe) still green; unit driver
tests unchanged (default `death_report` → no log).

## Task 5 — verify + ship

`cargo test -p kastellan-worker-matrix` (default + `--features live-matrix`),
`-p kastellan-core`, `-p kastellan-protocol`; `cargo clippy --workspace --all-targets
-- -D warnings` (+ `--features live-matrix`). Update HANDOVER + ROADMAP. PR linked to #348.
