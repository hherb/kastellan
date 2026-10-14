//! How the polled driver logs a worker that is **down** — dead, or being
//! respawned underneath it by the supervisor (#674, #769).
//!
//! A worker that *answered* with a refusal is not down; that has its own
//! pacing and its own lines in [`super::refusal`]. Split out of [`super`] to
//! keep it under the 500-LOC soft cap. Everything here is pure except
//! [`note_answer`] and [`report_down`], which only emit a line.
//!
//! ## Why these lines carry no stderr marker (#788)
//!
//! The refusal lines, and the line that says a refusal ended, go through the
//! marked `[worker-refusal]` emitter so a binary with no `tracing` subscriber
//! still sees them. The lines [`note_answer`] and [`report_down`] emit stay on
//! `tracing` alone, deliberately. An outage is the **supervisor's** story, and
//! it already tells it on the marked stream: `[worker-death]` when the worker
//! dies, and a `[worker-down]` line for every respawn attempt that fails
//! (#738). So on that stream a death followed by no `[worker-down]` means no
//! respawn attempt has failed. Marking the driver's "down" line would print
//! every death twice; marking "back up" would add an ending the marked stream
//! already implies.

/// The driver's worker-down latch: says "down" once per outage and "back up"
/// once when it ends. Pure. Refusals are not tracked here; see
/// [`super::refusal`].
#[derive(Debug, Default)]
pub(super) struct OutageLog {
    /// The last worker call failed because the worker was down.
    down: bool,
    /// How many outages have ended, whichever call noticed: a run kept
    /// outside the refusal path compares it to see that the worker came back
    /// since it last spoke (#836, `super::undecodable`).
    ended: u64,
}

impl OutageLog {
    /// Record a call that failed because the worker is down. True for the
    /// first failure of an outage, which is the one that gets a line.
    #[must_use]
    pub(super) fn on_down(&mut self) -> bool {
        !std::mem::replace(&mut self.down, true)
    }

    /// Record that the worker answered — accepted the call, or refused it.
    /// Either way it is up. True when that ends an outage, so the caller logs
    /// "back up" once.
    #[must_use]
    pub(super) fn on_answer(&mut self) -> bool {
        let ended = std::mem::take(&mut self.down);
        self.ended += u64::from(ended);
        ended
    }

    /// How many outages have ended so far.
    pub(super) fn ended(&self) -> u64 {
        self.ended
    }

    /// Whether the last worker call found the worker down. Read once, as the
    /// driver exits, so it does not drain a worker that is being respawned
    /// (#835, `worker_drain.rs`).
    pub(super) fn is_down(&self) -> bool {
        self.down
    }
}

/// The worker answered a call (accepted or refused), so it is up: log "back
/// up" if that ends an outage. True when it did.
pub(super) fn note_answer(outage: &mut OutageLog, label: &str) -> bool {
    let ended = outage.on_answer();
    if ended {
        tracing::info!(label, "worker back up; polled driver resumed");
    }
    ended
}

/// Emit the one line an outage gets ([`OutageLog::on_down`] said it is due).
///
/// The error text is worker-influenced, so it is logged through
/// [`crate::untrusted_text::neutralise_controls`], as every other
/// worker-to-log path is.
pub(super) fn report_down(label: &str, e: &anyhow::Error, what: &str) {
    let error = crate::untrusted_text::neutralise_controls(&e.to_string());
    tracing::warn!(label, error = %error, "{what}");
}
