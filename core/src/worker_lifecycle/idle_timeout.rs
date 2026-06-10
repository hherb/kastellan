//! Idle-timeout lifecycle runtime — slice 2.
//!
//! Spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
//! Plan: `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-2.md`.
//!
//! Slice 2 fills in `IdleTimeoutLifecycle::acquire` (the slice-1 stub) with the
//! warm-cache runtime: spawn-on-demand, post-completion cap evaluation, idle teardown,
//! crash detection, exponential restart backoff, and request serialisation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kastellan_protocol::client::ClientError;
use kastellan_sandbox::SandboxBackend;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::sleep;

use crate::scheduler::tool_dispatch::ToolEntry;
use crate::tool_host::{spawn_worker, SupervisedWorker, ToolHostError, WorkerSpec};
use crate::worker_lifecycle::manager::WorkerHandle;
use crate::worker_lifecycle::types::Lifecycle;

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
        // Exhaustive on `ClientError` so any future variant added to `kastellan-protocol`
        // breaks the build here and forces a deliberate classification decision rather
        // than silently inheriting the "dead" default.
        Err(ToolHostError::Protocol(ClientError::Rpc(_))) => false,
        Err(ToolHostError::Protocol(ClientError::Io(_))) => true,
        Err(ToolHostError::Protocol(ClientError::Decode(_))) => true,
        Err(ToolHostError::Protocol(ClientError::EarlyExit)) => true,
        Err(ToolHostError::Protocol(ClientError::IdMismatch { .. })) => true,
        // SecretRedemptionFailed fires before the worker is called —
        // the worker process was never contacted, so it is not dead.
        Err(ToolHostError::SecretRedemptionFailed(_)) => false,
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
///
/// `pending_acquires` (issue #84) is read OUTSIDE the tokio mutex — an atomic counter
/// of acquires that are currently queued behind the lock for this tool. Incremented
/// in `acquire_impl` before `lock_owned().await` via [`PendingAcquireGuard`];
/// decremented when the guard drops (i.e. once the lock has been acquired). The
/// counter therefore reflects "depth of the queue" — callers waiting, not callers
/// in flight. Exposed via `IdleTimeoutLifecycle::_test_slot_pending_acquires` for
/// tests and the future `kastellan-cli supervisor status` operator surface.
///
/// `last_warn_unix_nanos` (issue #136) is the unix-nanos timestamp of the most
/// recent queue-depth warn for this slot. Reads + CAS happen on the hot path in
/// `acquire_impl` (no mutex). The first acquirer to observe `depth >= threshold`
/// AND `(now - last) >= cooldown` wins the CAS and emits the warn; concurrent
/// losers stay silent until the next cooldown window. Initial value is 0 so the
/// very first crossing into the warn band fires immediately. See [`debounce_warn`]
/// for the pure predicate and [`PENDING_ACQUIRES_WARN_COOLDOWN`] for the window.
pub(crate) struct ToolSlot {
    pub(crate) state: Arc<TokioMutex<ToolState>>,
    pub(crate) pending_acquires: AtomicU32,
    pub(crate) last_warn_unix_nanos: AtomicI64,
}

/// Threshold at which `acquire` emits a `tracing::warn!` because the per-slot
/// pending-acquire queue depth has reached operator-visible territory (issue #84).
///
/// Picked at 5 by rule-of-thumb: under typical inference-worker latency
/// (~100-500ms per request), 5 queued requests = ~0.5-2.5s tail latency, which is
/// the boundary where users start to notice. Tunable later if operators want a
/// different floor; bumped here as a constant rather than env var because tuning
/// belongs at code-review time, not at deploy time.
pub const PENDING_ACQUIRES_WARN_THRESHOLD: u32 = 5;

/// Cooldown between consecutive queue-depth warns for the same slot (issue #136).
///
/// Without debouncing, every acquirer that lands while `depth >= threshold` emits a
/// warn — that's the request-rate (potentially many per second) under exactly the
/// scenario the warn is designed to surface. Operators would filter the line out,
/// defeating its purpose. With a 30 s cooldown, sustained backpressure logs ~twice
/// per minute per slot — enough to stay visible, sparse enough to avoid log-spam.
///
/// 30 s is a rule-of-thumb pick: long enough to dedupe steady-state spam, short
/// enough that an operator paging on the metric still sees the next warn within
/// half a minute of the original episode resolving and re-emerging. Pinned by
/// `pending_acquires_warn_cooldown_is_thirty_seconds` — bumping it changes
/// operator log volume noticeably and should go through code review.
pub const PENDING_ACQUIRES_WARN_COOLDOWN: Duration = Duration::from_secs(30);

/// Pure predicate: should an acquirer that just observed a post-increment pending
/// depth of `depth` emit a queue-depth warning?
///
/// Extracted as a named helper so the threshold semantics are unit-testable without
/// constructing a real `ToolSlot` or driving concurrent acquires.
pub(crate) fn pending_acquires_should_warn(depth: u32) -> bool {
    depth >= PENDING_ACQUIRES_WARN_THRESHOLD
}

/// Pure predicate: given the slot's last-warn timestamp and the current time
/// (both unix-nanos), should an acquirer that already passed
/// [`pending_acquires_should_warn`] go on to emit a warn, or has another acquirer
/// warned recently enough to dedupe this one?
///
/// Returns `true` iff `now_nanos - last_warn_nanos >= cooldown` (cooldown-elapsed,
/// inclusive). `saturating_sub` over `i64` handles the "clock stepped backward"
/// case (e.g. NTP correction) by returning a negative or zero delta, which is
/// always less than the positive cooldown — so the predicate suppresses warns
/// during clock drift rather than firing extra ones. Both kinds of inaccuracy
/// (over- and under-warning) are acceptable for a 30 s window in practice.
///
/// `last_warn_nanos == 0` (initial, no warn has ever fired) trivially returns
/// `true` against any reasonable post-epoch `now_nanos` — the first warn fires
/// immediately as intended.
///
/// Caller pattern (in `acquire_impl`): after this predicate returns `true`,
/// claim the warn slot via `compare_exchange(last_nanos, now_nanos)` — only the
/// CAS winner actually emits the warn, so concurrent acquirers that all see the
/// debounce gate open still get at most one warn per cooldown window.
pub(crate) fn debounce_warn(last_warn_nanos: i64, now_nanos: i64, cooldown: Duration) -> bool {
    let cooldown_nanos = cooldown.as_nanos() as i64;
    now_nanos.saturating_sub(last_warn_nanos) >= cooldown_nanos
}

/// RAII guard that brackets the queued-acquire interval.
///
/// Construction (`enter`) increments `pending_acquires`; `Drop` decrements it. Used
/// in `acquire_impl` so the counter reflects callers waiting for the slot's tokio
/// mutex — once the mutex is acquired the guard is dropped explicitly to bound the
/// "queued" lifetime to lock-wait time only (in-flight callers don't count).
///
/// Drop runs even on panic or `?`-style early return — the bracketed accounting
/// can't leak.
pub(crate) struct PendingAcquireGuard<'a> {
    counter: &'a AtomicU32,
}

impl<'a> PendingAcquireGuard<'a> {
    /// Increment the per-slot pending counter and produce a guard that decrements
    /// it on Drop.
    pub(crate) fn enter(counter: &'a AtomicU32) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }

    /// The slot's current pending-acquire depth. Guaranteed to be `>= 1` for the
    /// lifetime of the guard (this caller's own slot has been counted) and may be
    /// strictly greater under concurrency (other threads can `fetch_add` between
    /// our increment and this load). Used by callers to decide whether to emit a
    /// queue-depth warning, where ">= threshold" is the load-bearing property —
    /// exact identity with this caller's post-increment value is NOT promised.
    ///
    /// `Acquire` is used (not `Relaxed`) only for consistency with the increment
    /// site; this counter has no synchronizes-with relationship to other state, so
    /// `Relaxed` would also be sound — kept as `Acquire` for one-knob simplicity.
    pub(crate) fn depth(&self) -> u32 {
        self.counter.load(Ordering::Acquire)
    }
}

impl Drop for PendingAcquireGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
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
            pending_acquires: AtomicU32::new(0),
            last_warn_unix_nanos: AtomicI64::new(0),
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

    // Issue #84 — bracket the lock-acquisition wait with a pending-acquire guard so
    // the per-slot atomic counter reflects "depth of the queue" (callers waiting,
    // not callers in flight). Operator-visible signal in `_test_slot_pending_acquires`
    // and (when this slot's queue depth crosses the threshold) a `tracing::warn!`.
    // The pending guard MUST be alive across `lock_owned().await` — that's the
    // queued interval. We drop it explicitly right after the await so in-flight
    // dispatch time doesn't inflate the queue-depth metric.
    let pending_guard = PendingAcquireGuard::enter(&slot.pending_acquires);
    if pending_acquires_should_warn(pending_guard.depth()) {
        // Issue #136 — debounce so sustained queue depth doesn't log at request rate.
        // Two-phase: (1) check the elapsed-since-last-warn gate via the pure
        // `debounce_warn` predicate; (2) on pass, CAS to claim the warn slot so
        // exactly one concurrent acquirer wins per cooldown window. Losers stay
        // silent until the next window. `now_nanos` falls back to 0 on the
        // (impossible in practice) pre-1970 clock — the predicate handles that
        // gracefully by suppressing.
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let last_nanos = slot.last_warn_unix_nanos.load(Ordering::Relaxed);
        if debounce_warn(last_nanos, now_nanos, PENDING_ACQUIRES_WARN_COOLDOWN)
            && slot
                .last_warn_unix_nanos
                .compare_exchange(last_nanos, now_nanos, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                tool = tool_name,
                pending_acquires = pending_guard.depth(),
                threshold = PENDING_ACQUIRES_WARN_THRESHOLD,
                cooldown_secs = PENDING_ACQUIRES_WARN_COOLDOWN.as_secs(),
                "idle_timeout worker request queue is deep — \
                 requests are stacking up behind a slow or stuck warm worker \
                 (next warn for this slot suppressed for the cooldown window)"
            );
        }
    }
    let mut guard = Arc::clone(&slot.state).lock_owned().await;
    // We hold the lock — caller is no longer "queued", they're in flight. Drop the
    // pending guard to bound the counter to lock-wait time only.
    drop(pending_guard);

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
//
// Lifted into a sibling submodule to keep this file under the 500-LOC soft cap;
// see `release.rs` for the implementation and its module doc-comment explaining
// the split. `release_idle_timeout_worker` is re-exported below so external
// callers (notably `manager::WorkerHandle::Drop` via
// `super::idle_timeout::release_idle_timeout_worker`) continue to resolve
// unchanged.

mod release;
pub(crate) use release::release_idle_timeout_worker;

#[cfg(test)]
mod tests;
