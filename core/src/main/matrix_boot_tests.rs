//! `matrix_boot`'s unit tests. Split out of `matrix_boot.rs` to keep it under the 500-LOC
//! soft cap; `#[path]`-included there, so `super::` is `matrix_boot`.

use super::*;

/// The classification #514's fix depends on: a `localhost`-NAME homeserver
/// under force-routing can NEVER succeed (the proxy resolves the name to
/// loopback and range-denies every CONNECT), so it must be FATAL. Were it
/// merely retryable, the new supervisor would spin on it forever —
/// precisely the respawn loop #459's check exists to prevent.
#[test]
fn a_force_routed_localhost_homeserver_is_fatal_not_retryable() {
    let outcome = classify_homeserver("http://localhost:8008", true);
    assert!(matches!(outcome, Some(BootOutcome::Fatal(_))), "{outcome:?}");
}

/// The same URL without force-routing is reachable — the worker resolves
/// localhost itself (dev conduit) — so nothing is refused up front.
#[test]
fn a_localhost_homeserver_without_force_routing_is_not_refused() {
    assert!(classify_homeserver("http://localhost:8008", false).is_none());
}

/// A routable homeserver is never refused up front: an unreachable one is
/// a *transient* condition and belongs to the retry loop, not to this
/// static check.
#[test]
fn a_routable_homeserver_is_not_refused() {
    assert!(classify_homeserver("https://matrix.kastellan.dev", true).is_none());
}

/// #789's twin: the reply-undelivered sink returns at once against a
/// Postgres that never answers — it is called on the driver's thread.
#[test]
fn the_reply_undelivered_sink_does_not_hold_the_driver_thread() {
    use crate::audit_sink::test_support::{
        assert_insert_attempted, assert_returns_at_once, stalled_pool,
    };
    use kastellan_core::channel::{
        ChannelId, ConversationId, OutgoingMessage, PeerId, UndeliveredReason, UndeliveredReply,
    };
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, listener) = rt.block_on(async { stalled_pool() });
    static LEDGER: crate::audit_sink::Ledger = crate::audit_sink::Ledger::new(crate::audit_sink::Bounds { queued: 8, connections: 8 });
    let sink = reply_undelivered_audit_sink(
        crate::audit_sink::SinkWriter::with_ledger(
            &LEDGER,
            pool,
            rt.handle().clone(),
            crate::audit_sink::SinkKind::AuditsOnExit,
        ),
        |_, _| {},
    );
    let out = OutgoingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: "b".into(),
    };
    assert_returns_at_once("the Matrix reply-undelivered sink", || {
        sink(UndeliveredReply::of(&out, UndeliveredReason::GaveUp, time::OffsetDateTime::now_utc()))
    });
    // #802: the row's label for the shutdown line, read at once — the row is
    // pending until the stalled pool gives up on it.
    assert_eq!(
        LEDGER.final_snapshot().named,
        [r#"matrix reply to conversation "!room:srv" (gave_up)"#]
    );
    assert_insert_attempted("the Matrix reply-undelivered sink", &listener);
    // #792: the sink holds its lease until the driver drops it, so the
    // shutdown drain waits for the driver that owns it.
    assert_eq!(LEDGER.snapshot().sinks_live, 1, "the Matrix reply-undelivered sink holds a lease");
    drop(sink);
    assert_eq!(LEDGER.snapshot().sinks_live, 0, "and gives it back when dropped");
}

static MATRIX_SAID: std::sync::Mutex<Vec<(AuditLostWriter, String)>> =
    std::sync::Mutex::new(Vec::new());

/// #799: the Matrix sink's failure closure reaches the reporter, as the
/// MATRIX writer, naming the conversation and the reason.
#[test]
fn the_matrix_sink_reports_a_row_it_could_not_write() {
    use kastellan_core::channel::{
        ChannelId, ConversationId, OutgoingMessage, PeerId, UndeliveredReason, UndeliveredReply,
    };
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    static LEDGER: crate::audit_sink::Ledger =
        crate::audit_sink::Ledger::new(crate::audit_sink::Bounds { queued: 0, connections: 1 });
    let sink = reply_undelivered_audit_sink(
        crate::audit_sink::SinkWriter::with_ledger(
            &LEDGER,
            pool,
            rt.handle().clone(),
            crate::audit_sink::SinkKind::AuditsOnExit,
        ),
        |w, line| MATRIX_SAID.lock().unwrap().push((w, line.to_string())),
    );
    let out = OutgoingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!shed-me:srv".into()),
        body: "b".into(),
    };
    sink(UndeliveredReply::of(&out, UndeliveredReason::QueueFull, time::OffsetDateTime::now_utc()));
    let said = MATRIX_SAID.lock().unwrap();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, AuditLostWriter::Matrix);
    assert!(
        said[0].1.contains("!shed-me:srv") && said[0].1.contains("queue_full"),
        "{said:?}"
    );
}

/// #798: a conversation id is bounded where it is quoted.
#[test]
fn a_hostile_conversation_id_is_capped_in_the_lost_row_line() {
    let long = "!".repeat(10_000);
    let line = format_reply_row_lost(&long, "gave_up", &"shed");
    assert!(line.len() < 1_000, "{} bytes", line.len());
}

/// #792: a reply row that was not written says which reply's drop it
/// belonged to, and why it was not written.
#[test]
fn a_lost_reply_row_names_its_conversation_reason_and_cause() {
    let line = format_reply_row_lost("!room:srv", "gave_up", &"shed: too many");
    assert_eq!(
        line,
        "channel.reply_undelivered row for a reply to conversation \"!room:srv\" (gave_up) not \
         written: shed: too many. The dropped reply's [worker-refusal] line stands"
    );
}

/// #802: the sink `attempt` builds takes a *starting* lease — not waited for,
/// no loss — that becomes a live lease of a driver that audits on exit once
/// started.
#[test]
fn attempt_s_sink_starts_as_starting_and_audits_on_exit_once_started() {
    use crate::audit_sink::{Bounds, InFlight, Ledger};
    static LEDGER: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    let (sink, starting) = reply_sink(&LEDGER, pool, rt.handle().clone(), |_, _| {});
    assert_eq!(LEDGER.snapshot(), InFlight { starting: 1, ..InFlight::default() });
    starting.started();
    assert_eq!(
        LEDGER.snapshot(),
        InFlight { sinks_live: 1, auditing_live: 1, ..InFlight::default() }
    );
    drop(sink);
    assert_eq!(LEDGER.snapshot(), InFlight::default());
}

/// #802: only a login that came up starts the sink's lease. A failed, a
/// cancelled or panicked, and a timed-out login leave it *starting* — the
/// timed-out one being the abandoned bring-up whose lease made every shutdown
/// wait and report a loss.
#[test]
fn only_a_login_that_came_up_starts_the_sink_s_lease() {
    use crate::audit_sink::{Bounds, InFlight, Ledger};
    static LEDGER: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    let mut sinks = Vec::new();
    let mut sink = || {
        let (s, starting) = reply_sink(&LEDGER, pool.clone(), rt.handle().clone(), |_, _| {});
        sinks.push(s);
        starting
    };

    let failed = login_outcome::<u8>(Ok(Ok(Err(anyhow::anyhow!("login refused")))), sink());
    assert!(matches!(failed, Err(BootOutcome::Retry(_))));
    let cancelled = {
        let task = rt.spawn(std::future::pending::<anyhow::Result<u8>>());
        task.abort();
        rt.block_on(task).expect_err("aborted")
    };
    assert!(matches!(login_outcome::<u8>(Ok(Err(cancelled)), sink()), Err(BootOutcome::Retry(_))));
    let elapsed = rt
        .block_on(async {
            tokio::time::timeout(std::time::Duration::ZERO, std::future::pending::<()>()).await
        })
        .expect_err("timed out");
    assert!(matches!(login_outcome::<u8>(Err(elapsed), sink()), Err(BootOutcome::Retry(_))));
    assert_eq!(LEDGER.snapshot(), InFlight { starting: 3, ..InFlight::default() }, "none started");

    assert!(matches!(login_outcome(Ok(Ok(Ok(7u8))), sink()), Ok(7)));
    assert_eq!(
        LEDGER.snapshot(),
        InFlight { starting: 3, sinks_live: 1, auditing_live: 1, ..InFlight::default() },
        "the one that came up did"
    );
    drop(sinks);
}

/// The row the Matrix driver's sink writes: the bus's actor, the
/// reply-undelivered action, and a payload that names the reason but
/// never carries the reply body (#782). A recording sink in the driver
/// tests sees only what the driver passed; this pins what is stored.
#[test]
fn the_reply_undelivered_row_is_the_bus_s_shape_without_the_body() {
    use kastellan_core::channel::{
        ChannelId, ConversationId, OutgoingMessage, PeerId, UndeliveredReason, UndeliveredReply,
    };
    let out = OutgoingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: "SECRET-BODY".into(),
    };
    let at = time::macros::datetime!(2026-09-30 12:34:56 UTC);
    let reply = UndeliveredReply::of(&out, UndeliveredReason::QueueFull, at);
    let (actor, action, payload) = reply_undelivered_row(&reply);
    assert_eq!(actor, "channel");
    assert_eq!(action, "channel.reply_undelivered");
    assert_eq!(
        payload,
        serde_json::json!({
            "channel": "matrix",
            "peer": "@me:srv",
            "reason": "queue_full",
            "observed_at": "2026-09-30T12:34:56Z",
        })
    );
    assert!(!payload.to_string().contains("SECRET-BODY"));
}
