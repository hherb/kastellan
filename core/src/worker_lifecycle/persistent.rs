//! Backend-agnostic supervisor for a LONG-LIVED worker: a persistent OS thread
//! owns the worker, forwards serialized RPC calls to it, and respawns it on
//! death (capped-exponential backoff + sliding-window rate alarm). PDEATHSIG-safe
//! (the spawning thread outlives the worker — required under the slice-5a
//! bwrap-confined launcher). A generalization of the Matrix channel's
//! historical self-spawning supervised-driver pattern, with no
//! channel/poll-send coupling — the Matrix channel now consumes this
//! supervisor directly (see `channel::matrix::spawn_matrix_worker`).
//!
//! Also houses [`ClientTransport`]: the production [`PersistentTransport`] impl
//! that wraps a real [`kastellan_protocol::client::Client`] over a sandboxed
//! worker's stdio, with stderr-tail death reporting (the same pattern the
//! Matrix channel used before adopting this shared supervisor).
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kastellan_protocol::client::Client;
use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

use crate::channel::respawn_alarm::RespawnRateAlarm;
use crate::worker_lifecycle::RestartBackoff;

/// How long [`ClientTransport::death_report`] waits for the stderr drainer to
/// reach EOF before snapshotting the tail.
///
/// The same 250 ms `tool_host::EARLY_EXIT_DRAIN_WAIT` uses, and deliberately so:
/// both are "the worker just closed its pipes; give the drain thread a moment to
/// finish" and there is no reason for a persistent worker's last words to be
/// collected less patiently than a tool worker's. A separate const rather than a
/// shared one only because the two live in unrelated modules; if a third appears,
/// hoist them.
///
/// ⚠️ **This is a cap, not a cost.** `StderrTail::wait_for_drain` returns as soon
/// as `mark_drained` lands, so a worker whose stderr has already closed costs ~2 ms.
const DEATH_DRAIN_WAIT: Duration = Duration::from_millis(250);

// ── ClientTransport ──────────────────────────────────────────────────────────

/// Production [`PersistentTransport`]: a JSON-RPC [`Client`] over a spawned
/// worker's stdio, with a bounded stderr-tail for death diagnostics.
///
/// Reuses the lockdown-env derivation + stderr-tail drain that the Matrix
/// channel's worker spawn uses, without coupling to that module.
pub struct ClientTransport {
    client: Client,
    /// Bounded tail of the worker's recent stderr lines, retained by the drain
    /// thread so [`PersistentTransport::death_report`] can surface the death
    /// cause. `None` when the child had no piped stderr (should not happen in
    /// practice — backends always pipe stderr — but we handle it gracefully).
    stderr_tail: Option<crate::worker_stderr::StderrTail>,
}

impl ClientTransport {
    /// Spawn a sandboxed worker under `backend` + `policy`, drain its stderr
    /// into a bounded tail, and connect a [`Client`] over its stdio.
    ///
    /// Applies the same worker-side lockdown-env derivation
    /// (`KASTELLAN_LANDLOCK_*` / `KASTELLAN_SECCOMP_PROFILE`) that
    /// `tool_host::spawn_worker` does, so the worker is locked down
    /// identically regardless of spawn path.
    pub fn spawn(
        backend: &dyn SandboxBackend,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> anyhow::Result<Self> {
        let derived = crate::tool_host::derive_lockdown_env(policy);
        // #388.2: the matrix channel's `--enforce-sandbox=false` dev opt-out
        // flows through here; make a sandbox-disabled persistent worker loud.
        crate::tool_host::warn_lockdown_overrides(program, &derived);
        let mut child = backend
            .spawn_under_policy(&derived, program, args)
            .map_err(|e| anyhow::anyhow!("spawn persistent worker: {e}"))?;
        // Drain the worker's piped stderr. The JSON-RPC client reads only
        // stdout; an undrained pipe is a deadlock risk past ~64 KiB.
        let pid = child.id();
        let stderr_tail = child
            .stderr
            .take()
            .map(|s| crate::worker_stderr::spawn_drain_with_tail(pid, s));
        let client = Client::from_child(child)
            .map_err(|e| anyhow::anyhow!("connect persistent worker: {e}"))?;
        Ok(Self { client, stderr_tail })
    }

    /// Wrap an ALREADY-CONNECTED client (no sandbox spawn) — the hermetic-test
    /// path over a plain child process.
    ///
    /// ⚠️ **No stderr tail ⇒ `death_report` returns `None` entirely** — not "a
    /// report carrying the exit status only", which is what this said until the
    /// #735 review read the code: `death_report`'s first statement is
    /// `self.stderr_tail.as_ref()?`, which short-circuits *before* the
    /// `try_wait()` that would supply the status. So a worker wrapped this way
    /// dies with no report on any channel. That is acceptable here because this
    /// constructor is the hermetic-test path, but it is a different statement
    /// from the one it replaced.
    pub fn from_client(client: Client) -> Self {
        Self { client, stderr_tail: None }
    }
}

impl PersistentTransport for ClientTransport {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.client
            .call(method, params)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn death_report(&mut self) -> Option<String> {
        let tail = collect_death_tail(self.stderr_tail.as_ref()?);
        // A SINGLE non-blocking reap, deliberately: unlike the drain above there
        // is no flag to wait on, so this would be an unbounded poll loop against
        // a slow-exiting VM launcher. `format_death_report` renders a `None`
        // status as "not yet reaped", so an un-exited child degrades the
        // message, not the supervisor's responsiveness.
        let status = self.client.try_wait().ok().flatten();
        Some(crate::worker_stderr::format_death_report(status, &tail))
    }
}

/// Wait for the stderr drainer to finish, then snapshot the tail.
///
/// ⚠️ **A free function, not an inlined pair of calls, so the wait is reachable
/// from a unit test.** The decision it encodes — *wait before snapshotting* —
/// otherwise lives inside `ClientTransport::death_report`, which needs a real
/// `Client` over a spawned child to construct, so no test could reach the
/// accepting branch and a mutant deleting the wait would survive. Extracting it
/// until a unit test can reach it is what the test below depends on.
fn collect_death_tail(tail_ring: &crate::worker_stderr::StderrTail) -> Vec<String> {
    // ⚠️ **Wait for the drainer before snapshotting, exactly as the
    // tool-worker path does** (`tool_host::warn_early_exit`). The worker has
    // already closed stdout, so its stderr is closing too; without this we
    // usually snapshot an EMPTY ring and report "no stderr captured" for a
    // worker that explained itself a millisecond later. That turns #730's
    // whole point — routing this report to a channel a human reads — into a
    // contentless line.
    //
    // ⚠️ **This costs the supervisor nothing, which is why the earlier
    // "would stall the driver" reasoning was wrong.** The wait is bounded,
    // and it returns the instant `mark_drained` lands (`wait_for_drain`
    // polls a flag every 2 ms, so the common case is ~2 ms, not the cap).
    // More decisively: the driver's very next act on this path is
    // `thread::sleep(backoff.next_delay(0))`, and BOTH production users
    // configure `base: Duration::from_secs(1)` (`channel::matrix`,
    // `channel::email`). The driver is about to sleep a full second
    // regardless — spending a fraction of it collecting the explanation
    // delays no respawn at all.
    tail_ring.wait_for_drain(DEATH_DRAIN_WAIT);
    tail_ring.snapshot()
}

impl Drop for ClientTransport {
    /// Reap the worker's child so it cannot survive as a zombie. A `Client`
    /// wraps a std `process::Child`, which is NOT reaped on drop — only an
    /// explicit `wait()` collects it. On the respawn path the worker is already
    /// dying (the driver detaches this drop to its own thread so the blocking
    /// wait never stalls the supervisor); on shutdown `--die-with-parent` takes
    /// it down. `kill()` is idempotent belt-and-suspenders for a worker whose
    /// pipe broke but whose process is still alive.
    fn drop(&mut self) {
        let _ = self.client.kill();
        let _ = self.client.wait();
    }
}

// ── PersistentWorker + PersistentHandle (unchanged below) ───────────────────

pub trait PersistentTransport: Send {
    fn call(&mut self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value>;
    fn death_report(&mut self) -> Option<String> { None }
}

pub type PersistentFactory =
    Box<dyn FnMut() -> anyhow::Result<Box<dyn PersistentTransport>> + Send>;

struct Job {
    method: String,
    params: serde_json::Value,
    reply: mpsc::Sender<anyhow::Result<serde_json::Value>>,
}

pub struct PersistentWorker;

pub struct PersistentHandle {
    req_tx: Option<mpsc::Sender<Job>>,
    driver: Option<thread::JoinHandle<()>>,
}

const ALARM_THRESHOLD: usize = 5;
const ALARM_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

impl PersistentWorker {
    pub fn spawn(label: impl Into<String>, factory: PersistentFactory) -> anyhow::Result<PersistentHandle> {
        Self::spawn_with_backoff(label, factory, RestartBackoff::default())
    }

    pub fn spawn_with_backoff(
        label: impl Into<String>,
        mut factory: PersistentFactory,
        backoff: RestartBackoff,
    ) -> anyhow::Result<PersistentHandle> {
        let label = label.into();
        let (req_tx, req_rx) = mpsc::channel::<Job>();
        let (init_tx, init_rx) = mpsc::channel::<anyhow::Result<()>>();
        let driver = thread::spawn(move || {
            // Initial spawn ON this persistent thread (PDEATHSIG parent).
            let mut transport = match factory() {
                Ok(t) => { let _ = init_tx.send(Ok(())); t }
                Err(e) => { let _ = init_tx.send(Err(e)); return; }
            };
            let mut alarm = RespawnRateAlarm::new(ALARM_WINDOW, ALARM_THRESHOLD);
            // Serve jobs; respawn on transport error.
            while let Ok(job) = req_rx.recv() {
                match transport.call(&job.method, job.params) {
                    Ok(v) => { let _ = job.reply.send(Ok(v)); }
                    Err(e) => {
                        // MINOR 1 fix: reply to the in-flight caller FIRST so a
                        // panicking death_report cannot prevent the reply.
                        let _ = job.reply.send(Err(e));
                        if let Some(r) = transport.death_report() {
                            // NOT a bare `tracing::warn!` (#730). This driver
                            // runs the Matrix and email channel workers, and
                            // 8 of the 9 `core/tests` suites that drive a
                            // `PersistentWorker` install no subscriber — so a
                            // `tracing`-only report is discarded in exactly
                            // the place a human is reading. The emitter owns
                            // both channels, the marker and the
                            // neutralisation; see
                            // `worker_stderr::emit_persistent_death_report`
                            // (the re-exported item — `worker_stderr::report`
                            // is a private module and resolves to nothing).
                            //
                            // ⚠️ The census is of `PersistentWorker::spawn*`
                            // suites, NOT the `dispatch` suites the
                            // early-exit emitter cites. The #735 review
                            // measured the two populations DISJOINT: no suite
                            // does both, so the dispatch figure — true as it
                            // is — would be evidence about tests that cannot
                            // reach this line.
                            crate::worker_stderr::emit_persistent_death_report(&label, &r);
                        }
                        // Respawn with backoff.  IMPORTANT fix: after each
                        // sleep/attempt we poll req_rx so that a concurrent
                        // shutdown() (which drops req_tx) is detected even
                        // when factory() keeps failing forever.
                        let mut restarts = 0u32;
                        loop {
                            let delay = backoff.next_delay(restarts);
                            thread::sleep(delay);

                            // Check for shutdown or queued jobs while the
                            // worker is down.
                            match req_rx.try_recv() {
                                Err(mpsc::TryRecvError::Disconnected) => {
                                    // All handles dropped → shutdown requested.
                                    tracing::info!(%label, "persistent worker: shutdown detected during respawn; exiting");
                                    return;
                                }
                                Ok(queued_job) => {
                                    // A caller arrived while we are still dead;
                                    // fail it immediately so it doesn't hang.
                                    let _ = queued_job.reply.send(
                                        Err(anyhow::anyhow!("persistent worker is restarting"))
                                    );
                                    // keep respawning
                                }
                                Err(mpsc::TryRecvError::Empty) => {
                                    // Nothing pending — proceed with factory attempt.
                                }
                            }

                            match factory() {
                                Ok(fresh) => {
                                    // Reap the dead worker's child OFF the driver
                                    // thread. `Client` wraps a std `Child`, which
                                    // is never reaped on drop — only an explicit
                                    // wait() collects it — and death_report's
                                    // best-effort try_wait can miss a slow-exiting
                                    // bwrap/VMM child, leaving a zombie (the leak
                                    // behind the recurring daemon zombies). Detach
                                    // the drop so ClientTransport::drop's blocking
                                    // kill()+wait() reaps without stalling respawn.
                                    let dead = std::mem::replace(&mut transport, fresh);
                                    thread::spawn(move || drop(dead));
                                    tracing::info!(%label, "persistent worker respawned");
                                    if let Some(n) = alarm.record(Instant::now()) {
                                        tracing::warn!(%label, respawns = n, "persistent worker respawn-rate alarm");
                                    }
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!(%label, error = %format!("{e:#}"), "respawn failed; backing off");
                                    restarts += 1;
                                }
                            }
                        }
                    }
                }
            }
            // req_tx dropped (shutdown): transport drops here via RAII.
            // MINOR 2 fix: removed redundant explicit drop(transport).
        });
        init_rx.recv()
            .map_err(|_| anyhow::anyhow!("persistent driver exited before initial spawn"))??;
        Ok(PersistentHandle { req_tx: Some(req_tx), driver: Some(driver) })
    }
}

impl PersistentHandle {
    pub fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.req_tx.as_ref().ok_or_else(|| anyhow::anyhow!("persistent worker shut down"))?
            .send(Job { method: method.to_string(), params, reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("persistent driver gone"))?;
        reply_rx.recv().map_err(|_| anyhow::anyhow!("persistent driver dropped reply"))?
    }

    pub fn shutdown(mut self) {
        self.req_tx.take(); // drop sender → driver loop exits → transport teardown
        if let Some(d) = self.driver.take() { let _ = d.join(); }
    }
}

impl Drop for PersistentHandle {
    fn drop(&mut self) {
        self.req_tx.take();
        if let Some(d) = self.driver.take() { let _ = d.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn collecting_a_death_tail_waits_for_the_drainer_instead_of_racing_it() {
        // The property #735 restored: a worker that has just closed its pipes
        // has usually NOT been drained yet, so snapshotting immediately yields
        // an empty ring and `format_death_report` renders "no stderr captured"
        // for a worker that explained itself a millisecond later. That is the
        // contentless line #730's whole point was to avoid.
        //
        // The drainer here lands its line AFTER this thread has already called
        // `collect_death_tail`, which is exactly the race in production. Deleting
        // the `wait_for_drain` makes this return empty and the test fails.
        let tail = crate::worker_stderr::StderrTail::new(8);
        let writer = tail.clone();
        let drain = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            crate::worker_stderr::drain_reader(
                0,
                std::io::Cursor::new(b"matrix sync failed, refusing to continue\n".to_vec()),
                Some(&writer),
            );
            writer.mark_drained();
        });

        let collected = collect_death_tail(&tail);
        drain.join().expect("the fixture's drain thread");

        assert_eq!(
            collected,
            vec!["matrix sync failed, refusing to continue".to_string()],
            "the dying worker's explanation must be collected, not raced — an empty tail here \
             is the `no stderr captured` line that made #730's fix contentless"
        );
    }

    #[test]
    fn collecting_a_death_tail_gives_up_rather_than_hanging_on_a_drainer_that_never_finishes() {
        // The other half: the wait is a CAP. A drainer that never marks itself
        // done (a worker still alive and chatting, a drain thread wedged) must
        // not stall the supervisor's driver — it degrades the message instead.
        // Without a bound this would hang the whole test binary.
        let tail = crate::worker_stderr::StderrTail::new(8);
        let started = Instant::now();
        let collected = collect_death_tail(&tail);
        let waited = started.elapsed();

        assert!(collected.is_empty(), "nothing was ever drained: {collected:?}");
        assert!(
            waited >= DEATH_DRAIN_WAIT,
            "it must actually wait the cap before giving up, or it is racing again: {waited:?}"
        );
        assert!(
            waited < DEATH_DRAIN_WAIT * 4,
            "the wait must be BOUNDED — the driver is about to respawn and cannot block on a \
             drainer that never finishes: {waited:?}"
        );
    }

    /// Fake transport that answers `die_after` calls, then errors (simulating
    /// worker death). Each spawn gets a fresh counter.
    struct FakeTransport { calls: usize, die_after: usize, gen: usize }
    impl PersistentTransport for FakeTransport {
        fn call(&mut self, _m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
            if self.calls >= self.die_after {
                anyhow::bail!("simulated worker death");
            }
            self.calls += 1;
            Ok(serde_json::json!({ "gen": self.gen, "n": self.calls }))
        }
    }

    fn fast_backoff() -> RestartBackoff {
        RestartBackoff { base: Duration::from_millis(1), factor_num: 1, factor_den: 1, cap: Duration::from_millis(1) }
    }

    #[test]
    fn serves_many_calls_on_one_worker() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let s = spawns.clone();
        let factory: PersistentFactory = Box::new(move || {
            let g = s.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransport { calls: 0, die_after: 1000, gen: g }))
        });
        let h = PersistentWorker::spawn("test", factory).unwrap();
        for _ in 0..5 {
            let v = h.call("ping", serde_json::json!({})).unwrap();
            assert_eq!(v["gen"], 0);
        }
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "no respawn while healthy");
        h.shutdown();
    }

    #[test]
    fn respawns_on_death_and_serves_again() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let s = spawns.clone();
        let factory: PersistentFactory = Box::new(move || {
            let g = s.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransport { calls: 0, die_after: 1, gen: g }))
        });
        let h = PersistentWorker::spawn_with_backoff("test", factory, fast_backoff()).unwrap();
        // gen 0 serves 1 call then dies on the 2nd
        assert_eq!(h.call("a", serde_json::json!({})).unwrap()["gen"], 0);
        assert!(h.call("b", serde_json::json!({})).is_err(), "in-flight call on death errors");
        // supervisor respawned → gen 1 serves.
        // Calls sent while the driver is still in the respawn loop are
        // rejected with "is restarting"; retry until the worker is up.
        let v = loop {
            match h.call("c", serde_json::json!({})) {
                Ok(v) => break v,
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        };
        assert_eq!(v["gen"], 1);
        assert!(spawns.load(Ordering::SeqCst) >= 2);
        h.shutdown();
    }

    #[test]
    fn call_after_shutdown_errors() {
        let factory: PersistentFactory = Box::new(|| Ok(Box::new(FakeTransport { calls: 0, die_after: 1000, gen: 0 })));
        let h = PersistentWorker::spawn("test", factory).unwrap();
        h.call("a", serde_json::json!({})).unwrap();
        h.shutdown();
        // a fresh handle can't be used post-shutdown — covered by the move semantics of shutdown(self).
    }

    /// Regression test: shutdown() must return promptly even when the driver is
    /// wedged in a perpetual respawn loop (factory always fails after the first
    /// successful spawn).  Before the fix the driver never polled req_rx during
    /// the respawn loop, so dropping req_tx in shutdown() had no effect and
    /// join() would block forever — this test would hang CI if the fix regresses.
    #[test]
    fn shutdown_returns_promptly_during_perpetual_respawn_loop() {
        // The factory succeeds exactly once (the initial spawn), then always errors.
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let sc = spawn_count.clone();
        let factory: PersistentFactory = Box::new(move || {
            let n = sc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // First call: supply a transport that dies immediately on any call.
                Ok(Box::new(FakeTransport { calls: 0, die_after: 0, gen: 0 }))
            } else {
                // All subsequent respawn attempts fail.
                anyhow::bail!("factory permanently broken")
            }
        });

        // Use a very fast backoff (1 ms) so the respawn loop spins quickly.
        let h = PersistentWorker::spawn_with_backoff("respawn-hang-test", factory, fast_backoff()).unwrap();

        // Trigger a worker death: the transport dies on the very first call.
        let _ = h.call("trigger-death", serde_json::json!({}));
        // Give the driver a moment to enter the perpetual respawn loop.
        thread::sleep(Duration::from_millis(20));

        // shutdown() must return within a generous but bounded time.
        // We verify this by running it in a separate thread with a timeout.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            h.shutdown();
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(Duration::from_secs(5))
            .expect("shutdown() hung — driver did not observe Disconnected during respawn loop");

        // Confirm the factory was called more than once (the loop really ran).
        assert!(spawn_count.load(Ordering::SeqCst) >= 2, "factory should have been retried");
    }

    /// Regression test for the zombie-reap leak: when the supervisor respawns, the
    /// dead transport must be DROPPED (its `Drop` is where `ClientTransport` reaps
    /// the worker's child — a std `Child` is never reaped on drop, so a missed drop
    /// is a leaked zombie). The drop is detached to its own thread, so allow a
    /// moment for it to run.
    #[test]
    fn respawn_drops_the_dead_transport() {
        struct ReapTracking { dropped: Arc<AtomicUsize>, calls: usize, die_after: usize }
        impl PersistentTransport for ReapTracking {
            fn call(&mut self, _m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
                if self.calls >= self.die_after { anyhow::bail!("simulated death"); }
                self.calls += 1;
                Ok(serde_json::json!({}))
            }
        }
        impl Drop for ReapTracking {
            fn drop(&mut self) { self.dropped.fetch_add(1, Ordering::SeqCst); }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        let d = dropped.clone();
        let factory: PersistentFactory = Box::new(move || {
            Ok(Box::new(ReapTracking { dropped: d.clone(), calls: 0, die_after: 1 }))
        });
        let h = PersistentWorker::spawn_with_backoff("reap-test", factory, fast_backoff()).unwrap();
        // First transport serves one call, dies on the second → triggers a respawn.
        let _ = h.call("a", serde_json::json!({}));
        let _ = h.call("b", serde_json::json!({}));
        // Drive a successful post-respawn call so we know the swap happened.
        loop {
            match h.call("c", serde_json::json!({})) {
                Ok(_) => break,
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        // The dead transport's detached drop should have run.
        let mut seen = 0;
        for _ in 0..100 {
            seen = dropped.load(Ordering::SeqCst);
            if seen >= 1 { break; }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(seen >= 1, "respawn must drop (and thus reap) the dead transport");
        h.shutdown();
    }

    /// #348 invariant: the initial factory() call — which forks the worker, so
    /// bwrap's --die-with-parent PDEATHSIG binds to the calling THREAD — must
    /// run on the persistent driver thread, never the (possibly ephemeral,
    /// e.g. tokio spawn_blocking) caller thread.
    #[test]
    fn initial_spawn_runs_on_the_driver_thread_not_the_caller() {
        let caller = thread::current().id();
        let (tid_tx, tid_rx) = mpsc::channel();
        let factory: PersistentFactory = Box::new(move || {
            let _ = tid_tx.send(thread::current().id());
            Ok(Box::new(FakeTransport { calls: 0, die_after: 1000, gen: 0 }))
        });
        let h = PersistentWorker::spawn("thread-parent-test", factory).unwrap();
        let spawn_thread = tid_rx.recv().unwrap();
        assert_ne!(spawn_thread, caller, "initial factory() must run on the driver thread (#348)");
        h.shutdown();
    }
}
