//! The vocabulary the supervisor and its callers share: what one bring-up
//! attempt produced, what gets recorded about it, and the handle to a channel
//! that is running.
//!
//! Split out of [`super`] to keep the retry loop and the types it moves around
//! separately readable (and the parent file under the 500-line cap). Nothing
//! here makes a policy decision — the loop does that.

use std::future::Future;

use futures::future::BoxFuture;

use crate::channel::ChannelBus;

/// A running channel, plus the one thing the supervisor ever does to it: stop
/// it.
///
/// Deliberately opaque. The supervisor never names [`ChannelBus`], which keeps
/// the retry policy independent of the channel layer and lets a test hand it a
/// probe that records whether shutdown ran.
pub struct StartedChannel {
    /// Boxed because the supervisor stores one of these across an await and
    /// must not be generic over the channel type to do it. `FnOnce` because
    /// stopping twice is not a thing that should be expressible.
    shutdown: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>,
    /// Completes when the channel has stopped working on its own (#517).
    /// Defaults to a future that never completes, so a caller that has no
    /// liveness signal keeps the old park-until-shutdown behaviour exactly.
    died: BoxFuture<'static, ()>,
}

impl StartedChannel {
    /// Wrap anything whose shutdown is an async, by-value call.
    ///
    /// The result reports no liveness: it is treated as alive until the daemon
    /// shuts down. Add a signal with [`with_death`](Self::with_death).
    pub fn new<F, Fut>(shutdown: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            shutdown: Box::new(move || Box::pin(shutdown())),
            died: Box::pin(std::future::pending()),
        }
    }

    /// Attach a liveness signal: a future that completes once the channel has
    /// stopped working (#517).
    ///
    /// Separate from [`new`](Self::new) so that "I cannot tell you when I die"
    /// stays expressible and stays the default — a channel that silently never
    /// reports is strictly better than one whose signal is wrong, since a
    /// spurious death costs a full restart.
    pub fn with_death(mut self, died: BoxFuture<'static, ()>) -> Self {
        self.died = died;
        self
    }

    /// The production case: a running [`ChannelBus`].
    ///
    /// The death signal is taken **before** the bus moves into the shutdown
    /// closure — it outlives the borrow precisely because
    /// [`ChannelBus::death_signal`] hands back a `'static` future.
    pub fn from_bus(bus: ChannelBus) -> Self {
        let died = bus.death_signal();
        // The panicked-pump count is already logged by `shutdown` itself.
        Self::new(move || async move {
            bus.shutdown().await;
        })
        .with_death(died)
    }

    /// Wait until the channel reports that it has stopped working.
    ///
    /// Borrows rather than consumes, so the caller can still
    /// [`stop`](Self::stop) it afterwards — which it must: a channel that lost
    /// one pump still has the others running, and the per-channel task's drop
    /// is what tears the worker and its sidecar down.
    pub(super) async fn wait_for_death(&mut self) {
        (&mut self.died).await;
    }

    /// Stop the channel. Consuming, so it cannot run twice.
    pub(super) async fn stop(self) {
        (self.shutdown)().await;
    }
}

impl std::fmt::Debug for StartedChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The payload is a closure; there is nothing useful to render, and a
        // `Debug` bound is needed only so `BootOutcome` can derive it.
        f.write_str("StartedChannel")
    }
}

/// What one bring-up attempt produced.
///
/// The distinction that carries the weight is [`Retry`](Self::Retry) versus
/// [`Fatal`](Self::Fatal), and the question to ask is: *could a later attempt
/// succeed with the same process environment?* A refused sandbox cgroup, an
/// unreachable homeserver and a LISTEN/NOTIFY hiccup are all yes. A missing or
/// malformed environment variable is no — the environment cannot change under
/// a running daemon, so the honest response is a loud line telling the
/// operator to fix it and restart.
#[derive(Debug)]
pub enum BootOutcome {
    /// The channel is not configured. Stop, and say nothing: this is the
    /// default for every deployment that does not use this channel.
    NotConfigured,
    /// The channel is up.
    Started(StartedChannel),
    /// Failed in a way a later attempt could plausibly absorb.
    Retry(anyhow::Error),
    /// Failed in a way no retry can fix.
    Fatal(anyhow::Error),
}

/// One durable record of a bring-up event.
#[derive(Debug, Clone)]
pub enum BootAudit {
    /// The channel came up after `attempts` total attempts (`1` = first try).
    Started { attempts: u32 },
    /// An attempt failed. `retry_in_ms` is `None` exactly when `fatal` is
    /// `true` — there is no next attempt to schedule.
    Failed { attempt: u32, retry_in_ms: Option<u64>, fatal: bool, cause: String },
    /// A channel that **was** running stopped working on its own (#517), and
    /// will be brought back up after `retry_in_ms`.
    ///
    /// Deliberately not a [`Failed`](Self::Failed): "never came up" and "was up
    /// for six hours, then died" are different events with different causes and
    /// different operator responses, and collapsing them would discard the only
    /// durable evidence that the channel ever worked. `ran_ms` is that
    /// evidence — it also separates a genuine outage from a flap.
    Died { ran_ms: u64, retry_in_ms: u64 },
}

/// Where [`BootAudit`] records go. A boxed closure rather than a trait so the
/// supervisor stays database-free and a test can record into a `Vec`;
/// production is [`pg_sink::pg_boot_audit_sink`].
///
/// `Sync` as well as `Send`: the loop holds a *reference* to the sink across
/// the await that writes the row, and a `&T` only crosses threads when `T` is
/// `Sync`. Both real implementations capture `Arc`/`PgPool`/`String`, so this
/// costs nothing.
pub type BootAuditSink = Box<dyn Fn(BootAudit) -> BoxFuture<'static, ()> + Send + Sync>;
