//! The daemon's lost audit rows, thinned per channel (#807), per action
//! (#837, #865) and per kind: [`Refusals`] holds one [`Thinning`] per
//! channel, action and kind — shed, late, and since #861 failed inserts, by
//! outcome since #864 — and [`refuse`] reads the clock and reports. The
//! pure core — [`Thinning`], the predicates and [`BurstTally`] — is the
//! library's `kastellan_core::audit_thinning` since #829, which the channel
//! bus's lost-row writer shares; why rows are
//! thinned at all is said there. Split out of `audit_sink.rs` to keep it
//! under the 500-LOC soft cap; `#[path]`-included there. Its tests are in
//! `audit_sink_thinning_tests.rs`.
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
//! conversation whose audit row is missing, became an anonymous count. And
//! what each action's thinning left unsaid is said per action at shutdown
//! too ([`Unreported`], #867), as the channel bus says its own.
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
/// row of. A `Vec`: an action is a `&'static str` — in practice the core's
/// own sinks' constants, a few per channel — so it stays short, and no peer-
/// or runtime-spelled string reaches it short of a deliberate leak.
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

    /// Each action's, in the order each first lost a row; the actions with
    /// none left out. Never summed across actions (#867): the shutdown says
    /// each action's apart, so a thinned `channel.reply_undelivered` row is
    /// not hidden in a total with the channel's other rows.
    fn unreported(&self) -> Vec<ActionUnsaid> {
        let actions = self.actions.lock().unwrap_or_else(|p| p.into_inner());
        actions
            .iter()
            .map(|a| ActionUnsaid { action: a.action, counts: a.unreported() })
            .filter(|a| a.counts.total() > 0)
            .collect()
    }
}

/// Every channel's refused rows, each action and kind thinned on its own
/// (#807; per action #837, #865).
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

    /// The refused rows that got no line of their own, so far, per channel
    /// and action.
    pub(super) fn unreported(&self) -> Unreported {
        Unreported { matrix: self.matrix.unreported(), email: self.email.unreported() }
    }
}

/// One action's refused rows with no line of their own on one channel, by
/// kind (#807: said apart at shutdown, so last week's flood reads apart from
/// this shutdown).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnsaidCounts {
    /// Shed: too many inserts already waiting for the pool.
    pub(super) shed: usize,
    /// Tried after the shutdown drain closed the ledger.
    pub(super) late: usize,
    /// Tried, and the insert failed with the row known not to be in the
    /// table: Postgres answered with an error or wrote no row, or it was
    /// never sent (#858, #864).
    pub(super) failed_not_written: usize,
    /// Tried, and the insert failed in a way that does not prove the row
    /// absent — a lost connection, a server crash, an ended session — so it
    /// may have been committed (#858, #864; `InsertOutcome::Unknown`).
    pub(super) failed_unknown: usize,
}

impl UnsaidCounts {
    pub(super) fn total(self) -> usize {
        self.shed + self.late + self.failed_not_written + self.failed_unknown
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

/// One action's [`UnsaidCounts`] on one channel: what one shutdown line says
/// (#867).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActionUnsaid {
    pub(super) action: &'static str,
    pub(super) counts: UnsaidCounts,
}

/// Every channel's refused rows with no line of their own: per action, in
/// the order each action first lost a row, the actions with none left out —
/// so [`Unreported::default`] is "nothing unsaid".
///
/// Per action, not summed per channel (#867): the channel bus's shutdown
/// says its rows per action too (`pg_events/lost_rows.rs`), and a sum would
/// fold a thinned `channel.reply_undelivered` row — the one row naming a
/// conversation whose reply was lost — into an anonymous total with the
/// channel's `channel.inbound_dropped` ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Unreported {
    pub(super) matrix: Vec<ActionUnsaid>,
    pub(super) email: Vec<ActionUnsaid>,
}

impl Unreported {
    /// `channel`'s actions.
    pub(super) fn of(&self, channel: SinkChannel) -> &[ActionUnsaid] {
        match channel {
            SinkChannel::Matrix => &self.matrix,
            SinkChannel::Email => &self.email,
        }
    }

    /// Test builds only: every channel's and action's, all kinds. The daemon
    /// says each channel's (#807) and action's (#867) apart and never sums
    /// them.
    #[cfg(test)]
    pub(super) fn total(&self) -> usize {
        self.matrix.iter().chain(&self.email).map(|a| a.counts.total()).sum()
    }

    /// Test builds only: `channel`'s counts of `action`, none if it has no
    /// entry.
    #[cfg(test)]
    pub(super) fn counts(&self, channel: SinkChannel, action: &str) -> UnsaidCounts {
        self.of(channel).iter().find(|a| a.action == action).map(|a| a.counts).unwrap_or_default()
    }

    /// The rows counted since `base` was taken, per channel, action and kind
    /// — so a residual line can still say whose they were (#807) and of
    /// which action (#867).
    pub(super) fn since(&self, base: &Self) -> Self {
        Self { matrix: actions_since(&self.matrix, &base.matrix), email: actions_since(&self.email, &base.email) }
    }
}

/// Pure: each of `now`'s actions less its count in `base` (none, if `base`
/// has no entry for it), kind by kind; the actions with none left out. Each
/// count only grows and an action, once counted, keeps its entry, so nothing
/// in `base` is missing from `now`.
fn actions_since(now: &[ActionUnsaid], base: &[ActionUnsaid]) -> Vec<ActionUnsaid> {
    now.iter()
        .map(|a| {
            let before = base.iter().find(|b| b.action == a.action).map(|b| b.counts).unwrap_or_default();
            ActionUnsaid { action: a.action, counts: a.counts.since(before) }
        })
        .filter(|a| a.counts.total() > 0)
        .collect()
}

/// Tell `on_failure` of `channel`'s lost row of `action` — unless its burst
/// has passed the thinning and its cause is not new (#859), in which case it
/// is only counted — and hand the
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
    if refusal.report() {
        on_failure(why);
    }
    why
}

#[cfg(test)]
#[path = "audit_sink_thinning_tests.rs"]
mod tests;
