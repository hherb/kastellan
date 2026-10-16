//! Worker output about inbound messages that did not decode (#836, #852,
//! #853).
//!
//! The worker answered, but the core could not read the answer. That is a
//! worker bug — or a compromised worker: workers are outside the core's trust
//! boundary — so it is said at ERROR, on the `[worker-refusal]` emitter (which
//! falls back to stderr when `tracing` would not record the line). The decode
//! error is in the line, so the bug can be found, capped at
//! [`DECODE_ERROR_CAP_CHARS`]; it never goes in a row, because a decode error
//! can quote a field of the message, and so a peer's words.
//!
//! Three answers can fail to decode, each a [`UndecodablePart`] on the row:
//!
//! - **a whole poll result** ([`UndecodablePart::Batch`]): `parse_poll`
//!   returned an `Err`;
//! - **some events of one** ([`UndecodablePart::Events`], #852): a decoder
//!   that reads events one at a time (Matrix's) returned the rest, and how
//!   many it could not read;
//! - **the worker's drain answer** ([`UndecodablePart::Drain`], #853): how
//!   many messages the worker still held at exit is unknown
//!   (`worker_drain.rs` says that one).
//!
//! What happened to a poll's messages depends on the channel:
//!
//! - **Without an ack method** (Matrix): the worker's cursor has already
//!   moved past them, so what did not decode is **lost**. Said every time, and
//!   handed to the channel's [`InboundUndecodableAudit`] hook for a
//!   `channel.inbound_undecodable` row: channel, part, when, and for events
//!   how many — a separate action rather than a `channel.inbound_dropped` row,
//!   so `sum(dropped)` over those stays a count of messages a closed bus never
//!   took, and a whole batch, whose size is unknown, has no false 0. The
//!   events of a batch that did decode still reach the bus.
//! - **With one** (email): the batch is not acked, so the worker redelivers
//!   it — and nothing after it arrives until it decodes. Not a loss, so no
//!   row. But a batch that never decodes would come back at the very next
//!   poll, which the worker answers at once while the message is pending — a
//!   hot loop of polls and lines. A batch with undecodable events is treated
//!   as wholly undecodable here ([`whole_batch_on_ack_channel`]): acking the
//!   events after a bad one would move the monotonic cursor past it for good.
//!
//! So on **every** channel the poll then waits out the refusal backoff
//! (`spec.refusal_backoff`, [`super::REFUSAL_BACKOFF`] in production) on its
//! own [`UndecodableRun`], and a redelivered batch is said on that run's
//! cadence: the first, then once per [`super::refusal::REFUSAL_REPEAT`] — and
//! at once when the decode error changes, or when the worker was just
//! respawned. The first batch that decodes whole again ends the run, said
//! once at INFO.
//!
//! On Matrix the hold has a cost: the worker keeps buffering while it lasts.
//! Since #852 that cost is latency, not loss, unless the next result fails
//! as a whole too, and takes what was buffered with it. It is kept as a bound
//! on rows — a worker answering every poll at once with something that does
//! not decode would otherwise write one per poll.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use super::audit::{record_undecodable, DriverAudit, InboundUndecodableAudit};
use super::outage::OutageLog;
use super::refusal::{RefusalRun, REFUSAL_REPEAT};
use super::{PolledBatch, PolledEvent, PolledWorkerSpec, UndecodableEvents};
use crate::channel::actions;
use crate::channel::audit_text::cap_chars;
use crate::channel::undelivered::observed_at_json;
use crate::channel::ChannelId;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

/// How many chars of a decode error its line quotes. serde quotes an
/// offending value whole, so a worker can put a message body of any length in
/// one — and every Matrix loss is said.
pub(super) const DECODE_ERROR_CAP_CHARS: usize = 256;

/// Every undecodable line's reason, said the same way wherever it is said.
pub(super) const WHY_UNDECODABLE: &str =
    "The worker sent output the core cannot read: a worker bug, or a compromised worker.";

/// Which answer of the worker's did not decode: the row's `part`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndecodablePart {
    /// A whole poll result. How many messages it held is unknown.
    Batch,
    /// This many events of a poll result whose envelope decoded — possibly
    /// all of them (#852). Any others decoded and went on to the bus, or, if
    /// it had closed, were counted as `channel.inbound_dropped` at the
    /// `polled` stage.
    Events(NonZeroUsize),
    /// The worker's drain answer as the driver exited: how many messages it
    /// still held is unknown (#853).
    Drain,
}

impl UndecodablePart {
    /// The row's spelling, pinned: operators query `audit_log` by it. A row
    /// written before #852 shipped (PR #855) has no `part`, and is a batch.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Batch => "batch",
            Self::Events(_) => "events",
            Self::Drain => "drain",
        }
    }
}

/// What the [`InboundUndecodableAudit`] hook is told about inbound messages
/// lost because the worker's answer did not decode: whose channel, which
/// answer, and when — and for [`UndecodablePart::Events`] how many. Nothing
/// the worker or a peer wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboundUndecodable<'a> {
    pub channel: &'a ChannelId,
    pub part: UndecodablePart,
    /// When they were lost. The row is written after the fact (#789), so
    /// `audit_log.ts` is the insert's time, not this.
    pub observed_at: time::OffsetDateTime,
}

impl InboundUndecodable<'_> {
    /// Pure: the payload of an [`actions::INBOUND_UNDECODABLE`] row — the
    /// channel, the part, when, and for events the count; nothing else.
    pub fn payload(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "channel": self.channel.0,
            "part": self.part.as_str(),
            "observed_at": observed_at_json(self.observed_at),
        });
        if let UndecodablePart::Events(n) = self.part {
            v["undecodable"] = n.get().into();
        }
        v
    }
}

/// Pure: refuse a hook that would never be called — an
/// [`InboundUndecodableAudit`] on a channel with an ack method, whose
/// undecodable batches are redelivered, not lost (and which has no drain
/// method: `worker_drain::check_drain`). Checked once, by
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

/// Pure: on a channel that acks (`acks`), a batch with any undecodable event
/// is an undecodable batch: none of it may be acked, because acking the
/// events after the bad one would move the monotonic cursor past it for good.
/// Anything else passes through unchanged.
///
/// The counts are the error's context and the first decode error its root
/// cause: the line says both, but the run's cadence follows only the cause
/// ([`on_undecodable_batch`]), so a redelivered batch that grows as new
/// messages arrive behind the bad one is still the same error.
pub(super) fn whole_batch_on_ack_channel(batch: PolledBatch, acks: bool) -> anyhow::Result<PolledBatch> {
    match (&batch.undecodable, acks) {
        (Some(u), true) => Err(anyhow::anyhow!("{}", u.first_error).context(format!(
            "{} of the batch's {} events did not decode",
            u.count,
            u.count.get() + batch.events.len(),
        ))),
        _ => Ok(batch),
    }
}

/// One run of consecutive poll results that did not decode whole: when the
/// poll may run again, and when the run was last said. A [`RefusalRun`]
/// underneath — the same backoff and repeat cadence — but its own type, so
/// `run`'s three runs cannot be passed in each other's place.
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

    /// One more undecodable batch: re-arm the line after an outage, and hold
    /// the poll. Returns the hold and whether a redelivered batch is due to
    /// be said.
    fn on_undecodable(&mut self, error: &str, spec: &PolledWorkerSpec, outage: &OutageLog) -> (Duration, bool) {
        if std::mem::replace(&mut self.outages_ended, outage.ended()) != outage.ended() {
            self.run.rearm_report();
        }
        let next = self.run.on_refusal(Instant::now(), &spec.refusal_backoff, error_code(error));
        (next.delay, next.report)
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

/// What of a poll result was lost undecodable, on a channel that does not
/// redeliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LostPart {
    /// The whole result: how many messages it held is unknown.
    Batch,
    /// `lost` of its events, besides the `decoded` ones (#852). Two counts
    /// that add, not a part and a total, so no pair is inconsistent.
    Events { lost: NonZeroUsize, decoded: usize },
}

impl LostPart {
    /// The events of a result that did not decode, beside the `decoded` ones.
    pub(super) fn events(u: &UndecodableEvents, decoded: usize) -> Self {
        Self::Events { lost: u.count, decoded }
    }

    /// The row's part for this loss.
    fn row_part(self) -> UndecodablePart {
        match self {
            Self::Batch => UndecodablePart::Batch,
            Self::Events { lost, .. } => UndecodablePart::Events(lost),
        }
    }
}

/// What became of what did not decode, for its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fate {
    /// The channel acks, so the batch comes back (email). The poll runs again
    /// in `retry_in`; this is the run's `in_a_row`th undecodable batch.
    Redelivered { retry_in: Duration, in_a_row: u32 },
    /// The channel does not redeliver, so `part` is lost (Matrix).
    /// `retry_in` is `None` when the driver is exiting; `recorded` is whether
    /// the channel has a sink to record it.
    Lost { part: LostPart, retry_in: Option<Duration>, recorded: bool },
}

/// Pure: the end of a loss's line — whether the channel records it as a row.
pub(super) fn recorded_as(recorded: bool) -> String {
    let action = actions::INBOUND_UNDECODABLE;
    if recorded {
        format!("recording it as {action}")
    } else {
        format!("NOT recorded as {action}: this channel has no audit sink")
    }
}

/// Pure: the line for a poll result that did not decode, wholly or in part:
/// the decode error — the first event's, for part of one — capped at
/// [`DECODE_ERROR_CAP_CHARS`], and what became of what did not decode.
pub(super) fn format_undecodable_report(error: &str, fate: Fate) -> String {
    let error = cap_chars(error, DECODE_ERROR_CAP_CHARS);
    let why = WHY_UNDECODABLE;
    match fate {
        Fate::Redelivered { retry_in, in_a_row } => format!(
            "a poll result did not decode ({error}), so its batch was not acked, so the worker \
             redelivers it, and nothing after it arrives until it decodes. {why} Polling again \
             in {} ms with backoff ({in_a_row} in a row so far). Repeated every {} min while it \
             lasts",
            retry_in.as_millis(),
            REFUSAL_REPEAT.as_secs() / 60,
        ),
        Fate::Lost { part, retry_in, recorded } => {
            let retry = retry_in.map(|d| format!(" Polling again in {} ms;", d.as_millis())).unwrap_or_default();
            let recorded = recorded_as(recorded);
            let what = match part {
                LostPart::Batch => format!(
                    "a poll result did not decode ({error}), so its batch is lost: this channel \
                     does not redeliver, and how many messages it held is unknown."
                ),
                LostPart::Events { lost, decoded } => {
                    let of = lost.get() + decoded;
                    // "decoded", not "reached the bus": when the bus has
                    // closed they did not, and their own line says so.
                    let rest = match decoded {
                        0 => String::new(),
                        1 => " The other one decoded.".to_string(),
                        n => format!(" The other {n} decoded."),
                    };
                    format!(
                        "{lost} of the {of} events in a poll result did not decode (first: \
                         {error}), so {} lost: this channel does not redeliver.{rest}",
                        if lost.get() == 1 { "it is" } else { "they are" },
                    )
                }
            };
            format!("{what} {why}{retry} {recorded}")
        }
    }
}

/// Pure: the line that ends a run of `n` poll results that did not decode
/// whole, or `None` when no run ended (`n == 0`). "Whole", not "undecodable":
/// the run counts results some of whose events did not decode too (#852),
/// whose good events reached the bus.
pub(super) fn format_decodes_again_report(n: u32) -> Option<String> {
    (n > 0).then(|| format!("poll results decode whole again after {n} that did not; that run has ended"))
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
    // The line says the whole chain; the cadence follows the root cause only
    // (see `whole_batch_on_ack_channel`).
    let error = format!("{e:#}");
    let (delay, report) = run.on_undecodable(&e.root_cause().to_string(), spec, outage);
    if spec.ack_method.is_none() {
        report_lost_undecodable(spec, cid, &error, LostPart::Batch, Some(delay), audit);
    } else if report {
        let fate = Fate::Redelivered { retry_in: delay, in_a_row: run.run.consecutive() };
        emit_worker_refusal_report(spec.label, &format_undecodable_report(&error, fate), RefusalSeverity::Error);
    }
}

/// A poll result decoded, while the bus is up: return its events for the bus.
/// When some of its events did not (#852, on a channel that does not
/// redeliver: [`whole_batch_on_ack_channel`] has already turned one on an ack
/// channel into an `Err`), say and record those, and hold the next poll as
/// for a whole undecodable batch — so the run goes on. Otherwise a run of
/// undecodable batches, if any, has ended.
pub(super) fn on_decoded_batch(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    batch: PolledBatch,
    run: &mut UndecodableRun,
    outage: &OutageLog,
    audit: Option<&InboundUndecodableAudit>,
) -> Vec<PolledEvent> {
    let PolledBatch { events, undecodable } = batch;
    match undecodable {
        Some(u) => {
            // The cadence's `report` is ignored, as for a lost batch: each
            // lost event is a different one, so every loss is said.
            let (delay, _) = run.on_undecodable(&u.first_error, spec, outage);
            report_lost_undecodable(spec, cid, &u.first_error, LostPart::events(&u, events.len()), Some(delay), audit);
        }
        None => decoded(run, spec.label),
    }
    events
}

/// Say `part` of a poll result lost because it did not decode, and hand it to
/// the hook. For a channel with no ack method only. `retry_in` is the poll's
/// backoff, or `None` when the driver is exiting (the bus closed while the
/// poll was out).
pub(super) fn report_lost_undecodable(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    error: &str,
    part: LostPart,
    retry_in: Option<Duration>,
    audit: Option<&InboundUndecodableAudit>,
) {
    let fate = Fate::Lost { part, retry_in, recorded: audit.is_some() };
    emit_worker_refusal_report(spec.label, &format_undecodable_report(error, fate), RefusalSeverity::Error);
    record_lost(spec.label, cid, part.row_part(), audit);
}

/// Hand an undecodable loss to the hook, timed like every hook call.
pub(super) fn record_lost(
    label: &str,
    cid: &ChannelId,
    part: UndecodablePart,
    audit: Option<&InboundUndecodableAudit>,
) {
    let lost = InboundUndecodable { channel: cid, part, observed_at: time::OffsetDateTime::now_utc() };
    record_undecodable(audit, label, lost);
}

/// A poll result decoded whole: a run of undecodable batches, if any, has
/// ended — said once, at INFO.
pub(super) fn decoded(run: &mut UndecodableRun, label: &str) {
    if let Some(report) = format_decodes_again_report(run.run.on_accepted()) {
        emit_worker_refusal_report(label, &report, RefusalSeverity::Recovered);
    }
}
