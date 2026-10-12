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
//! accepts it, not once the bus reads it (`polled_driver::run`). So
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
/// not yet settled — a send the driver had already been granted a slot for
/// when the buffer closed, still on its way in. That send is a few
/// instructions on the driver's thread, so a short spin of `yield_now`s covers
/// it; the bound keeps a `Drop` from ever spinning on a thread that is not
/// coming back.
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
        let unread = drain_unread(&mut self.rx);
        report_inbound_dropped(
            self.label,
            &self.channel,
            InboundDropStage::Buffered,
            unread,
            self.acks,
            self.audit.as_ref(),
        );
    }
}

/// Close `rx` and count what was still in it, discarding each value.
///
/// Closing first stops the driver adding more while the count is taken: its
/// next send fails, and the driver reports what it still holds itself, at the
/// [`InboundDropStage::Polled`] stage. One send can still land after the
/// close, if it had been granted its slot just before; `try_recv` reports
/// `Empty` (not `Disconnected`) until it has, so the loop waits for it —
/// at most [`SETTLE_SPINS`] times, after which the count is a lower bound.
pub(super) fn drain_unread<T>(rx: &mut tok_mpsc::Receiver<T>) -> usize {
    rx.close();
    let mut unread = 0;
    let mut spins = 0;
    loop {
        match rx.try_recv() {
            Ok(_) => unread += 1,
            Err(tok_mpsc::error::TryRecvError::Disconnected) => return unread,
            Err(tok_mpsc::error::TryRecvError::Empty) if spins < SETTLE_SPINS => {
                spins += 1;
                std::thread::yield_now();
            }
            Err(tok_mpsc::error::TryRecvError::Empty) => return unread,
        }
    }
}

