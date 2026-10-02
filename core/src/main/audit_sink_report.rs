//! What the daemon's shutdown says about audit rows it could not write (#792,
//! #796, #797, #798, #802): the pure line formatters and the functions that
//! report a [`Drained`]. Split out of `audit_sink.rs` to keep it under the
//! 500-LOC soft cap; `#[path]`-included there and re-exported by name, so every
//! path is `audit_sink::…`.

use std::time::Duration;

use kastellan_core::worker_stderr::AuditLostWriter;

use super::{Drained, DRAIN_BOUND};

/// How long an id quoted in an `[audit-lost]` line or a row label may be.
/// Worker-supplied ids are uncapped (email's `skipped` list), and a line is
/// written on the driver's thread (#798).
const QUOTED_ID_CAP_CHARS: usize = 128;

/// Pure: `id`, shortened and **quoted** for a report line or a row label:
/// in double quotes, with any `"`, `\` or control character inside escaped
/// (Rust's `{:?}` of a string).
///
/// Quoted because the ids are worker-supplied and the shutdown line joins row
/// labels with `"; "` (#802): an unquoted id `x; matrix reply to …` would read
/// as two rows. Inside quotes whose own `"` cannot appear unescaped, an id
/// cannot end its entry early.
pub(crate) fn quoted_id(id: &str) -> String {
    format!("{:?}", kastellan_core::channel::audit_text::cap_chars(id, QUOTED_ID_CAP_CHARS))
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
             on is refused and reported",
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
             anything to audit, and an audit row one writes from now on is refused and reported",
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

/// Pure: the `[audit-lost]` line for refused rows that got no report of their
/// own (`audit_sink_thinning.rs`), or `None` when there were none (#798).
pub(crate) fn format_unreported_at_shutdown(d: &Drained) -> Option<String> {
    (d.unreported > 0).then(|| {
        format!(
            "{} refused (shed, or tried after shutdown began) without a line of its own, \
             to keep a flood from stalling its driver",
            counted(d.unreported, "channel audit row", "was", "were"),
        )
    })
}

/// Pure: the last `[audit-lost]` line, for `n` refused rows thinned out after
/// the shutdown lines were written, or `None` when there were none (#802).
pub(crate) fn format_unreported_since(n: usize) -> Option<String> {
    (n > 0).then(|| {
        format!(
            "{} refused after the shutdown report without a line of its own (a driver still \
             running past the drain)",
            counted(n, "more channel audit row", "was", "were"),
        )
    })
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
/// exit and thinned reports on the `[audit-lost]` marker through `report`;
/// other drivers still running, and bring-ups that never finished, at INFO.
pub(crate) fn report_drained(d: &Drained, report: Reporter) {
    let lost = [
        format_pending_at_shutdown(d, DRAIN_BOUND),
        format_stuck_replies_at_shutdown(d, DRAIN_BOUND),
        format_unreported_at_shutdown(d),
    ];
    for line in lost.into_iter().flatten() {
        report(AuditLostWriter::Shutdown, &line);
    }
    let info = [format_live_at_shutdown(d, DRAIN_BOUND), format_starting_at_shutdown(d)];
    for line in info.into_iter().flatten() {
        tracing::info!("{line}");
    }
}

/// Say, as the daemon's very last word, how many refused rows were thinned
/// out after [`report_drained`] ran: `unreported` is
/// [`super::unreported_since`]. Rows refused after this are the one gap left
/// (the module doc of `audit_sink.rs`).
pub(crate) fn report_unreported_since(unreported: usize, report: Reporter) {
    if let Some(line) = format_unreported_since(unreported) {
        report(AuditLostWriter::Shutdown, &line);
    }
}

#[cfg(test)]
#[path = "audit_sink_report_tests.rs"]
mod tests;
