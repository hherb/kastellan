//! Pure retry policy for the live worker's continuous sync loop.
//!
//! The live `sync()` call (`sdk_live::LiveSdk::connect`) returns whenever
//! `matrix-rust-sdk` hits an interruption — a transient server 5xx, a network
//! blip through the egress tunnel, a long-poll hiccup. Treating *every* such
//! return as fatal (the original `process::exit(1)`) means a single transient
//! interruption kills the whole worker and forces a supervised respawn — the
//! ~20–90s churn [#348](https://github.com/hherb/kastellan/issues/348) reports.
//!
//! Instead the sync loop retries in place with capped exponential backoff, and
//! only gives up (fail-loud exit → fresh respawn) after *sustained* failure,
//! since a persistently-wedged client (bad token, corrupt store) only a fresh
//! `connect` can recover.
//!
//! This module is deliberately **pure** and **not** feature-gated, so its policy
//! is unit-tested in the default build even though the `live-matrix` SDK glue
//! that uses it is DGX-gated (cf. #331 — CI doesn't compile `--features
//! live-matrix`).

use std::time::Duration;

/// What the sync loop should do after one `sync()` invocation returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Retry the sync loop after sleeping this long (capped exponential backoff).
    Backoff(Duration),
    /// Too many consecutive fast failures — stop retrying and let the supervisor
    /// respawn a fresh worker.
    GiveUp,
}

/// Update the consecutive-fast-failure counter after a `sync()` return.
///
/// A `sync()` that ran for at least `healthy` before returning means the worker
/// was up and serving — the return is a fresh transient blip, so the counter
/// resets to `0`. A fast return (`ran_for < healthy`) is a failure that didn't
/// recover, so the counter increments.
pub fn update_consecutive(prev: u32, ran_for: Duration, healthy: Duration) -> u32 {
    if ran_for >= healthy {
        0
    } else {
        prev.saturating_add(1)
    }
}

/// Decide whether to back off + retry or give up, given the current run of
/// consecutive fast failures.
///
/// - `consecutive >= max_consecutive` ⇒ [`SyncOutcome::GiveUp`].
/// - otherwise [`SyncOutcome::Backoff`] of `min(max, base * 2^(consecutive - 1))`
///   (the first failure backs off by `base`; the cap prevents unbounded growth).
///
/// `consecutive` is expected to be `>= 1` on the failure path (the caller bumps
/// it via [`update_consecutive`] before asking); `0` is treated as the first
/// step (`base`) defensively.
pub fn next_action(
    consecutive: u32,
    max_consecutive: u32,
    base: Duration,
    max: Duration,
) -> SyncOutcome {
    if consecutive >= max_consecutive {
        return SyncOutcome::GiveUp;
    }
    let shift = consecutive.saturating_sub(1).min(31);
    // Saturating doubling so a large `shift` can't overflow the multiply.
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let scaled = base.checked_mul(factor as u32).unwrap_or(max);
    SyncOutcome::Backoff(scaled.min(max))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEALTHY: Duration = Duration::from_secs(60);
    const BASE: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(30);
    const MAX_CONSEC: u32 = 10;

    #[test]
    fn healthy_run_resets_counter() {
        // A sync that ran a long time before returning was working; reset.
        assert_eq!(update_consecutive(7, Duration::from_secs(120), HEALTHY), 0);
        assert_eq!(update_consecutive(0, HEALTHY, HEALTHY), 0, "exactly healthy counts as healthy");
    }

    #[test]
    fn fast_failure_increments_counter() {
        assert_eq!(update_consecutive(0, Duration::from_millis(50), HEALTHY), 1);
        assert_eq!(update_consecutive(3, Duration::from_secs(5), HEALTHY), 4);
    }

    #[test]
    fn backoff_escalates_then_caps() {
        // 1st failure → base; doubles each step; never exceeds max.
        assert_eq!(next_action(1, MAX_CONSEC, BASE, MAX), SyncOutcome::Backoff(Duration::from_secs(1)));
        assert_eq!(next_action(2, MAX_CONSEC, BASE, MAX), SyncOutcome::Backoff(Duration::from_secs(2)));
        assert_eq!(next_action(3, MAX_CONSEC, BASE, MAX), SyncOutcome::Backoff(Duration::from_secs(4)));
        assert_eq!(next_action(4, MAX_CONSEC, BASE, MAX), SyncOutcome::Backoff(Duration::from_secs(8)));
        // 2^5 = 32s would exceed the 30s cap → clamped.
        assert_eq!(next_action(6, MAX_CONSEC, BASE, MAX), SyncOutcome::Backoff(MAX));
        assert_eq!(next_action(9, MAX_CONSEC, BASE, MAX), SyncOutcome::Backoff(MAX));
    }

    #[test]
    fn gives_up_at_threshold() {
        assert_eq!(next_action(MAX_CONSEC, MAX_CONSEC, BASE, MAX), SyncOutcome::GiveUp);
        assert_eq!(next_action(MAX_CONSEC + 5, MAX_CONSEC, BASE, MAX), SyncOutcome::GiveUp);
    }

    #[test]
    fn zero_consecutive_is_defensive_base() {
        // Defensive: callers bump before asking, but 0 must not panic/overflow.
        assert_eq!(next_action(0, MAX_CONSEC, BASE, MAX), SyncOutcome::Backoff(BASE));
    }

    #[test]
    fn huge_consecutive_does_not_overflow() {
        // Below the give-up threshold but a large shift must clamp to max, not panic.
        assert_eq!(next_action(40, 100, BASE, MAX), SyncOutcome::Backoff(MAX));
    }
}
