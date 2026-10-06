//! The channel bus runtime: an inbound pump per channel (recv → classify →
//! audit + enqueue) and one outbound pump (completed-task NOTIFY → route → send).
//! All DB access is behind two seams so the pumps are testable without Postgres.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use kastellan_db::tasks::{self, Lane};

use super::auth::PeerAuthorizer;
use super::pump_liveness::DeathBell;
use super::route::reply_for_completed_task;
use super::{actions, Channel, ChannelId, OutgoingMessage, PeerId};

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
    /// `shutdown`'s abort does to a pump parked here (#813).
    async fn audit(&self, action: &str, payload: Value);
}

/// Outbound source seam: a stream of completed task ids + a reader for the row.
#[async_trait::async_trait]
pub trait CompletedTasks: Send + Sync {
    /// Next completed task id, or `None` when the stream ends.
    async fn next_completed(&mut self) -> Option<i64>;
    /// Fetch `(payload, result)` for a task id, or `None` if absent.
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>>;
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
/// failed audit insert an `[audit-lost]` report.
pub use super::pg_events::PgChannelEvents;

/// The inbound path; its own module since #824 (`bus.rs` was already over the
/// 500-LOC soft cap, and #815 was about to grow it).
pub use super::bus_inbound::handle_inbound;

/// Real `CompletedTasks` over a `PgListener` on `tasks_completed` + `tasks::get`.
/// Construct via [`PgCompletedTasks::connect`].
pub struct PgCompletedTasks {
    listener: sqlx::postgres::PgListener,
    pool: sqlx::PgPool,
}
impl PgCompletedTasks {
    pub async fn connect(pool: sqlx::PgPool) -> anyhow::Result<Self> {
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;
        listener.listen("tasks_completed").await?;
        Ok(Self { listener, pool })
    }
}
#[async_trait::async_trait]
impl CompletedTasks for PgCompletedTasks {
    async fn next_completed(&mut self) -> Option<i64> {
        loop {
            match self.listener.recv().await {
                Ok(n) => {
                    if let Ok(id) = n.payload().parse::<i64>() {
                        return Some(id);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "tasks_completed listener error; stopping outbound pump");
                    return None;
                }
            }
        }
    }
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        Ok(tasks::get(&self.pool, id).await?.map(|t| (t.payload, t.result)))
    }
}

/// `ch.send(out)` — and when the transport refuses it, a WARN and a
/// `channel.reply_undelivered` row (`send_failed`), so the audit trail does
/// not assert a delivery that never happened. Every message a per-channel pump
/// hands its transport goes through here: a reply, a raised ask, an inbound
/// ack. `what` is the WARN's message, so the log still says which.
///
/// The row is the same view a polled driver's sink is handed, so it has one
/// definition (`UndeliveredReply::payload`, #800); the error stays in the log,
/// since it is transport text, not a fixed label, and the body is never
/// persisted.
async fn send_or_record(
    ch: &dyn Channel,
    events: &dyn ChannelEvents,
    id: &ChannelId,
    out: OutgoingMessage,
    what: &'static str,
) {
    let (peer, conversation) = (out.peer.clone(), out.conversation.clone());
    if let Err(e) = ch.send(out).await {
        warn!(channel = %id.0, error = %e, "{what}");
        let reply = super::UndeliveredReply {
            channel: id,
            peer: &peer,
            conversation: &conversation,
            reason: super::UndeliveredReason::SendFailed,
            observed_at: time::OffsetDateTime::now_utc(),
        };
        events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
    }
}

/// Handle one completed-task id on the outbound side: load it, route it (pure),
/// and `send` via the matching channel. `senders` maps `ChannelId` → an outbound
/// `send` handle. Returns the `OutgoingMessage` actually sent (for tests).
pub async fn handle_completed(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
    id: i64,
) -> Option<OutgoingMessage> {
    let (payload, result) = match completed.load(id).await {
        Ok(Some(pr)) => pr,
        Ok(None) => return None, // rolled back between NOTIFY and SELECT — benign
        Err(e) => {
            warn!(task_id = id, error = %e, "outbound load failed");
            return None;
        }
    };
    let Some(out) = reply_for_completed_task(&payload, result.as_ref()) else {
        // `None` is the normal answer for a completion that is not a channel
        // task (an `ask`/`l3_run`). A channel task with no routing metadata
        // is a reply nobody can deliver, and before #824 it went without a
        // word. A WARN, not a row: there is no channel or peer to put in one,
        // and both buses see every NOTIFY (#497), so expect one line per bus.
        if payload.get("kind").and_then(Value::as_str) == Some("channel") {
            warn!(task_id = id, "channel task has no routing metadata; reply dropped");
        }
        return None;
    };
    let Some(tx) = senders.get(&out.channel) else {
        // NOT a warning: the daemon runs one `ChannelBus` per channel family
        // (`main.rs` spawns a Matrix bus and an email bus), and every bus's
        // completed-task pump sees EVERY completed channel task via
        // LISTEN/NOTIFY. So "this reply isn't for a channel I serve" is the
        // normal case on each reply — the other bus is handling it — and
        // logging it at `warn` fired a misleading "dropping" line for every
        // successfully delivered reply, which is exactly how an operator learns
        // to ignore warnings (review finding). Unifying the buses would remove
        // the ambiguity outright and let this go back to being a real `warn!`
        // (it would then mean "nothing serves this channel at all") — #497.
        debug!(channel = %out.channel.0, "reply is for a channel this bus does not serve; ignoring");
        return None;
    };
    if let Err(mpsc::error::SendError(dropped)) = tx.send(out.clone()).await {
        // The queue's only receiver is this channel's pump, so a closed queue
        // means the pump has ended. Before #815 this was a WARN and nothing
        // else: no row, so `channel.replied`/`channel.reply_undelivered` no
        // longer accounted for every reply. No `channel.replied` either — the
        // reply was never routed.
        warn!(channel = %dropped.channel.0, "outbound send queue closed; reply dropped");
        let reply = super::UndeliveredReply::of(
            &dropped,
            super::UndeliveredReason::QueueClosed,
            time::OffsetDateTime::now_utc(),
        );
        events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
        return None;
    }
    events
        .audit(
            actions::REPLIED,
            serde_json::json!({"task_id": id, "channel": out.channel.0, "peer": out.peer.0}),
        )
        .await;
    Some(out)
}

/// A running bus. Owns the spawned pump tasks; `shutdown()` aborts them.
pub struct ChannelBus {
    handles: Vec<JoinHandle<()>>,
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
        mut completed: Box<dyn CompletedTasks>,
        asks: Option<Arc<AskWiring>>,
    ) -> Self {
        let mut handles = Vec::new();
        let mut senders: HashMap<ChannelId, mpsc::Sender<OutgoingMessage>> = HashMap::new();
        let mut registered = Vec::new();
        // Every pump below takes a guard off this bell. Each of them has at
        // least one terminal exit — a `break`, a `while let` that ends, a panic
        // — and before #517 all of them were silent: the bus kept looking
        // healthy while nothing pumped. The guard is held, never called, so no
        // pump has to remember to report exits it does not know it has.
        let bell = DeathBell::new();

        for mut ch in channels {
            let id = ch.id();
            let (tx, mut rx) = mpsc::channel::<OutgoingMessage>(32);
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
            let life = bell.guard();
            handles.push(tokio::spawn(async move {
                let _life = life;
                loop {
                    tokio::select! {
                        inbound = ch.recv() => match inbound {
                            Some(msg) => {
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
                            }
                            None => { info!(channel = %id.0, "inbound closed"); break; }
                        },
                        Some(out) = rx.recv() => {
                            // TWO producers feed this `rx`, and they leave
                            // different rows behind:
                            //   - a completed-task reply from
                            //     `handle_completed`, which already wrote
                            //     `channel.replied` when it queued this —
                            //     that row means "routed", not "delivered"
                            //     (see `actions::REPLIED`);
                            //   - a core-initiated message from the
                            //     `ChannelOutbox` (#564 slice 2), i.e. a
                            //     raised ask, which has `ask.delivered`
                            //     behind it instead and NO `channel.replied`.
                            // Until slice 2 the first was the only producer,
                            // so "already wrote `channel.replied`" read as an
                            // invariant; it is not one any more, which is why
                            // a `channel.reply_undelivered` can now be an
                            // orphan with no `channel.replied` to pair with.
                            //
                            // The actual transport attempt is HERE either
                            // way, so a failure must leave its own durable
                            // trace (`send_or_record`), or the audit trail
                            // asserts a delivery that never happened.
                            // `EmailChannel::send` still always fails, so
                            // without it every email answer looked
                            // delivered. The row is channel + peer only, so
                            // an ask failure recorded here names neither the
                            // ask nor the task — correlating it needs the
                            // outbound message to carry its `ask_id`,
                            // tracked separately.
                            send_or_record(&*ch, &*events, &id, out, "channel send failed").await;
                        }
                    }
                }
            }));
        }

        // Outbound pump: NOTIFY → load → route → push into the per-channel sender.
        let events_out = events.clone();
        let life = bell.guard();
        handles.push(tokio::spawn(async move {
            let _life = life;
            while let Some(id) = completed.next_completed().await {
                handle_completed(&*completed, &*events_out, &senders, id).await;
            }
            info!("outbound pump stopped");
        }));

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
    /// resources they hold are released before the caller proceeds. The
    /// completed-task pump owns a `PgListener`, which holds a checked-out pool
    /// connection that sqlx 0.9 only releases when the listener is dropped; the
    /// daemon's shutdown closes the pool right after, and `Pool::close()` blocks
    /// until every connection is returned. Aborting without joining would leave
    /// that release racing the pool close. Mirrors the scheduler/audit-mirror
    /// shutdowns, which signal-then-join for the same reason.
    pub async fn shutdown(self) {
        // Stop being a delivery target first: an ask queued after this point
        // would go into a channel whose pump is about to be aborted, which
        // is a message that vanishes rather than one that fails.
        if let Some(w) = &self.asks {
            for id in &self.registered {
                w.outbox.deregister(id);
            }
        }
        for h in &self.handles {
            h.abort();
        }
        for h in self.handles {
            let _ = h.await;
        }
    }
}

#[cfg(test)]
mod tests;
