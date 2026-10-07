//! A sink's **lease** on the ledger (#792), and when it starts to count
//! (#802). Split out of `audit_sink.rs` to keep it under the 500-LOC soft cap;
//! `#[path]`-included there; [`SinkChannel`] and [`Starting`] are re-exported
//! by name.
//!
//! While a [`SinkWriter`] is alive, [`super::drain`] counts its driver as one
//! that may still write a row. A sink's hook closure owns its writer, and the
//! driver owns the hook, so the lease ends exactly when the driver drops its
//! hooks — as its thread returns, after any `driver_exit` rows are spawned. No
//! driver has to remember to report its exit.
//!
//! **A lease can start before its driver does**
//! ([`SinkWriter::starting_with_ledger`]). Matrix builds its writer before a
//! login that can time out, and a timed-out login's blocking task cannot be
//! cancelled: it keeps the writer, and with it the lease, until the SDK's own
//! timeouts end it. Counted from the start, that lease made a shutdown after
//! an abandoned login wait the full drain bound for a driver that never came
//! up, then report it on `[audit-lost]` as one whose queued replies might go
//! unaudited — when none was ever queued (#796, #802). So such a
//! lease counts only as *starting* (said at INFO, not waited for) until
//! [`Starting::started`] says the driver is up.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use kastellan_core::worker_stderr::AuditLostWriter;
use sqlx::PgPool;

use super::{Ledger, SinkWriter};

/// What a sink's driver does with rows at the end of its life — which decides
/// whether a driver [`super::drain`] finds still running is a loss (#796).
/// Named for that property, not for the channel that has it (#802).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SinkKind {
    /// The driver audits what it still holds as it exits — Matrix's: every
    /// reply still queued becomes a `channel.reply_undelivered` row, reason
    /// `driver_exit`. Stuck past the drain bound (in a worker call, or
    /// flushing a long queue) it never gets there, so those replies are
    /// **unaudited**: a possible loss, and reported as one.
    AuditsOnExit,
    /// The driver audits nothing once its bus is gone — email's: the skipped
    /// ids' acks stop there, so a driver still in its 15 s long-poll costs
    /// nothing. INFO.
    SilentOnExit,
}

/// Which channel a sink writes for (#807): what its refused rows are thinned
/// and counted under, apart from the other channel's, and the writer its
/// `[audit-lost]` lines name. Each channel's [`SinkKind`] follows from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SinkChannel {
    /// `channel.reply_undelivered` and `channel.inbound_dropped` (#826), from
    /// the Matrix driver.
    Matrix,
    /// `channel.skipped_ack_only`, from the email driver.
    Email,
}

impl SinkChannel {
    /// Both, in the order the shutdown lines are said.
    pub(crate) const ALL: [Self; 2] = [Self::Matrix, Self::Email];

    /// What this channel's driver does with rows at the end of its life.
    pub(crate) const fn kind(self) -> SinkKind {
        match self {
            Self::Matrix => SinkKind::AuditsOnExit,
            Self::Email => SinkKind::SilentOnExit,
        }
    }

    /// The writer this channel's `[audit-lost]` lines name.
    pub(crate) const fn writer(self) -> AuditLostWriter {
        match self {
            Self::Matrix => AuditLostWriter::Matrix,
            Self::Email => AuditLostWriter::Email,
        }
    }
}

/// Where a lease stands. One lock for the state and its counters' moves, so a
/// [`Starting::started`] racing the writer's drop can neither count a driver
/// twice nor leave one counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaseState {
    /// Counted in `Ledger::starting` only: not waited for, not a loss.
    Starting,
    /// Counted in `Ledger::live_sinks` (and `live_auditing` for
    /// [`SinkKind::AuditsOnExit`]).
    Live,
    /// The writer is gone; counted nowhere. A late `started` is a no-op.
    Ended,
}

/// One lease: shared by its writer and, while it starts, a [`Starting`].
pub(super) struct Lease {
    ledger: &'static Ledger,
    kind: SinkKind,
    state: Mutex<LeaseState>,
}

impl Lease {
    /// A lease that counts as *starting* until promoted.
    fn starting(ledger: &'static Ledger, kind: SinkKind) -> Arc<Self> {
        ledger.starting.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self { ledger, kind, state: Mutex::new(LeaseState::Starting) })
    }

    /// A lease that counts as live at once.
    fn live(ledger: &'static Ledger, kind: SinkKind) -> Arc<Self> {
        count_live(ledger, kind, true);
        Arc::new(Self { ledger, kind, state: Mutex::new(LeaseState::Live) })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `Starting` → `Live`; anything else stays as it is.
    fn promote(&self) {
        self.promote_around(|| {});
    }

    /// [`Self::promote`], running `between` once the lease counts as live and
    /// before it stops counting as starting: the seam a test uses to take a
    /// snapshot exactly inside the window that order guards (#802).
    fn promote_around(&self, between: impl FnOnce()) {
        let mut state = self.state();
        if *state == LeaseState::Starting {
            count_live(self.ledger, self.kind, true);
            between();
            self.ledger.starting.fetch_sub(1, Ordering::SeqCst);
            *state = LeaseState::Live;
        }
    }

    /// The writer is gone: stop counting it, wherever it was counted. Only
    /// the writer's `Drop` calls it, so a writer cannot outlive its lease.
    fn end(&self) {
        let mut state = self.state();
        match *state {
            LeaseState::Starting => {
                self.ledger.starting.fetch_sub(1, Ordering::SeqCst);
            }
            LeaseState::Live => count_live(self.ledger, self.kind, false),
            LeaseState::Ended => {}
        }
        *state = LeaseState::Ended;
    }
}

/// Count a live lease of `kind` in or out. In: `live_sinks` LAST, so a
/// snapshot that sees it sees the kind's count too (the converse does not
/// hold: one read can see the kind counted and `live_sinks` not yet, so
/// `auditing_live` can exceed `sinks_live` there; the formatters saturate). Out:
/// `live_sinks` last as well — the driver has spawned its final rows before
/// this, and `Ledger::snapshot` reads the writers before `pending` (see there).
/// So one read can also see a Matrix driver mid-exit as live but not
/// auditing, and say it at INFO; its rows are already spawned and counted.
/// A promotion calls this BEFORE it uncounts `starting`, which the snapshot
/// reads first.
fn count_live(ledger: &'static Ledger, kind: SinkKind, inc: bool) {
    let step = |n: &std::sync::atomic::AtomicUsize| {
        if inc {
            n.fetch_add(1, Ordering::SeqCst);
        } else {
            n.fetch_sub(1, Ordering::SeqCst);
        }
    };
    if kind == SinkKind::AuditsOnExit {
        step(&ledger.live_auditing);
    }
    step(&ledger.live_sinks);
}

/// A lease that is still starting. [`Starting::started`] once the driver can
/// hold rows it will audit; dropped instead (the bring-up failed or was
/// abandoned), the lease stays *starting* until its writer goes.
///
/// Forgetting `started()` on a bring-up that DID succeed fails toward
/// under-reporting: that driver is neither waited for nor a loss, only an
/// INFO line. `login_outcome`'s test pins each of its arms for that reason.
#[must_use = "a starting lease is never waited for or reported as a loss until `started()`"]
pub(crate) struct Starting(Arc<Lease>);

impl Starting {
    /// The driver is up: from now on [`super::drain`] waits for it, and a
    /// stuck [`SinkKind::AuditsOnExit`] one is reported as a possible loss.
    pub(crate) fn started(self) {
        self.0.promote();
    }
}

impl SinkWriter {
    /// A writer for `channel` on `ledger`, spawning onto `handle`, whose
    /// lease counts at once: for a driver whose bring-up is never abandoned
    /// with the writer still held (email's: the writer is handed straight to
    /// the driver, with no timeout between). The daemon passes
    /// [`super::daemon_ledger`]; tests pass their own, so their counts are
    /// theirs.
    pub(crate) fn with_ledger(
        ledger: &'static Ledger,
        pool: PgPool,
        handle: tokio::runtime::Handle,
        channel: SinkChannel,
    ) -> Self {
        let lease = Lease::live(ledger, channel.kind());
        Self { ledger, handle, pool, channel, lease }
    }

    /// A writer for `channel` on `ledger` whose lease counts only once [`Starting::started`]
    /// is called: for a driver whose bring-up can be abandoned with the writer
    /// still held (Matrix's login).
    pub(crate) fn starting_with_ledger(
        ledger: &'static Ledger,
        pool: PgPool,
        handle: tokio::runtime::Handle,
        channel: SinkChannel,
    ) -> (Self, Starting) {
        let lease = Lease::starting(ledger, channel.kind());
        let starting = Starting(lease.clone());
        (Self { ledger, handle, pool, channel, lease }, starting)
    }
}

impl Drop for SinkWriter {
    fn drop(&mut self) {
        self.lease.end();
    }
}

#[cfg(test)]
#[path = "audit_sink_lease_tests.rs"]
mod tests;
