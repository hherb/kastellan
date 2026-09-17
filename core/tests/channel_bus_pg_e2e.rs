//! PG-gated e2e for the channel bus: pins the real DB seams
//! (`PgChannelEvents` enqueue + audit, `PgCompletedTasks` over the
//! `tasks_completed` NOTIFY) against a live cluster. Skip-as-pass when no
//! `KASTELLAN_PG_BIN_DIR` is configured (mirrors `injection_guard_e2e`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;

use tokio::sync::mpsc;

use kastellan_core::channel::ask_message::{ack_resolved, AskChoice};
use kastellan_core::channel::auth::{
    AuthDecision, DbPeerAuthorizer, PeerAuthorizer, StaticPairings, UnauthenticReason,
};
use kastellan_core::channel::bus::{
    handle_completed, handle_inbound, AskWiring, CompletedTasks, PgAskResolver, PgChannelEvents,
    PgCompletedTasks,
};
use kastellan_core::channel::ingest::{build_channel_task_payload, sha256_hex};
use kastellan_core::channel::outbox::ChannelOutbox;
use kastellan_core::channel::{
    actions, ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerEvidence, PeerId,
};
use kastellan_db::tasks::{self, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "channel-bus-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_inbound_enqueues_and_completion_routes_a_reply() {
    // Skip-as-pass without a `systemd --user`/launchd supervisor (e.g. a root
    // CI container) — `bring_up_pg_cluster` needs one to run the PG service, and
    // `initdb` itself refuses to run as root. Mirrors `postgres_e2e` /
    // `injection_guard_e2e`. Live path runs on the DGX (real PG) + Mac.
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ch-d",
        "ch-l",
        &format!("kastellan-supervisor-test-pg-ch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // ── Inbound: a paired, clean message must enqueue a `channel` task + audit. ──
    let events = PgChannelEvents::new(pool.clone());
    let authorizer = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let msg = IncomingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: "what's on my calendar?".into(),
        evidence: None,
    };
    handle_inbound(&authorizer, None, None, &events, &msg).await;

    let pending = tasks::list(&pool, Some(Lane::Fast), Some("pending"), 10)
        .await
        .expect("list pending");
    assert_eq!(pending.len(), 1, "exactly one channel task enqueued");
    let task = &pending[0];
    assert_eq!(task.payload["kind"], "channel");
    assert_eq!(task.payload["instruction"], "what's on my calendar?");
    assert_eq!(task.payload["channel"], "matrix");
    assert_eq!(task.payload["peer"], "@me:srv");
    assert_eq!(task.payload["conversation"], "!room:srv");

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 200).await.expect("audit fetch");
    assert!(
        audits.iter().any(|r| r.actor == "channel" && r.action == actions::RECEIVED),
        "expected a channel.received audit row"
    );

    // ── Outbound: listen, finalize the task, route the completion to a reply. ──
    // LISTEN before finalize so the NOTIFY is not missed.
    let mut completed = PgCompletedTasks::connect(pool.clone())
        .await
        .expect("connect completed-tasks listener");

    // Claim (pending → running) then finalize (running → completed, fires NOTIFY).
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60)
        .await
        .expect("claim")
        .expect("a pending task to claim");
    assert_eq!(claimed.id, task.id);
    tasks::finalize(
        &pool,
        claimed.id,
        "completed",
        Some(serde_json::json!({"kind": "completed", "message": "You have 2 meetings."})),
        None,
    )
    .await
    .expect("finalize");

    let id = completed.next_completed().await.expect("a completed-task id");
    assert_eq!(id, task.id);

    let (tx, mut rx) = mpsc::channel::<OutgoingMessage>(4);
    let mut senders = HashMap::new();
    senders.insert(ChannelId("matrix".into()), tx);
    let out = handle_completed(&completed, &events, &senders, id)
        .await
        .expect("routed reply");
    assert_eq!(out.body, "You have 2 meetings.");
    assert_eq!(out.conversation, ConversationId("!room:srv".into()));
    let delivered = rx.recv().await.expect("reply delivered to channel sender");
    assert_eq!(delivered.peer, PeerId("@me:srv".into()));

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 200).await.expect("audit fetch 2");
    assert!(
        audits.iter().any(|r| r.actor == "channel" && r.action == actions::REPLIED),
        "expected a channel.replied audit row"
    );

    // Drop the listener before pool.close() — `PgCompletedTasks` holds a
    // checked-out PoolConnection (sqlx 0.9 `PgListener` only releases it from
    // inside `recv()`), and `pool.close()` blocks until every connection is
    // returned, so a listener still in scope at close-time deadlocks the test.
    drop(completed);
    pool.close().await;
}

/// Exercises `DbPeerAuthorizer::authorize` — the REAL production authorizer,
/// not the `TokenAuthorizer` fake in `bus.rs`'s unit tests — against a live
/// `pairings` table, covering every decision arm of `token_hash_for`'s
/// `Ok(None)` / `Ok(Some(None))` / `Ok(Some(Some(_)))` split, INCLUDING each
/// arm's specific `UnauthenticReason` (the audit label an operator diagnoses
/// with) and the `Ok(Some(None))` + evidence guard added by the final review.
///
/// Seeding uses the ADMIN pool (`connect_admin_pool`), same rationale as
/// `db/tests/pairings_e2e.rs`'s token round-trip test: not because
/// `insert_pairing_with_token` itself needs it (migration 0018 grants
/// `kastellan_runtime` INSERT on `pairings`), but to keep every fixture-setup
/// call site in this suite using one consistent, deliberately-superuser
/// connection, matching the operator-only mental model pairing rows are
/// supposed to have. The authorizer itself is built over a **runtime** pool,
/// exactly like production (`main::matrix_boot` passes the daemon-scoped
/// runtime pool into `DbPeerAuthorizer::new`) — `token_hash_for` is a SELECT,
/// which the runtime role does have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_peer_authorizer_covers_all_evidence_arms_against_a_real_pairing_table() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "dpa-d",
        "dpa-l",
        &format!("kastellan-supervisor-test-pg-dpa-{suffix}"),
    );
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "db-peer-authorizer-e2e"}),
    )
    .await
    .expect("probe run");

    let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");
    let runtime = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");
    let authorizer = DbPeerAuthorizer::new(runtime.clone());

    // ---- 1. No active pairing at all → Rejected. ----
    let no_row = ChannelId("email".into());
    let nobody = PeerId("nobody@example.org".into());
    assert_eq!(
        authorizer.authorize(&no_row, &nobody, None).await,
        AuthDecision::Rejected,
        "an address with no pairings row at all must be Rejected"
    );

    // ---- 2. Paired, token_sha256 IS NULL (the Matrix shape) + NO evidence
    //         → Recognised. THIS assertion is the Matrix-parity pin: Matrix
    //         hard-codes `evidence: None` (`matrix/wire.rs`) and its pairing
    //         rows carry a NULL token, so this is the exact production shape.
    //         It must stay green for Matrix to be byte-identical. ----
    let matrix_ch = ChannelId("matrix".into());
    let matrix_peer = PeerId("matrix-shape@example.org".into());
    kastellan_db::pairings::insert_pairing_with_token(
        &admin,
        &matrix_ch.0,
        &matrix_peer.0,
        "code",
        None,
    )
    .await
    .expect("seed NULL-token pairing");
    assert_eq!(
        authorizer.authorize(&matrix_ch, &matrix_peer, None).await,
        AuthDecision::Recognised,
        "a NULL token_sha256 row must admit with no evidence at all (Matrix)"
    );

    // ---- 2b. Same NULL-token row, but the transport DID supply evidence →
    //          RejectedUnauthentic(PairingHasNoToken).
    //
    //          This assertion was INVERTED by the final whole-branch review
    //          (Important 1). It previously pinned `Recognised` — i.e. it
    //          pinned as *correct* the behaviour that a `channel='email'`
    //          pairing row with a NULL `token_sha256` would admit a sender
    //          whose DMARC FAILED and whose token was wrong, collapsing the
    //          entire email gate for that address. Nothing creates such a row
    //          today, but no DB CHECK, code guard, or test prevented one.
    //
    //          `evidence.is_some()` is the branch's general "this transport
    //          cannot vouch for its sender" marker (it already gates the
    //          pairing carve-out in `bus::handle_inbound`); `auth.rs` now
    //          applies the same marker here. Matrix is unaffected because it
    //          never produces evidence — which is exactly what assertion 2
    //          above pins. ----
    let hostile_evidence =
        PeerEvidence { dmarc_pass: false, presented_token: Some("wrong".into()) };
    assert_eq!(
        authorizer.authorize(&matrix_ch, &matrix_peer, Some(&hostile_evidence)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::PairingHasNoToken),
        "a token-less pairing row must REFUSE an evidence-bearing transport: such a \
         peer is admitted only on the strength of its token, so a row without one is \
         misconfigured for it, not permissive"
    );
    // Not just the hostile shape: even evidence that looks perfect must be
    // refused, so the guard cannot be weakened to "bad evidence only".
    let good_looking_evidence =
        PeerEvidence { dmarc_pass: true, presented_token: Some("anything".into()) };
    assert_eq!(
        authorizer.authorize(&matrix_ch, &matrix_peer, Some(&good_looking_evidence)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::PairingHasNoToken),
        "the refusal is about the MISSING pairing token, not about the evidence quality"
    );

    // ---- 3-6. Paired WITH a token (the email shape): four evidence arms. ----
    let email_ch = ChannelId("email".into());
    let email_peer = PeerId("token-required@example.org".into());
    let good_token = "e2e-good-token";
    let good_hash = sha256_hex(good_token.as_bytes());
    kastellan_db::pairings::insert_pairing_with_token(
        &admin,
        &email_ch.0,
        &email_peer.0,
        "operator",
        Some(&good_hash),
    )
    .await
    .expect("seed token-required pairing");

    // 3. Correct token + dmarc_pass → Recognised.
    let good = PeerEvidence { dmarc_pass: true, presented_token: Some(good_token.into()) };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&good)).await,
        AuthDecision::Recognised,
        "correct token + DMARC pass must admit"
    );

    // 4. Correct token but dmarc_pass == false → RejectedUnauthentic(DmarcFail).
    let bad_dmarc = PeerEvidence { dmarc_pass: false, presented_token: Some(good_token.into()) };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&bad_dmarc)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::DmarcFail),
        "a correct token with a failed DMARC verdict must not admit"
    );

    // 5. Wrong token (DMARC otherwise fine) → RejectedUnauthentic(TokenMismatch).
    let wrong_token =
        PeerEvidence { dmarc_pass: true, presented_token: Some("not-the-token".into()) };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&wrong_token)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::TokenMismatch),
        "a wrong token must not admit even with a good DMARC verdict"
    );

    // 5b. DMARC fine but NO token presented at all → RejectedUnauthentic(NoToken).
    //     Distinct from 5: an HTML-only mail (no `body_text`) lands here, and
    //     the operator needs to tell it apart from a wrong token.
    let no_token = PeerEvidence { dmarc_pass: true, presented_token: None };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&no_token)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::NoToken),
        "DMARC pass with no token presented must not admit"
    );

    // 6. Token required but the transport supplied no evidence → RejectedUnauthentic(NoEvidence).
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, None).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::NoEvidence),
        "a token-required pairing with no evidence at all must not admit"
    );

    admin.close().await;
    runtime.close().await;
}

/// `PgAskResolver` — the ONLY production glue between an inbound
/// `/approve` and the database — driven through `handle_inbound` against a
/// real cluster.
///
/// **Nothing else covers it.** `bus.rs`'s unit tests use a fake
/// `AskResolver`; `db/tests/asks_e2e.rs` builds its own resolution JSON;
/// `scheduler_asks_e2e` calls `resolve_with_nonce` directly. So renaming
/// the resolver's `"choice"` key to anything else left the whole suite
/// green, while live every operator answer came back "That approval token
/// isn't answerable" — `reject_choice_outside_options` returns `Err`, the
/// bus collapses `Err` and `Ok(None)` into one arm on purpose (D9), and the
/// operator has no way to tell a key-name bug from a mistyped token.
///
/// Deliberately end to end rather than a unit test of `PgAskResolver`: the
/// composition is the part with no other coverage — the claimant built from
/// the message's own `(channel, peer)`, the D16 entitlement guard matching
/// it against the task payload, the resolution JSON, the `ask.resolved`
/// audit row, and the task returning to `pending` so the lane runner picks
/// it up.
///
/// Runs **both verbs** against the one cluster. The second half is not
/// symmetry for its own sake: it is the only assertion in the workspace
/// that reads the STORED choice for a `/deny`, and so the only one that can
/// see the wire verb and the persisted value disagree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_pg_ask_resolver_resolves_an_answer_arriving_over_the_bus() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ar-d",
        "ar-l",
        &format!("kastellan-supervisor-test-pg-ar-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // A channel-originated task, from the real producer: the D16 guard
    // matches the claimant against `payload->>'channel'`/`'peer'`, so a
    // hand-written payload would test the guard against its own fiction.
    let channel = ChannelId("matrix".into());
    let peer = PeerId("@me:srv".into());
    let conversation = ConversationId("!room:srv".into());
    let payload = build_channel_task_payload(&IncomingMessage {
        channel: channel.clone(),
        peer: peer.clone(),
        conversation: conversation.clone(),
        body: "book the flight".into(),
        evidence: None,
    });
    let task_id = tasks::insert_pending(&pool, Lane::Fast, payload).await.expect("insert");
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");

    // Raise the ask the operator is about to answer. Raised through the db
    // layer rather than the scheduler because what is under test here is the
    // INBOUND half; the outbound half has its own e2e.
    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "sends money to a stranger",
        &serde_json::json!(["approve", "deny"]),
        Some("digest"),
        time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        None,
    )
    .await
    .expect("raise");
    // The one place a test may look at the plaintext: it stands in for the
    // rendered message the operator would be copying from.
    let token = raised.nonce.expose().to_string();

    let events = PgChannelEvents::new(pool.clone());
    let authorizer = StaticPairings::from_peers([peer.clone()]);
    let wiring = AskWiring {
        outbox: std::sync::Arc::new(ChannelOutbox::new()),
        resolver: std::sync::Arc::new(PgAskResolver::new(pool.clone())),
    };
    let answer = IncomingMessage {
        channel: channel.clone(),
        peer: peer.clone(),
        conversation: conversation.clone(),
        body: format!("/approve {token}"),
        evidence: None,
    };

    let ack = handle_inbound(&authorizer, None, Some(&wiring), &events, &answer)
        .await
        .expect("an answered command must be acknowledged");
    assert_eq!(
        ack.body,
        ack_resolved(AskChoice::Approve, task_id),
        "the success ack, not the not-answerable one — a resolver whose resolution JSON \
         does not name `choice` fails the ask's `options` check and lands here",
    );
    assert_eq!(ack.conversation, conversation, "the ack goes back where the command came from");

    // The durable consequences: the ask is answered and the task is back in
    // the queue for the lane runner.
    let ask = kastellan_db::asks::get(&pool, raised.ask_id).await.expect("get").expect("an ask");
    assert_eq!(ask.state, "resolved");
    assert_eq!(
        ask.resolution.as_ref().and_then(|r| r.get("choice")).and_then(|c| c.as_str()),
        Some("approve"),
    );
    assert_eq!(
        ask.resolved_by.as_deref(),
        Some("matrix/@me:srv"),
        "composed by `resolve_with_nonce` from the claimant its guard matched",
    );
    assert_eq!(tasks::observe_state(&pool, task_id).await.expect("state"), "pending");

    // ... and the audit row both surfaces share, under the channel actor.
    let audits = kastellan_db::audit::fetch_since(&pool, 0, 500).await.expect("audit fetch");
    let resolved_row = audits
        .iter()
        .find(|r| r.action == kastellan_core::scheduler::audit::ACTION_ASK_RESOLVED)
        .expect("an ask.resolved row");
    assert_eq!(resolved_row.actor, "channel");
    assert_eq!(resolved_row.payload["via"], "channel");
    assert_eq!(resolved_row.payload["choice"], "approve");
    assert_eq!(resolved_row.payload["task_id"], task_id);
    assert_eq!(resolved_row.payload["resolved_by"], "matrix/@me:srv");
    assert!(
        !serde_json::to_string(&resolved_row.payload).unwrap().contains(&token),
        "the audit payload must never carry the token",
    );

    // ── The same round trip with the other verb. ──
    //
    // Reusing this cluster rather than standing up a second one: the PG
    // bring-up is the expensive part, and what needs pinning is one line of
    // agreement, not a second environment.
    //
    // Without this, `cmd.choice.as_str()` in `handle_inbound` could be
    // replaced by the literal `"approve"` and the entire workspace stayed
    // green — every other test that reaches the resolver's success arm
    // sends `/approve`. Live, the operator types `/deny`, the ack says
    // "Denied" and the audit row says `choice: "deny"` (both read
    // `cmd.choice` directly), while the row below would say `approve` and
    // `run_one` would execute the plan the human refused. This assertion is
    // the only one in the workspace positioned to see that divergence,
    // because it reads the STORED value rather than the ack.
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("the resumed task");
    let denied = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "still sends money to a stranger",
        &serde_json::json!(["approve", "deny"]),
        Some("digest2"),
        time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        None,
    )
    .await
    .expect("raise the second ask");
    let deny_token = denied.nonce.expose().to_string();

    let deny_ack = handle_inbound(
        &authorizer,
        None,
        Some(&wiring),
        &events,
        &IncomingMessage {
            channel: channel.clone(),
            peer: peer.clone(),
            conversation: conversation.clone(),
            body: format!("/deny {deny_token}"),
            evidence: None,
        },
    )
    .await
    .expect("a denial must be acknowledged");
    assert_eq!(deny_ack.body, ack_resolved(AskChoice::Deny, task_id));

    let deny_ask =
        kastellan_db::asks::get(&pool, denied.ask_id).await.expect("get").expect("the second ask");
    assert_eq!(deny_ask.state, "resolved");
    assert_eq!(
        deny_ask.resolution.as_ref().and_then(|r| r.get("choice")).and_then(|c| c.as_str()),
        Some("deny"),
        "the STORED choice must be the verb the operator typed, not the other one",
    );

    pool.close().await;
}

/// A second peer in the same room holds the same bearer token — the ask was
/// delivered as a message — and must still not be able to answer (D16).
///
/// The refusal is indistinguishable from a wrong token by design, so this
/// asserts the two things that ARE observable: the ack is the vague one, and
/// the task did not move.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paired_peer_who_does_not_own_the_task_cannot_answer_its_ask() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ax-d",
        "ax-l",
        &format!("kastellan-supervisor-test-pg-ax-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let channel = ChannelId("matrix".into());
    let owner = PeerId("@me:srv".into());
    let bystander = PeerId("@someone-else:srv".into());
    let conversation = ConversationId("!room:srv".into());
    let payload = build_channel_task_payload(&IncomingMessage {
        channel: channel.clone(),
        peer: owner.clone(),
        conversation: conversation.clone(),
        body: "book the flight".into(),
        evidence: None,
    });
    let task_id = tasks::insert_pending(&pool, Lane::Fast, payload).await.expect("insert");
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");

    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "sends money to a stranger",
        &serde_json::json!(["approve", "deny"]),
        Some("digest"),
        time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        None,
    )
    .await
    .expect("raise");

    let events = PgChannelEvents::new(pool.clone());
    // Both peers are PAIRED — the bystander is authorized to talk to the
    // bot. Entitlement to answer THIS ask is a separate question, and that
    // separation is the whole of D16.
    let authorizer = StaticPairings::from_peers([owner.clone(), bystander.clone()]);
    let wiring = AskWiring {
        outbox: std::sync::Arc::new(ChannelOutbox::new()),
        resolver: std::sync::Arc::new(PgAskResolver::new(pool.clone())),
    };
    let answer = IncomingMessage {
        channel,
        peer: bystander,
        conversation,
        body: format!("/approve {}", raised.nonce.expose()),
        evidence: None,
    };

    let ack = handle_inbound(&authorizer, None, Some(&wiring), &events, &answer)
        .await
        .expect("even a refused answer is acknowledged");
    assert_eq!(
        ack.body,
        kastellan_core::channel::ask_message::ACK_NOT_ANSWERABLE,
        "a bystander holding the bearer token gets the same vague refusal as a mistyped one",
    );
    assert_eq!(
        kastellan_db::asks::get(&pool, raised.ask_id).await.unwrap().unwrap().state,
        "pending",
        "the ask must still be open for the peer who actually owns the task",
    );
    assert_eq!(tasks::observe_state(&pool, task_id).await.expect("state"), "awaiting_operator");

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 500).await.expect("audit fetch");
    assert!(
        audits.iter().any(|r| r.action == actions::ASK_ANSWER_REJECTED),
        "a refused answer must leave a countable row",
    );

    pool.close().await;
}

/// The #582 slice through the **real** resolver against real Postgres.
///
/// `bus/tests.rs` drives these arms against a fake, so this is the only leg
/// where `PgAskResolver`, the shared `live_ask_for_claimant!` predicate and
/// the four arms actually meet. A fake that agrees with a wrong query
/// proves nothing about the query.
#[tokio::test]
async fn containment_holds_end_to_end_against_real_postgres() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ac-d",
        "ac-l",
        &format!("kastellan-supervisor-test-pg-ac-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let channel = ChannelId("matrix".into());
    let owner = PeerId("@me:srv".into());
    let conversation = ConversationId("!room:srv".into());
    let payload = build_channel_task_payload(&IncomingMessage {
        channel: channel.clone(),
        peer: owner.clone(),
        conversation: conversation.clone(),
        body: "book the flight".into(),
        evidence: None,
    });
    let task_id = tasks::insert_pending(&pool, Lane::Fast, payload).await.expect("insert");
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");

    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "sends money to a stranger",
        &serde_json::json!(["approve", "deny"]),
        Some("digest"),
        time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        None,
    )
    .await
    .expect("raise");
    let token = raised.nonce.expose().to_string();

    let events = PgChannelEvents::new(pool.clone());
    let authorizer = StaticPairings::from_peers([owner.clone()]);
    let wiring = AskWiring {
        outbox: std::sync::Arc::new(ChannelOutbox::new()),
        resolver: std::sync::Arc::new(PgAskResolver::new(pool.clone())),
    };
    let deliver = |body: String| IncomingMessage {
        channel: channel.clone(),
        peer: owner.clone(),
        conversation: conversation.clone(),
        body,
        evidence: None,
    };

    // (a) A quoted reply carrying the LIVE token. Its first token is `>`,
    // so the narrow shape check cannot see it — only the exact check can.
    let quoted = deliver(format!("> Approval needed\n> /approve {token}\nyes, go ahead"));
    let ack = handle_inbound(&authorizer, None, Some(&wiring), &events, &quoted)
        .await
        .expect("a refusal is acknowledged");
    assert_eq!(ack.body, kastellan_core::channel::ask_message::ACK_MALFORMED_COMMAND);

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 500).await.expect("audit fetch");
    let row = audits
        .iter()
        .rev()
        .find(|r| r.action == actions::ASK_ANSWER_REJECTED)
        .expect("a rejection row");
    assert_eq!(
        row.payload["reason"], actions::ASK_REASON_CARRIES_LIVE_TOKEN,
        "the only row that ever shows the containment guard firing",
    );

    // The token must appear nowhere in `tasks` — the durable, DELETE-less
    // column this whole arm exists to keep it out of.
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE payload::text LIKE $1")
        .bind(format!("%{token}%"))
        .fetch_one(&pool)
        .await
        .expect("scan tasks");
    assert_eq!(leaked, 0, "a live token must never reach tasks.payload");

    // (b) #582: an ordinary message that merely MENTIONS a verb carries no
    // live token, so it is a task. This is the arm that was refused before
    // the split — and on email, dropped unsent.
    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tasks").fetch_one(&pool).await.expect("count");
    let ordinary = deliver("should I /approve the PR?".to_string());
    let out = handle_inbound(&authorizer, None, Some(&wiring), &events, &ordinary).await;
    assert!(out.is_none(), "an enqueued message gets no ack");
    let after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tasks").fetch_one(&pool).await.expect("count");
    assert_eq!(after, before + 1, "#582: no live token, so it is a task");

    // (c) The same live token with a full stop stuck to it, in prose that is
    // not verb-first. Whitespace tokenisation alone hashes `<token>.`, which
    // is not the nonce, so the first version of this arm answered "no live
    // token" and ENQUEUED — the exact leak the arm exists to prevent, and one
    // `main` did not have (its broad predicate refused every body it matched).
    // Driven through the REAL resolver because a fake that agrees with a wrong
    // candidate set proves nothing about the candidate set.
    let punctuated = deliver(format!("ok, /approve {token}."));
    let ack = handle_inbound(&authorizer, None, Some(&wiring), &events, &punctuated)
        .await
        .expect("a refusal is acknowledged");
    assert_eq!(ack.body, kastellan_core::channel::ask_message::ACK_MALFORMED_COMMAND);

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 500).await.expect("audit fetch");
    let row = audits
        .iter()
        .rev()
        .find(|r| r.action == actions::ASK_ANSWER_REJECTED)
        .expect("a rejection row");
    assert_eq!(row.payload["reason"], actions::ASK_REASON_CARRIES_LIVE_TOKEN);

    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE payload::text LIKE $1")
        .bind(format!("%{token}%"))
        .fetch_one(&pool)
        .await
        .expect("scan tasks");
    assert_eq!(leaked, 0, "punctuation must not carry a live token past containment");

    pool.close().await;
}
