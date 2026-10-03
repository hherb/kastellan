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

use super::auth::{AuthDecision, PeerAuthorizer};
use super::ingest::{screen_and_classify, InboundDecision};
use super::pump_liveness::DeathBell;
use super::route::reply_for_completed_task;
use super::{actions, Channel, ChannelId, IncomingMessage, OutgoingMessage, PeerId};

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
    /// marker (#808).
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
/// pre-#564-slice-2 behaviour; see `containment_refusal`.
pub struct AskWiring {
    pub outbox: Arc<super::outbox::ChannelOutbox>,
    pub resolver: Arc<dyn AskResolver>,
}

/// Real DB-backed `ChannelEvents`; its own module since #808, which made a
/// failed audit insert an `[audit-lost]` report.
pub use super::pg_events::PgChannelEvents;

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

/// Handle one inbound message. Order is security-load-bearing:
///   1. **authorize** (`(channel, peer, evidence)`), yielding three distinct
///      outcomes:
///      - `RejectedUnauthentic(reason)` — the transport-supplied evidence
///        didn't check out (bad DMARC / missing-or-wrong token / a pairing
///        row with no token at all). Dropped + audited immediately, carrying
///        `reason`'s stable label so the four denial arms are tellable apart
///        in `audit_log`, and BEFORE and
///        WITHOUT the pairing carve-out: that carve-out compares unpaired
///        input against a live single-use code, and a transport that cannot
///        authenticate its sender must never get to attempt that;
///      - `Rejected` — no active pairing at all. The pairing carve-out
///        (compare-only) is consulted **only when `msg.evidence.is_none()`**
///        — i.e. only for a transport that authenticates its own peers
///        (Matrix). A transport that hands us `evidence` (email) is, by
///        construction, one that cannot vouch for its sender; letting an
///        unpaired evidence-bearing peer probe the carve-out would turn an
///        operator-issued single-use code into a brute-force target reachable
///        over a spoofable transport — email pairing is meant to be
///        operator-only. So an evidence-bearing `Rejected` skips straight to
///        the drop + audit, same as if no `PairingService` were configured;
///      - `Recognised` — proceed to step 2.
///   2. **recognise an answer** — if the body parses as `/approve <token>`
///      or `/deny <token>`, it is an answer to a raised ask, not an
///      instruction. Placement is the security content (spec D5): AFTER
///      authorization, so only a paired peer can resolve anything and the
///      claimant is the sender the transport vouched for; and BEFORE
///      screening + enqueue, so an answer can never become a task.
///
///      The injection guard deliberately does **not** run on it (spec D6):
///      the body is a closed set — one of two fixed verbs plus an opaque
///      token — that is parsed into a command and never interpolated into
///      a plan, a prompt or a tool argument, so there is nothing for a
///      screen to protect, and a false positive would block the one action
///      this whole path exists to enable.
///
///      **A body that does not parse is then split three ways (#582, spec
///      D4)**, and the split replaced a single guess that refused all
///      three:
///
///      - **carries a live token** — some candidate in the body hashes to a
///        live nonce of *this peer's own* ask. Refused, never enqueued:
///        enqueueing would write a live token verbatim into `tasks.payload`
///        (durable, no DELETE grant) and hand it to the planner. A quoted
///        reply echoing the ask's own command lines is this shape, and so
///        is `/approve tok9 thanks!` when `tok9` is live.
///      - **could not be answered** — no resolver wired, the body is over
///        the candidate caps, or the resolver errored. Also refused; an
///        unanswered question must never enqueue.
///      - **verb-first but unparseable** — a human fumbled the syntax
///        (`/deny` alone). Refused with the usage hint.
///
///      Anything else — including an ordinary instruction that merely
///      *mentions* a verb, such as `should I /approve the PR?` — **does**
///      fall through to step 3 and is enqueued. That is #582's fix: the old
///      guess refused it, and on email the refusal could not even be sent,
///      so the message vanished. The first two arms share one ack so the
///      All three refusal arms send the *same* ack, so the peer cannot tell
///      them apart; only the audit `reason` differs.
///
///      **This containment arm runs whether or not `asks` is wired.** The
///      wiring decides whether an answer can be *resolved*; it must not
///      decide whether a command-shaped body is kept out of the task
///      queue. A token raised on one channel can be pasted into another,
///      and a channel added later whose boot path forgets the wiring would
///      otherwise silently reopen the leak.
///   3. **screen** (injection guard) and enqueue or block.
///
/// Returns `Some(ack)` on a successful pairing or a recognised answer (the
/// per-channel task delivers it via the same channel).
pub async fn handle_inbound(
    authorizer: &dyn PeerAuthorizer,
    pairing: Option<&dyn PairingService>,
    asks: Option<&AskWiring>,
    events: &dyn ChannelEvents,
    msg: &IncomingMessage,
) -> Option<OutgoingMessage> {
    match authorizer.authorize(&msg.channel, &msg.peer, msg.evidence.as_ref()).await {
        AuthDecision::Recognised => {}
        AuthDecision::RejectedUnauthentic(reason) => {
            // Deliberately BEFORE and WITHOUT the pairing carve-out: the carve-out
            // compares unpaired input against a live code, and a transport that
            // cannot authenticate its sender must not get to attempt that.
            // Payload carries the channel + peer + the reason CODE only — never
            // the body, never the token, never a header. `reason` is one of a
            // fixed set of `[a-z_]` labels (`UnauthenticReason::as_str`), which
            // is what makes it safe to persist: without it every denial arm
            // (DMARC fail, no token, wrong token, token-less pairing) writes a
            // byte-identical row, and a wrong `KASTELLAN_EMAIL_AUTHSERV_ID` —
            // the single most likely misconfiguration here, which rejects
            // EVERY message — is indistinguishable from a token typo.
            events
                .audit(
                    actions::REJECTED_UNAUTHENTIC,
                    serde_json::json!({
                        "channel": msg.channel.0,
                        "peer": msg.peer.0,
                        "reason": reason.as_str(),
                    }),
                )
                .await;
            return None;
        }
        AuthDecision::Rejected => {
            // Pairing carve-out: the ONLY place unpaired input is touched, and
            // only ever compared against an operator-issued code (never
            // enqueued/echoed) — and ONLY for a transport that authenticates
            // its own peers (`evidence.is_none()`, e.g. Matrix). A transport
            // that supplies evidence (email) cannot vouch for its sender, so
            // an unpaired peer on it must not get a shot at the carve-out
            // either — same reasoning as `RejectedUnauthentic` above, just
            // for "no pairing at all" instead of "pairing but bad evidence".
            if msg.evidence.is_none() {
                if let Some(p) = pairing {
                    if p.try_pair(&msg.channel, &msg.peer, &msg.body).await == PairingOutcome::Paired {
                        events
                            .audit(
                                actions::PAIRED,
                                serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0}),
                            )
                            .await;
                        return Some(OutgoingMessage {
                            channel: msg.channel.clone(),
                            peer: msg.peer.clone(),
                            conversation: msg.conversation.clone(),
                            body: PAIRED_ACK_BODY.to_string(),
                        });
                    }
                }
            }
            events
                .audit(
                    actions::REJECTED_UNPAIRED,
                    serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0}),
                )
                .await;
            return None;
        }
    }

    if let Some(wiring) = asks {
        if let Some(cmd) = super::ask_message::parse_ask_command(&msg.body) {
            let claimant =
                kastellan_db::asks::Claimant::new(msg.channel.0.clone(), msg.peer.0.clone());
            let nonce = kastellan_db::asks::Nonce::from_wire(cmd.token);
            let resolution = wiring.resolver.resolve(&nonce, cmd.choice.as_str(), &claimant).await;
            // `Ok(None)` (wrong/expired/not-this-peer's token) and `Err`
            // (e.g. a DB outage) are collapsed into ONE arm below, sharing
            // one audit call and one ack body: a DB error and a refused
            // answer must look the same to the peer, or the error path
            // becomes the existence oracle the refusal path refuses to be.
            // Structural, not just parallel code, so a later edit cannot
            // let the two drift apart by touching only one arm.
            let body = if let Ok(Some(resolved)) = resolution {
                events
                    .audit(
                        crate::scheduler::audit::ACTION_ASK_RESOLVED,
                        serde_json::json!({
                            "ask_id": resolved.ask_id,
                            "task_id": resolved.task_id,
                            "choice": cmd.choice.as_str(),
                            "resolved_by": claimant.attribution(),
                            "via": "channel",
                        }),
                    )
                    .await;
                super::ask_message::ack_resolved(cmd.choice, resolved.task_id)
            } else {
                if let Err(e) = &resolution {
                    // Logged only here, never audited: the DB crate's
                    // `DbError` renders query context, not the token — but
                    // even so this must never reach a durable,
                    // operator-queried row, only the rotating daemon log.
                    warn!(error = %e, "ask resolution failed");
                }
                events
                    .audit(
                        actions::ASK_ANSWER_REJECTED,
                        serde_json::json!({
                            "channel": msg.channel.0,
                            "peer": msg.peer.0,
                            "reason": actions::ASK_REASON_UNRESOLVABLE,
                        }),
                    )
                    .await;
                super::ask_message::ACK_NOT_ANSWERABLE.to_string()
            };
            return Some(OutgoingMessage {
                channel: msg.channel.clone(),
                peer: msg.peer.clone(),
                conversation: msg.conversation.clone(),
                body,
            });
        }
    }

    // Arms 2 and 3 (#582, spec D4). Containment first, so a live token in
    // a malformed command audits the security-relevant cause; both arms
    // return the same ack, so the peer cannot tell them apart.
    //
    // Deliberately OUTSIDE the `if let Some(wiring)` above: containment is
    // a property of the inbound path, not of this bus's configuration. See
    // the ordering doc on `handle_inbound`.
    let refusal = containment_refusal(msg, asks).await.or_else(|| {
        super::ask_message::is_command_shaped(&msg.body).then_some(actions::ASK_REASON_MALFORMED)
    });
    if let Some(reason) = refusal {
        events
            .audit(
                actions::ASK_ANSWER_REJECTED,
                serde_json::json!({
                    "channel": msg.channel.0,
                    "peer": msg.peer.0,
                    "reason": reason,
                }),
            )
            .await;
        return Some(OutgoingMessage {
            channel: msg.channel.clone(),
            peer: msg.peer.clone(),
            conversation: msg.conversation.clone(),
            body: super::ask_message::ACK_MALFORMED_COMMAND.to_string(),
        });
    }

    match screen_and_classify(msg) {
        InboundDecision::Enqueue { payload } => match events.enqueue(Lane::Fast, payload).await {
            Ok(id) => {
                events
                    .audit(
                        actions::RECEIVED,
                        serde_json::json!({
                            "task_id": id, "channel": msg.channel.0,
                            "peer": msg.peer.0, "conversation": msg.conversation.0,
                        }),
                    )
                    .await;
            }
            Err(e) => warn!(error = %e, "channel enqueue failed; message dropped"),
        },
        InboundDecision::InjectionBlocked { sha256, reason_codes, score } => {
            events
                .audit(
                    actions::INJECTION_BLOCKED,
                    serde_json::json!({
                        "channel": msg.channel.0, "peer": msg.peer.0,
                        "sha256": sha256, "reason_codes": reason_codes, "score": score,
                    }),
                )
                .await;
        }
    }
    None
}

/// The containment arm (#582, spec D5): may this body be enqueued, or does
/// it carry a live approval token?
///
/// `Some(reason)` refuses. `None` means there is nothing here to refuse —
/// either the cheap gate never fired, or the question was asked and the
/// answer was no. Enqueueing a live token writes it verbatim into
/// `tasks.payload` — a durable column with no DELETE grant — and hands it
/// to the planner as an instruction.
///
/// **Exact, where the old guard was a shape guess.** A quoted reply, a
/// leading mention pill and prose around a command each carried a live
/// token past the first version of that guess; widening it a third time
/// only trades a false negative for a bigger false positive. This asks the
/// real question instead, so it catches command shapes nobody has thought
/// of yet — within the gate's reach, which is bodies that mention a verb
/// somewhere (spec Open risk 3) —
/// and its complement is what lets `is_command_shaped` narrow back to the
/// UX job it is actually good at.
///
/// **Four fail-closed edges** (spec D7), all reported as `unscannable`
/// because all of them mean *we could not answer*, not *we judged the
/// syntax*: no resolver wired, a body larger than `CANDIDATE_BYTE_CAP`,
/// more distinct candidates than `CANDIDATE_TOKEN_CAP`, and a resolver
/// error. The last is why `any_live_nonce` returns a `Result` —
/// `Ok(false)` enqueues and `Err` refuses, so collapsing the two into a
/// bool would silently pick the wrong arm on every database hiccup.
async fn containment_refusal(
    msg: &IncomingMessage,
    asks: Option<&AskWiring>,
) -> Option<&'static str> {
    // The cheap, DB-free gate (spec D3): ordinary traffic never reaches
    // Postgres, which is what makes the exact check affordable.
    if !super::ask_message::looks_like_ask_command(&msg.body) {
        return None;
    }
    // NOT `asks?` and NOT `candidate_tokens(..)?`. Both would propagate
    // `None`, and `None` here means ENQUEUE — the fail-open this arm
    // exists to prevent, wearing idiomatic Rust. Written out so the
    // refusing branch cannot be mistaken for shorthand.
    // Three code sites, four triggers (the `candidate_tokens` site covers
    // both caps). All of them audit the same `unscannable` reason — that
    // collapse is deliberate (the peer must not learn which) — so the
    // daemon log is the ONLY place the trigger is recoverable. Two of these
    // were silent, which made a bus that had lost its wiring (refusing
    // every verb-mentioning message, indefinitely) indistinguishable from a
    // large paste and from a DB outage whose log had rotated.
    let Some(wiring) = asks else {
        warn!(channel = %msg.channel.0, "ask containment: no resolver wired; refusing");
        return Some(actions::ASK_REASON_UNSCANNABLE);
    };
    let Some(tokens) = super::ask_message::candidate_tokens(&msg.body) else {
        warn!(
            channel = %msg.channel.0,
            body_len = msg.body.len(),
            "ask containment: body exceeds the candidate caps; refusing",
        );
        return Some(actions::ASK_REASON_UNSCANNABLE);
    };
    let nonces: Vec<_> = tokens.into_iter().map(kastellan_db::asks::Nonce::from_wire).collect();
    let claimant = kastellan_db::asks::Claimant::new(msg.channel.0.clone(), msg.peer.0.clone());
    match wiring.resolver.any_live_nonce(&nonces, &claimant).await {
        Ok(true) => Some(actions::ASK_REASON_CARRIES_LIVE_TOKEN),
        Ok(false) => None,
        Err(e) => {
            // Logged, never audited: `DbError` renders query context, and
            // a durable operator-queried row is the wrong place for it.
            warn!(error = %e, "ask containment check failed; refusing");
            Some(actions::ASK_REASON_UNSCANNABLE)
        }
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
    let out = reply_for_completed_task(&payload, result.as_ref())?;
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
    if let Err(e) = tx.send(out.clone()).await {
        warn!(error = %e, "outbound send queue closed; reply dropped");
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
                                    // says "pairing".
                                    if let Err(e) = ch.send(ack).await {
                                        warn!(channel = %id.0, error = %e, "inbound ack send failed");
                                    }
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
                            // trace, or the audit trail asserts a delivery
                            // that never happened. `EmailChannel::send` still
                            // always fails, so without this pair every email
                            // answer looked delivered. Payload is channel +
                            // peer only: the error is transport text, not a
                            // fixed label, and the body must never be
                            // persisted. That also means an ask failure
                            // recorded here names neither the ask nor the
                            // task — correlating it needs the outbound
                            // message to carry its `ask_id`, tracked
                            // separately.
                            let (peer, conversation) = (out.peer.clone(), out.conversation.clone());
                            if let Err(e) = ch.send(out).await {
                                warn!(channel = %id.0, error = %e, "channel send failed");
                                // The same view a polled driver's sink is
                                // handed, so the row has one definition
                                // (`UndeliveredReply::payload`, #800).
                                let reply = super::UndeliveredReply {
                                    channel: &id,
                                    peer: &peer,
                                    conversation: &conversation,
                                    reason: super::UndeliveredReason::SendFailed,
                                    observed_at: time::OffsetDateTime::now_utc(),
                                };
                                events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
                            }
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
