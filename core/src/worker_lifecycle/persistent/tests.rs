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

/// Retry `h.call(method)` until it succeeds — the worker is being respawned
/// underneath — but FAIL rather than hang if it never does: a keep/retire
/// mistake leaves a dead worker kept, which errors forever.
fn call_until_up(h: &PersistentHandle, method: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match h.call(method, serde_json::json!({})) {
            Ok(v) => return v,
            Err(e) => {
                assert!(Instant::now() < deadline, "the worker never came back: {e:#}");
                thread::sleep(Duration::from_millis(5));
            }
        }
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
    let v = call_until_up(&h, "c");
    assert_eq!(v["gen"], 1);
    assert!(spawns.load(Ordering::SeqCst) >= 2);
    h.shutdown();
}

/// #674: the polled driver reads a credential refusal by downcasting the
/// error `PersistentHandle::call` returns. `client_error_to_anyhow` keeping
/// the type is not enough on its own — the driver thread and the reply
/// channel sit in between, and re-flattening there (`anyhow!("{e}")`, the
/// pre-#674 shape) would leave every unit test green while the ERROR
/// branch stopped firing — and, since #769, while every refusal read as a
/// death to the driver, losing its refusal pacing. This crosses them. (A
/// `.context(..)` would not break it: anyhow's `downcast_ref` sees through
/// context.)
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

/// One sample of every `ClientError` variant. The `match` is the point: it
/// has no `_` arm, so a variant added to `kastellan-protocol` stops this file
/// compiling until it is listed here too — a hand-written list alone would
/// silently leave it out of the census test below.
fn every_client_error() -> Vec<ClientError> {
    let samples = vec![
        ClientError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe gone")),
        ClientError::Decode(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
        ClientError::EarlyExit,
        ClientError::ResponseTooLarge { cap: 1 },
        ClientError::IdMismatch { expected: serde_json::json!(1), got: serde_json::json!(2) },
        ClientError::Rpc(kastellan_protocol::RpcError::new(kastellan_protocol::codes::OPERATION_FAILED, "no")),
    ];
    let mut seen = [false; 6];
    for e in &samples {
        let i = match e {
            ClientError::Io(_) => 0,
            ClientError::Decode(_) => 1,
            ClientError::EarlyExit => 2,
            ClientError::ResponseTooLarge { .. } => 3,
            ClientError::IdMismatch { .. } => 4,
            ClientError::Rpc(_) => 5,
        };
        seen[i] = true;
    }
    assert!(seen.iter().all(|s| *s), "every variant has a sample: {seen:?}");
    samples
}

/// The supervisor's classifier and the idle-timeout lifecycle's census
/// (`WorkerRetirementCause::from_client_error`) must agree on which
/// `ClientError`s mean the worker is gone, as each reaches the classifier —
/// i.e. after `client_error_to_anyhow`. Before #769 they disagreed on exactly
/// one: the census called `Rpc` alive, the driver respawned on it.
#[test]
fn the_driver_retires_exactly_what_the_census_calls_fatal() {
    use crate::worker_lifecycle::idle_timeout::WorkerRetirementCause;
    let census: Vec<bool> =
        every_client_error().iter().map(|v| WorkerRetirementCause::from_client_error(v).is_some()).collect();
    let driver: Vec<bool> = every_client_error()
        .into_iter()
        .map(|v| classify_call_error(&client_error_to_anyhow(v)) == CallFailure::Gone)
        .collect();
    assert_eq!(driver, census);
    assert_eq!(census.iter().filter(|fatal| !**fatal).count(), 1, "exactly one survivable variant: Rpc");
}

#[test]
fn only_a_typed_credential_refusal_is_classified_as_one() {
    let credential = kastellan_protocol::upstream_auth_refusal("localmail", 401).unwrap();
    let flattened = anyhow::anyhow!("{credential}");
    let credential = client_error_to_anyhow(ClientError::Rpc(credential));
    assert_eq!(classify_call_error(&credential), CallFailure::CredentialRefused);
    let other = kastellan_protocol::RpcError::new(kastellan_protocol::codes::OPERATION_FAILED, "no");
    assert_eq!(classify_call_error(&client_error_to_anyhow(ClientError::Rpc(other))), CallFailure::Refused);
    assert_eq!(classify_call_error(&flattened), CallFailure::Gone, "a flattened refusal reads as a death");
    let wrapped = credential.context("while polling");
    assert_eq!(classify_call_error(&wrapped), CallFailure::CredentialRefused, "context does not hide it");
}

/// #782: an unreachable upstream is its own class, so the polled driver can
/// hold every conversation on it instead of charging one room's give-up.
#[test]
fn an_unavailable_upstream_is_classified_as_one_and_keeps_the_worker() {
    let down = kastellan_protocol::RpcError::new(kastellan_protocol::codes::UPSTREAM_UNAVAILABLE, "503");
    let down = client_error_to_anyhow(ClientError::Rpc(down));
    assert_eq!(classify_call_error(&down), CallFailure::Unavailable);
    assert_eq!(classify_call_error(&down.context("while sending")), CallFailure::Unavailable);
}

// ----- a credential refusal replaces the worker; a dead sidecar retires it (#769 review) -----

/// A live worker that refuses `"login"` with `UPSTREAM_AUTH_FAILED` and
/// answers everything else, optionally with a dead sidecar. Counts the death
/// reports it is asked for.
struct Credentialed { gen: usize, sidecar_dead: bool, death_reports: Arc<AtomicUsize> }
impl PersistentTransport for Credentialed {
    fn call(&mut self, m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        match m {
            "login" => Err(client_error_to_anyhow(ClientError::Rpc(
                kastellan_protocol::upstream_auth_refusal("localmail", 401).unwrap(),
            ))),
            "no" => Err(client_error_to_anyhow(ClientError::Rpc(kastellan_protocol::RpcError::new(
                kastellan_protocol::codes::OPERATION_FAILED,
                "transport: connect proxy uds: refused",
            )))),
            _ => Ok(serde_json::json!({ "gen": self.gen })),
        }
    }
    fn death_report(&mut self) -> Option<String> {
        self.death_reports.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn sidecar_exited(&mut self) -> Option<String> {
        self.sidecar_dead.then(|| "egress sidecar exited (exit status: 1)".to_string())
    }
}

/// Spawn a supervisor over [`Credentialed`] workers. `factory_ok(n)` says
/// whether the n-th spawn (0-based) succeeds; `sidecar_dead(n)` whether that
/// worker's sidecar is dead. Returns the handle, the spawn count and the
/// death-report count.
fn spawn_credentialed(
    factory_ok: fn(usize) -> bool,
    sidecar_dead: fn(usize) -> bool,
) -> (PersistentHandle, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let spawns = Arc::new(AtomicUsize::new(0));
    let death_reports = Arc::new(AtomicUsize::new(0));
    let (s, d) = (spawns.clone(), death_reports.clone());
    let factory: PersistentFactory = Box::new(move || {
        let n = s.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(factory_ok(n), "spawn {n} refused by the test");
        Ok(Box::new(Credentialed { gen: n, sidecar_dead: sidecar_dead(n), death_reports: d.clone() }))
    });
    let h = PersistentWorker::spawn_with_backoff("test", factory, fast_backoff()).unwrap();
    (h, spawns, death_reports)
}

/// A worker reads its credential once, at spawn. Before #769 every credential
/// refusal respawned it, so a renewed token was picked up; keeping the worker
/// would keep the stale token until the daemon restarts. So a credential
/// refusal still gets a fresh worker — but quietly: the caller sees the typed
/// refusal (the driver's ERROR and backoff depend on it), and there is no
/// death report, no "is restarting" window and no alarm.
#[test]
fn a_credential_refusal_replaces_the_worker_quietly() {
    let (h, spawns, death_reports) = spawn_credentialed(|_| true, |_| false);
    for n in 1..=ALARM_THRESHOLD + 2 {
        let e = h.call("login", serde_json::json!({})).unwrap_err();
        let rpc = e.downcast_ref::<kastellan_protocol::RpcError>().expect("still typed for the driver");
        assert_eq!(rpc.code, kastellan_protocol::codes::UPSTREAM_AUTH_FAILED);
        // No retry loop: the replacement is in place before the next call.
        let v = h.call("yes", serde_json::json!({})).expect("the fresh worker serves at once");
        assert_eq!(v["gen"], n, "a fresh worker after each credential refusal");
    }
    assert_eq!(spawns.load(Ordering::SeqCst), ALARM_THRESHOLD + 3, "the first worker and one per refusal");
    assert_eq!(death_reports.load(Ordering::SeqCst), 0, "a credential refusal is not a death");
    h.shutdown();
}

/// If no fresh worker can be started, the running one — alive, merely
/// refused — is kept rather than torn down into a respawn loop.
#[test]
fn a_credential_refusal_keeps_the_worker_when_no_fresh_one_starts() {
    let (h, spawns, death_reports) = spawn_credentialed(|n| n == 0, |_| false);
    h.call("login", serde_json::json!({})).unwrap_err();
    let v = h.call("yes", serde_json::json!({})).expect("the running worker is kept");
    assert_eq!(v["gen"], 0);
    assert_eq!(spawns.load(Ordering::SeqCst), 2, "one replacement was attempted");
    assert_eq!(death_reports.load(Ordering::SeqCst), 0);
    h.shutdown();
}

/// A worker whose egress sidecar died is alive, but every upstream request it
/// makes fails and it says so — a refusal, which on its own would keep the
/// pair cut off for good. The supervisor asks the transport, and a dead
/// sidecar retires the worker with it: the caller reads a death (the refusal
/// flattened), and the respawned pair serves.
#[test]
fn a_refusal_from_behind_a_dead_sidecar_retires_the_worker() {
    // Workers 0 and 1 sit behind a dead sidecar; worker 2's is healthy.
    let (h, spawns, _) = spawn_credentialed(|_| true, |n| n < 2);
    // Any refusal — a credential one too, which would otherwise be a quiet
    // replacement rather than a death.
    for (i, method) in ["no", "login"].into_iter().enumerate() {
        let e = h.call(method, serde_json::json!({})).unwrap_err();
        assert!(e.downcast_ref::<kastellan_protocol::RpcError>().is_none(), "reads as a death, not a refusal: {e}");
        assert!(format!("{e}").contains("egress sidecar exited"), "{e}");
        let v = call_until_up(&h, "yes");
        assert_eq!(v["gen"], i + 1, "the pair was respawned");
    }
    assert_eq!(spawns.load(Ordering::SeqCst), 3);
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
    call_until_up(&h, "c");
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
