//! A poll result that did not decode (#836).
//!
//! The worker answered the poll, but `parse_poll` could not read the answer.
//! That is a worker bug — or a compromised worker: workers are outside the
//! core's trust boundary — so it is said at ERROR, on the `[worker-refusal]`
//! emitter (which falls back to stderr when `tracing` would not record the
//! line). The decode error is in the line, so the bug can be found, capped at
//! [`DECODE_ERROR_CAP_CHARS`]; it never goes in a row, because a decode error
//! can quote a field of the message, and so a peer's words.
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
//!   it — and nothing after it arrives until it decodes. Not a loss, so no
//!   row. But a batch that never decodes would come back at the very next
//!   poll, which the worker answers at once while the message is pending — a
//!   hot loop of polls and lines.
//!
//! So on **every** channel the poll then waits out the refusal backoff
//! (`spec.refusal_backoff`, [`super::REFUSAL_BACKOFF`] in production) on its
//! own [`UndecodableRun`], and a redelivered batch is said on that run's
//! cadence: the first, then once per [`super::refusal::REFUSAL_REPEAT`] — and
//! at once when the decode error changes, or when the worker was just
//! respawned. The first batch that decodes again ends the run, said once at
//! INFO.
//!
//! On Matrix the hold has a cost: the worker keeps buffering while it lasts,
//! so a batch that then fails too takes more messages with it. It is kept
//! there as a bound on rows — a worker answering every poll at once with
//! garbage would otherwise write one per poll. Decoding event by event, so one
//! bad event does not lose its batch, is #852.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use super::audit::{record_undecodable, DriverAudit, InboundUndecodableAudit};
use super::outage::OutageLog;
use super::refusal::{RefusalRun, REFUSAL_REPEAT};
use super::PolledWorkerSpec;
use crate::channel::actions;
use crate::channel::audit_text::cap_chars;
use crate::channel::undelivered::observed_at_json;
use crate::channel::ChannelId;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

/// How many chars of a decode error its line quotes. serde quotes an
/// offending value whole, so a worker can put a message body of any length in
/// one — and every Matrix loss is said.
pub(super) const DECODE_ERROR_CAP_CHARS: usize = 256;

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

/// Pure: refuse a hook that would never be called — an
/// [`InboundUndecodableAudit`] on a channel with an ack method, whose
/// undecodable batches are redelivered, not lost. Checked once, by
/// [`super::PolledWorkerDriver::spawn`], so the miswiring is said at
/// construction rather than ignored at every batch.
pub(super) fn check_undecodable_audit(spec: &PolledWorkerSpec, audit: &DriverAudit) -> anyhow::Result<()> {
    if let (Some(_), Some(ack)) = (&audit.inbound_undecodable, spec.ack_method) {
        anyhow::bail!(
            "{}: an inbound-undecodable hook on a channel with ack method {ack}: its undecodable \
             batches are redelivered, not lost, so the hook would never be called (#836)",
            spec.label,
        );
    }
    Ok(())
}

/// One run of consecutive poll results that did not decode: when the poll may
/// run again, and when the run was last said. A [`RefusalRun`] underneath —
/// the same backoff and repeat cadence — but its own type, so `run`'s three
/// runs cannot be passed in each other's place.
#[derive(Debug, Default)]
pub(super) struct UndecodableRun {
    run: RefusalRun,
    /// [`OutageLog::ended`] as of this run's last batch. When it has moved,
    /// the worker came back since — whichever call noticed — and its "back
    /// up" line must not be the last word on a batch that still does not
    /// decode: that one is said whatever the cadence, the rule
    /// `refusal::refused` keeps for refusals.
    outages_ended: u64,
}

impl UndecodableRun {
    /// May the poll run at `now`?
    pub(super) fn ready(&self, now: Instant) -> bool {
        self.run.ready(now)
    }
}

/// Pure: a code for a decode error, so the run says a **different** error at
/// once — as a refusal run does when its code changes — rather than at the
/// next repeat. A hash of the text: compared only within one run in one
/// process, so a fixed-key hash is enough, and a collision only delays a line
/// to the repeat window.
pub(super) fn error_code(error: &str) -> i32 {
    let mut h = DefaultHasher::new();
    error.hash(&mut h);
    h.finish() as i32
}

/// What became of an undecodable batch, for its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fate {
    /// The channel acks, so the batch comes back (email). The poll runs again
    /// in `retry_in`; this is the run's `in_a_row`th undecodable batch.
    Redelivered { retry_in: Duration, in_a_row: u32 },
    /// The channel does not redeliver, so the batch is lost (Matrix).
    /// `retry_in` is `None` when the driver is exiting; `recorded` is whether
    /// the channel has a sink to record it.
    Lost { retry_in: Option<Duration>, recorded: bool },
}

/// Pure: the line for one undecodable batch: the decode error (capped at
/// [`DECODE_ERROR_CAP_CHARS`]) and what became of the batch.
pub(super) fn format_undecodable_report(error: &str, fate: Fate) -> String {
    let action = actions::INBOUND_UNDECODABLE;
    let error = cap_chars(error, DECODE_ERROR_CAP_CHARS);
    let why = "The worker sent output the core cannot read: a worker bug, or a compromised worker.";
    match fate {
        Fate::Redelivered { retry_in, in_a_row } => format!(
            "a poll result did not decode ({error}), so its batch was not acked, so the worker \
             redelivers it, and nothing after it arrives until it decodes. {why} Polling again \
             in {} ms with backoff ({in_a_row} in a row so far). Repeated every {} min while it \
             lasts",
            retry_in.as_millis(),
            REFUSAL_REPEAT.as_secs() / 60,
        ),
        Fate::Lost { retry_in, recorded } => {
            let retry = retry_in.map(|d| format!(" Polling again in {} ms;", d.as_millis())).unwrap_or_default();
            let recorded = if recorded {
                format!("recording it as {action}")
            } else {
                format!("NOT recorded as {action}: this channel has no audit sink")
            };
            format!(
                "a poll result did not decode ({error}), so its batch is lost: this channel does \
                 not redeliver, and how many messages it held is unknown. {why}{retry} {recorded}"
            )
        }
    }
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
/// the backoff, and say it — every time for a lost batch, each a different
/// one; on the run's cadence for a redelivered one, the same one again — and
/// record a lost one.
pub(super) fn on_undecodable_batch(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    e: &anyhow::Error,
    run: &mut UndecodableRun,
    outage: &OutageLog,
    audit: Option<&InboundUndecodableAudit>,
) {
    if std::mem::replace(&mut run.outages_ended, outage.ended()) != outage.ended() {
        run.run.rearm_report();
    }
    let error = e.to_string();
    let next = run.run.on_refusal(Instant::now(), &spec.refusal_backoff, error_code(&error));
    if spec.ack_method.is_none() {
        report_lost_undecodable(spec, cid, &error, Some(next.delay), audit);
    } else if next.report {
        let fate = Fate::Redelivered { retry_in: next.delay, in_a_row: run.run.consecutive() };
        emit_worker_refusal_report(spec.label, &format_undecodable_report(&error, fate), RefusalSeverity::Error);
    }
}

/// Say a batch lost because it did not decode, and hand it to the hook. For a
/// channel with no ack method only. `retry_in` is the poll's backoff, or
/// `None` when the driver is exiting (the bus closed while the poll was out).
pub(super) fn report_lost_undecodable(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    error: &str,
    retry_in: Option<Duration>,
    audit: Option<&InboundUndecodableAudit>,
) {
    let fate = Fate::Lost { retry_in, recorded: audit.is_some() };
    emit_worker_refusal_report(spec.label, &format_undecodable_report(error, fate), RefusalSeverity::Error);
    let batch = UndecodableBatch { channel: cid, observed_at: time::OffsetDateTime::now_utc() };
    record_undecodable(audit, spec.label, batch);
}

/// A poll result decoded: a run of undecodable batches, if any, has ended —
/// said once, at INFO.
pub(super) fn decoded(run: &mut UndecodableRun, label: &str) {
    if let Some(report) = format_decodes_again_report(run.run.on_accepted()) {
        emit_worker_refusal_report(label, &report, RefusalSeverity::Recovered);
    }
}
