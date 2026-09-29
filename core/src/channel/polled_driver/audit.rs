//! The polled driver's optional audit hooks: the caller's way to record a
//! skipped id or a dropped reply durably, while the driver itself stays
//! DB-free. Split out of [`super`] to keep it under the 500-LOC soft cap;
//! re-exported there, so every path is unchanged.

use crate::channel::UndeliveredReply;

/// Best-effort side channel for a caller to record "this id was discarded
/// without ever becoming a bus event" somewhere durable (e.g. an
/// `audit_log` row) — called once per skipped id, just before its ack, as
/// `audit(message_id, reason)`. The driver itself stays DB-free by design
/// (see the module docs); this is a boxed closure rather than a bare `fn`
/// pointer specifically so a caller CAN capture state (a `PgPool` +
/// `tokio::runtime::Handle`). `None` means no audit call is ever made — the
/// default, and Matrix's case (it never supplies a `parse_ack_only` either,
/// so this is moot for it).
///
/// It is called on the driver's own thread, so it must not block on I/O —
/// the same rule as [`ReplyUndeliveredAudit`]. The daemon's hook spawns its
/// insert rather than `block_on`ing it (#789).
pub type AckOnlyAudit = Box<dyn Fn(&str, &str) + Send + 'static>;

/// Best-effort side channel for a caller to record "this reply was never
/// delivered" durably — the `channel.reply_undelivered` row (#782). Called
/// once per reply the driver drops, with why
/// ([`UndeliveredReason`](crate::channel::UndeliveredReason)): given
/// up after its refusals, past a full conversation queue, or still queued
/// when the driver exits. Same shape and reasons as [`AckOnlyAudit`].
///
/// It is handed an [`UndeliveredReply`], not the reply: the row carries
/// channel + peer + reason only, and the view has no body to leak (#790).
/// It is called on the driver's own thread, so it must not block on I/O: a
/// stalled audit insert would stall every conversation and the poll with it.
pub type ReplyUndeliveredAudit = Box<dyn Fn(UndeliveredReply<'_>) + Send + 'static>;

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
}

impl DriverAudit {
    /// No audit hooks: every drop and skip is still logged (on the
    /// `[worker-refusal]` emitter for a dropped reply, whose line then says it
    /// was **not** recorded), but nothing is written durably.
    pub fn none() -> Self {
        Self { ack_only: None, reply_undelivered: None }
    }
}
