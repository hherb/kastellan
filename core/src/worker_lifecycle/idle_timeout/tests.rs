//! Pure unit tests for the `idle_timeout` lifecycle module.
//!
//! Lifted from an inline `#[cfg(test)] mod tests` block in `idle_timeout.rs`
//! to keep the production file under the 500-LOC soft cap. The body is
//! byte-identical to what it was inline; `use super::*` still resolves to
//! the parent `idle_timeout` module per the Rust 2018 sibling-directory
//! module pattern.

use super::*;
// `replace_idle_teardown_handle` moved into the sibling `release` submodule when
// `idle_timeout.rs` grew past the 500-LOC cap. Tests reach into it directly here
// rather than via a `#[cfg(test)]` re-export on the parent so the structural
// home of the helper is explicit at the import site.
use super::release::replace_idle_teardown_handle;
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
fn dispatch_classifier_secret_redemption_failed_is_not_a_crash() {
    // Item 31 — SecretRedemptionFailed surfaces from the substitution
    // chokepoint BEFORE worker.call is invoked. The worker is never
    // contacted, so this MUST NOT be classified as a worker crash —
    // otherwise the lifecycle backoff counter would tick incorrectly
    // on a planner-side error and degrade the warm-worker hit rate.
    use crate::secrets::{MissingReason, SubstituteError};
    use crate::tool_host::ToolHostError;

    let err = ToolHostError::SecretRedemptionFailed(SubstituteError::MissingRef {
        ref_hash: "test-hash".to_string(),
        reason: MissingReason::NotFound,
    });
    assert!(!dispatch_indicates_worker_dead(&Err::<(), _>(err)));
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

/// Pin the RAII-bracket semantics of `PendingAcquireGuard` (issue #84).
///
/// `enter` increments the per-slot pending-acquire counter; `Drop` decrements it.
/// Nested guards stack (matches the "concurrent same-tool acquires" shape that the
/// production `acquire_impl` would see) — N nested enters = depth N; N drops = back
/// to 0. The Drop semantics ensure that even on panic or `?`-style early return the
/// accounting can't leak.
#[test]
fn pending_acquire_guard_increments_on_enter_and_decrements_on_drop() {
    let counter = std::sync::atomic::AtomicU32::new(0);
    assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 0);
    {
        let _g1 = PendingAcquireGuard::enter(&counter);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 1);
        {
            let _g2 = PendingAcquireGuard::enter(&counter);
            assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 2);
            let _g3 = PendingAcquireGuard::enter(&counter);
            assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 3);
        } // _g2 + _g3 drop here
        assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 1);
    } // _g1 drops here
    assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 0);
}

/// Pin that `depth()` reports the post-increment value (i.e. the depth as observed
/// at `enter` time, including the caller's own slot). This is the contract the
/// `tracing::warn!` site in `acquire_impl` relies on.
#[test]
fn pending_acquire_guard_depth_reports_post_increment_value() {
    let counter = std::sync::atomic::AtomicU32::new(0);
    let g1 = PendingAcquireGuard::enter(&counter);
    assert_eq!(g1.depth(), 1, "first guard sees depth=1 including itself");
    let g2 = PendingAcquireGuard::enter(&counter);
    assert_eq!(g2.depth(), 2, "second guard sees depth=2 including itself");
    let g3 = PendingAcquireGuard::enter(&counter);
    assert_eq!(g3.depth(), 3);
    drop(g3);
    drop(g2);
    drop(g1);
    assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 0);
}

/// Pin the `tracing::warn!` threshold semantics — the predicate fires AT and
/// ABOVE the threshold, not just strictly above. This matches the issue #84
/// AC ("operator notices BEFORE users do" — i.e. fire at the boundary).
#[test]
fn pending_acquires_should_warn_fires_at_and_above_threshold() {
    assert!(!pending_acquires_should_warn(0));
    assert!(!pending_acquires_should_warn(1));
    assert!(!pending_acquires_should_warn(
        PENDING_ACQUIRES_WARN_THRESHOLD - 1
    ));
    assert!(pending_acquires_should_warn(PENDING_ACQUIRES_WARN_THRESHOLD));
    assert!(pending_acquires_should_warn(
        PENDING_ACQUIRES_WARN_THRESHOLD + 1
    ));
    assert!(pending_acquires_should_warn(u32::MAX));
}

/// Pin the threshold constant — if anyone bumps it, the test fires and forces
/// a re-review of the operator-visibility tradeoff. The constant is part of the
/// operator-facing observability contract; flipping it silently would change
/// when warnings fire across all deployments.
#[test]
fn pending_acquires_warn_threshold_is_five() {
    assert_eq!(
        PENDING_ACQUIRES_WARN_THRESHOLD, 5,
        "threshold change requires a re-think of the issue #84 operator-visibility \
         tradeoff (see the const's doc-comment). Update this test if the change is \
         intentional."
    );
}

// --- Issue #136: queue-depth warn debounce ---------------------------------

/// Pin the cooldown constant — same defensive pattern as
/// `pending_acquires_warn_threshold_is_five`. Bumping the cooldown changes
/// operator log volume noticeably (longer windows = sparser warns; shorter
/// windows = noisier under sustained queue depth), so the change should go
/// through code review.
#[test]
fn pending_acquires_warn_cooldown_is_thirty_seconds() {
    assert_eq!(
        PENDING_ACQUIRES_WARN_COOLDOWN,
        Duration::from_secs(30),
        "cooldown change requires a re-think of the issue #136 operator-visibility \
         tradeoff (see the const's doc-comment). Update this test if the change is \
         intentional."
    );
}

/// First warn ever (`last_warn_nanos == 0`): the gate collapses to
/// `now >= cooldown`. In production this is trivially true because
/// `SystemTime::now()` is decades past epoch (~`1.78e18` ns in 2026,
/// vastly larger than any sensible cooldown). The load-bearing
/// "first crossing into the warn band fires immediately" property
/// therefore holds in practice — pinned here with a realistic `now`
/// rather than a fabricated tiny one so the assertion matches reality.
///
/// Note: the predicate does NOT short-circuit on `last == 0`. A
/// freshly-constructed slot whose first warn happened to fire in the
/// first 30 ns after the unix epoch (impossible in practice) would
/// suppress for the cooldown — same shape as any other warn. Avoids
/// a special case in the production path.
#[test]
fn debounce_warn_fires_when_no_prior_warn() {
    let cooldown = Duration::from_secs(30);
    let realistic_now_nanos: i64 = 1_700_000_000_000_000_000; // ~2023-11
    assert!(debounce_warn(0, realistic_now_nanos, cooldown));
    // Boundary case for completeness: `now == cooldown` (≈30 s after epoch).
    // Inclusive at the boundary by design — fires.
    assert!(debounce_warn(0, cooldown.as_nanos() as i64, cooldown));
}

/// Within-cooldown: suppress. This is the whole point of the debounce — under
/// sustained queue depth the predicate must return false at request rate.
#[test]
fn debounce_warn_suppresses_within_cooldown() {
    let cooldown = Duration::from_secs(30);
    let last = 1_700_000_000_000_000_000; // arbitrary plausible 2023-era ns
    // 1 ns after last warn — squarely inside the cooldown.
    assert!(!debounce_warn(last, last + 1, cooldown));
    // 29 s after — still inside.
    assert!(!debounce_warn(
        last,
        last + 29 * 1_000_000_000,
        cooldown
    ));
    // 30 s minus 1 ns — still strictly inside (boundary is inclusive at 30 s).
    assert!(!debounce_warn(
        last,
        last + 30 * 1_000_000_000 - 1,
        cooldown
    ));
}

/// At the cooldown boundary (now - last == cooldown) the predicate fires. The
/// `>= cooldown` form ensures the cooldown is the *minimum* time between warns,
/// not a strict-greater bound — a one-nanosecond rounding lower would only hurt.
#[test]
fn debounce_warn_fires_at_cooldown_boundary_inclusive() {
    let cooldown = Duration::from_secs(30);
    let last = 1_700_000_000_000_000_000;
    assert!(debounce_warn(
        last,
        last + cooldown.as_nanos() as i64,
        cooldown
    ));
}

/// Past the cooldown: fires. The natural case for the operator — episode of
/// queue depth resolved, queue empties, queue refills > 30 s later, fresh warn.
#[test]
fn debounce_warn_fires_after_cooldown_elapsed() {
    let cooldown = Duration::from_secs(30);
    let last = 1_700_000_000_000_000_000;
    assert!(debounce_warn(
        last,
        last + 31 * 1_000_000_000,
        cooldown
    ));
    // Hours later — still fires; the gate is monotonic.
    assert!(debounce_warn(
        last,
        last + 3600 * 1_000_000_000,
        cooldown
    ));
}

/// Clock-skew backward (NTP correction stepping the wall clock into the past):
/// suppress. The `saturating_sub` makes `now - last` negative, which is always
/// less than the positive cooldown — predicate returns false. We choose to
/// under-warn during clock drift rather than over-warn; in practice NTP
/// corrections are tiny and rare, so the worst case is the next warn waiting
/// until the apparent clock catches back up to `last + cooldown`.
#[test]
fn debounce_warn_suppresses_on_clock_skew_backward() {
    let cooldown = Duration::from_secs(30);
    let last = 2_000_000_000_000_000_000;
    let now = 1_000_000_000_000_000_000; // 31+ years earlier — extreme skew
    assert!(!debounce_warn(last, now, cooldown));
    // Even 1 ns of backward skew suppresses (correct: any "negative elapsed"
    // is not "cooldown elapsed").
    assert!(!debounce_warn(last, last - 1, cooldown));
}

/// Fresh slots from `slot_for` start with `last_warn_unix_nanos == 0` so the
/// first warn fires immediately — pinned here because the initial value is the
/// load-bearing input to `debounce_warn` on the cold path.
#[test]
fn slot_for_initializes_last_warn_unix_nanos_to_zero() {
    let registry: WarmRegistry = empty_registry();
    let slot = slot_for(&registry, "fresh-tool-for-debounce-init-pin");
    assert_eq!(
        slot.last_warn_unix_nanos.load(Ordering::Acquire),
        0,
        "fresh slots must start at 0 so the very first queue-depth crossing \
         emits a warn immediately (issue #136 cold-path contract)"
    );
}

/// Pin issue #85's "exactly one idle-teardown task per slot at steady state" invariant.
///
/// Before this fix shipped, every successful release path called `tokio::spawn` to
/// schedule a teardown task and *never aborted* prior ones. Under steady-state high
/// request rate this accumulated ~`idle_seconds` stale tasks per tool (e.g. one
/// request per second with `idle_seconds = 60` → ~60 pending sleepers per tool).
///
/// `replace_idle_teardown_handle` is the single mutator now; it aborts the prior
/// handle (if any) before spawning a new one. This test pins the contract:
///
///   1. After the first call, the slot holds `Some(handle)`.
///   2. After a second call, the slot's `JoinHandle` is a NEW task — the prior one
///      was replaced (the old `JoinHandle` ID is no longer the one stored).
///   3. `idle_seconds = 0` aborts the prior handle and leaves `None` (disabled).
///
/// If this test fires:
///   - You stopped storing the JoinHandle in `ToolState.idle_teardown_handle` →
///     issue #85's accumulation regression is back. Restore the storage.
///   - You forgot to abort the prior handle before spawning a new one → wasted
///     sleepers. Add the `abort_idle_teardown_handle` call.
#[tokio::test]
async fn replace_idle_teardown_handle_aborts_prior_and_stores_new() {
    // Use `slot.state` directly (via `lock_owned()`) so the `state` argument the
    // helper mutates and the mutex the spawned task will later try to lock are
    // the same object — mirrors production wiring in `release_idle_timeout_worker`.
    let slot: Arc<ToolSlot> = Arc::new(ToolSlot {
        state: Arc::new(TokioMutex::new(ToolState::fresh())),
        pending_acquires: std::sync::atomic::AtomicU32::new(0),
        last_warn_unix_nanos: std::sync::atomic::AtomicI64::new(0),
    });
    let mut state = Arc::clone(&slot.state).lock_owned().await;

    // 1: schedule a handle. idle_seconds=60 keeps the task sleeping well beyond
    //    the test's runtime, so we never observe it actually firing.
    replace_idle_teardown_handle(&mut state, Arc::clone(&slot), Instant::now(), 60);
    let first_id = state
        .idle_teardown_handle
        .as_ref()
        .expect("first call should store a handle")
        .id();

    // 2: replace. The new handle MUST be a different tokio task (different ID).
    replace_idle_teardown_handle(&mut state, Arc::clone(&slot), Instant::now(), 60);
    let second_id = state
        .idle_teardown_handle
        .as_ref()
        .expect("second call should store a handle (still exactly one alive)")
        .id();
    assert_ne!(
        first_id, second_id,
        "expected the prior handle to be replaced; got the same task ID — \
         the abort-then-spawn path is broken (issue #85 regression)"
    );

    // 3: idle_seconds = 0 aborts the prior handle and leaves None.
    //    Mirrors the disabled-teardown semantics other "0 = unlimited / disabled"
    //    knobs use elsewhere in the workspace (`max_requests`, `cpu_quota_pct`).
    replace_idle_teardown_handle(&mut state, Arc::clone(&slot), Instant::now(), 0);
    assert!(
        state.idle_teardown_handle.is_none(),
        "idle_seconds=0 must clear the slot's idle-teardown handle (teardown disabled)"
    );
}

/// Pin the steady-state invariant: after N rapid releases, the slot still holds
/// exactly one teardown handle (not N). This is the load-bearing observable for
/// issue #85: at any moment the supervisor has at most one pending idle-teardown
/// task per warm slot, regardless of how fast requests come in.
#[tokio::test]
async fn replace_idle_teardown_handle_steady_state_holds_at_most_one_alive_per_slot() {
    let slot: Arc<ToolSlot> = Arc::new(ToolSlot {
        state: Arc::new(TokioMutex::new(ToolState::fresh())),
        pending_acquires: std::sync::atomic::AtomicU32::new(0),
        last_warn_unix_nanos: std::sync::atomic::AtomicI64::new(0),
    });
    let mut state = Arc::clone(&slot.state).lock_owned().await;

    // Simulate 10 rapid successful releases. Pre-fix this would have spawned 10
    // tasks all sleeping for `idle_seconds`. Post-fix: each call aborts the
    // prior and spawns one. At the end, exactly one Some(handle) remains.
    for _ in 0..10 {
        replace_idle_teardown_handle(&mut state, Arc::clone(&slot), Instant::now(), 60);
    }
    assert!(
        state.idle_teardown_handle.is_some(),
        "after N releases, the slot must still hold a handle (the most recent one)"
    );
    // The prior 9 handles were aborted by the helper; nothing observable from out
    // here, but the production single-task-per-slot invariant is held: the slot
    // carries at most one `Option<JoinHandle<()>>`, structurally.
}

/// Pins the IdleTimeoutLifecycle warm-cache key invariant (issue #121).
///
/// The warm-cache key is `tool_name` only; `ToolEntry.container_image` is
/// deliberately NOT in the key signature. Two `slot_for` calls under the same
/// tool name MUST return the same `Arc<ToolSlot>` regardless of any
/// hypothetical image-tag variation in the caller's `ToolEntry`. This is
/// safe today because image tags are baked in at daemon startup and a
/// restart flushes the registry; a future live-reconfigure path that allows
/// the same tool name to swap image tags without a restart would silently
/// serve requests through a worker spawned under the stale image.
///
/// If this test fires:
///   - You widened `slot_for`'s key signature → either intentional (then
///     update this test + every call site) or accidental (revert).
///   - You introduced a live-reconfigure path → either widen the key as
///     above, OR explicitly evict the warm slot for the tool before
///     serving requests through the re-registered entry.
#[test]
fn slot_for_key_excludes_container_image() {
    let registry: WarmRegistry = empty_registry();
    let slot1 = slot_for(&registry, "twice-name");
    let slot2 = slot_for(&registry, "twice-name");
    assert!(
        Arc::ptr_eq(&slot1, &slot2),
        "warm-cache widened: second slot_for under same tool_name returned a different Arc. \
         If this is intentional (live-reconfigure path landed), the warm-cache key MUST \
         widen to (tool_name, container_image) — see issue #121 and the slot_for doc."
    );
    // Sibling tool names get distinct slots (sanity check that the key is
    // not collapsing everything).
    let other = slot_for(&registry, "other-tool");
    assert!(
        !Arc::ptr_eq(&slot1, &other),
        "warm-cache collapsed: distinct tool names returned the same slot. \
         The HashMap<String, Arc<ToolSlot>> shape is violated."
    );
}
