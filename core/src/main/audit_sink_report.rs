//! What the daemon's shutdown says about audit rows it could not write (#792,
//! #796, #797, #798): the pure line formatters and the one function that
//! reports a [`Drained`]. Split out of `audit_sink.rs` to keep it under the
//! 500-LOC soft cap; `#[path]`-included there and re-exported, so every path is
//! `audit_sink::…` as before.

use std::time::Duration;

use kastellan_core::worker_stderr::AuditLostWriter;

use super::{Drained, DRAIN_BOUND};

/// How long an id quoted in an `[audit-lost]` line or a row label may be.
/// Worker-supplied ids are uncapped (email's `skipped` list), and a line is
/// written on the driver's thread (#798).
const QUOTED_ID_CAP_CHARS: usize = 128;

/// Pure: `id`, shortened for a report line or a row label.
pub(crate) fn quoted_id(id: &str) -> String {
    kastellan_core::channel::audit_text::cap_chars(id, QUOTED_ID_CAP_CHARS)
}

/// Pure: "1 row was" / "2 rows were" — `n` of `noun`, with its verb.
pub(crate) fn counted(n: usize, noun: &str, one: &str, many: &str) -> String {
    format!("{n} {noun}{} {}", if n == 1 { "" } else { "s" }, if n == 1 { one } else { many })
}

/// Pure: the `[audit-lost]` line for rows still pending when [`super::drain`] gave
/// up, or `None` when there were none. Names the first few (#797), so the line
/// can be matched to the driver's own line for the event.
pub(crate) fn format_pending_at_shutdown(d: &Drained, bound: Duration) -> Option<String> {
    (d.rows_pending > 0).then(|| {
        let who = if d.named.is_empty() {
            String::new()
        } else {
            let more = d.rows_pending.saturating_sub(d.named.len());
            format!(
                " ({}{})",
                d.named.join("; "),
                if more > 0 { format!("; and {more} more") } else { String::new() }
            )
        };
        format!(
            "{} still unwritten after waiting {} s at shutdown{who}; the database pool \
             closes next, so {} lost unless already mid-write",
            counted(d.rows_pending, "channel audit row", "was", "were"),
            bound.as_secs(),
            if d.rows_pending == 1 { "it is" } else { "they are" },
        )
    })
}

/// Pure: the `[audit-lost]` line for Matrix drivers still running when
/// [`super::drain`] gave up, or `None` when there were none (#796). A stuck one never
/// audits the replies still queued behind it, so this one is a possible loss
/// (it is also what a Matrix bring-up abandoned mid-login looks like, which
/// queued none).
pub(crate) fn format_stuck_replies_at_shutdown(d: &Drained, bound: Duration) -> Option<String> {
    (d.replies_live > 0).then(|| {
        format!(
            "{} not exited (or finished starting) after {} s at shutdown; the replies still queued behind a stuck \
             one may not be audited as `channel.reply_undelivered` (a driver whose bring-up \
             was abandoned never queued any), and an audit row one writes from now on is \
             refused and reported",
            counted(d.replies_live, "channel driver", "had", "had"),
            bound.as_secs(),
        )
    })
}

/// Pure: the INFO line for the other drivers still running when [`super::drain`] gave
/// up, or `None` when there were none. Not a loss by itself — see
/// [`DRAIN_BOUND`] — and a row one of them does try is reported as it happens.
pub(crate) fn format_live_at_shutdown(d: &Drained, bound: Duration) -> Option<String> {
    let others = d.sinks_live.saturating_sub(d.replies_live);
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
/// own ([`super::should_report`]), or `None` when there were none (#798).
pub(crate) fn format_unreported_at_shutdown(d: &Drained) -> Option<String> {
    (d.unreported > 0).then(|| {
        format!(
            "{} refused (shed, or tried after shutdown began) without a line of its own, \
             to keep a flood from stalling its driver",
            counted(d.unreported, "channel audit row", "was", "were"),
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

/// Say what [`super::drain`] left behind: lost rows, stuck Matrix drivers and thinned
/// reports on the `[audit-lost]` marker through `report`; other drivers still
/// running at INFO.
pub(crate) fn report_drained(d: &Drained, report: Reporter) {
    let lost = [
        format_pending_at_shutdown(d, DRAIN_BOUND),
        format_stuck_replies_at_shutdown(d, DRAIN_BOUND),
        format_unreported_at_shutdown(d),
    ];
    for line in lost.into_iter().flatten() {
        report(AuditLostWriter::Shutdown, &line);
    }
    if let Some(line) = format_live_at_shutdown(d, DRAIN_BOUND) {
        tracing::info!("{line}");
    }
}
