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
//! sinks apply to their shed, after-shutdown and (since #861) failed-insert
//! rows (`main/audit_sink_thinning.rs`).
//!
//! **Counted per writer, channel, action and outcome**, as the daemon's sinks
//! count per channel (#807) and action (#837, #865): one channel's flood must
//! not thin the other's lines, nor one action's another's (a flood of failed
//! `channel.received` rows would thin the one `channel.reply_undelivered`
//! line naming a lost reply), and the bus and the boot supervisor are
//! separate writers. The outcome (#864) keeps the rows known not to be in
//! `audit_log` apart from those that may be, so the shutdown can say both.
//! Within a count, the first row of each new cause gets its own line (#859,
//! [`crate::audit_thinning`]). The channel is read from the row's own
//! `channel` field, because one [`super::PgChannelEvents`] serves every row
//! its bus writes. A row with no `channel` (`ask.resolved`,
//! `channel.reply_unroutable`) is counted under [`RowChannel::Unnamed`].
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

use kastellan_db::audit::{InsertCause, InsertOutcome};

use crate::audit_thinning::{Refusal, Thinning};
use crate::worker_stderr::AuditLostWriter;

/// The most channels counted apart per writer; past it, rows count under
/// [`RowChannel::Other`]. Channel names are spelled by the core's own code,
/// never by a peer, so this is never reached in practice — it bounds
/// the count's memory if that ever stops being true. (Actions need no such
/// bound: [`RowKey::action`] is `&'static str`, so only the core's own
/// constants can be one — a few dozen at most.)
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

/// What a lost row is counted by: who wrote it, the row's own `channel`
/// field (if it has one), its action, and what its failed insert says about
/// it (#858).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowKey<'a> {
    pub(crate) writer: AuditLostWriter,
    pub(crate) channel: Option<&'a str>,
    pub(crate) action: &'static str,
    pub(crate) outcome: InsertOutcome,
}

/// The rows one writer could not write on one channel, of one action and
/// outcome.
#[derive(Debug)]
struct Entry {
    writer: AuditLostWriter,
    channel: RowChannel,
    action: &'static str,
    outcome: InsertOutcome,
    thinning: Thinning<InsertCause>,
}

/// Every count's lost rows, each thinned on its own.
#[derive(Debug)]
pub(crate) struct LostRows {
    /// A `Vec`, not a map: it keeps the order each entry was first lost in,
    /// which [`Self::unsaid`] reports by, and it stays short (per writer,
    /// [`MAX_NAMED_CHANNELS`] plus `Unnamed` and `Other` channels, each with
    /// the few actions the core writes, by two outcomes).
    entries: Mutex<Vec<Entry>>,
}

/// The production count, shared by every bus and boot supervisor.
pub(crate) static LOST_ROWS: LostRows = LostRows::new();

/// One count's rows that got no line of their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unsaid {
    pub(crate) writer: AuditLostWriter,
    pub(crate) channel: RowChannel,
    pub(crate) action: &'static str,
    pub(crate) outcome: InsertOutcome,
    pub(crate) rows: usize,
}

impl LostRows {
    /// No row lost yet.
    pub(crate) const fn new() -> Self {
        Self { entries: Mutex::new(Vec::new()) }
    }

    /// Count one row that could not be written, counted by `key`, whose
    /// insert failed for `cause` (#859): whether it gets a line of its own,
    /// and where it stands in its burst. The clock is read under the lock,
    /// as the daemon's sinks read theirs; the lock is released before the
    /// caller reports.
    pub(crate) fn lose(&self, key: RowKey<'_>, cause: InsertCause) -> Refusal {
        self.lose_when(key, cause, Instant::now)
    }

    /// [`Self::lose`], reading the clock through `now` once the lock is held.
    fn lose_when(&self, key: RowKey<'_>, cause: InsertCause, now: impl FnOnce() -> Instant) -> Refusal {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let now = now();
        entry(&mut entries, key).thinning.refuse(now, cause)
    }

    /// Every count's rows with no line of their own so far, in the order each
    /// was first lost; those with none are left out.
    pub(crate) fn unsaid(&self) -> Vec<Unsaid> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .iter()
            .map(|e| Unsaid {
                writer: e.writer,
                channel: e.channel.clone(),
                action: e.action,
                outcome: e.outcome,
                rows: e.thinning.unreported(),
            })
            .filter(|u| u.rows > 0)
            .collect()
    }

    /// Test builds only: [`Self::lose`] at a given instant, so a test can
    /// step past the quiet gap without sleeping.
    #[cfg(test)]
    fn lose_at(&self, key: RowKey<'_>, cause: InsertCause, now: Instant) -> Refusal {
        self.lose_when(key, cause, || now)
    }
}

/// Pure: which channel `writer`'s row on `channel` counts under, given the
/// entries so far — its own name, until the writer already has
/// [`MAX_NAMED_CHANNELS`] distinct ones.
fn row_channel(entries: &[Entry], writer: AuditLostWriter, channel: Option<&str>) -> RowChannel {
    let Some(name) = channel else { return RowChannel::Unnamed };
    let mut named: Vec<&str> = entries
        .iter()
        .filter(|e| e.writer == writer)
        .filter_map(|e| match &e.channel {
            RowChannel::Named(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    named.sort_unstable();
    named.dedup();
    if named.contains(&name) || named.len() < MAX_NAMED_CHANNELS {
        RowChannel::Named(name.to_string())
    } else {
        RowChannel::Other
    }
}

/// The entry a row counted by `key` goes in, made on first use.
fn entry<'a>(entries: &'a mut Vec<Entry>, key: RowKey<'_>) -> &'a mut Entry {
    let channel = row_channel(entries, key.writer, key.channel);
    let mine =
        |e: &Entry| e.writer == key.writer && e.channel == channel && e.action == key.action && e.outcome == key.outcome;
    let at = match entries.iter().position(mine) {
        Some(at) => at,
        None => {
            entries.push(Entry {
                writer: key.writer,
                channel,
                action: key.action,
                outcome: key.outcome,
                thinning: Thinning::new(),
            });
            entries.len() - 1
        }
    };
    &mut entries[at]
}

/// Pure: the shutdown's line for one count's rows that got no line of their
/// own — said once, so a flood's tail is not lost with the process (the
/// per-line tally says the rest). It says whether they are known not to be
/// in `audit_log` (#864): since #858 some failed inserts lost their
/// connection and may have been committed, and those are counted apart.
pub(crate) fn format_unsaid(unsaid: &Unsaid) -> String {
    let whose = match &unsaid.channel {
        RowChannel::Named(name) => format!(" for channel {}", super::quoted_id(name)),
        RowChannel::Unnamed => " naming no channel".to_string(),
        RowChannel::Other => format!(" for channels past the first {MAX_NAMED_CHANNELS}"),
    };
    let rows = unsaid.rows;
    let (row, its) = if rows == 1 { ("row", "its own") } else { ("rows", "their own") };
    format!(
        "{rows} {} audit {row}{whose} failed to insert ({}) and had no line of {its} (thinned past a flood)",
        unsaid.action,
        super::insert_verdict(unsaid.outcome),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_thinning::{BURST_QUIET_GAP, REPORT_EACH_UP_TO};

    const READ_ONLY: InsertCause = InsertCause::Client("read-only");

    /// A `channel.received` row of `writer` on `channel`, known not written.
    fn received(writer: AuditLostWriter, channel: Option<&str>) -> RowKey<'_> {
        RowKey { writer, channel, action: "channel.received", outcome: InsertOutcome::NotWritten }
    }

    /// Lose `n` rows counted by `key` at `at`, returning how many got a line.
    fn lose_n(rows: &LostRows, key: RowKey<'_>, n: usize, at: Instant) -> usize {
        (0..n).filter(|_| rows.lose_at(key, READ_ONLY, at).report).count()
    }

    fn unsaid(writer: AuditLostWriter, channel: RowChannel, action: &'static str, outcome: InsertOutcome, rows: usize) -> Unsaid {
        Unsaid { writer, channel, action, outcome, rows }
    }

    /// A flood is thinned like the daemon's: the first rows each, then the
    /// 2^k-th — and what was left out is counted.
    #[test]
    fn a_flood_of_lost_rows_is_thinned_and_counted() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        let said = lose_n(&rows, received(AuditLostWriter::Bus, Some("matrix")), 100, t0);
        assert_eq!(said, REPORT_EACH_UP_TO + 3, "0..16, then 16, 32 and 64");
        let matrix = RowChannel::Named("matrix".into());
        assert_eq!(
            rows.unsaid(),
            [unsaid(AuditLostWriter::Bus, matrix, "channel.received", InsertOutcome::NotWritten, 100 - said)]
        );
    }

    /// #807's lesson, for the bus: one channel's flood leaves the other
    /// channel's lines, and the other writer's, unthinned.
    #[test]
    fn each_writer_and_channel_thins_on_its_own() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        lose_n(&rows, received(AuditLostWriter::Bus, Some("email")), 100, t0);
        let matrix = rows.lose_at(received(AuditLostWriter::Bus, Some("matrix")), READ_ONLY, t0);
        assert!(matrix.report && matrix.tally.nth == 1, "another channel's first row: {matrix:?}");
        let supervisor_email = received(AuditLostWriter::ChannelSupervisor, Some("email"));
        let supervisor = rows.lose_at(supervisor_email, READ_ONLY, t0);
        assert!(supervisor.report && supervisor.tally.nth == 1, "another writer's first row: {supervisor:?}");
        let ask = rows.lose_at(received(AuditLostWriter::Bus, None), READ_ONLY, t0);
        assert!(ask.report && ask.tally.nth == 1, "a row with no channel counts apart: {ask:?}");
        // The shutdown's count is said as each writer's own, in the order
        // each was first lost.
        let supervisor_said = 1 + lose_n(&rows, supervisor_email, 99, t0);
        let bus_said = REPORT_EACH_UP_TO + 3;
        let email = || RowChannel::Named("email".into());
        let nw = InsertOutcome::NotWritten;
        assert_eq!(
            rows.unsaid(),
            [
                unsaid(AuditLostWriter::Bus, email(), "channel.received", nw, 100 - bus_said),
                unsaid(AuditLostWriter::ChannelSupervisor, email(), "channel.received", nw, 100 - supervisor_said),
            ]
        );
    }

    /// #865, for the bus: one action's flood leaves another action's first
    /// row on the same channel its line.
    #[test]
    fn each_action_thins_on_its_own() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        lose_n(&rows, received(AuditLostWriter::Bus, Some("matrix")), 100, t0);
        let reply = RowKey { action: "channel.reply_undelivered", ..received(AuditLostWriter::Bus, Some("matrix")) };
        let first = rows.lose_at(reply, READ_ONLY, t0);
        assert!(first.report && first.tally.nth == 1, "another action's first row: {first:?}");
    }

    /// #864: the rows that may have been written are counted apart from
    /// those that certainly were not, and said apart at shutdown.
    #[test]
    fn each_outcome_is_counted_on_its_own() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        let refused = received(AuditLostWriter::Bus, Some("matrix"));
        let reset = RowKey { outcome: InsertOutcome::Unknown, ..refused };
        let refused_said = lose_n(&rows, refused, 100, t0);
        let first = rows.lose_at(reset, READ_ONLY, t0);
        assert!(first.report && first.tally.nth == 1, "the other outcome's first row: {first:?}");
        let reset_said = 1 + lose_n(&rows, reset, 39, t0);
        let matrix = || RowChannel::Named("matrix".into());
        assert_eq!(
            rows.unsaid(),
            [
                unsaid(AuditLostWriter::Bus, matrix(), "channel.received", InsertOutcome::NotWritten, 100 - refused_said),
                unsaid(AuditLostWriter::Bus, matrix(), "channel.received", InsertOutcome::Unknown, 40 - reset_said),
            ]
        );
    }

    /// #859, reached through this count: a new cause mid-flood gets its line.
    #[test]
    fn a_new_cause_mid_flood_gets_its_line() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        let key = received(AuditLostWriter::Bus, Some("matrix"));
        lose_n(&rows, key, 100, t0);
        let revoked = rows.lose_at(key, InsertCause::Client("revoked"), t0);
        assert!(revoked.report && revoked.tally.first_of_cause, "{revoked:?}");
        assert!(!rows.lose_at(key, READ_ONLY, t0).report, "the flood's own cause is still thinned");
    }

    /// A quiet gap starts a new burst, whose first line says how the last
    /// one ended — the shared core's behaviour, reached through this count.
    #[test]
    fn a_quiet_gap_starts_a_new_burst_of_lines() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        let key = received(AuditLostWriter::Bus, Some("matrix"));
        lose_n(&rows, key, 20, t0);
        let next = rows.lose_at(key, READ_ONLY, t0 + BURST_QUIET_GAP);
        assert!(next.report, "{next:?}");
        assert_eq!(next.tally.nth, 1);
        assert!(next.tally.ended.is_some(), "it says how the 20-row burst ended: {next:?}");
    }

    /// Past [`MAX_NAMED_CHANNELS`] a writer's further channels share one
    /// count, so the count's memory stays bounded — while a channel it
    /// already knows keeps its own, and another writer is unaffected. The
    /// cap counts channels, not entries: a channel's other actions and
    /// outcomes are not further channels.
    #[test]
    fn channels_past_the_cap_share_one_count() {
        let rows = LostRows::new();
        let t0 = Instant::now();
        let names: Vec<String> = (0..MAX_NAMED_CHANNELS).map(|i| format!("c{i}")).collect();
        for name in &names[..MAX_NAMED_CHANNELS - 1] {
            rows.lose_at(received(AuditLostWriter::Bus, Some(name)), READ_ONLY, t0);
        }
        // c0's other outcome is a further entry, not a further channel: the
        // last channel under the cap is still named.
        let c0_unknown = RowKey { outcome: InsertOutcome::Unknown, ..received(AuditLostWriter::Bus, Some("c0")) };
        rows.lose_at(c0_unknown, READ_ONLY, t0);
        let last = &names[MAX_NAMED_CHANNELS - 1];
        rows.lose_at(received(AuditLostWriter::Bus, Some(last)), READ_ONLY, t0);
        let entries = |rows: &LostRows| rows.entries.lock().unwrap().len();
        assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 1, "POSITIVE CONTROL: each channel named apart");
        assert!(
            rows.entries.lock().unwrap().iter().all(|e| e.channel != RowChannel::Other),
            "no channel past the cap yet"
        );
        rows.lose_at(received(AuditLostWriter::Bus, Some("one-more")), READ_ONLY, t0);
        rows.lose_at(received(AuditLostWriter::Bus, Some("and-another")), READ_ONLY, t0);
        assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 2, "both counted under one Other entry");
        let c0 = rows.lose_at(received(AuditLostWriter::Bus, Some("c0")), READ_ONLY, t0);
        assert_eq!(c0.tally.nth, 2, "c0 keeps its own count");
        rows.lose_at(received(AuditLostWriter::ChannelSupervisor, Some("new")), READ_ONLY, t0);
        assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 3, "another writer names its own");
        let other = rows.entries.lock().unwrap().iter().filter(|e| e.channel == RowChannel::Other).count();
        assert_eq!(other, 1);
    }

    /// The shutdown's line names the action and the channel quoted, says
    /// what is known of the rows (#864), and agrees in number.
    #[test]
    fn the_unsaid_line_names_its_count_and_what_is_known() {
        let nw = InsertOutcome::NotWritten;
        let line = |channel, outcome, rows| {
            format_unsaid(&unsaid(AuditLostWriter::Bus, channel, "channel.received", outcome, rows))
        };
        assert_eq!(
            line(RowChannel::Named("matrix".into()), nw, 83),
            r#"83 channel.received audit rows for channel "matrix" failed to insert (not written) and had no line of their own (thinned past a flood)"#
        );
        assert_eq!(
            line(RowChannel::Unnamed, InsertOutcome::Unknown, 1),
            "1 channel.received audit row naming no channel failed to insert (may not have been written) and had no \
             line of its own (thinned past a flood)"
        );
        assert_eq!(
            line(RowChannel::Other, nw, 2),
            "2 channel.received audit rows for channels past the first 16 failed to insert (not written) and had no \
             line of their own (thinned past a flood)"
        );
        let hostile = line(RowChannel::Named("x\"\n[SKIP]".into()), nw, 2);
        assert!(!hostile.contains('\n'), "a channel name is quoted: {hostile}");
    }
}
