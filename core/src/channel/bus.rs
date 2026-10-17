//! The channel bus runtime: one pump per channel (inbound: recv → classify →
//! audit + enqueue; outbound: its queue → claim → send) and one completed-task
//! pump (NOTIFY or backlog sweep → route → that channel's queue). Since #832
//! a reply is claimed where it is sent, not where it is routed.
//! All DB access is behind two seams so the pumps are testable without Postgres.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info};

use kastellan_db::tasks::Lane;

use super::auth::PeerAuthorizer;
use super::catch_up::Via;
use super::pump_liveness::DeathBell;
use super::{Channel, ChannelId, PeerId};

/// Body of the reply sent back when a peer pairs successfully.
pub const PAIRED_ACK_BODY: &str = "\u{2713} Paired \u{2014} you can now message me.";

/// Inbound side-effects seam: enqueue a task + write audit rows. Real impl wraps
/// `kastellan_db::{tasks::insert_pending, audit::insert}`; the fake records calls.
#[async_trait::async_trait]
pub trait ChannelEvents: Send + Sync {
    /// Enqueue a channel task; returns its id.
    async fn enqueue(&self, lane: Lane, payload: Value) -> anyhow::Result<i64>;
    /// Best-effort audit row: never fatal. The production impl,
    /// `PgChannelEvents`, reports a failed insert on the `[audit-lost]`
    /// marker (#808) — and one whose future is dropped mid-insert, as
    /// `shutdown`'s abort does to a pump parked here (#813). `action` is
    /// `'static` — one of [`actions`](super::actions)' constants — because
    /// the lost-row count is kept per action (#865): no runtime string can
    /// grow it.
    async fn audit(&self, action: &'static str, payload: Value);
    /// Say that work was lost without a row — a stop landing mid-flight
    /// (#832, `bus_guards`). Sync, because it is called from `Drop`. The
    /// default is the bus's own `[audit-lost]` writer; `PgChannelEvents`
    /// routes it through its reporter so a test can read it.
    fn report_lost(&self, line: &str) {
        crate::worker_stderr::emit_audit_lost_report(crate::worker_stderr::AuditLostWriter::Bus, line);
    }
}

/// The reply claim's types, re-exported so `CompletedTasks` implementors in
/// other crates' tests need no direct `kastellan-db` path.
pub use kastellan_db::tasks::reply_claim::{ClaimedReply, ReplyDisposition};

/// Outbound source seam: a stream of completed task ids, a reader for the
/// row, and — since #825 — the reply claim and the backlog it settles.
#[async_trait::async_trait]
pub trait CompletedTasks: Send + Sync {
    /// Next completed task id, or `None` when the stream ends. Called by the
    /// completed-task pump only; `&self` (since #832) so the channels' pumps
    /// can share the seam for their claims.
    async fn next_completed(&self) -> Option<i64>;
    /// Fetch `(payload, result)` for a task id, or `None` if absent.
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>>;
    /// Settle `id`'s reply as `d`. `Ok(None)`: already settled, or not
    /// finished — either way, not this caller's to send. See
    /// `kastellan_db::tasks::reply_claim::claim_reply`.
    async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>>;
    /// Up to `limit` unsettled finished channel task ids above `after_id`,
    /// ascending.
    async fn unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>>;
    /// Whether `id`'s reply is settled — asked only after `claim` returned an
    /// error, which a real claim can do *after* committing. See
    /// `kastellan_db::tasks::reply_claim::reply_settled`.
    ///
    /// The default, `false`, is right for an implementation whose failed
    /// claim never commits — every in-memory fake. `PgCompletedTasks`
    /// overrides it.
    async fn settled(&self, _id: i64) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// Pairing carve-out seam: consulted **only** for authorizer-rejected peers, and
/// only ever compares the body against an operator-issued single-use code — never
/// interprets it, never reaches the agent. See the slice-#3 design's security
/// analysis. Real impl: `channel::pairing::DbPairingService`.
#[async_trait::async_trait]
pub trait PairingService: Send + Sync {
    async fn try_pair(&self, channel: &ChannelId, peer: &PeerId, body: &str) -> PairingOutcome;
}

/// Outcome of a pairing attempt by an unpaired peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingOutcome {
    /// The body matched an active code; the peer is now bound.
    Paired,
    /// No active code, or the body didn't match one — treat as a normal
    /// (dropped) unpaired message.
    NotAPairingAttempt,
}

/// Resolution seam for an answer arriving over a channel.
///
/// A trait because the real implementation needs a `PgPool` and this
/// module's tests are deliberately PG-free (spec D12). Its counterpart
/// [`super::outbox::ChannelOutbox`] gets no trait: the real registry with a
/// drained receiver *is* the perfect fake, so wrapping it would only stop
/// the tests covering the real thing.
#[async_trait::async_trait]
pub trait AskResolver: Send + Sync {
    /// Resolve the ask the nonce correlates to, if `claimant` owns its task.
    /// `Ok(None)` covers every refusal, indistinguishably.
    async fn resolve(
        &self,
        nonce: &kastellan_db::asks::Nonce,
        choice: &str,
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<Option<kastellan_db::asks::ResolvedAsk>>;

    /// Does any of `nonces` hash to a live ask owned by `claimant`'s own
    /// task? The containment question, asked before a body that mentions
    /// an ask verb is allowed anywhere near `screen_and_classify`.
    ///
    /// On the trait rather than reached for directly because
    /// `bus/tests.rs` is deliberately PG-free (spec D12) — and because an
    /// `Err` here must stay distinguishable from `Ok(false)`: the two take
    /// **opposite** arms, since an unanswered question refuses while a
    /// definite "no" lets the body continue to the remaining arms (where
    /// `is_command_shaped` may still refuse it). Collapsing them into a
    /// `bool` would pick the wrong arm on every database hiccup, silently.
    async fn any_live_nonce(
        &self,
        nonces: &[kastellan_db::asks::Nonce],
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<bool>;
}

/// Real DB-backed `AskResolver`.
pub struct PgAskResolver {
    pool: sqlx::PgPool,
}

impl PgAskResolver {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl AskResolver for PgAskResolver {
    async fn resolve(
        &self,
        nonce: &kastellan_db::asks::Nonce,
        choice: &str,
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<Option<kastellan_db::asks::ResolvedAsk>> {
        Ok(kastellan_db::asks::resolve_with_nonce(
            &self.pool,
            nonce,
            claimant,
            // Built by `db::asks` rather than spelled here: this document's
            // key has to match what `reject_choice_outside_options`
            // validates and what `scheduler::asks::resolution_choice`
            // reads, and those live in two other places.
            &kastellan_db::asks::resolution(choice, None),
        )
        .await?)
    }

    async fn any_live_nonce(
        &self,
        nonces: &[kastellan_db::asks::Nonce],
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<bool> {
        Ok(kastellan_db::asks::any_live_nonce_for_claimant(&self.pool, nonces, claimant).await?)
    }
}

/// Everything a bus needs to take part in the operator-ask loop: the
/// registry it publishes its outbound queue into, and the resolver it hands
/// answers to. `None` at `spawn` means this bus does neither — but
/// containment still applies: without a resolver the exact question cannot
/// be asked, so D7's no-wiring fallback refuses every body the broad
/// predicate matches, audited `unscannable`. That is deliberately NOT the
/// pre-#564-slice-2 behaviour; see `bus_inbound::containment_refusal`.
pub struct AskWiring {
    pub outbox: Arc<super::outbox::ChannelOutbox>,
    pub resolver: Arc<dyn AskResolver>,
}

/// Real DB-backed `ChannelEvents`; its own module since #808, which made a
/// failed audit insert an `[audit-lost]` report. Since #829 also the
/// shutdown's report of the lost rows its lines left out, for `main`.
pub use super::pg_events::{report_unsaid_lost_rows, PgChannelEvents};

/// The inbound path; its own module since #824 (`bus.rs` was already over the
/// 500-LOC soft cap, and #815 was about to grow it).
pub use super::bus_inbound::handle_inbound;

/// The outbound half — `handle_completed`, the catch-up `sweep`,
/// `PgCompletedTasks` and the transport-failure row — in its own module since
/// #825.
pub use super::bus_outbound::{handle_completed, sweep, PgCompletedTasks};
use super::bus_guards::{InFlight, InFlightGuard};
use super::bus_outbound::send_or_record;

/// What a channel's queue carries, and the per-channel pump's delivery of
/// it — the claim at the send (#832).
pub use super::bus_deliver::{deliver, note_claim, Delivery, Queued};

/// A running bus. Owns the spawned pump tasks; `shutdown()` aborts them.
pub struct ChannelBus {
    /// Each pump, with what `shutdown` calls it if it panicked: its channel's
    /// id, or `completed-task`.
    handles: Vec<(String, JoinHandle<()>)>,
    /// Rung by whichever pump ends first. Read by the channel supervisor
    /// through [`death_signal`](Self::death_signal) — see [`DeathBell`] for why
    /// the bus reports its own death rather than being polled for liveness.
    bell: DeathBell,
    /// Kept so `shutdown` can deregister; also keeps the wiring alive for
    /// the bus's lifetime.
    asks: Option<Arc<AskWiring>>,
    /// The ids registered into the outbox, so shutdown removes exactly what
    /// spawn added.
    registered: Vec<ChannelId>,
}

impl ChannelBus {
    /// Spawn one inbound/outbound pump per channel + one completed-task pump. Each
    /// per-channel task owns its `Channel` and `select!`s `recv()` (inbound)
    /// against an mpsc bridge carrying replies (outbound `send`), so the single
    /// `&mut Channel` owner does both and there is no cross-task contention.
    pub fn spawn(
        channels: Vec<Box<dyn Channel>>,
        authorizer: Arc<dyn PeerAuthorizer>,
        pairing: Option<Arc<dyn PairingService>>,
        events: Arc<dyn ChannelEvents>,
        completed: Arc<dyn CompletedTasks>,
        asks: Option<Arc<AskWiring>>,
    ) -> Self {
        let mut handles = Vec::new();
        let mut senders: HashMap<ChannelId, mpsc::Sender<Queued>> = HashMap::new();
        let mut registered = Vec::new();
        // Every pump below takes a guard off this bell. Each of them has at
        // least one terminal exit — a `break`, a `while let` that ends, a panic
        // — and before #517 all of them were silent: the bus kept looking
        // healthy while nothing pumped. The guard is held, never called, so no
        // pump has to remember to report exits it does not know it has.
        let bell = DeathBell::new();

        for mut ch in channels {
            let id = ch.id();
            // The receiving end is owned by a `PumpQueue`: whatever is still
            // queued when this pump ends is counted (replies, recoverable)
            // or said (asks) (#832).
            let (tx, mut queue) = super::bus_guards::PumpQueue::channel(32, events.clone(), id.clone());
            senders.insert(id.clone(), tx.clone());

            // Publish this channel's reply queue so core-initiated messages
            // (a raised ask) go through the same pump replies do — one queue
            // per channel, no second delivery path.
            if let Some(w) = &asks {
                w.outbox.register(id.clone(), tx.clone());
                registered.push(id.clone());
            }

            let authorizer = authorizer.clone();
            let pairing = pairing.clone();
            let events = events.clone();
            let asks_for_pump = asks.clone();
            let completed = completed.clone();
            let life = bell.guard();
            handles.push((id.0.clone(), tokio::spawn(async move {
                let _life = life;
                // Consecutive failed reply claims: the claim happens here
                // since #832, so this is where a database refusing it shows.
                let mut claims = super::catch_up::ClaimStreak::default();
                loop {
                    tokio::select! {
                        inbound = ch.recv() => match inbound {
                            Some(msg) => {
                                // The transport has handed this message over;
                                // a stop from here to the ack's send would
                                // lose it with no row (#832).
                                let guard = InFlightGuard::arm(&*events, InFlight::Inbound, Some(&msg.channel), Some(&msg.peer));
                                if let Some(ack) = handle_inbound(
                                    &*authorizer,
                                    pairing.as_deref(),
                                    asks_for_pump.as_deref(),
                                    &*events,
                                    &msg,
                                )
                                .await
                                {
                                    // Any ack `handle_inbound` returns, not
                                    // just the pairing one: since #564
                                    // slice 2 this also carries "✓
                                    // Approved", "✗ not answerable" and the
                                    // malformed-command usage line. An
                                    // operator whose approval landed but
                                    // whose confirmation never arrived
                                    // should not be reading a log line that
                                    // says "pairing" — nor finding no row at
                                    // all: until #824 a refused ack was a
                                    // WARN only, and `EmailChannel::send`
                                    // refuses every one.
                                    send_or_record(&*ch, &*events, &id, ack, "inbound ack send failed").await;
                                }
                                guard.settle();
                            }
                            None => { info!(channel = %id.0, "inbound closed"); break; }
                        },
                        Some(item) = queue.recv() => {
                            // TWO producers feed this queue (see `Queued`): a
                            // finished task's reply from `handle_completed`,
                            // queued unclaimed — `deliver` claims it, writes
                            // `channel.replied` ("routed", see
                            // `actions::REPLIED`) and sends it; and a
                            // core-initiated message from the `ChannelOutbox`
                            // (#564 slice 2), i.e. a raised ask, which has
                            // `ask.delivered` behind it and NO
                            // `channel.replied` — which is why a
                            // `channel.reply_undelivered` can be an orphan.
                            //
                            // The transport attempt is in `deliver` either
                            // way, through `send_or_record`, so a refusal
                            // leaves its own durable row rather than an audit
                            // trail asserting a delivery that never happened
                            // (`EmailChannel::send` still always fails).
                            let d = deliver(&*completed, &*events, &*ch, &id, item).await;
                            if let Some(n) = note_claim(&mut claims, d, std::time::Instant::now()) {
                                error!(
                                    channel = %id.0, consecutive = n,
                                    "reply claims keep failing; finished tasks' replies are not going out"
                                );
                            }
                        }
                    }
                }
            })));
        }

        // Completed-task pump: sweep the backlog, then NOTIFY → load → route →
        // push, unclaimed, into the per-channel queue, re-sweeping every
        // `SWEEP_EVERY` (#825); the channel's pump claims at the send (#832).
        // The start sweep needs no ordering care: the production `completed`
        // is a `PgCompletedTasks`, whose `connect()` established its LISTEN
        // before `spawn` was called, so a task finishing during the sweep is
        // announced afterwards, and the claim drops the duplicate.
        let events_out = events.clone();
        let life = bell.guard();
        handles.push(("completed-task".to_string(), tokio::spawn(async move {
            let _life = life;
            // A failed sweep is a WARN and the pump carries on; a run of
            // them is an ERROR, since replies are then piling up unseen.
            let mut streak = super::catch_up::Streak::default();
            let mut note = move |report: super::catch_up::SweepReport| {
                if let Some(n) = streak.record(report.is_failure()) {
                    error!(
                        consecutive = n,
                        "reply catch-up keeps failing; finished tasks' replies are not going out"
                    );
                }
            };
            note(sweep(&*completed, &*events_out, &senders).await);
            let every = super::catch_up::SWEEP_EVERY;
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // A tick that wins this race drops an in-flight
                // `next_completed`, and sqlx does not document
                // `PgListener::recv` as cancel-safe. A NOTIFY lost that way
                // is still not a lost reply: its task stays unsettled, and
                // the very sweep that won the race finds it.
                tokio::select! {
                    next = completed.next_completed() => match next {
                        Some(id) => {
                            handle_completed(&*completed, &*events_out, &senders, id, Via::Notify).await;
                        }
                        None => break,
                    },
                    _ = tick.tick() => note(sweep(&*completed, &*events_out, &senders).await),
                }
            }
            info!("outbound pump stopped");
        })));

        Self { handles, bell, asks, registered }
    }

    /// A future that completes as soon as **any** pump task has ended — by
    /// returning, by panicking, or by being aborted.
    ///
    /// This is what turns "the channel is up" from a one-time observation into
    /// a supervised claim (#517). Every pump has a terminal exit that nothing
    /// used to watch: `next_completed` returning `None` (replies stop going
    /// out), a per-channel task's `break` on a closed `recv` (inbound stops
    /// coming in), or a panic in either. All three leave the daemon looking
    /// perfectly healthy — the units are `active`, Postgres is fine, and the
    /// log is quiet because there is nothing left to log. That is #514's
    /// signature reached after boot instead of during it, which is why the
    /// answer is the same one: hand it to the supervisor and let it restart.
    ///
    /// Deliberately **not** "which pump died". A dead pump means a degraded
    /// channel whatever its name, and the recovery — stop the bus, bring the
    /// channel back up — is identical either way.
    ///
    /// `'static`, so the supervisor can hold it across awaits while also owning
    /// the bus it is about to stop.
    pub fn death_signal(&self) -> futures::future::BoxFuture<'static, ()> {
        self.bell.signal()
    }

    /// Abort all pump tasks (called on daemon shutdown), then join them so any
    /// resources they hold are released before the caller proceeds. Every
    /// pump holds a clone of the `Arc<dyn CompletedTasks>` (since #832), whose
    /// `PgCompletedTasks` owns a `PgListener` — a checked-out pool connection
    /// that sqlx 0.9 only releases when the listener is dropped, i.e. when the
    /// last pump is. The daemon's shutdown closes the pool right after, and
    /// `Pool::close()` blocks until every connection is returned, so aborting
    /// without joining would leave that release racing the pool close. Mirrors
    /// the scheduler/audit-mirror shutdowns, which signal-then-join for the
    /// same reason.
    ///
    /// Returns how many pumps had **panicked** (each logged at ERROR with its
    /// name, #832): the join used to discard that, leaving only the death
    /// signal. Always 0 in a release build, which is `panic = "abort"`.
    /// Callers that only stop the bus can ignore it.
    pub async fn shutdown(self) -> usize {
        // Stop being a delivery target first: an ask queued after this point
        // would go into a channel whose pump is about to be aborted, which
        // is a message that vanishes rather than one that fails.
        if let Some(w) = &self.asks {
            for id in &self.registered {
                w.outbox.deregister(id);
            }
        }
        for (_, h) in &self.handles {
            h.abort();
        }
        let mut panicked = 0;
        for (pump, h) in self.handles {
            // An aborted pump joins as a cancellation — the normal case here.
            if let Err(e) = h.await {
                if e.is_panic() {
                    error!(pump = %pump, error = %e, "channel bus pump panicked");
                    panicked += 1;
                }
            }
        }
        panicked
    }
}

#[cfg(test)]
mod tests;
