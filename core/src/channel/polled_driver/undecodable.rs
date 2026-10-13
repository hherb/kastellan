//! A poll result that did not decode (#836).
//!
//! The worker answered the poll, but `parse_poll` could not read the answer.
//! That is a worker bug — or a compromised worker: workers are outside the
//! core's trust boundary — so it is said at ERROR, on the `[worker-refusal]`
//! emitter (which falls back to stderr when `tracing` would not record the
//! line). The decode error is in the line, so the bug can be found; it
//! never goes in a row, because a decode error can quote a field of the
//! message, and so a peer's words.
//!
//! What happened to the batch depends on the channel:
//!
//! - **Without an ack method** (Matrix): the worker's cursor has already
//!   moved past it, so it is **lost**, and how many messages it held is
//!   unknown. Said every time, and handed to the channel's
//!   [`InboundUndecodableAudit`] hook for a `channel.inbound_undecodable` row:
//!   channel and when, **no count** — a separate action rather than a
//!   `channel.inbound_dropped` row, so `sum(dropped)` over those stays a true
//!   count and the type of its `dropped` stays non-zero.
//! - **With one** (email): the batch is not acked, so the worker redelivers
//!   it. Not a loss, so no row. But a batch that never decodes would come
//!   back at the very next poll, which the worker answers at once while the
//!   message is pending — a hot loop of polls and lines.
//!
//! So on **every** channel the poll then waits out the refusal backoff
//! (`spec.refusal_backoff`, its own [`RefusalRun`]: 1 s doubling to 60 s in
//! production), and a redelivered batch is said on that run's cadence — the
//! first, then once per [`super::refusal::REFUSAL_REPEAT`]. The first batch that
//! decodes again ends the run, said once at INFO.

use std::time::{Duration, Instant};

use super::audit::{record_undecodable, InboundUndecodableAudit};
use super::refusal::{Refused, RefusalRun};
use super::PolledWorkerSpec;
use crate::channel::actions;
use crate::channel::undelivered::observed_at_json;
use crate::channel::ChannelId;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

/// What the [`InboundUndecodableAudit`] hook is told about a lost batch that
/// did not decode: whose channel, and when. No count — it is unknown — and
/// nothing the worker or a peer wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UndecodableBatch<'a> {
    pub channel: &'a ChannelId,
    /// When the batch was lost. The row is written after the fact (#789), so
    /// `audit_log.ts` is the insert's time, not this.
    pub observed_at: time::OffsetDateTime,
}

impl UndecodableBatch<'_> {
    /// Pure: the payload of an [`actions::INBOUND_UNDECODABLE`] row — the
    /// channel and when, nothing else.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "channel": self.channel.0,
            "observed_at": observed_at_json(self.observed_at),
        })
    }
}

/// Pure: the line for one undecodable batch. `redelivers` is whether the
/// channel acks (then the batch comes back); `error` the decode error;
/// `retry_in` when the poll runs again (`None` when the driver is exiting);
/// `sink` whether a lost batch is recorded as a row.
pub(super) fn format_undecodable_report(
    redelivers: bool,
    error: &str,
    retry_in: Option<Duration>,
    sink: bool,
) -> String {
    let action = actions::INBOUND_UNDECODABLE;
    let fate = if redelivers {
        "its batch was not acked, so the worker redelivers it"
    } else {
        "its batch is lost: this channel does not redeliver, and how many messages it held is \
         unknown"
    };
    let why = "The worker sent output the core cannot read: a worker bug, or a compromised worker.";
    let retry = retry_in.map(|d| d.as_millis());
    let tail = match (redelivers, retry) {
        (true, Some(ms)) => {
            // "15 min" is `REFUSAL_REPEAT`, pinned by a test in `tests/refusal.rs`.
            format!(" Polling again in {ms} ms with backoff. Repeated every 15 min while it lasts")
        }
        // The driver is exiting: an ack channel never decodes then, so this
        // arm is for completeness.
        (true, None) => String::new(),
        (false, retry) => {
            let retry = retry.map(|ms| format!(" Polling again in {ms} ms;")).unwrap_or_default();
            let recorded = if sink {
                format!("recording it as {action}")
            } else {
                format!("NOT recorded as {action}: this channel has no audit sink")
            };
            format!("{retry} {recorded}")
        }
    };
    format!("a poll result did not decode ({error}), so {fate}. {why}{tail}")
}

/// Pure: the line that ends a run of `n` undecodable batches, or `None` when
/// no run ended (`n == 0`).
pub(super) fn format_decodes_again_report(n: u32) -> Option<String> {
    (n > 0).then(|| {
        format!(
            "poll results decode again after {n} undecodable batch{}; that run has ended",
            if n == 1 { "" } else { "es" }
        )
    })
}

/// A poll result did not decode while the bus is up: hold the next poll for
/// the backoff, and say it — and, on a channel that does not redeliver,
/// record it.
pub(super) fn on_undecodable_batch(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    e: &anyhow::Error,
    run: &mut RefusalRun,
    audit: Option<&InboundUndecodableAudit>,
) {
    // Code 0: not an RPC refusal, and the same every time, so the run's
    // cadence is the repeat window alone.
    let next = run.on_refusal(Instant::now(), &spec.refusal_backoff, 0);
    report_undecodable(spec, cid, e, Some(&next), audit);
}

/// Say an undecodable batch and, on a channel that does not redeliver, hand
/// it to the hook. `next` is the poll's backoff, or `None` when the driver is
/// exiting (the bus closed while the poll was out).
pub(super) fn report_undecodable(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    e: &anyhow::Error,
    next: Option<&Refused>,
    audit: Option<&InboundUndecodableAudit>,
) {
    let redelivers = spec.ack_method.is_some();
    // A loss is said every time — each is a different batch. A redelivered
    // batch is the same one again, so it follows the run's cadence.
    if !redelivers || next.map_or(true, |r| r.report) {
        let report = format_undecodable_report(
            redelivers,
            &e.to_string(),
            next.map(|r| r.delay),
            audit.is_some(),
        );
        emit_worker_refusal_report(spec.label, &report, RefusalSeverity::Error);
    }
    if !redelivers {
        let batch = UndecodableBatch { channel: cid, observed_at: time::OffsetDateTime::now_utc() };
        record_undecodable(audit, spec.label, batch);
    }
}

/// A poll result decoded: a run of undecodable batches, if any, has ended —
/// said once, at INFO.
pub(super) fn decoded(run: &mut RefusalRun, label: &str) {
    if let Some(report) = format_decodes_again_report(run.on_accepted()) {
        emit_worker_refusal_report(label, &report, RefusalSeverity::Recovered);
    }
}
