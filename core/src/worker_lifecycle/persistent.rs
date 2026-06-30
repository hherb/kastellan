//! Backend-agnostic supervisor for a LONG-LIVED worker: a persistent OS thread
//! owns the worker, forwards serialized RPC calls to it, and respawns it on
//! death (capped-exponential backoff + sliding-window rate alarm). PDEATHSIG-safe
//! (the spawning thread outlives the worker — required under the slice-5a
//! bwrap-confined launcher). A generalization of the Matrix channel's
//! `supervised_self_spawn`/`drive`, with no channel/poll-send coupling.
//!
//! Also houses [`ClientTransport`]: the production [`PersistentTransport`] impl
//! that wraps a real [`kastellan_protocol::client::Client`] over a sandboxed
//! worker's stdio, with stderr-tail death reporting (same pattern as the Matrix
//! channel's `ProtocolWorkerClient`).
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use kastellan_protocol::client::Client;
use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

use crate::channel::respawn_alarm::RespawnRateAlarm;
use crate::worker_lifecycle::RestartBackoff;

// ── ClientTransport ──────────────────────────────────────────────────────────

/// Production [`PersistentTransport`]: a JSON-RPC [`Client`] over a spawned
/// worker's stdio, with a bounded stderr-tail for death diagnostics.
///
/// Reuses the lockdown-env derivation + stderr-tail drain that the Matrix
/// channel's `spawn_worker_client` uses, without coupling to that module.
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
    /// `tool_host::spawn_worker` and `channel::matrix::spawn_worker_client` do,
    /// so the worker is locked down identically regardless of spawn path.
    pub fn spawn(
        backend: &dyn SandboxBackend,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> anyhow::Result<Self> {
        let derived = crate::tool_host::derive_lockdown_env(policy);
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
        // Snapshot the tail (non-blocking; the drain thread owns the push side).
        let tail = self.stderr_tail.as_ref()?.snapshot();
        // A SINGLE non-blocking reap. This runs on the supervisor's driver
        // thread, which cannot observe a concurrent shutdown() or start the
        // respawn while it is here — a poll loop with sleeps (the Matrix
        // channel's approach) would stall the driver up to half a second per
        // death just to enrich a log line, and a slow-exiting VM launcher would
        // hit the full stall every time. `format_death_report` already renders a
        // `None` status as "not yet reaped", so an un-exited child degrades the
        // message, not the supervisor's responsiveness.
        let status = self.client.try_wait().ok().flatten();
        Some(crate::worker_stderr::format_death_report(status, &tail))
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
                            tracing::warn!(%label, "persistent worker died: {r}");
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
                                    transport = fresh;
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
}
