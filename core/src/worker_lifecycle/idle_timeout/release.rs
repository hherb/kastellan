//! Release path for the idle-timeout lifecycle.
//!
//! Lifted from `idle_timeout.rs` to keep both files under the 500-LOC soft cap
//! (`idle_timeout.rs` was 647 LOC after the issue #136 warn-debounce slice,
//! 147 LOC over the cap). Splits the file along the natural seam between the
//! "acquire path" (warm-cache lookup, lock wait, queue-depth observability,
//! crash classification) which stays in `idle_timeout.rs`, and the "release
//! path" (cap evaluation, teardown task lifecycle, restart backoff bookkeeping)
//! which lives here.
//!
//! Three functions move together as one unit because they are tightly coupled:
//! `release_idle_timeout_worker` is the only external caller of
//! `replace_idle_teardown_handle`, which itself is the only external caller of
//! `abort_idle_teardown_handle`. Splitting them across modules would only
//! scatter the implementation without aiding navigation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::OwnedMutexGuard;
use tokio::time::sleep;

use crate::tool_host::SupervisedWorker;
use crate::worker_lifecycle::types::IdleTimeoutCaps;

use super::{
    is_aged_out, is_request_capped, RestartBackoff, ToolSlot, ToolState, WarmWorker,
};

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
    } else {
        // Defensive: current production callers always thread `Some(slot)` through
        // (`WorkerHandle::idle_timeout` constructs the handle with `Some`; the Drop
        // path that calls us takes that `Some` exactly once). But the function
        // signature accepts `Option<Arc<ToolSlot>>`, so a future caller passing
        // `None` would silently skip the abort and revert this branch to the pre-#85
        // accumulation pattern (the prior teardown task continues to sleep). Keep
        // every release path symmetric: crash/cap branches above also unconditionally
        // abort. Without `slot` we cannot spawn a replacement task, but the warm
        // slot's `last_completion` was just refreshed so no teardown would be due yet.
        abort_idle_teardown_handle(&mut guard);
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
/// The single-task-per-slot invariant — and its pin test in the parent module's
/// `tests.rs` — is the regression guarantee for issue #85.
pub(crate) fn replace_idle_teardown_handle(
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
