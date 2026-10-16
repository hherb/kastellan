//! Which of the bus writers' lost rows get a line of their own (#829).
//!
//! Before #829, [`super::audit_or_report`] said each `channel.*` (or
//! `ask.resolved`) row it could not write on `[audit-lost]` at ERROR. Against
//! a Postgres that is *refused or wedged* that is about one line per 10 s per
//! pump — each insert waits out the pool's acquire timeout first. Against one
//! that **answers quickly with an error** (read-only, disk full, a permission
//! REVOKE) it is one line per inbound message: a flood of messages is a flood
//! of lines, each of which the reporter may block on. This module thins them
//! with the daemon's sinks' thinning ([`crate::audit_thinning`]), which those
//! sinks apply to their shed and after-shutdown refusals
//! (`main/audit_sink_thinning.rs`) — not to their own failed inserts.
//!
//! **Counted per writer and per channel**, as the daemon's sinks count per
//! channel (#807): one channel's flood must not thin the other's lines, and
//! the bus and the boot supervisor are separate writers. The channel is read
//! from the row's own `channel` field, because one [`super::PgChannelEvents`]
//! serves every row its bus writes. A row with no `channel` (`ask.resolved`,
//! `channel.reply_unroutable`)
//! is counted under [`RowChannel::Unnamed`].
//!
//! **One process-wide count** ([`LOST_ROWS`]): a channel's bus is rebuilt on
//! every bring-up, so a count held by the bus would start afresh after each
//! restart — and the rows a restart's count had left unsaid would be said by
//! no one. The shutdown says what is left ([`super::report_unsaid_lost_rows`]);
//! a crash loses it, as it loses the daemon's sinks' (#817).
//!
//! What is **not** thinned: a row abandoned mid-insert by a stop (#813) and
//! work a stop abandoned (#832). Both are said once per stop, per pump or per
//! queued item, never per inbound message.

use std::sync::Mutex;
use std::time::Instant;

use crate::audit_thinning::{Refusal, Thinning};
use crate::worker_stderr::AuditLostWriter;

/// The most channels counted apart per writer; past it, rows count under
/// [`RowChannel::Other`]. Channel names are spelled by the core's own code,
/// never by a peer, so this is never reached in practice — it bounds
/// the count's memory if that ever stops being true.
const MAX_NAMED_CHANNELS: usize = 16;

/// Which channel a lost row is counted under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowChannel {
    /// The row's own `channel` field.
    Named(String),
    /// The row has no `channel` field (`ask.resolved`,
    /// `channel.reply_unroutable`).
    Unnamed,
    /// A channel past [`MAX_NAMED_CHANNELS`] for this writer.
    Other,
}

/// The rows one writer could not write on one channel.
#[derive(Debug)]
struct Entry {
    writer: AuditLostWriter,
    channel: RowChannel,
    thinning: Thinning,
}

/// Every writer's and channel's lost rows, each thinned on its own.
#[derive(Debug)]
pub(crate) struct LostRows {
    /// A `Vec`, not a map: it keeps the order each entry was first lost in,
    /// which [`Self::unsaid`] reports by, and it holds at most a few dozen
    /// (per writer, [`MAX_NAMED_CHANNELS`] plus `Unnamed` and `Other`).
    entries: Mutex<Vec<Entry>>,
}

/// The production count, shared by every bus and boot supervisor.
pub(crate) static LOST_ROWS: LostRows = LostRows::new();

/// One writer's and channel's rows that got no line of their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unsaid {
    pub(crate) writer: AuditLostWriter,
    pub(crate) channel: RowChannel,
    pub(crate) rows: usize,
}

impl LostRows {
    /// No row lost yet.
    pub(crate) const fn new() -> Self {
        Self { entries: Mutex::new(Vec::new()) }
    }

    /// Count one row `writer` could not write, on `channel` (the row's own
    /// field, if it has one): whether it gets a line of its own, and where it
    /// stands in its burst. The clock is read under the lock, as the daemon's
    /// sinks read theirs; the lock is released before the caller reports.
    pub(crate) fn lose(&self, writer: AuditLostWriter, channel: Option<&str>) -> Refusal {
        self.lose_when(writer, channel, Instant::now)
    }

    /// [`Self::lose`], reading the clock through `now` once the lock is held.
    fn lose_when(&self, writer: AuditLostWriter, channel: Option<&str>, now: impl FnOnce() -> Instant) -> Refusal {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let now = now();
        entry(&mut entries, writer, channel).thinning.refuse(now)
    }

    /// Every writer's and channel's rows with no line of their own so far, in
    /// the order each was first lost; those with none are left out.
    pub(crate) fn unsaid(&self) -> Vec<Unsaid> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .iter()
            .map(|e| Unsaid { writer: e.writer, channel: e.channel.clone(), rows: e.thinning.unreported() })
            .filter(|u| u.rows > 0)
            .collect()
    }

    /// Test builds only: [`Self::lose`] at a given instant, so a test can
    /// step past the quiet gap without sleeping.
    #[cfg(test)]
    fn lose_at(&self, writer: AuditLostWriter, channel: Option<&str>, now: Instant) -> Refusal {
        self.lose_when(writer, channel, || now)
    }
}

/// Pure: which channel `writer`'s row on `channel` counts under, given the
/// entries so far — its own name, until the writer already has
/// [`MAX_NAMED_CHANNELS`] of them.
fn row_channel(entries: &[Entry], writer: AuditLostWriter, channel: Option<&str>) -> RowChannel {
    let Some(name) = channel else { return RowChannel::Unnamed };
    let mine = |e: &&Entry| e.writer == writer;
    let known = entries.iter().filter(mine).any(|e| matches!(&e.channel, RowChannel::Named(n) if n == name));
    let named = entries.iter().filter(mine).filter(|e| matches!(e.channel, RowChannel::Named(_))).count();
    if known || named < MAX_NAMED_CHANNELS {
        RowChannel::Named(name.to_string())
    } else {
        RowChannel::Other
    }
}

/// The entry `writer`'s row on `channel` counts under, made on first use.
fn entry<'a>(entries: &'a mut Vec<Entry>, writer: AuditLostWriter, channel: Option<&str>) -> &'a mut Entry {
    let channel = row_channel(entries, writer, channel);
    let at = match entries.iter().position(|e| e.writer == writer && e.channel == channel) {
        Some(at) => at,
        None => {
            entries.push(Entry { writer, channel, thinning: Thinning::new() });
            entries.len() - 1
        }
    };
    &mut entries[at]
}

/// Pure: the shutdown's line for one writer's and channel's rows that got
/// no line of their own — said once, so a flood's tail is not lost with the
/// process (the per-line tally says the rest).
pub(crate) fn format_unsaid(channel: &RowChannel, rows: usize) -> String {
    let whose = match channel {
        RowChannel::Named(name) => format!(" for channel {}", super::quoted_id(name)),
        RowChannel::Unnamed => " naming no channel".to_string(),
        RowChannel::Other => format!(" for channels past the first {MAX_NAMED_CHANNELS}"),
    };
    let (row, was, its) = if rows == 1 { ("row", "was", "its own") } else { ("rows", "were", "their own") };
    format!("{rows} audit {row}{whose} {was} not written and had no line of {its} (thinned past a flood)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_thinning::{BURST_QUIET_GAP, REPORT_EACH_UP_TO};

    /// Lose `n` rows at `at`, returning how many got a line.
    fn lose_n(rows: &LostRows, writer: AuditLostWriter, channel: Option<&str>, n: usize, at: Instant) -> usize {
        (0..n).filter(|_| rows.lose_at(writer, channel, at).report).count()
    }

    /// A flood is thinned like the daemon's: the first rows each, then the
    /// 2^k-th — and what was left out is counted.
    #[test]
    fn a_flood_of_lost_rows_is_thinned_and_counted() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        let said = lose_n(&rows, AuditLostWriter::Bus, Some("matrix"), 100, t0);
        assert_eq!(said, REPORT_EACH_UP_TO + 3, "0..16, then 16, 32 and 64");
        assert_eq!(
            rows.unsaid(),
            [Unsaid { writer: AuditLostWriter::Bus, channel: RowChannel::Named("matrix".into()), rows: 100 - said }]
        );
    }

    /// #807's lesson, for the bus: one channel's flood leaves the other
    /// channel's lines, and the other writer's, unthinned.
    #[test]
    fn each_writer_and_channel_thins_on_its_own() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        lose_n(&rows, AuditLostWriter::Bus, Some("email"), 100, t0);
        let matrix = rows.lose_at(AuditLostWriter::Bus, Some("matrix"), t0);
        assert!(matrix.report && matrix.tally.nth == 1, "another channel's first row: {matrix:?}");
        let supervisor = rows.lose_at(AuditLostWriter::ChannelSupervisor, Some("email"), t0);
        assert!(supervisor.report && supervisor.tally.nth == 1, "another writer's first row: {supervisor:?}");
        let ask = rows.lose_at(AuditLostWriter::Bus, None, t0);
        assert!(ask.report && ask.tally.nth == 1, "a row with no channel counts apart: {ask:?}");
        // The shutdown's count is said as each writer's own, in the order
        // each was first lost.
        let supervisor_said = 1 + lose_n(&rows, AuditLostWriter::ChannelSupervisor, Some("email"), 99, t0);
        let bus_said = REPORT_EACH_UP_TO + 3;
        let email = || RowChannel::Named("email".into());
        assert_eq!(
            rows.unsaid(),
            [
                Unsaid { writer: AuditLostWriter::Bus, channel: email(), rows: 100 - bus_said },
                Unsaid { writer: AuditLostWriter::ChannelSupervisor, channel: email(), rows: 100 - supervisor_said },
            ]
        );
    }

    /// A quiet gap starts a new burst, whose first line says how the last
    /// one ended — the shared core's behaviour, reached through this count.
    #[test]
    fn a_quiet_gap_starts_a_new_burst_of_lines() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        lose_n(&rows, AuditLostWriter::Bus, Some("matrix"), 20, t0);
        let next = rows.lose_at(AuditLostWriter::Bus, Some("matrix"), t0 + BURST_QUIET_GAP);
        assert!(next.report, "{next:?}");
        assert_eq!(next.tally.nth, 1);
        assert!(next.tally.ended.is_some(), "it says how the 20-row burst ended: {next:?}");
    }

    /// Past [`MAX_NAMED_CHANNELS`] a writer's further channels share one
    /// count, so the count's memory stays bounded — while a channel it
    /// already knows keeps its own, and another writer is unaffected.
    #[test]
    fn channels_past_the_cap_share_one_count() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        for i in 0..MAX_NAMED_CHANNELS {
            rows.lose_at(AuditLostWriter::Bus, Some(&format!("c{i}")), t0);
        }
        let entries = |rows: &LostRows| rows.entries.lock().unwrap().len();
        assert_eq!(entries(&rows), MAX_NAMED_CHANNELS, "POSITIVE CONTROL: each named apart");
        rows.lose_at(AuditLostWriter::Bus, Some("one-more"), t0);
        rows.lose_at(AuditLostWriter::Bus, Some("and-another"), t0);
        assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 1, "both counted under one Other entry");
        assert_eq!(rows.lose_at(AuditLostWriter::Bus, Some("c0"), t0).tally.nth, 2, "c0 keeps its own count");
        rows.lose_at(AuditLostWriter::ChannelSupervisor, Some("new"), t0);
        assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 2, "another writer names its own");
        let other = rows.entries.lock().unwrap().iter().filter(|e| e.channel == RowChannel::Other).count();
        assert_eq!(other, 1);
    }

    /// The shutdown's line names the channel quoted, and agrees in number.
    #[test]
    fn the_unsaid_line_names_its_channel_and_count() {
        assert_eq!(
            format_unsaid(&RowChannel::Named("matrix".into()), 83),
            r#"83 audit rows for channel "matrix" were not written and had no line of their own (thinned past a flood)"#
        );
        assert_eq!(
            format_unsaid(&RowChannel::Unnamed, 1),
            "1 audit row naming no channel was not written and had no line of its own (thinned past a flood)"
        );
        assert_eq!(
            format_unsaid(&RowChannel::Other, 2),
            "2 audit rows for channels past the first 16 were not written and had no line of their own \
             (thinned past a flood)"
        );
        let hostile = format_unsaid(&RowChannel::Named("x\"\n[SKIP]".into()), 2);
        assert!(!hostile.contains('\n'), "a channel name is quoted: {hostile}");
    }
}
