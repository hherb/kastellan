//! The daemon's refused audit rows, thinned per channel (#807): [`Refusals`]
//! holds one [`Thinning`] behind a lock per channel and kind, and [`refuse`]
//! reads the clock and reports. The pure core — [`Thinning`], the predicates
//! and [`BurstTally`] — is the library's `kastellan_core::audit_thinning`
//! since #829, which the channel bus's lost-row writer shares; why rows are
//! thinned at all is said there. Split out of `audit_sink.rs` to keep it
//! under the 500-LOC soft cap; `#[path]`-included there.
//!
//! **Each channel thins on its own** ([`Refusals`], #807). One shared count
//! let a compromised email worker — its `skipped` list has no cap — hold a
//! shed burst open with one refusal every 59 s, so a Matrix
//! `channel.reply_undelivered` row shed meanwhile got no line naming its
//! conversation, only an anonymous share of the shutdown count.

use std::sync::Mutex;
use std::time::Instant;

pub(crate) use kastellan_core::audit_thinning::BurstTally;
use kastellan_core::audit_thinning::Thinning;

use super::lease::SinkChannel;
use super::Unwritten;

/// One channel's refused rows: shed apart from late, so a flood shed earlier
/// cannot silence the rows refused at shutdown (#798).
#[derive(Debug)]
struct ChannelRefusals {
    shed: Mutex<Thinning>,
    late: Mutex<Thinning>,
}

impl ChannelRefusals {
    const fn new() -> Self {
        Self { shed: Mutex::new(Thinning::new()), late: Mutex::new(Thinning::new()) }
    }

    fn unreported(&self) -> UnsaidCounts {
        let count = |t: &Mutex<Thinning>| t.lock().unwrap_or_else(|p| p.into_inner()).unreported();
        UnsaidCounts { shed: count(&self.shed), late: count(&self.late) }
    }
}

/// Every channel's refused rows, each thinned on its own (#807).
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

    /// `channel`'s rows shed: the queue was full.
    pub(super) fn shed(&self, channel: SinkChannel) -> &Mutex<Thinning> {
        &self.of(channel).shed
    }

    /// `channel`'s rows refused after the ledger closed.
    pub(super) fn late(&self, channel: SinkChannel) -> &Mutex<Thinning> {
        &self.of(channel).late
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
}

impl UnsaidCounts {
    pub(super) fn total(self) -> usize {
        self.shed + self.late
    }

    /// The rows counted since `base` was taken. Each count only grows, so the
    /// saturation hides nothing.
    fn since(self, base: Self) -> Self {
        Self { shed: self.shed.saturating_sub(base.shed), late: self.late.saturating_sub(base.late) }
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

    /// Test builds only: every channel's, both kinds. The daemon says each
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

/// Tell `on_failure` of a refused row — unless the burst has passed the
/// thinning, in which case it is only counted — and hand the [`Unwritten`]
/// (`why`, given the row's tally) back for the caller's `Err`. Called on the
/// caller's thread for a shed or late row, and from the spawned task on its
/// (unreachable) closed-semaphore path.
pub(super) fn refuse(
    thinning: &Mutex<Thinning>,
    why: fn(BurstTally) -> Unwritten<'static>,
    on_failure: impl FnOnce(Unwritten<'_>),
) -> Unwritten<'static> {
    // The lock is released before `on_failure` runs: a report can block.
    let refusal = thinning.lock().unwrap_or_else(|p| p.into_inner()).refuse(Instant::now());
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

    /// #807: one channel's flood leaves the other's rows reported, and the
    /// counts stay apart by channel and by kind.
    #[test]
    fn one_channels_flood_does_not_thin_the_others_lines() {
        let refusals = Refusals::new();
        let t0 = Instant::now();
        for _ in 0..100 {
            refusals.shed(SinkChannel::Email).lock().unwrap().refuse(t0);
        }
        let matrix = refusals.shed(SinkChannel::Matrix).lock().unwrap().refuse(t0);
        assert!(matrix.report, "Matrix's first shed row is its own burst's first");
        assert_eq!(matrix.tally.nth, 1);
        refusals.late(SinkChannel::Email).lock().unwrap().refuse(t0);
        let unreported = refusals.unreported();
        assert_eq!(unreported.email, UnsaidCounts { shed: unreported_of(100), late: 0 });
        assert_eq!(unreported.matrix, UnsaidCounts::default());
        assert_eq!(unreported.total(), unreported_of(100));
    }
}
