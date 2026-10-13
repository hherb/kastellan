//! The polled driver's optional audit hooks: the caller's way to record a
//! skipped id, a dropped reply, a dropped inbound batch or an undecodable one
//! durably, while the driver itself stays DB-free. Split out of [`super`] to keep it under the
//! 500-LOC soft cap; re-exported there, so every path is unchanged.
//!
//! Every hook runs on the driver's only thread, which every conversation, the
//! poll and the ack wait on — or, for an inbound drop at the buffered stage,
//! inside the bus's stopping pump, which the channel's stop waits on (#846) —
//! so **a hook must not block** (#789). A type cannot say that, so every hook
//! call is timed ([`record_skipped`], [`record_undelivered`],
//! [`record_inbound_dropped`], [`record_undecodable`]) and said when one holds its caller past
//! [`HOOK_BUDGET`] (#793): what cannot be enforced is at least detected.

use std::time::{Duration, Instant};

use super::inbound_drop::{InboundDropStage, InboundDropped};
use super::undecodable::UndecodableBatch;
use crate::channel::{OutgoingMessage, SkippedId, UndeliveredReason, UndeliveredReply};

/// Best-effort side channel for a caller to record "this id was discarded
/// without ever becoming a bus event" somewhere durable (e.g. an
/// `audit_log` row) — called just before a skipped id's ack, with a
/// [`SkippedId`] (named fields, so the id and the reason cannot be passed
/// swapped: #793), once per poll that reaches the id: an id whose
/// ack failed is redelivered and audited again. The driver itself stays
/// DB-free by design (memory and database access are the core's, never a
/// channel driver's); this is a boxed closure rather than a bare `fn`
/// pointer specifically so a caller CAN capture state (a `PgPool` +
/// `tokio::runtime::Handle`). `None` means no audit call is ever made — the
/// default, and Matrix's case (it never supplies a `parse_ack_only` either,
/// so this is moot for it).
///
/// It is called on the driver's own thread, so it must not block on I/O —
/// the same rule as [`ReplyUndeliveredAudit`]. The daemon's hook spawns its
/// insert rather than `block_on`ing it (#789).
pub type AckOnlyAudit = Box<dyn Fn(SkippedId<'_>) + Send + 'static>;

/// Best-effort side channel for a caller to record "this reply was never
/// delivered" durably — the `channel.reply_undelivered` row (#782). Called
/// once per reply the driver drops, with why
/// ([`UndeliveredReason`](crate::channel::UndeliveredReason)): given
/// up after its refusals, past a full conversation queue, or still queued
/// when the driver exits. Optional and best-effort, like [`AckOnlyAudit`].
///
/// It is handed an [`UndeliveredReply`], not the reply: the row carries
/// channel + peer + reason + when only, and the view has no body to leak (#790).
/// It is called on the driver's own thread, so it must not block on I/O: a
/// stalled audit insert would stall every conversation and the poll with it.
pub type ReplyUndeliveredAudit = Box<dyn Fn(UndeliveredReply<'_>) + Send + 'static>;

/// Best-effort side channel for a caller to record "the bus closed and these
/// inbound messages never reached it, and nothing redelivers them" durably —
/// the `channel.inbound_dropped` row. Called from two places, so it is an
/// `Arc` (and `Sync`):
///
/// - **the driver**, at most once per exit, for a polled batch it could not
///   hand over ([`InboundDropStage::Polled`], #826) — only on a channel with
///   no ack method (Matrix): an ack channel's unacked batch is redelivered on
///   the next start, so it is not a loss — and, at most once per exit (only
///   when it held something), for what its worker still held
///   ([`InboundDropStage::Worker`], #835) — only on a channel whose spec
///   names a drain method (Matrix);
/// - **the bus-side receiver**, at most once (only when something was
///   unread), as the bus drops it, for what was still in the inbound buffer
///   unread ([`InboundDropStage::Buffered`], #846) — on every channel,
///   because an ack channel had already acked those.
///
/// It is handed an [`InboundDropped`]: the channel, the stage, how many, and
/// when — never a peer, an id or a body, all of which are peer-supplied. It
/// runs on the driver's own thread or inside the bus's stopping pump, so it
/// must not block on I/O, like the hooks above.
pub type InboundDroppedAudit = std::sync::Arc<dyn Fn(InboundDropped<'_>) + Send + Sync + 'static>;

/// Best-effort side channel for a caller to record "a poll result did not
/// decode, and the batch is lost" durably — the `channel.inbound_undecodable`
/// row (#836). Called by the driver, once per such batch, only on a channel
/// with no ack method (Matrix): an ack channel's undecodable batch is not
/// acked, so it is redelivered, and is not a loss.
///
/// It is handed an [`UndecodableBatch`]: the channel and when — no count
/// (the batch did not decode, so it is unknown) and no error text (a decode
/// error can quote a peer's message). It runs on the driver's own thread, so
/// it must not block on I/O, like the hooks above.
pub type InboundUndecodableAudit = Box<dyn Fn(UndecodableBatch<'_>) + Send + 'static>;

/// The driver's optional audit hooks.
///
/// No `Default`, on purpose: "no audit sink" means the driver's drops are
/// logged but leave **no** durable row, so a caller says so by name
/// ([`DriverAudit::none`]) rather than by `..Default::default()`.
pub struct DriverAudit {
    /// See [`AckOnlyAudit`]. Only the email channel supplies one.
    pub ack_only: Option<AckOnlyAudit>,
    /// See [`ReplyUndeliveredAudit`].
    pub reply_undelivered: Option<ReplyUndeliveredAudit>,
    /// See [`InboundDroppedAudit`]. The driver calls it only on a channel with
    /// no ack method; the bus-side receiver it hands a clone to calls it on
    /// every channel (#846).
    pub inbound_dropped: Option<InboundDroppedAudit>,
    /// See [`InboundUndecodableAudit`]. Called only on a channel with no ack
    /// method.
    pub inbound_undecodable: Option<InboundUndecodableAudit>,
}

impl DriverAudit {
    /// No audit hooks: every drop and skip is still logged (on the
    /// `[worker-refusal]` emitter for a dropped reply or a dropped inbound
    /// batch, whose line then says it was **not** recorded), but nothing is
    /// written durably.
    pub fn none() -> Self {
        Self {
            ack_only: None,
            reply_undelivered: None,
            inbound_dropped: None,
            inbound_undecodable: None,
        }
    }
}

/// How long an audit hook may hold its caller's thread before that is said
/// (#793). A hook that spawns its insert returns in microseconds; one that
/// waits for Postgres takes a round trip at best and the pool's acquire
/// timeout at worst, so 100 ms separates the two with room for a loaded host.
pub(super) const HOOK_BUDGET: Duration = Duration::from_millis(100);

/// Who called an audit hook, for the slow-hook line: what it held, and what
/// waited on that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HookCaller {
    /// The driver's only thread: every hook but a buffered-stage drop.
    DriverThread,
    /// The bus's stopping pump, dropping its [`super::InboundReceiver`]
    /// (#846).
    StoppingBus,
}

impl HookCaller {
    /// Pure: (what the hook held, what waits on it).
    const fn held_and_waiting(self) -> (&'static str, &'static str) {
        match self {
            Self::DriverThread => {
                ("the driver thread", "every conversation, the poll and the ack wait on this thread")
            }
            Self::StoppingBus => (
                "the bus's stopping pump",
                "the channel's stop, and so a restart or a daemon shutdown, waits on it",
            ),
        }
    }
}

/// Pure: the line for an audit hook that held `caller` for `held`, or `None`
/// within [`HOOK_BUDGET`].
pub(super) fn format_slow_hook_report(hook: &str, caller: HookCaller, held: Duration) -> Option<String> {
    (held > HOOK_BUDGET).then(|| {
        let (what, waiting) = caller.held_and_waiting();
        format!(
            "the {hook} audit hook held {what} for {} ms; an audit hook must not block (#789): \
             {waiting}",
            held.as_millis()
        )
    })
}

/// Test builds only: every slow-hook line, as `(channel label, line)`. Read
/// through [`slow_hook_reports_for`], one label per test, for the reason the
/// refusal emitter keeps its own record (a scoped `tracing` subscriber flakes
/// on the process-wide callsite interest cache).
#[cfg(test)]
static SLOW_HOOK_REPORTS: std::sync::Mutex<Vec<(String, String)>> =
    std::sync::Mutex::new(Vec::new());

/// Test builds only: the slow-hook lines reported for channel `label`.
#[cfg(test)]
pub(super) fn slow_hook_reports_for(label: &str) -> Vec<String> {
    SLOW_HOOK_REPORTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|(l, _)| l == label)
        .map(|(_, line)| line.clone())
        .collect()
}

/// Make one hook call, timed; report it if it held the thread too long.
///
/// On `tracing` at WARN, not a marked emitter: a slow hook is a defect in the
/// caller's sink; the WARN names the hook that held the thread.
fn timed(label: &str, hook: &str, caller: HookCaller, call: impl FnOnce()) {
    let started = Instant::now();
    call();
    if let Some(line) = format_slow_hook_report(hook, caller, started.elapsed()) {
        #[cfg(test)]
        SLOW_HOOK_REPORTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((label.to_string(), line.clone()));
        tracing::warn!(label, "{line}");
    }
}

/// Hand a skipped id to the audit hook, if there is one, timed (#793).
pub(super) fn record_skipped(audit: Option<&AckOnlyAudit>, label: &str, id: SkippedId<'_>) {
    if let Some(audit) = audit {
        timed(label, "skipped-id", HookCaller::DriverThread, || audit(id));
    }
}

/// Hand a dropped reply to the audit hook, if there is one — as an
/// [`UndeliveredReply`], which leaves the body behind (#790), stamped with the
/// time of the drop — timed (#793).
pub(super) fn record_undelivered(
    audit: Option<&ReplyUndeliveredAudit>,
    label: &str,
    out: &OutgoingMessage,
    reason: UndeliveredReason,
) {
    if let Some(audit) = audit {
        let reply = UndeliveredReply::of(out, reason, time::OffsetDateTime::now_utc());
        timed(label, "reply-undelivered", HookCaller::DriverThread, || audit(reply));
    }
}

/// Hand a dropped inbound batch to the audit hook, if there is one, timed
/// (#793, #826) — from the driver's thread or, at the buffered stage, the
/// bus's stopping pump (#846).
pub(super) fn record_inbound_dropped(
    audit: Option<&InboundDroppedAudit>,
    label: &str,
    dropped: InboundDropped<'_>,
) {
    if let Some(audit) = audit {
        let caller = match dropped.stage {
            InboundDropStage::Polled | InboundDropStage::Worker => HookCaller::DriverThread,
            InboundDropStage::Buffered => HookCaller::StoppingBus,
        };
        timed(label, "inbound-dropped", caller, || audit(dropped));
    }
}

/// Hand a lost undecodable batch to the audit hook, if there is one, timed
/// (#793, #836).
pub(super) fn record_undecodable(
    audit: Option<&InboundUndecodableAudit>,
    label: &str,
    batch: UndecodableBatch<'_>,
) {
    if let Some(audit) = audit {
        timed(label, "inbound-undecodable", HookCaller::DriverThread, || audit(batch));
    }
}
