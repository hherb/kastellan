//! What the daemon says about audit rows it could not write (#792, #796, #797,
//! #798, #802): why a row was lost, how a row is named, the pure line
//! formatters, and the functions that report a [`Drained`]. Split out of
//! `audit_sink.rs` to keep it under the 500-LOC soft cap; `#[path]`-included
//! there and re-exported by name, so every path is `audit_sink::…`.

use std::future::Future;
use std::time::Duration;

use kastellan_core::worker_stderr::AuditLostWriter;

use super::{
    unreported_on_since, BurstTally, Drained, Ledger, SinkChannel, UnsaidCounts, Unreported,
    DRAIN_BOUND, LEDGER,
};

/// Why a row was lost — never tried, or tried and failed — as `on_failure`
/// is told.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Unwritten<'a> {
    /// The insert ran and failed: the error — which says whether the row may
    /// have been committed anyway (#858) — and the row's place in its
    /// channel's burst of failed inserts, thinned like the others (#861).
    Insert { failed: &'a kastellan_db::audit::InsertError, tally: BurstTally },
    /// Never tried: the queue was full. With where the row stands in its
    /// channel's burst of shed rows (#807), so a thinned flood's lines still
    /// say how big it is.
    Shed(BurstTally),
    /// Never tried: the daemon had already drained its audit writes and was
    /// shutting down (#792). With its place in the channel's burst of late
    /// rows (#807).
    AfterShutdown(BurstTally),
}

impl std::fmt::Display for Unwritten<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Insert { failed, tally } => write!(f, "{failed}; {tally}"),
            Self::Shed(tally) => write!(
                f,
                "shed: too many audit inserts already waiting for the pool (a flood of \
                 audited events, or a wedged Postgres); {tally}"
            ),
            Self::AfterShutdown(tally) => write!(
                f,
                "not tried: the daemon was shutting down and had already drained its audit \
                 writes; {tally}"
            ),
        }
    }
}

impl Unwritten<'_> {
    /// What a sink's line says of the row: `not written`, unless its insert
    /// failed in a way that may have committed it (#858) — the same words
    /// the channel bus uses ([`kastellan_core::channel::audit_text::insert_verdict`]).
    /// A shed or late row was never tried, so it is not written.
    pub(crate) fn verdict(&self) -> &'static str {
        use kastellan_core::channel::audit_text::insert_verdict;
        use kastellan_db::audit::InsertOutcome;
        match self {
            Self::Insert { failed, .. } => insert_verdict(failed.outcome()),
            Self::Shed(_) | Self::AfterShutdown(_) => insert_verdict(InsertOutcome::NotWritten),
        }
    }

    /// What a sink's line says after naming the row: its [`Self::verdict`],
    /// then why — `not written: shed: …` or `may not have been written: …`.
    /// One call, so no sink's formatter can leave the verdict out.
    pub(crate) fn said(&self) -> String {
        format!("{}: {self}", self.verdict())
    }
}

/// Test builds only: a failed insert whose error reads `postgres query
/// failed: {msg}`, first of its burst — for a sink's line tests. Leaked, so
/// the `Unwritten` can borrow it for `'static`.
#[cfg(test)]
impl Unwritten<'static> {
    pub(crate) fn failed_insert(outcome: kastellan_db::audit::InsertOutcome, msg: &str) -> Self {
        let cause = kastellan_db::audit::InsertCause::Client("test");
        let failed = kastellan_db::audit::InsertError::assumed_for_test(outcome, cause, kastellan_db::DbError::Query(msg.into()));
        let tally = BurstTally { nth: 1, first_of_cause: false, unsaid_before: 0, ended: None };
        Self::Insert { failed: Box::leak(Box::new(failed)), tally }
    }
}

/// `id`, shortened and quoted for a report line or a row label — the lib's,
/// since #808, so the bus's own lost-row line quotes ids the same way.
pub(crate) use kastellan_core::channel::audit_text::quoted_id;

/// A pending row's name for the shutdown line (#797): what the row is, and
/// the worker-supplied id it is about, **always** [`quoted_id`]'d — so no sink
/// can hand the line an id that ends its entry early (#802). The other parts
/// are `'static`: the sink's own words, never the worker's.
pub(crate) struct RowLabel(String);

impl RowLabel {
    /// `what "id"`, then ` (detail)` if there is one: e.g. `matrix reply to
    /// conversation "!r:srv" (gave_up)`.
    pub(crate) fn new(what: &'static str, id: &str, detail: Option<&'static str>) -> Self {
        let detail = detail.map(|d| format!(" ({d})")).unwrap_or_default();
        Self(format!("{what} {}{detail}", quoted_id(id)))
    }

    /// `what (n)`, for a row about a number of things rather than one id:
    /// e.g. `matrix inbound messages dropped (3)` (#826). Nothing in it is
    /// worker-supplied, so there is nothing to quote.
    pub(crate) fn with_count(what: &'static str, n: usize) -> Self {
        Self(format!("{what} ({n})"))
    }

    /// `what` alone, for a row about one thing that has neither an id nor a
    /// count: e.g. `matrix inbound batch lost undecodable` (#836).
    pub(crate) fn named(what: &'static str) -> Self {
        Self(what.to_string())
    }

    pub(super) fn into_string(self) -> String {
        self.0
    }
}

/// Test builds only: a label taken as is, for rows whose name is the test's.
#[cfg(test)]
impl From<&str> for RowLabel {
    fn from(label: &str) -> Self {
        Self(label.to_string())
    }
}

/// Pure: "1 row was" / "2 rows were" — `n` of `noun`, with its verb.
pub(crate) fn counted(n: usize, noun: &str, one: &str, many: &str) -> String {
    format!("{n} {noun}{} {}", if n == 1 { "" } else { "s" }, if n == 1 { one } else { many })
}

/// Pure: the `[audit-lost]` line for rows still pending when [`super::drain`] gave
/// up, or `None` when there were none. Names the first few (#797), so the line
/// can be matched to the driver's own line for the event.
pub(crate) fn format_pending_at_shutdown(d: &Drained, bound: Duration) -> Option<String> {
    let pending = d.in_flight.rows_pending;
    (pending > 0).then(|| {
        let who = if d.named.is_empty() {
            String::new()
        } else {
            let more = pending.saturating_sub(d.named.len());
            format!(
                " ({}{})",
                d.named.join("; "),
                if more > 0 { format!("; and {more} more") } else { String::new() }
            )
        };
        format!(
            "{} still unwritten after waiting {} s at shutdown{who}; the database pool \
             closes next, so {} lost unless already mid-write",
            counted(pending, "channel audit row", "was", "were"),
            bound.as_secs(),
            if pending == 1 { "it is" } else { "they are" },
        )
    })
}

/// Pure: the `[audit-lost]` line for drivers that audit on exit (Matrix's)
/// still running when [`super::drain`] gave up, or `None` when there were none
/// (#796). A stuck one never audits the replies still queued behind it, so
/// this is a possible loss. A bring-up abandoned mid-login is not counted here
/// (#802): its lease never left *starting* ([`format_starting_at_shutdown`]).
pub(crate) fn format_stuck_replies_at_shutdown(d: &Drained, bound: Duration) -> Option<String> {
    let stuck = d.in_flight.auditing_live;
    (stuck > 0).then(|| {
        format!(
            "{} not exited after {} s at shutdown; the replies still queued behind it may not \
             be audited as `channel.reply_undelivered`, and an audit row it writes from now \
             on is refused, and reported unless thinned out",
            counted(stuck, "channel driver", "had", "had"),
            bound.as_secs(),
        )
    })
}

/// Pure: the INFO line for leases whose driver never finished starting (a
/// Matrix login abandoned at its timeout, its blocking task still running),
/// or `None` when there were none (#802). Not a loss: no such driver ever
/// queued a row, and [`super::drain`] did not wait for them.
pub(crate) fn format_starting_at_shutdown(d: &Drained) -> Option<String> {
    let starting = d.in_flight.starting;
    (starting > 0).then(|| {
        format!(
            "{} not finished starting at shutdown (an abandoned bring-up); none had queued \
             anything to audit, and an audit row one writes from now on is refused, and \
             reported unless thinned out",
            counted(starting, "channel driver", "had", "had"),
        )
    })
}

/// Pure: the INFO line for the other drivers still running when [`super::drain`] gave
/// up, or `None` when there were none. Not a loss by itself — see
/// [`DRAIN_BOUND`] — and a row one of them does try is reported as it happens.
pub(crate) fn format_live_at_shutdown(d: &Drained, bound: Duration) -> Option<String> {
    let others = d.in_flight.sinks_live.saturating_sub(d.in_flight.auditing_live);
    (others > 0).then(|| {
        format!(
            "{} not exited after {} s at shutdown; an audit row one writes from now on is \
             refused, and reported on the [audit-lost] marker unless thinned out",
            counted(others, "channel driver", "had", "had"),
            bound.as_secs(),
        )
    })
}

/// Pure: the `[audit-lost]` line for one channel's lost rows that got no
/// report of their own (`audit_sink_thinning.rs`), or `None` when there were
/// none (#798). Said at shutdown, but not all lost then: shed, late and
/// failed are said apart (#807, #861), because rows shed in last week's
/// flood are not this shutdown's — and failed inserts by outcome (#864): the
/// rows known not to be in `audit_log` apart from those that may be.
pub(crate) fn format_unreported_at_shutdown(counts: UnsaidCounts) -> Option<String> {
    let total = counts.total();
    (total > 0).then(|| {
        format!(
            "{} no line of {} (thinned past a flood): {}",
            counted(total, "channel audit row", "had", "had"),
            if total == 1 { "its own" } else { "their own" },
            by_kind(counts, " over the daemon's life"),
        )
    })
}

/// Pure: a last `[audit-lost]` line, for one channel's lost rows thinned out
/// since the drain's snapshot (or since the line before it), kind by kind as
/// [`format_unreported_at_shutdown`] says them, or `None` when there were
/// none (#802). Said under that channel's writer ([`report_unreported_since`]).
pub(crate) fn format_unreported_since(counts: UnsaidCounts) -> Option<String> {
    let n = counts.total();
    (n > 0).then(|| {
        format!(
            "{} no line of {} after the shutdown report (a driver still running past the \
             drain, or an insert still pending at it): {}",
            counted(n, "more channel audit row", "had", "had"),
            if n == 1 { "its own" } else { "their own" },
            by_kind(counts, ""),
        )
    })
}

/// Pure: `counts` said kind by kind, the kinds with none left out; `span`
/// follows the shed and failed counts (e.g. " over the daemon's life").
fn by_kind(counts: UnsaidCounts, span: &str) -> String {
    let mut kinds = Vec::new();
    if counts.shed > 0 {
        kinds.push(format!(
            "{} shed{span} (too many inserts already waiting for the pool)",
            counts.shed
        ));
    }
    if counts.late > 0 {
        kinds.push(format!("{} tried after shutdown began", counts.late));
    }
    if counts.failed_not_written > 0 {
        kinds.push(format!(
            "{} failed to insert{span}, not written (refused by Postgres, or never sent)",
            counts.failed_not_written
        ));
    }
    if counts.failed_unknown > 0 {
        kinds.push(format!(
            "{} failed to insert{span}, may have been written (the connection was lost mid-insert: \
             look for them in audit_log)",
            counts.failed_unknown
        ));
    }
    kinds.join("; ")
}

/// How an `[audit-lost]` report is delivered: the emitter in production, a
/// recorder in a test (#799), because the emitter's own record is the lib's
/// and a bin test cannot read it. A `fn` pointer, so a sink's closure stays
/// `Send + 'static` with nothing captured for it.
pub(crate) type Reporter = fn(AuditLostWriter, &str);

/// The daemon's [`Reporter`]: [`emit_audit_lost_report`]. Its "was the stderr
/// line written" answer is dropped, because there is nobody left to tell:
/// tracing is the primary channel and the stderr write the fallback.
///
/// [`emit_audit_lost_report`]: kastellan_core::worker_stderr::emit_audit_lost_report
pub(crate) fn emit_report(writer: AuditLostWriter, line: &str) {
    kastellan_core::worker_stderr::emit_audit_lost_report(writer, line);
}

/// Say what [`super::drain`] left behind: lost rows, stuck drivers that audit on
/// exit and thinned reports (under each channel's own writer) on the
/// `[audit-lost]` marker through `report`;
/// other drivers still running, and bring-ups that never finished, at INFO.
pub(crate) fn report_drained(d: &Drained, report: Reporter) {
    let lost = [
        format_pending_at_shutdown(d, DRAIN_BOUND),
        format_stuck_replies_at_shutdown(d, DRAIN_BOUND),
    ];
    for line in lost.into_iter().flatten() {
        report(AuditLostWriter::Shutdown, &line);
    }
    // Under each channel's own writer (#807): whose rows they were.
    for channel in SinkChannel::ALL {
        if let Some(line) = format_unreported_at_shutdown(d.unreported.of(channel)) {
            report(channel.writer(), &line);
        }
    }
    let info = [format_live_at_shutdown(d, DRAIN_BOUND), format_starting_at_shutdown(d)];
    for line in info.into_iter().flatten() {
        tracing::info!("{line}");
    }
}

/// Say `unreported` thinned-out rows on the `[audit-lost]` marker, if any:
/// one line per channel, under its own writer (#807), as [`report_drained`]
/// says the drain's.
fn report_unreported_since(unreported: Unreported, report: Reporter) {
    for channel in SinkChannel::ALL {
        if let Some(line) = format_unreported_since(unreported.of(channel)) {
            report(channel.writer(), &line);
        }
    }
}

/// The end of `main`'s shutdown, after [`report_drained`]: the daemon's last
/// word on audit rows (#802). Say how many lost rows were thinned out since
/// `drained` was taken, run `close` (the audit mirror, then the pool), and say
/// how many more were thinned out while it ran. Takes `drained` by value, so
/// its baseline is used once.
///
/// Said BEFORE `close` too, because `close` has no bound: sqlx's
/// `Pool::close` waits for every checked-out connection, and an insert stuck
/// in a wedged Postgres (there is no statement timeout) holds one until the
/// service manager's SIGKILL. A line only after it would never be written —
/// on the very shutdown most likely to have refused rows.
pub(crate) async fn close_then_report_unreported(
    drained: Drained,
    close: impl Future<Output = ()>,
    report: Reporter,
) {
    close_on_then_report(&LEDGER, &drained, close, report).await;
}

/// [`close_then_report_unreported`] on `ledger`.
pub(super) async fn close_on_then_report(
    ledger: &'static Ledger,
    drained: &Drained,
    close: impl Future<Output = ()>,
    report: Reporter,
) {
    let before = unreported_on_since(ledger, drained);
    report_unreported_since(before, report);
    close.await;
    report_unreported_since(unreported_on_since(ledger, drained).since(before), report);
}

#[cfg(test)]
#[path = "audit_sink_report_tests.rs"]
mod tests;
