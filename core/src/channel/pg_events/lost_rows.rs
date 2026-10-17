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
//! [`crate::audit_thinning`]). A row is counted under its own `channel`
//! field when it has one — the row says best whose it is. Only a row with no
//! `channel` (`ask.resolved`, `channel.reply_unroutable`) falls back to the
//! channel its writer writes for ([`RowChannel::Unnamed`], #860): before,
//! every bus's such rows shared one count, so one bus's flood thinned the
//! other's lines.
//!
//! **One process-wide count** ([`LOST_ROWS`]): a channel's bus is rebuilt on
//! every bring-up, so a count held by the bus would start afresh after each
//! restart — and the rows a restart's count had left unsaid would be said by
//! no one. The shutdown says what is left ([`super::report_unsaid_lost_rows`])
//! — before the channels stop, and the rows thinned during the stop after it
//! ([`unsaid_since`], #862); a crash loses it, as it loses the daemon's
//! sinks' (#817).
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
/// the count's memory if that ever stops being true. (Actions get no such
/// bound: [`RowKey::action`] is `&'static str`, which no peer- or
/// runtime-spelled string reaches short of a deliberate leak — in practice
/// the core's constants, `channel::actions` and `ask.resolved`, a few dozen
/// at most.)
const MAX_NAMED_CHANNELS: usize = 16;

/// Which channel a lost row is counted under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowChannel {
    /// The row's own `channel` field.
    Named(String),
    /// The row has no `channel` field (`ask.resolved`,
    /// `channel.reply_unroutable`): the channel its writer writes for — its
    /// bus's (#860). Apart from [`Self::Named`], so the shutdown line can say
    /// the rows themselves name none (a query by `payload->>'channel'` would
    /// not find them).
    Unnamed(String),
    /// A channel past [`MAX_NAMED_CHANNELS`] for this writer — a row's own
    /// name or, for a row naming none, its writer's. The latter's line then
    /// no longer says the rows name no channel; the cap is never reached in
    /// practice (see [`MAX_NAMED_CHANNELS`]).
    Other,
}

/// What a lost row is counted by: who wrote it, the row's own `channel`
/// field (if it has one) — else the channel its writer writes for (#860) —
/// its action, and what its failed insert says about it (#858).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowKey<'a> {
    pub(crate) writer: AuditLostWriter,
    /// The channel `writer` writes for: a bus's own, a boot supervisor's.
    pub(crate) writer_channel: &'a str,
    pub(crate) channel: Option<&'a str>,
    pub(crate) action: &'static str,
    pub(crate) outcome: InsertOutcome,
}

/// One count, resolved: a [`RowKey`] with its channel looked up
/// ([`row_channel`]). One struct, so the entry, its comparison and what
/// [`LostRows::unsaid`] hands back cannot fall out of step when a part is
/// added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CountKey {
    pub(crate) writer: AuditLostWriter,
    pub(crate) channel: RowChannel,
    pub(crate) action: &'static str,
    pub(crate) outcome: InsertOutcome,
}

/// The rows one writer could not write on one channel, of one action and
/// outcome.
#[derive(Debug)]
struct Entry {
    key: CountKey,
    thinning: Thinning<InsertCause>,
}

/// Every count's lost rows, each thinned on its own.
#[derive(Debug)]
pub(crate) struct LostRows {
    /// A `Vec`, not a map: it keeps the order each entry was first lost in,
    /// which [`Self::unsaid`] reports by, and it stays short (per writer,
    /// [`MAX_NAMED_CHANNELS`] names — each possibly both a `Named` and an
    /// `Unnamed` channel — plus `Other`, each with the few actions the core
    /// writes, by two outcomes).
    entries: Mutex<Vec<Entry>>,
}

/// The production count, shared by every bus and boot supervisor.
pub(crate) static LOST_ROWS: LostRows = LostRows::new();

/// One count's rows that got no line of their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unsaid {
    pub(crate) key: CountKey,
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
            .map(|e| Unsaid { key: e.key.clone(), rows: e.thinning.unreported() })
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

/// Pure: which channel `key`'s row counts under, given the entries so far —
/// its own `channel` field, or else its writer's channel (#860), until the
/// writer already has [`MAX_NAMED_CHANNELS`] distinct names. A row's own
/// name and a writer's count alike: the cap bounds the names, whoever spelled
/// them.
fn row_channel(entries: &[Entry], key: RowKey<'_>) -> RowChannel {
    let (name, channel): (&str, fn(String) -> RowChannel) = match key.channel {
        Some(own) => (own, RowChannel::Named),
        None => (key.writer_channel, RowChannel::Unnamed),
    };
    let mut named: Vec<&str> = entries
        .iter()
        .filter(|e| e.key.writer == key.writer)
        .filter_map(|e| match &e.key.channel {
            RowChannel::Named(n) | RowChannel::Unnamed(n) => Some(n.as_str()),
            RowChannel::Other => None,
        })
        .collect();
    named.sort_unstable();
    named.dedup();
    if named.contains(&name) || named.len() < MAX_NAMED_CHANNELS {
        channel(name.to_string())
    } else {
        RowChannel::Other
    }
}

/// The entry a row counted by `key` goes in, made on first use.
fn entry<'a>(entries: &'a mut Vec<Entry>, key: RowKey<'_>) -> &'a mut Entry {
    let key = CountKey {
        writer: key.writer,
        channel: row_channel(entries, key),
        action: key.action,
        outcome: key.outcome,
    };
    let at = match entries.iter().position(|e| e.key == key) {
        Some(at) => at,
        None => {
            entries.push(Entry { key, thinning: Thinning::new() });
            entries.len() - 1
        }
    };
    &mut entries[at]
}

/// Pure: each of `now`'s counts less its rows in `base` (none, if `base` has
/// no entry for its key); the counts with none left out. Each count only
/// grows and an entry, once made, stays, so nothing in `base` is missing
/// from `now` — the shutdown's second line says only the rows thinned since
/// its first (#862).
pub(crate) fn unsaid_since(now: Vec<Unsaid>, base: &[Unsaid]) -> Vec<Unsaid> {
    now.into_iter()
        .map(|u| {
            let before = base.iter().find(|b| b.key == u.key).map_or(0, |b| b.rows);
            Unsaid { rows: u.rows.saturating_sub(before), ..u }
        })
        .filter(|u| u.rows > 0)
        .collect()
}

/// Which of the shutdown's two lines for a count [`format_unsaid`] words
/// (#862).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnsaidPart {
    /// Said before the channels stop: every row thinned so far.
    BeforeStop,
    /// Said after they stopped: the rows thinned since the first line.
    DuringStop,
}

/// Pure: the shutdown's line for one count's rows that got no line of their
/// own — said, so a flood's tail is not lost with the process (the per-line
/// tally says the rest): once before the channels stop and, for the rows
/// thinned during the stop, once after (`part`, #862). It says whether they
/// are known not to be in `audit_log` (#864): since #858 an insert that
/// failed in a way that does not prove its row absent — a lost connection,
/// a server crash — is "may not have been written", and those are counted
/// apart.
pub(crate) fn format_unsaid(unsaid: &Unsaid, part: UnsaidPart) -> String {
    let key = &unsaid.key;
    let whose = match &key.channel {
        RowChannel::Named(name) => format!(" for channel {}", super::quoted_id(name)),
        RowChannel::Unnamed(name) => format!(" naming no channel (written for channel {})", super::quoted_id(name)),
        RowChannel::Other => format!(" for channels past the first {MAX_NAMED_CHANNELS}"),
    };
    let rows = unsaid.rows;
    let (row, its) = if rows == 1 { ("row", "its own") } else { ("rows", "their own") };
    let (more, when) = match part {
        UnsaidPart::BeforeStop => ("", ""),
        UnsaidPart::DuringStop => (" more", ", while the channels stopped"),
    };
    format!(
        "{rows}{more} {} audit {row}{whose} failed to insert ({}) and had no line of {its} (thinned past a \
         flood{when})",
        key.action,
        super::insert_verdict(key.outcome),
    )
}

#[cfg(test)]
mod tests;
