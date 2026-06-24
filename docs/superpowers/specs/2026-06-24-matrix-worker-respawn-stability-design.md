# Matrix worker respawn stability + death observability ([#348](https://github.com/hherb/kastellan/issues/348))

**Date:** 2026-06-24
**Status:** design → implementation

## Problem

The live Matrix channel worker (`kastellan-worker-matrix`) on the DGX dies and is
respawned by the supervised `MatrixChannel` repeatedly — roughly every ~20–90s in
bursts. The #321 self-healing respawn masks it (the channel recovers + replays
downtime messages), but the worker should not be this unstable. The death cause is
currently **invisible**: ruled out as seccomp (0 `type=1326`) and Landlock (0
`type=1422`), with no captured worker stderr.

## Root cause (from the code, high confidence)

The worker has exactly one internal self-termination path: the continuous **sync
task** in `workers/matrix/src/sdk_live.rs::connect`. When `client.sync()` returns
for **any** reason — a transient server 5xx, a network blip through the egress
tunnel, a long-poll hiccup — the task immediately calls `std::process::exit(1)`.
There is no retry. A single transient sync interruption kills the whole worker, and
the supervisor respawns it → the observed churn.

The cause is invisible because the task's `eprintln!("sync loop failed: {e}")` goes
to the worker's stderr, which is `Stdio::piped()` but — in the **matrix channel**
spawn path (`core/src/channel/matrix.rs::spawn_worker_client`) — **never drained**.
(`tool_host::spawn_worker` already drains tool-worker stderr; the channel path never
adopted it.) So both the diagnostic and the exit status are discarded.

## Fix (two complementary parts)

### Item 2 — resilience (the churn fix)

Replace the unconditional `process::exit(1)` with a **bounded retry-with-backoff
loop** around `sync()`:

- A `sync()` that ran healthily for a while before returning resets the
  consecutive-failure counter (it was up and working — a transient blip).
- A fast-returning failure increments the counter; the loop backs off
  (capped exponential) and retries in place.
- Only after **sustained** consecutive fast failures does the task fall back to the
  fail-loud `process::exit(1)` so the supervisor respawns a *fresh* worker (a
  persistently-wedged client — bad token, store corruption — only a fresh `connect`
  recovers, so giving up there is correct).

Worst case ≡ today's behavior (exit → supervised respawn); transient blips no longer
cause churn. The retry decision is a **pure, unit-tested** function
(`workers/matrix/src/sync_retry.rs`, compiled in the default build so CI covers it
despite `live-matrix` being DGX-gated, cf. #331).

### Item 1 — observability

Make the matrix worker's death diagnosable in the daemon log:

1. **Drain its stderr** (reuse the `tool_host` pattern, lifted into a shared
   `core/src/worker_stderr.rs`) so the worker's `eprintln!` diagnostics surface and
   the ~64 KiB pipe can't fill + deadlock. The drain also **retains a bounded tail**
   of recent lines.
2. **Log the exit status + stderr tail on death.** When the driver detects a dead
   worker (`poll`/`send` error), it reaps the child (non-blocking, bounded) and logs
   a `warn` with the `ExitStatus` (which distinguishes a clean `exit status: 1` —
   the sync-task fail-loud — from a `signal: 6 (SIGABRT)` — a deadpool crypto-store
   crash) plus the recent stderr lines.

This both fixes the deadlock-via-undrained-pipe latent bug for the channel worker
and lets the DGX *confirm* the root cause empirically.

## Out of scope

- Item 3 (respawn-rate alarm) — small separate follow-up.
- Full DGX churn-elimination confirmation — a deploy/verify follow-up; this change is
  verified hermetically on macOS (pure helpers + cross-platform stderr plumbing).

## Postscript (2026-06-25): the real root cause, found on deploy

The "from the code, high confidence" hypothesis above (sync-task `process::exit(1)`)
was **wrong** — and the observability half disproved it. Deploying the death-report
to the DGX showed `worker exited (signal: 9 (SIGKILL))` ~10s after every login, with
zero OOM/rlimit/seccomp records. The actual cause: the **initial** worker is spawned
via `tokio::task::spawn_blocking` (`main.rs`), so bwrap (`--die-with-parent` /
`PR_SET_PDEATHSIG`, which fires on *parent-thread* death) is forked on a blocking-pool
thread tokio reaps after its ~10s idle keep-alive → PDEATHSIG SIGKILLs the worker.
Respawns were already immune (issued by the persistent driver thread).

**The real fix:** `MatrixChannel::supervised_self_spawn` performs the initial spawn on
the driver thread too, so no worker is parented to an ephemeral thread. DGX-confirmed:
initial worker stable 2+ min, zero SIGKILL. The sync-retry change remains as
defense-in-depth (a genuine latent self-exit hazard, just not this churn's cause). This
is the textbook systematic-debugging outcome: instrument first, let the evidence refute
the guess.

## Verification

- `cargo test -p kastellan-worker-matrix` (default) + `--features live-matrix`.
- `cargo test -p kastellan-core` (tool_host + matrix channel units) + `kastellan-protocol`.
- `cargo clippy --workspace --all-targets -- -D warnings` (+ `--features live-matrix`).
- Pure-Rust, no migration, no OS-gated logic → DGX not required for the unit gate;
  DGX deploy is the empirical confirmation follow-up.
