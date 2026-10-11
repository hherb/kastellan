//! Drop guards that say what a bus stop abandons (#832).
//!
//! `ChannelBus::shutdown` aborts its pumps wherever they are parked. A pump
//! aborted mid-await has its future dropped: no `Err` arm runs, so whatever it
//! was doing ends with no row and no line. #813 covered the audit insert
//! itself (`pg_events::PendingInsert`); these guards cover the work around it:
//!
//! - [`InFlight::Inbound`] — a message the transport has already handed over
//!   and will not redeliver (Matrix has no ack; email acked it once the bus
//!   took it), between `recv()` and the end of its handling: authorize,
//!   pairing, ask resolution, containment, enqueue, and the send of any ack
//!   the bus answers it with.
//! - [`InFlight::Reply`] — a reply between its claim and the end of its send.
//!   The claim is one autocommit `UPDATE` that the server can still commit
//!   after the abort, so the task may be `routed` with nothing sent.
//! - [`InFlight::Unroutable`] — the same, for the claim that settles a reply
//!   nobody can deliver.
//! - [`InFlight::Ask`] — a raised ask handed to the transport. Its
//!   `ask.delivered` row is written and nothing will re-send it.
//!
//! Each is armed before the first await and settled after the last. Dropped
//! unsettled, it reports one line through [`ChannelEvents::report_lost`] — sync,
//! because `Drop` cannot await — naming only the channel, the peer and the
//! task id, never a body.
//!
//! [`PumpQueue`] covers what is still *queued* when a channel's pump ends. A
//! reply there is unclaimed, so the catch-up sweep delivers it (see
//! `bus_deliver`) and it is only counted, at INFO; a queued ask is said, one
//! line each.
//!
//! A guard dropped while its thread panics (unwinding builds only — release is
//! `panic = "abort"`) says so instead of claiming the bus stopped, exactly as
//! `PendingInsert` does.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::audit_text::quoted_id;
use super::bus::ChannelEvents;
use super::bus_deliver::Queued;
use super::{ChannelId, PeerId};

/// What a guard is protecting, for its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InFlight {
    /// An inbound message being handled.
    Inbound,
    /// A finished task's reply, from its claim to the end of its send.
    Reply { task_id: i64 },
    /// The claim that settles a channel task with no routing metadata.
    Unroutable { task_id: i64 },
    /// Task `task_id`'s raised ask, handed to the transport. Its
    /// `ask.delivered` row is written and nothing will re-send it, so a stop
    /// mid-send is said.
    Ask { task_id: i64 },
}

/// A drop guard held across one piece of in-flight work. [`settle`] it when
/// the work is over, whatever its outcome; dropped unsettled, it reports the
/// work as abandoned.
///
/// [`settle`]: InFlightGuard::settle
#[must_use = "an unsettled guard reports its work as abandoned when it drops"]
pub(crate) struct InFlightGuard<'a> {
    events: &'a dyn ChannelEvents,
    what: InFlight,
    /// ` for channel "x", peer "y"`, rendered when armed (the message it was
    /// taken from may be gone by the time the guard drops). Empty when there
    /// is neither.
    about: String,
    settled: bool,
}

impl<'a> InFlightGuard<'a> {
    /// Arm a guard. Construct it before the work's first `.await`: a stop
    /// landing earlier would abandon the work with nothing said.
    pub(crate) fn arm(
        events: &'a dyn ChannelEvents,
        what: InFlight,
        channel: Option<&ChannelId>,
        peer: Option<&PeerId>,
    ) -> Self {
        Self { events, what, about: describe(channel, peer), settled: false }
    }

    /// The work is over, whatever its outcome: say nothing.
    pub(crate) fn settle(mut self) {
        self.settled = true;
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            let line = format_abandoned(self.what, &self.about, std::thread::panicking());
            self.events.report_lost(&line);
        }
    }
}

/// Pure: ` for channel "x", peer "y"`, each part left out when absent, and
/// empty when both are. Both are [`quoted_id`]'d: a peer comes from outside
/// the core, and a raw one could forge a line.
fn describe(channel: Option<&ChannelId>, peer: Option<&PeerId>) -> String {
    let parts: Vec<String> = [("channel", channel.map(|c| &c.0)), ("peer", peer.map(|p| &p.0))]
        .into_iter()
        .filter_map(|(key, v)| v.map(|v| format!("{key} {}", quoted_id(v))))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" for {}", parts.join(", "))
    }
}

/// Pure: the `[audit-lost]` line for work a guard saw abandoned. `about` is
/// [`describe`]'s output; `panicked` tells a panic from a stop.
pub(crate) fn format_abandoned(what: InFlight, about: &str, panicked: bool) -> String {
    let cause = if panicked { "its handling panicked" } else { "the bus stopped while it was in flight" };
    match what {
        InFlight::Inbound => format!(
            "a message{about} was abandoned mid-handling: {cause}; it may not have been enqueued or recorded"
        ),
        InFlight::Reply { task_id } => format!("task {task_id}'s reply{about} may not have been sent: {cause}"),
        InFlight::Ask { task_id } => format!("task {task_id}'s ask message{about} may not have been sent: {cause}"),
        InFlight::Unroutable { task_id } => format!(
            "task {task_id}'s unroutable reply may have been settled without its channel.reply_unroutable row: {cause}"
        ),
    }
}

/// Pure: the `[audit-lost]` line for a reply whose claim errored and whose
/// re-read failed too. The claim may have committed — and then the task is
/// `routed`, with nothing sent and no row, and no sweep will find it — so
/// this is said rather than left to a WARN.
pub(crate) fn format_claim_unknowable(task_id: i64, channel: &ChannelId, peer: &PeerId) -> String {
    format!(
        "task {task_id}'s reply{} may have been settled without being sent: its claim failed, and so did the re-read that would have told",
        describe(Some(channel), Some(peer))
    )
}

/// A channel's outbound queue, as its pump owns it: the receiver, plus what
/// its `Drop` says about the items still in it when the pump ends — by
/// returning, by panicking, or by being aborted.
///
/// A [`Queued::Reply`] left behind is fine: it was never claimed, so the
/// catch-up sweep delivers it (#832), and it is only counted, at INFO. A
/// [`Queued::Ask`] is not recoverable — it has no task reply to re-find, and
/// `ask.delivered` was already written — so each one is said on
/// `[audit-lost]`, by task, channel and peer.
///
/// The drain is everything buffered when it runs, not everything ever sent: a
/// producer that took its slot just before `close()` (inside `send`/`try_send`
/// on another thread) can still land an item after it, which then goes with
/// the receiver unsaid. A reply lost that way is still unclaimed; an ask needs
/// a pump ending while still registered (`shutdown` deregisters first).
pub(crate) struct PumpQueue {
    rx: mpsc::Receiver<Queued>,
    events: Arc<dyn ChannelEvents>,
    channel: ChannelId,
}

impl PumpQueue {
    /// A channel's queue of `cap`: the sender for its producers, and the
    /// receiving end already wrapped, so no bare receiver exists to bypass
    /// this `Drop` with.
    pub(crate) fn channel(
        cap: usize,
        events: Arc<dyn ChannelEvents>,
        channel: ChannelId,
    ) -> (mpsc::Sender<Queued>, Self) {
        let (tx, rx) = mpsc::channel(cap);
        (tx, Self { rx, events, channel })
    }

    /// The next item; cancel-safe, as `mpsc::Receiver::recv` is.
    pub(crate) async fn recv(&mut self) -> Option<Queued> {
        self.rx.recv().await
    }
}

impl Drop for PumpQueue {
    fn drop(&mut self) {
        // Closing first stops producers from adding more while this drains;
        // `try_recv` still returns everything already buffered.
        self.rx.close();
        let mut replies = 0usize;
        while let Ok(item) = self.rx.try_recv() {
            match item {
                Queued::Reply { .. } => replies += 1,
                Queued::Ask { task_id, out } => {
                    let line = format_ask_unsent(task_id, &out.channel, &out.peer, std::thread::panicking());
                    self.events.report_lost(&line);
                }
            }
        }
        if replies > 0 {
            tracing::info!(
                channel = %self.channel.0, replies,
                "channel pump ended with replies queued; they were never claimed and are left for catch-up"
            );
        }
    }
}

/// Pure: the `[audit-lost]` line for a raised ask still queued when its
/// channel's pump ended. Its body carries the approval token, so only the
/// task, the channel and the peer are named. `panicked` tells a panic from a
/// stop.
pub(crate) fn format_ask_unsent(task_id: i64, channel: &ChannelId, peer: &PeerId, panicked: bool) -> String {
    let cause = if panicked { "its pump panicked with it queued" } else { "the bus stopped with it queued" };
    format!("task {task_id}'s ask message{} was not sent: {cause}", describe(Some(channel), Some(peer)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::bus::ChannelEvents;
    use crate::channel::{ChannelId, PeerId};
    use kastellan_db::tasks::Lane;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Records what the guards report; nothing else is ever called.
    #[derive(Default)]
    struct RecordingEvents {
        lost: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ChannelEvents for RecordingEvents {
        async fn enqueue(&self, _lane: Lane, _payload: Value) -> anyhow::Result<i64> {
            unreachable!("a guard never enqueues")
        }
        async fn audit(&self, _action: &str, _payload: Value) {
            unreachable!("a guard never audits: it runs in Drop")
        }
        fn report_lost(&self, line: &str) {
            self.lost.lock().unwrap().push(line.to_string());
        }
    }

    fn matrix() -> ChannelId {
        ChannelId("matrix".into())
    }
    fn me() -> PeerId {
        PeerId("@me:srv".into())
    }

    #[test]
    fn a_settled_guard_says_nothing() {
        let ev = RecordingEvents::default();
        InFlightGuard::arm(&ev, InFlight::Inbound, Some(&matrix()), Some(&me())).settle();
        InFlightGuard::arm(&ev, InFlight::Reply { task_id: 7 }, Some(&matrix()), Some(&me())).settle();
        InFlightGuard::arm(&ev, InFlight::Unroutable { task_id: 9 }, None, None).settle();
        InFlightGuard::arm(&ev, InFlight::Ask { task_id: 5 }, Some(&matrix()), Some(&me())).settle();
        assert!(ev.lost.lock().unwrap().is_empty(), "{:?}", ev.lost.lock().unwrap());
    }

    #[test]
    fn an_abandoned_inbound_message_is_said_once() {
        let ev = RecordingEvents::default();
        drop(InFlightGuard::arm(&ev, InFlight::Inbound, Some(&matrix()), Some(&me())));
        assert_eq!(
            *ev.lost.lock().unwrap(),
            [r#"a message for channel "matrix", peer "@me:srv" was abandoned mid-handling: the bus stopped while it was in flight; it may not have been enqueued or recorded"#]
        );
    }

    /// The other two texts, pinned literally: an operator's alert reads them.
    #[test]
    fn a_reply_and_an_unroutable_claim_are_said_with_their_task_id() {
        let ev = RecordingEvents::default();
        drop(InFlightGuard::arm(&ev, InFlight::Reply { task_id: 7 }, Some(&matrix()), Some(&me())));
        drop(InFlightGuard::arm(&ev, InFlight::Unroutable { task_id: 9 }, None, None));
        assert_eq!(
            *ev.lost.lock().unwrap(),
            [
                r#"task 7's reply for channel "matrix", peer "@me:srv" may not have been sent: the bus stopped while it was in flight"#,
                "task 9's unroutable reply may have been settled without its channel.reply_unroutable row: the bus stopped while it was in flight",
            ]
        );
    }

    /// A peer comes from outside the core: quoted and escaped, never raw, so
    /// it cannot forge a second line or a closing quote.
    #[test]
    fn a_hostile_peer_is_quoted_not_spliced() {
        let ev = RecordingEvents::default();
        let peer = PeerId("@x\" was fine\n[audit-lost] forged".into());
        drop(InFlightGuard::arm(&ev, InFlight::Inbound, Some(&matrix()), Some(&peer)));
        let line = ev.lost.lock().unwrap()[0].clone();
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains(r#"peer "@x\" was fine\n[audit-lost] forged""#), "{line}");
    }

    /// Unwinding builds only (release is `panic = "abort"`): a panic is not a
    /// stop, and the line must not say the bus stopped.
    #[test]
    fn a_guard_dropped_by_a_panic_says_so() {
        let ev = RecordingEvents::default();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = InFlightGuard::arm(&ev, InFlight::Reply { task_id: 7 }, Some(&matrix()), Some(&me()));
            panic!("test: the delivery panicked");
        }));
        assert!(r.is_err());
        assert_eq!(
            *ev.lost.lock().unwrap(),
            [r#"task 7's reply for channel "matrix", peer "@me:srv" may not have been sent: its handling panicked"#]
        );
    }

    #[test]
    fn a_queued_ask_is_named_by_channel_and_peer_only() {
        assert_eq!(
            format_ask_unsent(5, &matrix(), &me(), false),
            r#"task 5's ask message for channel "matrix", peer "@me:srv" was not sent: the bus stopped with it queued"#
        );
        assert_eq!(
            format_ask_unsent(5, &matrix(), &me(), true),
            r#"task 5's ask message for channel "matrix", peer "@me:srv" was not sent: its pump panicked with it queued"#
        );
    }

    /// An ask already handed to the transport when the bus stops: `ask.delivered`
    /// is written and nothing will re-send it, so it is said like a reply.
    #[test]
    fn an_ask_mid_send_is_said_by_channel_and_peer() {
        let ev = RecordingEvents::default();
        drop(InFlightGuard::arm(&ev, InFlight::Ask { task_id: 5 }, Some(&matrix()), Some(&me())));
        assert_eq!(
            *ev.lost.lock().unwrap(),
            [r#"task 5's ask message for channel "matrix", peer "@me:srv" may not have been sent: the bus stopped while it was in flight"#]
        );
    }

    #[test]
    fn an_unknowable_claim_is_named_by_task_channel_and_peer() {
        assert_eq!(
            format_claim_unknowable(7, &matrix(), &me()),
            r#"task 7's reply for channel "matrix", peer "@me:srv" may have been settled without being sent: its claim failed, and so did the re-read that would have told"#
        );
    }
}
