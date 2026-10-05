//! The channel bus's inbound path: [`handle_inbound`] — the security-ordered
//! steps one inbound message goes through (NUL boundary, authorize, answer
//! recognition and containment, screen + enqueue) — and the containment arm it
//! calls.
//!
//! Split out of `bus.rs` (over the 500-LOC soft cap) before #815 grew it, as a
//! sibling rather than a child so every `super::` path in the moved code still
//! names `channel`. `bus` re-exports [`handle_inbound`], so the path
//! `channel::bus::handle_inbound` is unchanged.

use tracing::warn;

use kastellan_db::tasks::Lane;

use super::auth::{AuthDecision, PeerAuthorizer};
use super::bus::{AskWiring, ChannelEvents, PairingOutcome, PairingService, PAIRED_ACK_BODY};
use super::ingest::{screen_and_classify, InboundDecision};
use super::{actions, IncomingMessage, OutgoingMessage};

/// Handle one inbound message. Order is security-load-bearing:
///   0. **NUL boundary** (`super::inbound_nul`, #818): a NUL in the channel,
///      peer or conversation id refuses the message with a
///      `channel.rejected_malformed` row; a NUL in the body is escaped to `␀`
///      and counted on the message's received / injection-blocked row.
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
    // Step 0: see the ordering doc above.
    let admitted = super::inbound_nul::admit(events, msg).await?;
    let msg = &*admitted.msg;
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
                        admitted.mark(serde_json::json!({
                            "task_id": id, "channel": msg.channel.0,
                            "peer": msg.peer.0, "conversation": msg.conversation.0,
                        })),
                    )
                    .await;
            }
            Err(e) => warn!(error = %e, "channel enqueue failed; message dropped"),
        },
        InboundDecision::InjectionBlocked { sha256, reason_codes, score } => {
            events
                .audit(
                    actions::INJECTION_BLOCKED,
                    admitted.mark(serde_json::json!({
                        "channel": msg.channel.0, "peer": msg.peer.0,
                        "sha256": sha256, "reason_codes": reason_codes, "score": score,
                    })),
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
