//! A sink's **lease** on the ledger (#792), and when it starts to count
//! (#802). Split out of `audit_sink.rs` to keep it under the 500-LOC soft cap;
//! `#[path]`-included there, and its items re-exported by name.
//!
//! While a [`SinkWriter`] is alive, [`super::drain`] counts its driver as one
//! that may still write a row. A sink's hook closure owns its writer, and the
//! driver owns the hook, so the lease ends exactly when the driver drops its
//! hooks — as its thread returns, after any `driver_exit` rows are spawned. No
//! driver has to remember to report its exit.
//!
//! **A lease can start before its driver does** ([`SinkWriter::starting`]).
//! Matrix builds its writer before a login that can time out, and a timed-out
//! login's blocking task cannot be cancelled: it keeps the writer, and with it
//! the lease, until the SDK's own timeouts end it. Counted from the start,
//! that lease made every shutdown wait the full drain bound for a driver that
//! never ran, then report it on `[audit-lost]` as one whose queued replies
//! might go unaudited — when none was ever queued (#796, #802). So such a
//! lease counts only as *starting* (said at INFO, not waited for) until
//! [`Starting::started`] says the driver is up.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use sqlx::PgPool;

use super::{Ledger, SinkWriter, LEDGER};

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
    fn new(ledger: &'static Ledger, kind: SinkKind, state: LeaseState) -> Arc<Self> {
        match state {
            LeaseState::Starting => {
                ledger.starting.fetch_add(1, Ordering::SeqCst);
            }
            LeaseState::Live => count_live(ledger, kind, true),
            LeaseState::Ended => {}
        }
        Arc::new(Self { ledger, kind, state: Mutex::new(state) })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `Starting` → `Live`; anything else stays as it is.
    fn promote(&self) {
        let mut state = self.state();
        if *state == LeaseState::Starting {
            count_live(self.ledger, self.kind, true);
            self.ledger.starting.fetch_sub(1, Ordering::SeqCst);
            *state = LeaseState::Live;
        }
    }

    /// The writer is gone: stop counting it, wherever it was counted.
    pub(super) fn end(&self) {
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
/// snapshot that sees it sees the kind's count too. Out: `live_sinks` last as
/// well — the driver has spawned its final rows before this, and
/// `Ledger::snapshot` reads `live_sinks` first (see there).
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
    /// A writer on the daemon's ledger, spawning onto `handle`, whose lease
    /// counts at once: for a driver built in the same call.
    pub(crate) fn new(pool: PgPool, handle: tokio::runtime::Handle, kind: SinkKind) -> Self {
        Self::with_ledger(&LEDGER, pool, handle, kind)
    }

    /// A writer on the daemon's ledger whose lease counts only once
    /// [`Starting::started`] is called: for a driver whose bring-up can be
    /// abandoned with the writer still held (Matrix's login).
    pub(crate) fn starting(
        pool: PgPool,
        handle: tokio::runtime::Handle,
        kind: SinkKind,
    ) -> (Self, Starting) {
        Self::starting_with_ledger(&LEDGER, pool, handle, kind)
    }

    /// [`Self::new`] on `ledger`: tests use their own, so their counts are theirs.
    pub(crate) fn with_ledger(
        ledger: &'static Ledger,
        pool: PgPool,
        handle: tokio::runtime::Handle,
        kind: SinkKind,
    ) -> Self {
        let lease = Lease::new(ledger, kind, LeaseState::Live);
        Self { ledger, handle, pool, lease }
    }

    /// [`Self::starting`] on `ledger`.
    pub(crate) fn starting_with_ledger(
        ledger: &'static Ledger,
        pool: PgPool,
        handle: tokio::runtime::Handle,
        kind: SinkKind,
    ) -> (Self, Starting) {
        let lease = Lease::new(ledger, kind, LeaseState::Starting);
        let starting = Starting(lease.clone());
        (Self { ledger, handle, pool, lease }, starting)
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
