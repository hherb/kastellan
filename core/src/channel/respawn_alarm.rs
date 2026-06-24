//! Sliding-window respawn-rate alarm for supervised channel workers (#348 item 3).
//!
//! The supervised Matrix driver (`channel::matrix::drive`) silently respawns a
//! dead worker with capped backoff. A *single* crash is benign, but a worker
//! that dies-and-respawns repeatedly ("churn") is a real fault — historically
//! the bwrap-PDEATHSIG bug (#348) produced exactly this, and it was only
//! diagnosed after deploying death-report observability. This module turns that
//! churn into an *up-front* warning: it counts respawns in a sliding time window
//! and signals once when the rate crosses an operator-chosen threshold.
//!
//! The type is deliberately a **pure state machine over caller-supplied
//! [`Instant`]s** — it owns no clock and spawns nothing — so the driver decides
//! when "now" is and the alarm logic is unit-testable without threads or sleeps.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Tracks worker respawn instants in a sliding window and fires once when the
/// in-window respawn count reaches a threshold.
///
/// "Fires once" is intentional: while a storm keeps the count at or above the
/// threshold the alarm stays *armed* and [`record`](Self::record) returns
/// `None`, so a sustained churn warns a single time rather than on every
/// respawn. The alarm re-arms automatically once enough time passes that the
/// in-window count falls back below the threshold (the storm cleared).
pub struct RespawnRateAlarm {
    /// Length of the sliding window. Respawns older than this (relative to the
    /// most recent `record`) are pruned before counting.
    window: Duration,
    /// Respawn count (within the window) that trips the alarm. `record` fires
    /// when the count reaches this value.
    threshold: usize,
    /// Respawn instants currently within the window, oldest-first.
    recent: VecDeque<Instant>,
    /// `true` while the current storm has already fired, suppressing repeats
    /// until the in-window count drops below `threshold` again.
    armed: bool,
}

impl RespawnRateAlarm {
    /// Create an alarm that fires when `threshold` respawns occur within
    /// `window`.
    pub fn new(window: Duration, threshold: usize) -> Self {
        Self {
            window,
            threshold,
            recent: VecDeque::new(),
            armed: false,
        }
    }

    /// Record a respawn that happened at `now`.
    ///
    /// Returns `Some(count)` — where `count` is the number of respawns in the
    /// window including this one — the first time the in-window count reaches
    /// the threshold for a given storm; returns `None` otherwise (below
    /// threshold, or already fired for the ongoing storm).
    ///
    /// `now` is expected to be monotonically non-decreasing across calls (it is
    /// in the driver, where it is always `Instant::now()`); out-of-order
    /// timestamps are tolerated but pruning uses each call's `now` as the
    /// window's right edge.
    pub fn record(&mut self, now: Instant) -> Option<usize> {
        // Drop respawns that have aged out of the window (right edge = `now`).
        while let Some(&front) = self.recent.front() {
            if now.saturating_duration_since(front) > self.window {
                self.recent.pop_front();
            } else {
                break;
            }
        }
        self.recent.push_back(now);

        let count = self.recent.len();
        if count < self.threshold {
            // Storm cleared (or never started): re-arm for the next one.
            self.armed = false;
            None
        } else if self.armed {
            // Threshold met but already fired for the ongoing storm: stay silent.
            None
        } else {
            self.armed = true;
            Some(count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    /// A respawn count below the threshold never fires the alarm.
    #[test]
    fn fewer_respawns_than_threshold_do_not_alarm() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 3);

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(1)), None);
    }

    /// Reaching the threshold inside the window fires exactly once; further
    /// respawns within the same window stay silent (no log spam).
    #[test]
    fn reaching_threshold_within_window_alarms_once() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 3);

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(5)), None);
        // Third respawn within the window trips the alarm, reporting the count.
        assert_eq!(alarm.record(base + Duration::from_secs(10)), Some(3));
        // A fourth, still within the window, must NOT re-fire.
        assert_eq!(alarm.record(base + Duration::from_secs(15)), None);
    }

    /// Respawns older than the window are pruned, so a slow trickle never trips
    /// the alarm.
    #[test]
    fn respawns_outside_window_are_pruned() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 3);

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(30)), None);
        // 200s after the first two: both are now outside the 60s window, so the
        // in-window count is just this one respawn — well below threshold.
        assert_eq!(alarm.record(base + Duration::from_secs(200)), None);
    }

    /// After a storm fires and then clears (window empties), a fresh storm fires
    /// again — the alarm re-arms rather than latching forever.
    #[test]
    fn alarm_rearms_after_storm_clears() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 2);

        // First storm: two respawns trip the alarm.
        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(1)), Some(2));

        // Long quiet gap empties the window. The first respawn of the second
        // storm is alone in the window → no fire yet (and it re-arms).
        let t = base + Duration::from_secs(500);
        assert_eq!(alarm.record(t), None);
        // Second respawn of the new storm trips it again.
        assert_eq!(alarm.record(t + Duration::from_secs(1)), Some(2));
    }

    /// A threshold of 1 fires on the very first respawn.
    #[test]
    fn threshold_one_fires_immediately() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 1);

        assert_eq!(alarm.record(base), Some(1));
    }
}
