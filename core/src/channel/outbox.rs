//! Core-initiated outbound: the seam that lets code outside the bus send a
//! message on a channel.
//!
//! The bus is otherwise strictly *inbound message → task → reply on
//! completion*, so nothing in core could start a conversation — which is
//! why `Verdict::Escalate` could raise a durable question that reached only
//! `kastellan-cli inbox`.
//!
//! **Why a registry and not a `Sender`.** The scheduler is spawned before
//! the channel supervisors, and each supervisor *restarts* its bus (#514,
//! #517). So the scheduler cannot hold a sender the bus owns: it does not
//! exist yet at scheduler spawn, and any sender it held would go stale on
//! the next respawn. This is the indirection both sides share — `main`
//! creates it before either, the bus registers into it on every bring-up,
//! and a stale entry surfaces as [`OutboxError::QueueClosed`] rather than a
//! message that quietly disappears.
//!
//! Spec: `docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`.

use std::collections::HashMap;
use std::sync::RwLock;

use tokio::sync::mpsc;

use super::bus_deliver::Queued;
use super::{ChannelId, OutgoingMessage};

/// Why a delivery did not reach a channel's queue.
///
/// Every variant is a **fixed label** (see [`Self::as_str`]) because it is
/// written verbatim into a durable audit payload. Same rule as
/// `auth::UnauthenticReason`: never derive one from input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxError {
    /// No channel is registered under that id — it is not configured, or it
    /// is between bring-ups.
    NoSuchChannel,
    /// The channel's outbound queue is full; its pump is not draining.
    QueueFull,
    /// A sender is registered but its receiver is gone — a bus that ended
    /// without deregistering.
    QueueClosed,
}

impl OutboxError {
    /// Stable audit label. These strings land in `audit_log` payloads that
    /// operators query: add freely, never rename.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSuchChannel => "no_such_channel",
            Self::QueueFull => "queue_full",
            Self::QueueClosed => "queue_closed",
        }
    }
}

/// The registry of live per-channel outbound queues.
///
/// **Synchronous** (spec D4): [`try_deliver`](Self::try_deliver) uses
/// `try_send`, so the raise path never blocks on a wedged transport, and no
/// lock is ever held across an `await` — which forecloses the whole family
/// of deadlocks a lock plus async invites.
#[derive(Default)]
pub struct ChannelOutbox {
    senders: RwLock<HashMap<ChannelId, mpsc::Sender<Queued>>>,
}

impl ChannelOutbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a channel's outbound queue. Called by
    /// `ChannelBus::spawn` with the *same* sender its own reply pump uses,
    /// so there is one queue per channel and no second delivery path.
    pub fn register(&self, id: ChannelId, tx: mpsc::Sender<Queued>) {
        self.senders.write().expect("outbox lock not poisoned").insert(id, tx);
    }

    /// Drop a channel's queue. Called by `ChannelBus::shutdown`, so a bus
    /// that is going away stops being a delivery target immediately rather
    /// than after its first failed send.
    pub fn deregister(&self, id: &ChannelId) {
        self.senders.write().expect("outbox lock not poisoned").remove(id);
    }

    /// Queue `msg` — task `task_id`'s raised ask — for the channel it names,
    /// as a [`Queued::Ask`].
    ///
    /// Never blocks — a full queue is `Err(QueueFull)`, not backpressure —
    /// which is what makes it callable from the sync `deliver_ask`. It can
    /// still panic on a poisoned lock, same as `register`/`deregister`; the
    /// `RwLock` is only ever held for a map lookup with no `await` inside,
    /// so the only way to poison it is a panic in `HashMap`.
    pub fn try_deliver(&self, task_id: i64, msg: OutgoingMessage) -> Result<(), OutboxError> {
        let senders = self.senders.read().expect("outbox lock not poisoned");
        let tx = senders.get(&msg.channel).ok_or(OutboxError::NoSuchChannel)?;
        tx.try_send(Queued::Ask { task_id, out: msg }).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => OutboxError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => OutboxError::QueueClosed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use super::super::{ConversationId, PeerId};

    fn msg(channel: &str) -> OutgoingMessage {
        OutgoingMessage {
            channel: ChannelId(channel.to_string()),
            peer: PeerId("@horst:srv".to_string()),
            conversation: ConversationId("!room:srv".to_string()),
            body: "hello".to_string(),
        }
    }

    #[tokio::test]
    async fn a_registered_channel_receives_what_was_delivered() {
        let outbox = ChannelOutbox::new();
        let (tx, mut rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);

        outbox.try_deliver(7, msg("matrix")).expect("delivered");
        assert_eq!(rx.recv().await.expect("received"), Queued::Ask { task_id: 7, out: msg("matrix") });
    }

    #[test]
    fn delivering_to_an_unregistered_channel_is_an_error_not_a_silent_drop() {
        let outbox = ChannelOutbox::new();
        assert_eq!(outbox.try_deliver(7, msg("matrix")), Err(OutboxError::NoSuchChannel));
    }

    #[test]
    fn a_deregistered_channel_stops_accepting() {
        let outbox = ChannelOutbox::new();
        let (tx, _rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        outbox.deregister(&ChannelId("matrix".into()));
        assert_eq!(outbox.try_deliver(7, msg("matrix")), Err(OutboxError::NoSuchChannel));
    }

    /// The bus is supervised and restarts, so a sender can outlive the pump
    /// that drained it. That must be a reported failure, not a message that
    /// vanishes: the whole delivery contract is best-effort *and audited*.
    #[test]
    fn a_sender_whose_receiver_is_gone_reports_closed() {
        let outbox = ChannelOutbox::new();
        let (tx, rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        drop(rx);
        assert_eq!(outbox.try_deliver(7, msg("matrix")), Err(OutboxError::QueueClosed));
    }

    /// try_send, not send (spec D4): the raise path must never block on a
    /// channel whose consumer has stopped draining. A blocking send here
    /// parks the scheduler's escalation path behind a wedged transport.
    #[test]
    fn a_full_queue_is_refused_immediately_rather_than_awaited() {
        let outbox = ChannelOutbox::new();
        let (tx, _rx) = mpsc::channel(1);
        outbox.register(ChannelId("matrix".into()), tx);
        outbox.try_deliver(7, msg("matrix")).expect("first fits");
        assert_eq!(outbox.try_deliver(7, msg("matrix")), Err(OutboxError::QueueFull));
    }

    /// A restarted bus registers a fresh sender under the same id; the stale
    /// one must be replaced, or every delivery after the first restart goes
    /// into a queue nobody drains.
    #[tokio::test]
    async fn re_registering_replaces_the_stale_sender() {
        let outbox = ChannelOutbox::new();
        let (old_tx, old_rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), old_tx);
        drop(old_rx);

        let (new_tx, mut new_rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), new_tx);

        outbox.try_deliver(7, msg("matrix")).expect("delivered to the new sender");
        assert_eq!(new_rx.recv().await.expect("received"), Queued::Ask { task_id: 7, out: msg("matrix") });
    }

    /// Routing is per channel: an ask for a channel this outbox does not
    /// serve must not be delivered to whatever else happens to be registered.
    #[test]
    fn delivery_is_routed_by_channel_id() {
        let outbox = ChannelOutbox::new();
        let (tx, _rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        assert_eq!(outbox.try_deliver(7, msg("email")), Err(OutboxError::NoSuchChannel));
    }

    /// The audit labels are a fixed set, and the payloads that carry them
    /// are durable. Pinned so a rename is a deliberate act.
    #[test]
    fn every_error_has_a_stable_audit_label() {
        assert_eq!(OutboxError::NoSuchChannel.as_str(), "no_such_channel");
        assert_eq!(OutboxError::QueueFull.as_str(), "queue_full");
        assert_eq!(OutboxError::QueueClosed.as_str(), "queue_closed");
    }
}
