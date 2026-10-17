//! The daemon's lost audit rows, thinned per channel (#807), per action
//! (#837, #865) and per kind: [`Refusals`] holds one [`Thinning`] per
//! channel, action and kind — shed, late, and since #861 failed inserts, by
//! outcome since #864 — and [`refuse`] reads the clock and reports. The
//! pure core — [`Thinning`], the predicates and [`BurstTally`] — is the library's `kastellan_core::audit_thinning`
//! since #829, which the channel bus's lost-row writer shares; why rows are
//! thinned at all is said there. Split out of `audit_sink.rs` to keep it
//! under the 500-LOC soft cap; `#[path]`-included there.
//!
//! **Each channel thins on its own** ([`Refusals`], #807). One shared count
//! let a compromised email worker — its `skipped` list has no cap — hold a
//! shed burst open with one refusal every 59 s, so a Matrix
//! `channel.reply_undelivered` row shed meanwhile got no line naming its
//! conversation, only an anonymous share of the shutdown count.
//!
//! **Each action thins on its own** (#837, #865). One channel's sinks write
//! several actions — Matrix's `channel.reply_undelivered`,
//! `channel.inbound_dropped` and `channel.inbound_undecodable` — and one
//! count for all of them let a flood of one push another past the thinning:
//! a failed `channel.reply_undelivered` row, the one line naming the
//! conversation whose audit row is missing, became an anonymous count.
//!
//! **Failed inserts are thinned too** (#861). Before, only rows never tried
//! (shed, late) were: a Postgres that answers every insert at once with an
//! error — read-only, disk full, a permission REVOKE — gave every audited
//! event its own `[audit-lost]` line, the flood #829 ended on the channel
//! bus. They are counted in a kind of their own, so a flood of them cannot
//! silence a shed row's line, nor the reverse — and in two, by outcome
//! (#864), so the shutdown can say how many rows are certainly missing and
//! how many to look for in `audit_log`. Within a kind, the first row of each
//! new cause gets its own line (#859, `kastellan_core::audit_thinning`).

use std::sync::Mutex;
use std::time::Instant;

pub(crate) use kastellan_core::audit_thinning::BurstTally;
use kastellan_core::audit_thinning::{Refusal, Thinning};
use kastellan_db::audit::{InsertCause, InsertError, InsertOutcome};

use super::lease::SinkChannel;
use super::Unwritten;

/// How a row was lost: which of its action's counts it goes in.
#[derive(Clone, Copy, Debug)]
pub(super) enum Loss {
    /// Shed: the queue was full.
    Shed,
    /// Refused after the ledger closed.
    Late,
    /// The insert ran and failed: whether the row may be there anyway
    /// (#858), and what kind of failure it was (#859).
    Failed { outcome: InsertOutcome, cause: InsertCause },
}

impl Loss {
    /// `failed`'s loss.
    pub(super) fn failed(failed: &InsertError) -> Self {
        Self::Failed { outcome: failed.outcome(), cause: failed.cause() }
    }
}

/// One action's lost rows on one channel: shed apart from late, so a flood
/// shed earlier cannot silence the rows refused at shutdown (#798), and
/// failed inserts apart from both (#861), by outcome (#864).
#[derive(Debug)]
struct ActionRefusals {
    action: &'static str,
    shed: Thinning,
    late: Thinning,
    not_written: Thinning<InsertCause>,
    unknown: Thinning<InsertCause>,
}

impl ActionRefusals {
    fn new(action: &'static str) -> Self {
        Self {
            action,
            shed: Thinning::new(),
            late: Thinning::new(),
            not_written: Thinning::new(),
            unknown: Thinning::new(),
        }
    }

    fn refuse(&mut self, now: Instant, loss: Loss) -> Refusal {
        match loss {
            Loss::Shed => self.shed.refuse(now, ()),
            Loss::Late => self.late.refuse(now, ()),
            Loss::Failed { outcome: InsertOutcome::NotWritten, cause } => self.not_written.refuse(now, cause),
            Loss::Failed { outcome: InsertOutcome::Unknown, cause } => self.unknown.refuse(now, cause),
        }
    }

    fn unreported(&self) -> UnsaidCounts {
        UnsaidCounts {
            shed: self.shed.unreported(),
            late: self.late.unreported(),
            failed_not_written: self.not_written.unreported(),
            failed_unknown: self.unknown.unreported(),
        }
    }
}

/// One channel's lost rows, one [`ActionRefusals`] per action it has lost a
/// row of. A `Vec`: an action is a `&'static str` the core's own sinks
/// spell (a few per channel), so it stays short — and never a peer's word.
#[derive(Debug)]
struct ChannelRefusals {
    actions: Mutex<Vec<ActionRefusals>>,
}

impl ChannelRefusals {
    const fn new() -> Self {
        Self { actions: Mutex::new(Vec::new()) }
    }

    /// Count one row of `action` lost as `loss`, reading the clock under the
    /// lock.
    fn refuse(&self, action: &'static str, loss: Loss) -> Refusal {
        let mut actions = self.actions.lock().unwrap_or_else(|p| p.into_inner());
        let at = match actions.iter().position(|a| a.action == action) {
            Some(at) => at,
            None => {
                actions.push(ActionRefusals::new(action));
                actions.len() - 1
            }
        };
        actions[at].refuse(Instant::now(), loss)
    }

    /// Every action's, summed: the shutdown says a channel's by kind.
    fn unreported(&self) -> UnsaidCounts {
        let actions = self.actions.lock().unwrap_or_else(|p| p.into_inner());
        actions.iter().map(ActionRefusals::unreported).fold(UnsaidCounts::default(), UnsaidCounts::plus)
    }
}

/// Every channel's refused rows, each action and kind thinned on its own
/// (#807, #837).
#[derive(Debug)]
pub(super) struct Refusals {
    matrix: ChannelRefusals,
    email: ChannelRefusals,
}

impl Refusals {
    pub(super) const fn new() -> Self {
        Self { matrix: ChannelRefusals::new(), email: ChannelRefusals::new() }
    }

    fn of(&self, channel: SinkChannel) -> &ChannelRefusals {
        match channel {
            SinkChannel::Matrix => &self.matrix,
            SinkChannel::Email => &self.email,
        }
    }

    /// Count one of `channel`'s rows of `action`, lost as `loss`: whether it
    /// gets a line of its own, and where it stands in its burst.
    pub(super) fn refuse(&self, channel: SinkChannel, action: &'static str, loss: Loss) -> Refusal {
        self.of(channel).refuse(action, loss)
    }

    /// The refused rows that got no line of their own, so far, per channel.
    pub(super) fn unreported(&self) -> Unreported {
        Unreported { matrix: self.matrix.unreported(), email: self.email.unreported() }
    }
}

/// One channel's refused rows with no line of their own, by kind (#807: said
/// apart at shutdown, so last week's flood reads apart from this shutdown).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnsaidCounts {
    /// Shed: too many inserts already waiting for the pool.
    pub(super) shed: usize,
    /// Tried after the shutdown drain closed the ledger.
    pub(super) late: usize,
    /// Tried, and the insert failed with the row known not to be in the
    /// table: refused by Postgres, or never sent (#858, #864).
    pub(super) failed_not_written: usize,
    /// Tried, and the insert failed in a way that may have committed the
    /// row: the connection was lost mid-insert (#858, #864).
    pub(super) failed_unknown: usize,
}

impl UnsaidCounts {
    pub(super) fn total(self) -> usize {
        self.shed + self.late + self.failed_not_written + self.failed_unknown
    }

    /// Both counts, kind by kind.
    fn plus(self, other: Self) -> Self {
        Self {
            shed: self.shed + other.shed,
            late: self.late + other.late,
            failed_not_written: self.failed_not_written + other.failed_not_written,
            failed_unknown: self.failed_unknown + other.failed_unknown,
        }
    }

    /// The rows counted since `base` was taken. Each count only grows, so the
    /// saturation hides nothing.
    fn since(self, base: Self) -> Self {
        Self {
            shed: self.shed.saturating_sub(base.shed),
            late: self.late.saturating_sub(base.late),
            failed_not_written: self.failed_not_written.saturating_sub(base.failed_not_written),
            failed_unknown: self.failed_unknown.saturating_sub(base.failed_unknown),
        }
    }
}

/// Every channel's [`UnsaidCounts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Unreported {
    pub(super) matrix: UnsaidCounts,
    pub(super) email: UnsaidCounts,
}

impl Unreported {
    pub(super) fn of(self, channel: SinkChannel) -> UnsaidCounts {
        match channel {
            SinkChannel::Matrix => self.matrix,
            SinkChannel::Email => self.email,
        }
    }

    /// Test builds only: every channel's, all kinds. The daemon says each
    /// channel's apart and never sums them (#807).
    #[cfg(test)]
    pub(super) fn total(self) -> usize {
        self.matrix.total() + self.email.total()
    }

    /// The rows counted since `base` was taken, per channel and kind — so a
    /// residual line can still say whose they were (#807).
    pub(super) fn since(self, base: Self) -> Self {
        Self { matrix: self.matrix.since(base.matrix), email: self.email.since(base.email) }
    }
}

/// Tell `on_failure` of `channel`'s lost row of `action` — unless its burst
/// has passed the thinning, in which case it is only counted — and hand the
/// [`Unwritten`] (`why`, given the row's tally) back for the caller's `Err`.
/// Called on the caller's thread for a shed or late row, and from the
/// spawned task for a failed insert (#861) and on its (unreachable)
/// closed-semaphore path.
pub(super) fn refuse<'a>(
    refusals: &Refusals,
    (channel, action): (SinkChannel, &'static str),
    loss: Loss,
    why: impl FnOnce(BurstTally) -> Unwritten<'a>,
    on_failure: impl FnOnce(Unwritten<'_>),
) -> Unwritten<'a> {
    // The lock is released before `on_failure` runs: a report can block.
    let refusal = refusals.refuse(channel, action, loss);
    let why = why(refusal.tally);
    if refusal.report {
        on_failure(why);
    }
    why
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_core::audit_thinning::unreported_of;

    const READ_ONLY: Loss =
        Loss::Failed { outcome: InsertOutcome::NotWritten, cause: InsertCause::Client("read-only") };
    const RESET: Loss = Loss::Failed {
        outcome: InsertOutcome::Unknown,
        cause: InsertCause::Io(std::io::ErrorKind::ConnectionReset),
    };

    /// Lose `n` of `channel`'s rows of `action` as `loss`.
    fn lose(refusals: &Refusals, channel: SinkChannel, action: &'static str, loss: Loss, n: usize) {
        for _ in 0..n {
            refusals.refuse(channel, action, loss);
        }
    }

    /// #807: one channel's flood leaves the other's rows reported, and the
    /// counts stay apart by channel and by kind.
    #[test]
    fn one_channels_flood_does_not_thin_the_others_lines() {
        let refusals = Refusals::new();
        lose(&refusals, SinkChannel::Email, "a", Loss::Shed, 100);
        let matrix = refusals.refuse(SinkChannel::Matrix, "a", Loss::Shed);
        assert!(matrix.report, "Matrix's first shed row is its own burst's first");
        assert_eq!(matrix.tally.nth, 1);
        refusals.refuse(SinkChannel::Email, "a", Loss::Late);
        let unreported = refusals.unreported();
        assert_eq!(unreported.email, UnsaidCounts { shed: unreported_of(100), ..UnsaidCounts::default() });
        assert_eq!(unreported.matrix, UnsaidCounts::default());
        assert_eq!(unreported.total(), unreported_of(100));
    }

    /// #837, #865: one action's flood leaves the same channel's other
    /// actions their lines — and the channel's count sums every action's.
    #[test]
    fn one_actions_flood_does_not_thin_anothers_lines() {
        let refusals = Refusals::new();
        lose(&refusals, SinkChannel::Matrix, "channel.inbound_dropped", READ_ONLY, 100);
        let reply = refusals.refuse(SinkChannel::Matrix, "channel.reply_undelivered", READ_ONLY);
        assert!(reply.report && reply.tally.nth == 1, "another action's first row: {reply:?}");
        lose(&refusals, SinkChannel::Matrix, "channel.reply_undelivered", Loss::Shed, 20);
        lose(&refusals, SinkChannel::Matrix, "channel.inbound_dropped", Loss::Shed, 20);
        let matrix = refusals.unreported().matrix;
        assert_eq!(
            matrix,
            UnsaidCounts { shed: 2 * unreported_of(20), failed_not_written: unreported_of(100), ..Default::default() }
        );
    }

    /// #861: failed inserts are a kind of their own — a flood of them leaves
    /// the same channel's first shed row its burst's first — and they are in
    /// the channel's total, so the shutdown line counts them.
    #[test]
    fn failed_inserts_thin_apart_from_shed_rows() {
        let refusals = Refusals::new();
        lose(&refusals, SinkChannel::Email, "a", READ_ONLY, 100);
        let shed = refusals.refuse(SinkChannel::Email, "a", Loss::Shed);
        assert!(shed.report, "the first shed row is its own burst's first");
        assert_eq!(shed.tally.nth, 1);
        let email = refusals.unreported().email;
        assert_eq!(email, UnsaidCounts { failed_not_written: unreported_of(100), ..Default::default() });
        assert_eq!(email.total(), unreported_of(100));
        let base = UnsaidCounts { failed_not_written: 10, ..Default::default() };
        assert_eq!(email.since(base).failed_not_written, unreported_of(100) - 10, "`since` counts it too");
    }

    /// #864: a failed insert's outcome is its count's, so the shutdown can
    /// say how many rows are certainly missing and how many may be there —
    /// and a flood of one outcome leaves the other's lines.
    #[test]
    fn failed_inserts_are_counted_by_outcome() {
        let refusals = Refusals::new();
        lose(&refusals, SinkChannel::Email, "a", READ_ONLY, 100);
        let reset = refusals.refuse(SinkChannel::Email, "a", RESET);
        assert!(reset.report && reset.tally.nth == 1, "the other outcome's first row: {reset:?}");
        lose(&refusals, SinkChannel::Email, "a", RESET, 39);
        let email = refusals.unreported().email;
        assert_eq!(
            email,
            UnsaidCounts { failed_not_written: unreported_of(100), failed_unknown: unreported_of(40), ..Default::default() }
        );
        let base = UnsaidCounts { failed_unknown: 1, ..Default::default() };
        assert_eq!(email.since(base).failed_unknown, unreported_of(40) - 1, "`since` counts it too");
        assert_eq!(email.plus(email).failed_unknown, 2 * unreported_of(40), "`plus` counts it too");
    }

    /// A failed insert's loss is its own outcome and cause.
    #[test]
    fn a_failed_inserts_loss_is_its_outcome_and_cause() {
        let cause = InsertCause::Io(std::io::ErrorKind::BrokenPipe);
        let error = InsertError::assumed_for_test(InsertOutcome::Unknown, cause, kastellan_db::DbError::Query("x".into()));
        assert!(matches!(
            Loss::failed(&error),
            Loss::Failed { outcome: InsertOutcome::Unknown, cause: c } if c == cause
        ));
    }

    /// #859, reached through the sinks' count: a new cause of the same
    /// outcome gets its line mid-flood.
    #[test]
    fn a_new_cause_mid_flood_gets_its_line() {
        let refusals = Refusals::new();
        lose(&refusals, SinkChannel::Email, "a", READ_ONLY, 100);
        let revoked = Loss::Failed { outcome: InsertOutcome::NotWritten, cause: InsertCause::Client("revoked") };
        let new = refusals.refuse(SinkChannel::Email, "a", revoked);
        assert!(new.report && new.tally.first_of_cause, "{new:?}");
        assert!(!refusals.refuse(SinkChannel::Email, "a", revoked).report, "then thinned again");
    }
}
