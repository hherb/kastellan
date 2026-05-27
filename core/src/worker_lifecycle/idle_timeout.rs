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
        // Exhaustive on `ClientError` so any future variant added to `hhagent-protocol`
        // breaks the build here and forces a deliberate classification decision rather
        // than silently inheriting the "dead" default.
        Err(ToolHostError::Protocol(ClientError::Rpc(_))) => false,
        Err(ToolHostError::Protocol(ClientError::Io(_))) => true,
        Err(ToolHostError::Protocol(ClientError::Decode(_))) => true,
        Err(ToolHostError::Protocol(ClientError::EarlyExit)) => true,
        Err(ToolHostError::Protocol(ClientError::IdMismatch { .. })) => true,
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
    /// JoinHandle of the currently-scheduled idle-teardown task, if any.
    ///
    /// Issue #85 — the pre-fix shape spawned a fresh teardown task on every release
    /// without aborting prior ones. Stale tasks no-op'd correctly via the
    /// `last_completion` mismatch check, but at steady state under high request
    /// rate ~`idle_seconds` tasks per tool accumulated (e.g. one request per
    /// second with `idle_seconds = 60` → ~60 pending sleeper tasks per tool).
    /// Not a leak (they all eventually exited), but inefficient and confusing
    /// in tokio-console output.
    ///
    /// Now: [`replace_idle_teardown_handle`] is the single mutator. It aborts
    /// the prior handle (if any) before spawning a new one, holding the
    /// single-task-per-slot invariant at steady state. Calling `.abort()` on a
    /// JoinHandle whose task has already finished is a no-op (per tokio docs),
    /// so the self-firing-then-next-release case is fine.
    pub(crate) idle_teardown_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ToolState {
    pub(crate) fn fresh() -> Self {
        Self {
            warm: None,
            next_spawn_allowed_at: None,
            consecutive_restarts: 0,
            idle_teardown_handle: None,
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
///
/// # Warm-cache key invariant (issue #121)
///
/// The key is `tool_name` only — it does NOT include `ToolEntry.container_image`.
/// Today's safety relies on daemon-restart-flushes-registry: image tags are baked
/// into `ToolEntry` at startup and stay fixed for the daemon's lifetime, so two
/// `acquire` calls under the same tool name necessarily resolve to the same image.
///
/// A future live-reconfigure path (e.g. operator hot-reloads a `ToolEntry` with a
/// new `container_image` tag without restarting) would silently reuse the warm
/// worker spawned under the stale image tag. That widens the key, so any such
/// path MUST either:
///
/// 1. Widen the key to `(tool_name, container_image)` (and update every call
///    site of `slot_for`, plus the test `slot_for_key_excludes_container_image`
///    that pins this invariant), OR
/// 2. Explicitly evict the warm slot for the tool before serving requests
///    through the re-registered entry.
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
///      `tool_name` is the logical registry key (i.e. `PlannedStep::tool`), NOT the
///      binary basename — keying by basename would silently collide for two tools
///      whose binaries happen to share a `file_name` and is a security-relevant bug.
///   3. `slot.state.clone().lock_owned().await` — concurrent same-tool acquires
///      serialise here.
///   4. Honor `next_spawn_allowed_at` (restart backoff): sleep until allowed.
///   5. Warm-reuse if the slot has a worker and it's not aged out; otherwise spawn.
///   6. Build the handle holding the owned guard + the slot Arc.
pub(crate) async fn acquire_impl(
    sandbox: &dyn SandboxBackend,
    backoff: RestartBackoff,
    registry: &WarmRegistry,
    tool_name: &str,
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

    let slot = slot_for(registry, tool_name);
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
        abort_idle_teardown_handle(&mut guard);
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
        abort_idle_teardown_handle(&mut guard);
        guard.warm = None;
        guard.consecutive_restarts = 0;
        guard.next_spawn_allowed_at = None;
        return;
    }

    // Cap B: max_age_seconds (post-completion check). Same load-bearing invariant:
    // checked after the response was written, never mid-flight.
    if is_aged_out(spawned_at.elapsed(), caps.max_age_seconds) {
        drop(worker);
        abort_idle_teardown_handle(&mut guard);
        guard.warm = None;
        guard.consecutive_restarts = 0;
        guard.next_spawn_allowed_at = None;
        return;
    }

    // Successful return: put the worker back into the slot, refresh `last_completion`,
    // reset backoff counters, and rebind the idle-teardown task — aborting the prior
    // one so we keep exactly one pending teardown per slot (issue #85). The newly
    // spawned task's first await is `sleep(idle_seconds)`, so it does NOT contend
    // with the guard we still hold: by the time the task tries to lock the slot's
    // state, the guard is long since dropped at function exit.
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

    if let Some(slot) = slot {
        replace_idle_teardown_handle(&mut guard, slot, last_completion, idle_seconds);
    }
}

/// Abort the slot's pending idle-teardown task (if any) and clear its handle.
///
/// Cap-driven release paths (crash / max_requests / max_age) call this so the pending
/// teardown task — which would otherwise sit sleeping for `idle_seconds` and then
/// no-op via the `last_completion` check — is dropped immediately. Calling `.abort()`
/// on an already-finished JoinHandle is a no-op per tokio docs, so this is safe even
/// if the task happened to fire concurrently.
fn abort_idle_teardown_handle(state: &mut ToolState) {
    if let Some(handle) = state.idle_teardown_handle.take() {
        handle.abort();
    }
}

/// Abort any pending idle-teardown task and schedule a fresh one — the load-bearing
/// helper for issue #85.
///
/// Before this helper existed, every successful release path called `tokio::spawn` to
/// schedule a teardown task and *never aborted* prior ones. Under steady-state high
/// request rate this accumulated ~`idle_seconds` stale tasks per tool (e.g. one
/// request per second with `idle_seconds = 60` → ~60 sleepers per tool). They all
/// no-op'd correctly via the `last_completion` mismatch check, but the accumulation
/// was inefficient and confusing in tokio-console output.
///
/// Behaviour:
///   - If `idle_seconds == 0`, idle teardown is disabled; aborts any prior handle
///     and leaves `state.idle_teardown_handle` as `None`.
///   - Otherwise, aborts the prior handle (if any) and spawns a fresh teardown task
///     that sleeps for `idle_seconds`, then re-acquires the slot mutex and tears
///     down the warm worker iff its `last_completion` still matches the captured
///     value. The new JoinHandle is stored in `state.idle_teardown_handle` so the
///     next release can abort it.
///
/// The single-task-per-slot invariant — and its pin test in this module's `tests.rs`
/// — is the regression guarantee for issue #85.
fn replace_idle_teardown_handle(
    state: &mut ToolState,
    slot: Arc<ToolSlot>,
    for_last_completion: Instant,
    idle_seconds: u64,
) {
    abort_idle_teardown_handle(state);
    if idle_seconds == 0 {
        // 0 = idle teardown disabled; spec uses non-zero `idle_seconds` as the
        // canonical opt-in.
        return;
    }
    let delay = Duration::from_secs(idle_seconds);
    let handle = tokio::spawn(async move {
        sleep(delay).await;
        let mut state = slot.state.lock().await;
        // NOTE: state's `MutexGuard` derefs to `&mut ToolState`; `if let Some(warm) =
        // &state.warm` reads through the deref. Reassigning `state.warm = None` works
        // because the guard is `&mut`.
        if let Some(warm) = &state.warm {
            if warm.last_completion == for_last_completion {
                // Take + drop the warm worker. `SupervisedWorker`'s own Drop closes
                // stdio + cancels the watchdog; the OS reaps the zombie on next
                // spawn cycle. Also self-clear the handle — best-effort hygiene; a
                // subsequent release would overwrite it anyway, but keeping the slot
                // accurate while idle helps any future operator-visible inspector.
                state.warm = None;
                state.idle_teardown_handle = None;
            }
        }
    });
    state.idle_teardown_handle = Some(handle);
}

#[cfg(test)]
mod tests;
