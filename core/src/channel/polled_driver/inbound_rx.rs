//! The bus-facing end of a polled driver's inbound buffer (#846).
//!
//! The driver hands each inbound message to the bus through a bounded tokio
//! mpsc ([`super::INBOUND_BUFFER`] slots). The bus's per-channel pump reads
//! them one at a time, and between reads it does database work (screening,
//! enqueueing, and since #832 two round-trips per reply). So when the bus
//! stops — a channel restart or a shutdown — messages the driver has already
//! handed over can still be sitting in the buffer, unread. A plain
//! `Receiver` drops them with no row and no line.
//!
//! Neither channel brings them back: Matrix never redelivers, and email has
//! already acked them, because the driver acks a message once the buffer
//! accepts it, not once the bus reads it (`polled_driver::run`) — all but any
//! held back by a refused ack earlier in the batch (or whose own ack failed
//! and no later ack moved the cursor past it), which may be redelivered but
//! cannot be told apart from here, so they are counted too (`inbound_drop`'s
//! module docs). So
//! [`InboundReceiver`] counts them as it is dropped and reports the count at
//! the [`InboundDropStage::Buffered`] stage: one `[worker-refusal]` WARN and
//! one `channel.inbound_dropped` row through the channel's
//! [`InboundDroppedAudit`] hook. Count only, never a peer or a body, like
//! #826.
//!
//! The drop happens inside the bus's stopping pump (an aborted task drops its
//! channel), which is before the daemon's audit-sink drain, so the row is one
//! the drain waits for.

use tokio::sync::mpsc as tok_mpsc;

use super::inbound_drop::{report_inbound_dropped, InboundDropStage};
use super::InboundDroppedAudit;
use crate::channel::{ChannelId, IncomingMessage};

/// How many times [`drain_unread`] looks again at a buffer that is closed but
/// not yet settled: a slot granted before the close is still held. Usually
/// that is a send on its way in, a few instructions on the driver's thread.
/// But on a full buffer the slot may just have been handed to the driver
/// while it waited in `blocking_send`; then the driver has to be scheduled
/// before it finds the buffer closed and gives the slot back, its send
/// refused — a wake-up, which a loaded host can delay past the spin. A short
/// spin of `yield_now`s covers the common case; the bound keeps a `Drop` from
/// ever spinning on a thread that is not coming back. (It bounds a held slot
/// only: tokio's own `try_recv` also waits, unbounded, on a send that has
/// claimed its place in the list but not yet written it — a few atomic
/// operations on the sending thread.)
const SETTLE_SPINS: usize = 1_000;

/// The receiving end of a polled driver's inbound buffer, as a channel holds
/// it. Reads exactly like a tokio `Receiver`; on drop it closes the buffer and
/// reports what was still in it (see the module docs).
pub struct InboundReceiver {
    rx: tok_mpsc::Receiver<IncomingMessage>,
    /// The driver's log label, for the drop line.
    label: &'static str,
    channel: ChannelId,
    /// Whether the channel acks what it hands over (email): only changes the
    /// line's reason for "not redelivered".
    acks: bool,
    /// A clone of the driver's hook, if it has one.
    audit: Option<InboundDroppedAudit>,
}

impl InboundReceiver {
    /// Wrap the receiving end `rx` of channel `channel`'s inbound buffer.
    pub(super) fn new(
        rx: tok_mpsc::Receiver<IncomingMessage>,
        label: &'static str,
        channel: ChannelId,
        acks: bool,
        audit: Option<InboundDroppedAudit>,
    ) -> Self {
        Self { rx, label, channel, acks, audit }
    }

    /// The next inbound message, or `None` once the driver has exited and the
    /// buffer is empty. Cancellation-safe, as a tokio `Receiver::recv` is: a
    /// dropped future leaves any buffered message for the next call.
    pub async fn recv(&mut self) -> Option<IncomingMessage> {
        self.rx.recv().await
    }

    /// Test builds only: [`Self::recv`] from a non-async test thread.
    #[cfg(test)]
    pub(crate) fn blocking_recv(&mut self) -> Option<IncomingMessage> {
        self.rx.blocking_recv()
    }

    /// Test builds only: a non-blocking read.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Result<IncomingMessage, tok_mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }

    /// Test builds only: how many messages are buffered unread.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rx.len()
    }
}

impl Drop for InboundReceiver {
    fn drop(&mut self) {
        let drained = drain_unread(&mut self.rx);
        if !drained.settled {
            // Diagnostic only: the count below already includes the slot.
            tracing::warn!(
                label = self.label,
                "a slot granted before the inbound buffer closed was still held after \
                 {SETTLE_SPINS} spins; counting its message as lost (if the driver's send was \
                 refused instead, the driver reports it too, or the channel redelivers it)"
            );
        }
        report_inbound_dropped(
            self.label,
            &self.channel,
            InboundDropStage::Buffered,
            drained.lost(),
            self.acks,
            self.audit.as_ref(),
        );
    }
}

/// What [`drain_unread`] found in a closed buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Drained {
    /// Messages taken out of the buffer and discarded.
    pub(super) unread: usize,
    /// Whether the buffer settled: closed, empty, and no slot still held.
    /// `false` only when a slot granted before the close was still held after
    /// [`SETTLE_SPINS`] looks.
    pub(super) settled: bool,
}

impl Drained {
    /// Pure: how many messages the stop lost, counting an unsettled slot as
    /// one. There is at most one such slot: the driver's thread is the
    /// buffer's only sender (`PolledWorkerDriver::spawn` moves the sole
    /// `Sender` into `run`; never clone it), and it sends one message at a
    /// time. Its message either lands after the drain, in a buffer about to
    /// be dropped — lost — or, when the slot was handed to a driver waiting
    /// on a full buffer, its send is refused, and the driver reports it at the
    /// polled stage (Matrix) or the worker redelivers it (email). The drain
    /// cannot tell which, so it counts it: over-counting is the honest
    /// direction (`inbound_drop`'s module docs).
    pub(super) fn lost(self) -> usize {
        self.unread + usize::from(!self.settled)
    }
}

/// Close `rx` and count what was still in it, discarding each value.
///
/// Closing first stops the driver adding more while the count is taken: its
/// next send fails, and the driver reports what it still holds itself, at the
/// [`InboundDropStage::Polled`] stage. One send can still land after the
/// close, if it had been granted its slot just before; `try_recv` reports
/// `Empty` (not `Disconnected`) while any slot is held, so the loop waits for
/// it — at most [`SETTLE_SPINS`] times, after which it says the buffer did not
/// settle ([`Drained::settled`]) rather than return a count that silently
/// misses it. A held slot can also end in a refused send rather than a landed
/// one (see [`SETTLE_SPINS`]); waiting tells the two apart, running out of
/// spins does not.
///
/// Relies on tokio ≥ 1.52.3's `try_recv`: after `close()`, `Empty` while a
/// slot is held and `Disconnected` once none is, even with the sender alive
/// (the workspace pins that floor; `tests/inbound_rx.rs` pins the behaviour).
pub(super) fn drain_unread<T>(rx: &mut tok_mpsc::Receiver<T>) -> Drained {
    rx.close();
    let mut unread = 0;
    let mut spins = 0;
    loop {
        match rx.try_recv() {
            Ok(_) => unread += 1,
            Err(tok_mpsc::error::TryRecvError::Disconnected) => {
                return Drained { unread, settled: true }
            }
            Err(tok_mpsc::error::TryRecvError::Empty) if spins < SETTLE_SPINS => {
                spins += 1;
                std::thread::yield_now();
            }
            Err(tok_mpsc::error::TryRecvError::Empty) => return Drained { unread, settled: false },
        }
    }
}
