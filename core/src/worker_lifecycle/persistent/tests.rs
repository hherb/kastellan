use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ----- client_error_to_anyhow (#674) -----

#[test]
fn an_rpc_refusal_stays_downcastable_and_renders_unchanged() {
    let rpc = kastellan_protocol::RpcError::new(kastellan_protocol::codes::UPSTREAM_AUTH_FAILED, "no");
    let flattened = format!("{}", ClientError::Rpc(rpc.clone()));
    let e = client_error_to_anyhow(ClientError::Rpc(rpc));
    let got = e.downcast_ref::<kastellan_protocol::RpcError>().expect("RpcError survives");
    assert_eq!(got.code, kastellan_protocol::codes::UPSTREAM_AUTH_FAILED);
    assert_eq!(format!("{e}"), flattened, "Display unchanged");
    assert_eq!(format!("{e:#}"), flattened, "alternate Display unchanged");
}

#[test]
fn every_other_client_error_renders_exactly_as_before() {
    let io = || ClientError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe gone"));
    let flattened = format!("{}", io());
    let e = client_error_to_anyhow(io());
    assert_eq!(format!("{e}"), flattened);
    // The case that decided the design: a preserved `Io` would print its
    // source a second time under `{e:#}`.
    assert_eq!(format!("{e:#}"), flattened, "no duplicated io text");
    assert!(e.downcast_ref::<kastellan_protocol::RpcError>().is_none());
    let early = client_error_to_anyhow(ClientError::EarlyExit);
    assert_eq!(format!("{early:#}"), "worker exited before responding");
}

#[test]
fn collecting_a_death_tail_gives_up_rather_than_hanging_on_a_drainer_that_never_finishes() {
    // The other half: the wait is a CAP. A drainer that never marks itself
    // done (a worker still alive and chatting, a drain thread wedged) must
    // not stall the supervisor's driver — it degrades the message instead.
    // Without a bound this would hang the whole test binary.
    let tail = crate::worker_stderr::StderrTail::new(8);
    let started = Instant::now();
    let collected = crate::worker_stderr::collect_tail_after_drain(&tail);
    let waited = started.elapsed();

    assert!(collected.lines().is_empty(), "nothing was ever drained: {collected:?}");
    assert!(
        waited >= crate::worker_stderr::TAIL_DRAIN_WAIT,
        "it must actually wait the cap before giving up, or it is racing again: {waited:?}"
    );
    assert!(
        waited < crate::worker_stderr::TAIL_DRAIN_WAIT * 4,
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

/// #674: the polled driver reads a credential refusal by downcasting the
/// error `PersistentHandle::call` returns. `client_error_to_anyhow` keeping
/// the type is not enough on its own — the driver thread and the reply
/// channel sit in between, and re-flattening there (`anyhow!("{e}")`, the
/// pre-#674 shape) would leave every unit test green while the ERROR
/// branch stopped firing. This crosses them. (A `.context(..)` would not
/// break it: anyhow's `downcast_ref` sees through context.)
#[test]
fn a_workers_rpc_refusal_reaches_the_handle_caller_still_typed() {
    struct Refusing;
    impl PersistentTransport for Refusing {
        fn call(&mut self, _m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
            let rpc = kastellan_protocol::upstream_auth_refusal("localmail", 401).unwrap();
            Err(client_error_to_anyhow(ClientError::Rpc(rpc)))
        }
    }
    let factory: PersistentFactory = Box::new(|| Ok(Box::new(Refusing)));
    let h = PersistentWorker::spawn_with_backoff("test", factory, fast_backoff()).unwrap();
    let e = h.call("email.poll", serde_json::json!({})).unwrap_err();
    let rpc = e.downcast_ref::<kastellan_protocol::RpcError>().expect("typed through the driver thread");
    assert_eq!(rpc.code, kastellan_protocol::codes::UPSTREAM_AUTH_FAILED);
    h.shutdown();
}

// ----- a refusal keeps the worker (#769) -----

/// A worker that refuses every `"no"` call with a structured `RpcError` and
/// answers everything else — alive throughout. Counts how often the driver
/// asked it for a death report, which it must never do for a refusal.
struct RefusesSome { gen: usize, death_reports: Arc<AtomicUsize> }
impl PersistentTransport for RefusesSome {
    fn call(&mut self, m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        if m == "no" {
            let rpc = kastellan_protocol::RpcError::new(kastellan_protocol::codes::OPERATION_FAILED, "send failed: nope");
            return Err(client_error_to_anyhow(ClientError::Rpc(rpc)));
        }
        Ok(serde_json::json!({ "gen": self.gen }))
    }
    fn death_report(&mut self) -> Option<String> {
        self.death_reports.fetch_add(1, Ordering::SeqCst);
        Some("should never be asked for".into())
    }
}

/// #769: a structured refusal comes from a worker that is alive and still
/// listening. Before the fix every refusal was treated as a death: a
/// `[worker-death]` report, a respawn, and — five refusals in five minutes —
/// a respawn-rate alarm. An expired email-channel credential read as a crash
/// loop. The refusal must reach the caller, and the SAME worker must serve
/// the next call.
#[test]
fn a_refusing_worker_is_kept_not_respawned() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let death_reports = Arc::new(AtomicUsize::new(0));
    let (s, d) = (spawns.clone(), death_reports.clone());
    let factory: PersistentFactory = Box::new(move || {
        let gen = s.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(RefusesSome { gen, death_reports: d.clone() }))
    });
    let h = PersistentWorker::spawn_with_backoff("test", factory, fast_backoff()).unwrap();
    // More refusals than ALARM_THRESHOLD: the old code would have respawned
    // on each one and raised the alarm.
    for _ in 0..ALARM_THRESHOLD + 2 {
        let e = h.call("no", serde_json::json!({})).unwrap_err();
        assert!(e.downcast_ref::<kastellan_protocol::RpcError>().is_some(), "the caller still sees the refusal: {e}");
    }
    // No retry loop: a respawn in progress would answer "is restarting".
    let v = h.call("yes", serde_json::json!({})).expect("the kept worker serves the next call at once");
    assert_eq!(v["gen"], 0, "still the first worker");
    assert_eq!(spawns.load(Ordering::SeqCst), 1, "a refusal must not respawn");
    assert_eq!(death_reports.load(Ordering::SeqCst), 0, "a refusal is not a death");
    h.shutdown();
}

/// The driver's rule and the idle-timeout lifecycle's census
/// (`WorkerRetirementCause::from_client_error`) must agree on every
/// `ClientError` variant, as it reaches the driver — i.e. after
/// `client_error_to_anyhow`. Before #769 they disagreed on exactly one: the
/// census called `Rpc` alive, the driver respawned on it. A new variant
/// breaks the census's exhaustive match first; this makes the driver follow.
#[test]
fn the_driver_retires_exactly_what_the_census_calls_fatal() {
    use crate::worker_lifecycle::idle_timeout::WorkerRetirementCause;
    let every_variant = || -> Vec<ClientError> {
        vec![
            ClientError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe gone")),
            ClientError::Decode(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
            ClientError::EarlyExit,
            ClientError::ResponseTooLarge { cap: 1 },
            ClientError::IdMismatch { expected: serde_json::json!(1), got: serde_json::json!(2) },
            ClientError::Rpc(kastellan_protocol::RpcError::new(kastellan_protocol::codes::OPERATION_FAILED, "no")),
        ]
    };
    let census: Vec<bool> = every_variant().iter().map(|v| WorkerRetirementCause::from_client_error(v).is_some()).collect();
    let driver: Vec<bool> = every_variant()
        .into_iter()
        .map(|v| call_error_retires_worker(&client_error_to_anyhow(v)))
        .collect();
    assert_eq!(driver, census);
    assert_eq!(census.iter().filter(|fatal| !**fatal).count(), 1, "exactly one survivable variant: Rpc");
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
