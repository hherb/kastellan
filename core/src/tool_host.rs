//! tool_host: spawn sandboxed worker processes and talk to them over the
//! JSON-RPC stdio protocol from `hhagent_protocol`.
//!
//! The agent core is the only thing that ever spawns a worker. Spawning goes
//! through the configured [`SandboxBackend`] so workers cannot run unjailed
//! by accident — there is intentionally no "spawn unsandboxed" escape hatch.
//!
//! Phase 0 covers single-shot spawn-and-talk usage. Long-lived workers,
//! restart-on-crash supervision, and per-worker UDS multiplexing are
//! follow-on work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hhagent_protocol::client::{Client, ClientError};
use hhagent_sandbox::{Profile, SandboxBackend, SandboxError, SandboxPolicy};

#[derive(Debug, thiserror::Error)]
pub enum ToolHostError {
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] ClientError),
}

/// What to launch and how to jail it.
pub struct WorkerSpec<'a> {
    pub policy: &'a SandboxPolicy,
    /// Absolute path of the worker binary, as visible *inside* the jail.
    /// Caller must add the binary's host path (or its parent dir) to
    /// `policy.fs_read` so bwrap can mount it.
    pub program: &'a str,
    pub args: &'a [&'a str],
    /// Optional wall-clock budget (milliseconds) for the *entire* worker
    /// process lifetime. If set, [`spawn_worker`] starts a watchdog thread
    /// that sends SIGKILL once the deadline passes — unless the worker
    /// already exited or the returned [`SupervisedWorker`] was dropped /
    /// closed first (which cancels the watchdog).
    ///
    /// `None` disables the watchdog entirely; the worker is bounded only
    /// by external means (caller closing it, sandbox CPU/mem caps, etc.).
    pub wall_clock_ms: Option<u64>,
}

/// Env var name read by `hhagent-worker-prelude::landlock_lock` for the
/// JSON-encoded list of writable scratch paths. Workers using
/// `prelude::serve_stdio` get a Landlock filter built from this.
pub const ENV_LANDLOCK_RW: &str = "HHAGENT_LANDLOCK_RW";
/// Env var name read by `hhagent-worker-prelude::seccomp_lock` for the
/// per-worker seccomp profile selector.
pub const ENV_SECCOMP_PROFILE: &str = "HHAGENT_SECCOMP_PROFILE";

/// Spawn the worker under `backend` and return a [`SupervisedWorker`].
///
/// Before spawning, [`derive_lockdown_env`] augments the policy with the
/// `HHAGENT_LANDLOCK_RW` + `HHAGENT_SECCOMP_PROFILE` env entries that
/// `hhagent-worker-prelude` reads at worker start-up. This is the
/// chokepoint for the worker-side defence-in-depth layer: callers cannot
/// accidentally skip it because tool_host always derives the env, and
/// the worker installs the filters from inside its own process.
///
/// If `spec.wall_clock_ms` is `Some`, a watchdog thread is started that
/// SIGKILLs the worker once the budget elapses. The watchdog is cancelled
/// when the returned [`SupervisedWorker`] is dropped (or closed), so
/// well-behaved callers never see spurious kills.
pub fn spawn_worker<B>(
    backend: &B,
    spec: &WorkerSpec<'_>,
) -> Result<SupervisedWorker, ToolHostError>
where
    B: SandboxBackend + ?Sized,
{
    let derived = derive_lockdown_env(spec.policy);
    let child = backend.spawn_under_policy(&derived, spec.program, spec.args)?;
    let pid = child.id();
    let client = Client::from_child(child)?;
    let watchdog = spec.wall_clock_ms.map(|ms| spawn_watchdog(pid, ms));
    Ok(SupervisedWorker {
        client,
        _watchdog: watchdog,
    })
}

/// Owning handle to a spawned worker. Wraps the JSON-RPC [`Client`] and a
/// [`WatchdogGuard`] (when `wall_clock_ms` was set on the spec).
///
/// Field drop order matters: `client` is declared first so it drops first,
/// closing stdio pipes. `_watchdog` drops second, setting the watchdog's
/// cancel flag. The watchdog thread checks the flag at most every 50 ms
/// and exits without firing SIGKILL — so closing a worker normally never
/// produces a kill on a reused PID.
pub struct SupervisedWorker {
    client: Client,
    _watchdog: Option<WatchdogGuard>,
}

impl SupervisedWorker {
    /// Make one JSON-RPC call against the worker.
    pub fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        self.client.call(method, params)
    }

    /// Close stdin (signals EOF to the worker), wait for it to exit, and
    /// cancel the watchdog. Returns the worker's exit status.
    pub fn close(self) -> std::io::Result<std::process::ExitStatus> {
        // Destructure to move `client` out by value (consumed by `close`)
        // while leaving `_watchdog` to drop at end-of-scope, which sets
        // the cancel flag. Safe because [`SupervisedWorker`] has no
        // [`Drop`] impl, so partial moves are allowed.
        let SupervisedWorker {
            client,
            _watchdog: _drop_at_scope_end,
        } = self;
        client.close()
    }

    /// Forcefully kill the worker without waiting for graceful shutdown.
    /// The watchdog is cancelled by the [`Drop`] of [`Self`] (or
    /// [`Self::close`]).
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.client.kill()
    }
}

/// Cancellation handle for the watchdog thread. When this guard is
/// dropped, the watchdog thread observes the cancel flag on its next
/// poll and exits without sending SIGKILL.
struct WatchdogGuard {
    cancel: Arc<AtomicBool>,
}

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        // Release ordering pairs with the Acquire load inside the
        // watchdog loop.
        self.cancel.store(true, Ordering::Release);
    }
}

/// Spawn the watchdog thread for a single worker.
///
/// Polling cadence is 50 ms — fine-grained enough that a normal close
/// races ahead of any kill (cancel flag is checked once per tick), and
/// coarse enough that the thread is essentially free.
///
/// Sending SIGKILL to a PID is a fundamentally racy operation if the PID
/// can be reused; the cancel flag closes that race for the *normal* exit
/// path. For pathological cases (worker exits naturally exactly at the
/// deadline) a SIGKILL to ESRCH is harmless. We rely on the fact that
/// Linux/macOS PID reuse is not instantaneous: short polling intervals
/// plus the fast-cancel-on-drop close any practical window.
fn spawn_watchdog(pid: u32, wall_clock_ms: u64) -> WatchdogGuard {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();
    let deadline = Instant::now() + Duration::from_millis(wall_clock_ms);

    std::thread::Builder::new()
        .name(format!("hhagent-watchdog-{pid}"))
        .spawn(move || watchdog_loop(pid, deadline, cancel_clone, send_sigkill))
        .expect("spawn watchdog thread");

    WatchdogGuard { cancel }
}

/// Pure-ish helper that the watchdog thread runs. Extracted so the loop
/// body is unit-testable without spawning a thread (see tests below).
///
/// `kill` is injected so tests can verify the control flow (cancel-flag
/// handling, deadline observance) without ever reaching `kill(2)`.
/// Production passes [`send_sigkill`]; tests pass a no-op. This is a
/// load-bearing test isolation — see the [`send_sigkill`] doc comment
/// for the 2026-05-08 host-blackout incident that motivated it.
fn watchdog_loop(pid: u32, deadline: Instant, cancel: Arc<AtomicBool>, kill: fn(u32)) {
    let tick = Duration::from_millis(50);
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            // Re-check the cancel flag right before firing — closes the
            // race where Drop set the flag while we were in the
            // pre-deadline branch.
            if !cancel.load(Ordering::Acquire) {
                kill(pid);
            }
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(std::cmp::min(remaining, tick));
    }
}

/// Whether `pid` is a value we are willing to deliver a SIGKILL to.
///
/// `kill(2)` treats certain `pid_t` values as broadcast operations:
///   - `0`  → signal every process in the caller's process group
///   - `-1` → signal every process the caller has permission to signal
///            (excluding init and the caller itself)
///
/// The Rust API here takes a `u32`; `pid as libc::pid_t` (an `i32` on
/// both Linux and macOS) collapses any value `> i32::MAX` to a negative
/// `pid_t`. The worst case is `u32::MAX → -1`.
///
/// PID 1 is init / launchd. We never spawn it; refusing to signal it
/// catches caller-bookkeeping bugs cheaply.
fn is_valid_target_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// Best-effort SIGKILL. ESRCH (worker already exited) is the common case
/// after a natural close; we ignore it.
///
/// **Incident, 2026-05-08 (do not regress):** an earlier version of this
/// function called `libc::kill(pid as i32, SIGKILL)` with no validation.
/// The watchdog unit tests used `SAFE_FAKE_PID = u32::MAX` as a
/// "never-allocated PID" — but `u32::MAX as i32 == -1`, and
/// `kill(-1, SIGKILL)` signals every process the user can signal.
/// Running `watchdog_loop_runs_until_deadline_when_not_cancelled`
/// therefore SIGKILLed the user's X session, gnome-shell, and
/// per-session sshd children, producing what looked like a GPU-driver
/// display blackout. The fix is two-layered:
///   1. defensive guard here via [`is_valid_target_pid`] (PID 0, 1, and
///      anything that would cast to a negative `pid_t` are rejected),
///   2. an injected killer in [`watchdog_loop`] so tests never reach
///      `kill(2)` at all.
/// Do not remove either layer.
fn send_sigkill(pid: u32) {
    if !is_valid_target_pid(pid) {
        return;
    }
    // SAFETY: `kill(2)` with a valid `pid_t` and a signal number is a
    // defined syscall with no preconditions on Rust state. A bad PID
    // returns ESRCH which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// Pure transform: clone `policy` and append the worker-prelude lockdown
/// env entries that aren't already present. Callers that explicitly set
/// either env var win — useful in tests and for future per-worker overrides
/// (e.g. a probe worker that needs `HHAGENT_SECCOMP_PROFILE=none`).
///
/// Exposed for unit testing the env-derivation logic without spinning up
/// a real sandbox.
pub fn derive_lockdown_env(policy: &SandboxPolicy) -> SandboxPolicy {
    let mut out = policy.clone();
    let has_landlock = out.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RW);
    let has_seccomp = out.env.iter().any(|(k, _)| k == ENV_SECCOMP_PROFILE);

    if !has_landlock {
        let rw_paths: Vec<String> = out
            .fs_write
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // serde_json on a Vec<String> is infallible — `unwrap` is safe here.
        let json = serde_json::to_string(&rw_paths).unwrap();
        out.env.push((ENV_LANDLOCK_RW.into(), json));
    }
    if !has_seccomp {
        let value = match out.profile {
            Profile::WorkerStrict => "strict",
            Profile::WorkerNetClient => "net_client",
        };
        out.env.push((ENV_SECCOMP_PROFILE.into(), value.into()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_sandbox::Net;
    use std::path::PathBuf;

    fn base_policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_read: vec![],
            fs_write: vec![],
            net: Net::Deny,
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::WorkerStrict,
            env: vec![],
        }
    }

    #[test]
    fn derive_adds_strict_profile_for_default() {
        let derived = derive_lockdown_env(&base_policy());
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .expect("seccomp env must be derived");
        assert_eq!(seccomp.1, "strict");
    }

    #[test]
    fn derive_adds_net_client_profile() {
        let mut p = base_policy();
        p.profile = Profile::WorkerNetClient;
        let derived = derive_lockdown_env(&p);
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .unwrap();
        assert_eq!(seccomp.1, "net_client");
    }

    #[test]
    fn derive_serialises_fs_write_into_landlock_env() {
        let mut p = base_policy();
        p.fs_write = vec![PathBuf::from("/tmp/scratch_a"), PathBuf::from("/tmp/b")];
        let derived = derive_lockdown_env(&p);
        let landlock = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_LANDLOCK_RW)
            .unwrap();
        // Both paths must appear in the JSON. Exact-string assertion is OK
        // because serde_json on a Vec<String> is deterministic.
        assert_eq!(landlock.1, r#"["/tmp/scratch_a","/tmp/b"]"#);
    }

    #[test]
    fn derive_does_not_overwrite_caller_supplied_env() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "none".into()));
        let derived = derive_lockdown_env(&p);
        let seccomp_entries: Vec<_> = derived
            .env
            .iter()
            .filter(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .collect();
        assert_eq!(
            seccomp_entries.len(),
            1,
            "caller-supplied env must not be duplicated"
        );
        assert_eq!(seccomp_entries[0].1, "none");
    }

    /// Opaque PID for the watchdog tests. With the injected [`noop_kill`]
    /// it is never delivered to `kill(2)` — the value is just plumbed
    /// through `watchdog_loop` to exercise the control flow (cancel
    /// flag, deadline observance). The previous version of these tests
    /// used `u32::MAX` *and* a real `kill(2)`, which casts to `-1` and
    /// SIGKILLs every process the user owns; see the `send_sigkill` doc
    /// comment for the incident write-up.
    const SAFE_FAKE_PID: u32 = u32::MAX;

    /// No-op killer injected into [`watchdog_loop`] from tests.
    /// Whatever PID is passed in is simply discarded.
    fn noop_kill(_pid: u32) {}

    #[test]
    fn watchdog_loop_returns_immediately_when_cancelled_before_start() {
        let cancel = Arc::new(AtomicBool::new(true));
        let deadline = Instant::now() + Duration::from_secs(60);
        let started = Instant::now();
        watchdog_loop(SAFE_FAKE_PID, deadline, cancel, noop_kill);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "pre-cancelled watchdog must not wait for the deadline"
        );
    }

    #[test]
    fn watchdog_loop_runs_until_deadline_when_not_cancelled() {
        let cancel = Arc::new(AtomicBool::new(false));
        let budget = Duration::from_millis(150);
        let deadline = Instant::now() + budget;
        let started = Instant::now();
        watchdog_loop(SAFE_FAKE_PID, deadline, cancel, noop_kill);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= budget,
            "watchdog returned before deadline: elapsed={elapsed:?}, budget={budget:?}"
        );
        assert!(
            elapsed < budget + Duration::from_millis(200),
            "watchdog overshot the deadline by more than tick+slop: elapsed={elapsed:?}"
        );
    }

    #[test]
    fn watchdog_loop_observes_cancel_during_polling() {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let deadline = Instant::now() + Duration::from_secs(60);

        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            watchdog_loop(SAFE_FAKE_PID, deadline, cancel_clone, noop_kill)
        });
        // Give the loop time to enter its sleep, then cancel.
        std::thread::sleep(Duration::from_millis(30));
        cancel.store(true, Ordering::Release);
        handle.join().expect("watchdog thread joined");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "cancelled watchdog must exit on the next poll, not wait the full deadline"
        );
    }

    /// Regression test for the 2026-05-08 host-blackout incident.
    ///
    /// `is_valid_target_pid` must reject every PID value that `kill(2)`
    /// would interpret as a broadcast: `0` (caller's process group),
    /// `1` (init / launchd, never our worker), and anything `> i32::MAX`
    /// — all of which collapse to a non-positive `pid_t`. The worst
    /// offender is `u32::MAX`, which casts to `-1` (every process the
    /// user can signal). If this test ever turns red, the fanout
    /// regression has resurfaced — fix `send_sigkill`, do **not**
    /// loosen the validator.
    #[test]
    fn is_valid_target_pid_rejects_broadcast_values() {
        assert!(!is_valid_target_pid(0), "PID 0 = process group broadcast");
        assert!(!is_valid_target_pid(1), "PID 1 = init/launchd");
        assert!(
            !is_valid_target_pid(u32::MAX),
            "u32::MAX casts to pid_t -1 (everyone-the-user-can-signal broadcast)"
        );
        assert!(
            !is_valid_target_pid(i32::MAX as u32 + 1),
            "first u32 that casts to a negative pid_t must be rejected"
        );
        // Realistic worker PIDs accepted.
        assert!(is_valid_target_pid(2));
        assert!(is_valid_target_pid(12_345));
        assert!(is_valid_target_pid(i32::MAX as u32));
    }

    #[test]
    fn watchdog_guard_drop_sets_cancel_flag() {
        let cancel = Arc::new(AtomicBool::new(false));
        let guard = WatchdogGuard {
            cancel: cancel.clone(),
        };
        assert!(!cancel.load(Ordering::Acquire));
        drop(guard);
        assert!(
            cancel.load(Ordering::Acquire),
            "WatchdogGuard::Drop must set the cancel flag so the watchdog thread exits cleanly"
        );
    }
}
