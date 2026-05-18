//! Idle-timeout lifecycle runtime — slice 2.
//!
//! Spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
//! Plan: `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-2.md`.
//!
//! Slice 2 fills in `IdleTimeoutLifecycle::acquire` (the slice-1 stub) with the
//! warm-cache runtime: spawn-on-demand, post-completion cap evaluation, idle teardown,
//! crash detection, exponential restart backoff, and request serialisation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use hhagent_protocol::client::ClientError;
use hhagent_sandbox::SandboxBackend;
use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};
use tokio::time::sleep;

use crate::scheduler::tool_dispatch::ToolEntry;
use crate::tool_host::{spawn_worker, SupervisedWorker, ToolHostError, WorkerSpec};
use crate::worker_lifecycle::manager::WorkerHandle;
use crate::worker_lifecycle::types::{IdleTimeoutCaps, Lifecycle};

/// Exponential restart-backoff calculator.
///
/// `next_delay(n)` is the cooldown between restart attempts — applied to *spawn*, not
/// to dispatch. Sequence (in seconds): `1, 2, 4, 8, 16, 32, 60, 60, …`. Resets to 0 on
/// any successful dispatch. Defaults match the spec's "Open questions" §3 recommendation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartBackoff {
    /// Base delay for the first restart (default 1 s).
    pub base: Duration,
    /// Multiplicative factor between restarts (default 2 — exponential).
    /// Stored as integer numerator/denominator to keep the type `Eq`/`Hash`-friendly.
    pub factor_num: u32,
    pub factor_den: u32,
    /// Maximum delay regardless of restart count (default 60 s).
    pub cap: Duration,
}

impl Default for RestartBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            factor_num: 2,
            factor_den: 1,
            cap: Duration::from_secs(60),
        }
    }
}

impl RestartBackoff {
    /// Pure: compute the next delay after `consecutive_restarts` restarts have already
    /// happened. `consecutive_restarts = 0` returns `base`; each subsequent value
    /// multiplies by `factor_num/factor_den`, capped at `cap`. Saturating on overflow.
    pub fn next_delay(&self, consecutive_restarts: u32) -> Duration {
        let base_ms = self.base.as_millis() as u64;
        let factor_num = self.factor_num.max(1) as u64;
        let factor_den = self.factor_den.max(1) as u64;
        let cap_ms = self.cap.as_millis() as u64;
        let mut delay_ms = base_ms;
        for _ in 0..consecutive_restarts {
            delay_ms = delay_ms.saturating_mul(factor_num) / factor_den;
            if delay_ms >= cap_ms {
                return self.cap;
            }
        }
        Duration::from_millis(delay_ms.min(cap_ms))
    }
}

/// Pure: classify a dispatch error as "worker died" or "worker still alive".
///
/// The spec's "Cap-check semantics" §"Mid-flight termination" §2 says a worker process
/// reported dead by the OS triggers restart; v1 slice 2 detects death *passively* on
/// the next dispatch attempt, classifying error variants:
///
/// | Variant                                          | Classification |
/// | ------------------------------------------------ | -------------- |
/// | `Ok(_)`                                          | alive          |
/// | `Err(Sandbox(_))`                                | n/a (no worker exists; pre-spawn) |
/// | `Err(Io(_))`                                     | dead           |
/// | `Err(Protocol(Rpc(_)))`                          | alive (worker rejected the call) |
/// | `Err(Protocol(Io(_)))`                           | dead           |
/// | `Err(Protocol(Decode(_)))`                       | dead           |
/// | `Err(Protocol(EarlyExit))`                       | dead           |
/// | `Err(Protocol(IdMismatch { .. }))`               | dead           |
pub fn dispatch_indicates_worker_dead<T>(result: &Result<T, ToolHostError>) -> bool {
    match result {
        Ok(_) => false,
        Err(ToolHostError::Sandbox(_)) => false, // pre-spawn; no worker to be dead
        Err(ToolHostError::Io(_)) => true,
        Err(ToolHostError::Protocol(ClientError::Rpc(_))) => false,
        Err(ToolHostError::Protocol(_)) => true,
    }
}

/// Pure: has this warm worker hit `max_requests`?
///
/// `max_requests == 0` disables the cap (the canonical "0 = unlimited" idiom used by
/// `cpu_quota_pct`/`tasks_max` in `SandboxPolicy`).
pub fn is_request_capped(request_count: u64, max_requests: u64) -> bool {
    max_requests > 0 && request_count >= max_requests
}

/// Pure: has this warm worker exceeded `max_age_seconds`?
///
/// `max_age_seconds == 0` disables the cap.
pub fn is_aged_out(age: Duration, max_age_seconds: u64) -> bool {
    max_age_seconds > 0 && age.as_secs() >= max_age_seconds
}

// --- Runtime types (slice-2 task 2) -----------------------------------------

/// Per-tool slot wrapping `ToolState` in a tokio mutex.
///
/// Held by `Arc` so the warm-cache map can hand out cheap clones for new requests on
/// the same tool. The inner `Arc<TokioMutex<ToolState>>` is what `lock_owned()` needs
/// to produce an `OwnedMutexGuard` (the guard `WorkerHandle::IdleTimeout` carries from
/// `acquire` through `Drop`). The tokio mutex serialises concurrent requests for the
/// same tool — matches the spec's v1 single-threaded contract.
pub(crate) struct ToolSlot {
    pub(crate) state: Arc<TokioMutex<ToolState>>,
}

/// State the supervisor tracks per warm-keeping tool.
pub(crate) struct ToolState {
    /// `Some` while the worker is warm and idle; `None` while a request is in flight
    /// or after a teardown.
    pub(crate) warm: Option<WarmWorker>,
    /// Wall-clock instant before which the next spawn is *not* allowed (restart
    /// backoff). `None` means "spawn is allowed immediately".
    pub(crate) next_spawn_allowed_at: Option<Instant>,
    /// Counter that drives `RestartBackoff::next_delay`. Increments on every crash;
    /// resets to 0 on every successful dispatch.
    pub(crate) consecutive_restarts: u32,
}

impl ToolState {
    pub(crate) fn fresh() -> Self {
        Self {
            warm: None,
            next_spawn_allowed_at: None,
            consecutive_restarts: 0,
        }
    }
}

/// A warm `SupervisedWorker` plus the bookkeeping the cap evaluators need.
///
/// Note: caps come from `entry.lifecycle` on each `acquire_impl` call, so we don't
/// store them on the warm worker — the entry is the single source of truth, and
/// operator-side caps changes propagate naturally on the next acquire.
pub(crate) struct WarmWorker {
    pub(crate) worker: SupervisedWorker,
    pub(crate) spawned_at: Instant,
    pub(crate) request_count: u64,
    pub(crate) last_completion: Instant,
}

/// Outer warm-cache registry. Keys are tool names (matches the registry in
/// `scheduler::tool_dispatch::ToolRegistry`).
pub(crate) type WarmRegistry = Arc<StdMutex<HashMap<String, Arc<ToolSlot>>>>;

/// Construct a fresh, empty registry.
pub(crate) fn empty_registry() -> WarmRegistry {
    Arc::new(StdMutex::new(HashMap::new()))
}

/// Get or create the slot for `tool_name`. The outer `std` mutex is held very briefly
/// (just the `HashMap::entry` call) so there's no contention even under load.
pub(crate) fn slot_for(registry: &WarmRegistry, tool_name: &str) -> Arc<ToolSlot> {
    let mut map = registry.lock().expect("warm-registry mutex poisoned");
    Arc::clone(map.entry(tool_name.to_string()).or_insert_with(|| {
        Arc::new(ToolSlot {
            state: Arc::new(TokioMutex::new(ToolState::fresh())),
        })
    }))
}

// --- Acquire path (slice-2 task 4) ------------------------------------------

/// Implementation of `IdleTimeoutLifecycle::acquire`. Public-in-crate so the
/// `manager.rs` facade can delegate without exposing the runtime types.
///
/// Flow:
///   1. Pull caps from entry's `Lifecycle::IdleTimeout`; reject `SingleUse` entries
///      as wiring bugs (return `Io(InvalidInput)` so the dispatcher's
///      `step.spawn_failed` audit row still fires).
///   2. `slot_for(registry, tool_name)` looks up or creates the per-tool slot.
///   3. `slot.state.clone().lock_owned().await` — concurrent same-tool acquires
///      serialise here.
///   4. Honor `next_spawn_allowed_at` (restart backoff): sleep until allowed.
///   5. Warm-reuse if the slot has a worker and it's not aged out; otherwise spawn.
///   6. Build the handle holding the owned guard + the slot Arc.
pub(crate) async fn acquire_impl(
    sandbox: &dyn SandboxBackend,
    backoff: RestartBackoff,
    registry: &WarmRegistry,
    entry: &ToolEntry,
) -> Result<WorkerHandle, ToolHostError> {
    let caps = match &entry.lifecycle {
        Lifecycle::IdleTimeout { caps, contract: _ } => caps.clone(),
        Lifecycle::SingleUse => {
            return Err(ToolHostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IdleTimeoutLifecycle::acquire called on a SingleUse ToolEntry — wiring bug",
            )));
        }
    };

    let tool_name = entry
        .binary
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.binary.to_string_lossy().into_owned());

    let slot = slot_for(registry, &tool_name);
    let mut guard = Arc::clone(&slot.state).lock_owned().await;

    // Honor restart backoff. If `next_spawn_allowed_at` is in the future, sleep until
    // it elapses. Reset the gate once we've waited so the next caller doesn't re-wait
    // (a fresh crash will set it again on release).
    if let Some(allowed_at) = guard.next_spawn_allowed_at {
        let now = Instant::now();
        if allowed_at > now {
            let to_sleep = allowed_at - now;
            sleep(to_sleep).await;
        }
        guard.next_spawn_allowed_at = None;
    }

    // Warm-reuse path.
    if let Some(existing) = guard.warm.take() {
        if !is_aged_out(existing.spawned_at.elapsed(), caps.max_age_seconds) {
            let spawned_at = existing.spawned_at;
            let request_count_so_far = existing.request_count;
            return Ok(WorkerHandle::idle_timeout(
                existing.worker,
                guard,
                Arc::clone(&slot),
                spawned_at,
                request_count_so_far,
                caps,
                backoff,
            ));
        }
        // Aged out — drop the worker (terminates) and fall through to spawn fresh.
        drop(existing.worker);
    }

    // Cold-spawn path.
    let policy = entry.policy.clone();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let worker = spawn_worker(sandbox, &spec)?;
    let spawned_at = Instant::now();
    Ok(WorkerHandle::idle_timeout(
        worker,
        guard,
        Arc::clone(&slot),
        spawned_at,
        0,
        caps,
        backoff,
    ))
}

// --- Release path (slice-2 tasks 5 + 6 + 7) ---------------------------------

/// Release path for `WorkerHandle::Drop` on an idle-timeout handle.
///
/// Implements the spec's "Cap-check semantics" §"Post-completion check":
///   - `died = true` → drop the worker (terminates) and bump backoff via
///     `RestartBackoff::next_delay` so the next acquire waits.
///   - `request_count + 1 >= max_requests` → drop worker (max-requests cap).
///   - `(now - spawned_at) >= max_age_seconds` → drop worker (max-age cap).
///   - Otherwise → put worker back into slot with refreshed `last_completion`,
///     spawn a one-shot idle-teardown task that fires after `idle_seconds`.
///
/// Successful release also resets `consecutive_restarts = 0` — one clean dispatch
/// is enough to restart the backoff sequence from base.
#[allow(clippy::too_many_arguments)]
pub(crate) fn release_idle_timeout_worker(
    worker: Option<SupervisedWorker>,
    mut guard: OwnedMutexGuard<ToolState>,
    slot: Option<Arc<ToolSlot>>,
    spawned_at: Instant,
    request_count_so_far: u64,
    caps: IdleTimeoutCaps,
    died: bool,
    backoff: RestartBackoff,
) {
    let Some(worker) = worker else {
        // Worker was already moved out; nothing to return. Should not fire in
        // practice — the Drop impl always passes Some — but a missing worker is
        // strictly safer than a panic in Drop.
        return;
    };

    // Crash branch — bump backoff, clear slot.
    if died {
        drop(worker);
        guard.warm = None;
        let next_count = guard.consecutive_restarts.saturating_add(1);
        let delay = backoff.next_delay(next_count.saturating_sub(1));
        guard.consecutive_restarts = next_count;
        guard.next_spawn_allowed_at = Some(Instant::now() + delay);
        return;
    }

    let new_count = request_count_so_far + 1;

    // Cap A: max_requests (post-completion check). Spec §"The two policies"
    // §"idle_timeout".
    if is_request_capped(new_count, caps.max_requests) {
        drop(worker);
        guard.warm = None;
        guard.consecutive_restarts = 0;
        guard.next_spawn_allowed_at = None;
        return;
    }

    // Cap B: max_age_seconds (post-completion check). Same load-bearing invariant:
    // checked after the response was written, never mid-flight.
    if is_aged_out(spawned_at.elapsed(), caps.max_age_seconds) {
        drop(worker);
        guard.warm = None;
        guard.consecutive_restarts = 0;
        guard.next_spawn_allowed_at = None;
        return;
    }

    // Successful return: put the worker back into the slot, refresh `last_completion`,
    // reset backoff counters. Spawn an idle-teardown task only after the guard drops
    // so the task doesn't immediately fight us for the mutex.
    let last_completion = Instant::now();
    let idle_seconds = caps.idle_seconds;
    guard.warm = Some(WarmWorker {
        worker,
        spawned_at,
        request_count: new_count,
        last_completion,
    });
    guard.consecutive_restarts = 0;
    guard.next_spawn_allowed_at = None;
    drop(guard);

    if let Some(slot) = slot {
        schedule_idle_teardown(slot, last_completion, idle_seconds);
    }
}

/// Spawn a one-shot teardown task that fires `idle_seconds` after `for_last_completion`.
///
/// The task re-acquires the slot's mutex, compares `state.warm`'s `last_completion`
/// against the captured value; if they match the worker has been idle since this
/// release and is torn down (drop terminates the inner `SupervisedWorker`). If they
/// differ, a newer request bumped the timestamp and this task is a no-op.
///
/// Multiple stale teardown tasks coexist harmlessly: only the newest one's captured
/// `last_completion` matches the current slot state.
fn schedule_idle_teardown(slot: Arc<ToolSlot>, for_last_completion: Instant, idle_seconds: u64) {
    if idle_seconds == 0 {
        // 0 = idle teardown disabled; spec uses non-zero `idle_seconds` as the
        // canonical opt-in.
        return;
    }
    let delay = Duration::from_secs(idle_seconds);
    tokio::spawn(async move {
        sleep(delay).await;
        let mut state = slot.state.lock().await;
        // NOTE: state's `MutexGuard` derefs to `&mut ToolState`; `if let Some(warm) =
        // &state.warm` reads through the deref. Reassigning `state.warm = None` works
        // because the guard is `&mut`.
        if let Some(warm) = &state.warm {
            if warm.last_completion == for_last_completion {
                // Take + drop the warm worker. `SupervisedWorker`'s own Drop closes
                // stdio + cancels the watchdog; the OS reaps the zombie on next
                // spawn cycle.
                state.warm = None;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_protocol::RpcError;
    use std::io;

    #[test]
    fn restart_backoff_default_starts_at_one_second() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(0), Duration::from_secs(1));
    }

    #[test]
    fn restart_backoff_default_doubles_per_step() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(0), Duration::from_secs(1));
        assert_eq!(bo.next_delay(1), Duration::from_secs(2));
        assert_eq!(bo.next_delay(2), Duration::from_secs(4));
        assert_eq!(bo.next_delay(3), Duration::from_secs(8));
        assert_eq!(bo.next_delay(4), Duration::from_secs(16));
        assert_eq!(bo.next_delay(5), Duration::from_secs(32));
    }

    #[test]
    fn restart_backoff_caps_at_default_60s() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(6), Duration::from_secs(60));
        assert_eq!(bo.next_delay(100), Duration::from_secs(60));
        // Saturating on overflow — even u32::MAX is bounded by cap.
        assert_eq!(bo.next_delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn restart_backoff_custom_cap_honoured() {
        let bo = RestartBackoff {
            base: Duration::from_millis(500),
            factor_num: 2,
            factor_den: 1,
            cap: Duration::from_secs(5),
        };
        assert_eq!(bo.next_delay(0), Duration::from_millis(500));
        assert_eq!(bo.next_delay(1), Duration::from_secs(1));
        assert_eq!(bo.next_delay(2), Duration::from_secs(2));
        assert_eq!(bo.next_delay(3), Duration::from_secs(4));
        assert_eq!(bo.next_delay(4), Duration::from_secs(5));
        assert_eq!(bo.next_delay(10), Duration::from_secs(5));
    }

    #[test]
    fn dispatch_classifier_ok_is_alive() {
        let r: Result<(), ToolHostError> = Ok(());
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_rpc_error_is_alive() {
        // Worker returned a structured RPC error; it's still listening on stdio.
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(
            ClientError::Rpc(RpcError {
                code: -32001,
                message: "POLICY_DENIED".into(),
                data: None,
            }),
        ));
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_io_error_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "stdio closed",
        )));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_protocol_io_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(ClientError::Io(
            io::Error::new(io::ErrorKind::UnexpectedEof, "eof"),
        )));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_early_exit_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(
            ClientError::EarlyExit,
        ));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_sandbox_is_not_a_warm_worker_crash() {
        // Sandbox errors come from a failed spawn — no worker existed; this is the
        // SPAWN_FAILED path, not a warm-worker crash. The classifier returns false so
        // the restart-backoff counter doesn't increment.
        let r: Result<(), ToolHostError> = Err(ToolHostError::Sandbox(
            hhagent_sandbox::SandboxError::Backend("test".into()),
        ));
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn is_request_capped_at_threshold() {
        assert!(!is_request_capped(0, 3));
        assert!(!is_request_capped(2, 3));
        assert!(is_request_capped(3, 3));
        assert!(is_request_capped(99, 3));
    }

    #[test]
    fn is_request_capped_zero_max_means_unlimited() {
        // A zero `max_requests` disables the cap (matches the "0 = unlimited" idiom
        // used by `cpu_quota_pct`/`tasks_max` in `SandboxPolicy`).
        assert!(!is_request_capped(u64::MAX, 0));
    }

    #[test]
    fn is_aged_out_at_threshold() {
        assert!(!is_aged_out(Duration::from_secs(9), 10));
        assert!(is_aged_out(Duration::from_secs(10), 10));
        assert!(is_aged_out(Duration::from_secs(11), 10));
    }

    #[test]
    fn is_aged_out_zero_max_means_unlimited() {
        assert!(!is_aged_out(Duration::from_secs(u64::MAX / 2), 0));
    }
}
