//! The optional ack step: an event is acked only after the bus has it,
//! never without an ack method or token, and a skipped id only when its
//! batch's events decoded (the shared monotonic cursor).
use super::*;

#[test]
fn ack_is_called_after_the_event_reaches_the_bus() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "hi", "ack_token": "42"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        None,
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "me@example.org");

    wait_until(|| st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
    let log = st.log.lock().unwrap();
    let entry = log.iter().find(|(m, _)| m == "email.ack").cloned().unwrap();
    assert_eq!(entry.1["cursor"], "42", "ack must carry the event's own cursor");
}

#[test]
fn no_ack_method_means_no_ack_call() {
    // Matrix must be untouched: its spec has ack_method: None. The event
    // carries an ack_token on purpose — this isolates "ack_method: None
    // gates the call" from "ack_token: None gates the call" (the latter has
    // its own dedicated test below); without a token here, this test would
    // pass even if the ack_method gate were broken.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!r", "body": "hi", "ack_token": "99"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_without_ack(),
        calls,
        test_parse,
        test_encode,
        None,
        None,
        DriverAudit::none(),
        ChannelId("matrix".into()),
    )
    .unwrap();
    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "@me:srv");
    // No ack ever fires here, so there is no bus-side signal to wait on;
    // give the (busy-spinning) driver a few loop iterations before asserting
    // absence, same pattern as the retention test above.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")),
        "a spec without ack_method must never call ack"
    );
}

#[test]
fn an_event_without_an_ack_token_is_not_acked() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "hi"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        None,
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .unwrap();
    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "me@example.org");
    std::thread::sleep(Duration::from_millis(150));
    assert!(!st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
}

#[test]
fn ack_failure_is_non_fatal_and_the_driver_keeps_polling() {
    // The whole reason the ack fires AFTER the bus hand-off is to guarantee
    // redelivery when the ack itself fails. This test pins that: it forces
    // `email.ack` to error while init/poll/send keep succeeding, and proves
    // the driver thread survives — it must still be alive and polling well
    // after the failed ack, not dead from an accidental early return.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "first", "ack_token": "1"}
    ]}));
    *st.fail_method.lock().unwrap() = Some("email.ack".to_string());
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        None,
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .unwrap();

    let first = driver.inbound_rx.blocking_recv().expect("first inbound event");
    assert_eq!(first.body, "first");

    // Wait for the (failing) ack attempt itself, so we know the ack branch
    // actually ran — not merely that it was skipped for some other reason.
    wait_until(|| st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));

    // Only now queue a second event: if the driver thread died in the
    // ack-failure arm (e.g. a stray `return;`), inbound_rx would be closed
    // and blocking_recv below would return None instead of the event —
    // panicking with a clear message rather than hanging, since a dead
    // driver drops its inbound_tx sender.
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "second", "ack_token": "2"}
    ]}));
    let second = driver
        .inbound_rx
        .blocking_recv()
        .expect("driver must keep polling after a failed ack, not die");
    assert_eq!(second.body, "second");
}

/// Extract `skipped[].(message_id, reason)` from a poll result shaped like
/// email-in's — the fixture `ParseAckOnly` these tests use.
fn test_parse_ack_only(v: &Value) -> Vec<(String, String)> {
    v.get("skipped")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let id = e.get("message_id").and_then(|m| m.as_str())?.to_string();
                    let reason = e.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string();
                    Some((id, reason))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn skipped_ids_are_acked_even_though_they_never_become_events() {
    // A poll result with an EMPTY events list but a non-empty `skipped` list
    // (email-in's real shape when every message in a batch was unattributable)
    // must still ack every skipped id — otherwise the worker's server-side
    // cursor wedges on the first one forever, since nothing else will ever
    // ack an id nobody ever saw as an event.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [],
        "skipped": [
            {"message_id": "10", "reason": "no usable From address"},
            {"message_id": "11", "reason": "localmail 404: not found"}
        ]
    }));
    let (_driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .unwrap();

    wait_until(|| st.log.lock().unwrap().iter().filter(|(m, _)| m == "email.ack").count() >= 2);
    let log = st.log.lock().unwrap();
    let acked: Vec<&str> = log
        .iter()
        .filter(|(m, _)| m == "email.ack")
        .map(|(_, p)| p["cursor"].as_str().unwrap())
        .collect();
    assert!(acked.contains(&"10"), "acked = {acked:?}");
    assert!(acked.contains(&"11"), "acked = {acked:?}");
}

#[test]
fn skipped_ids_alongside_real_events_are_both_acked() {
    // Realistic mixed batch: one usable event (acked via its own ack_token,
    // the existing per-event path) plus one unattributable message in
    // `skipped` (acked via the new ParseAckOnly path). Neither path must
    // starve the other.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [
            {"peer": "me@example.org", "conversation": "<a>", "body": "hi", "ack_token": "5"}
        ],
        "skipped": [{"message_id": "6", "reason": "no usable From address"}]
    }));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("the one real event");
    assert_eq!(msg.body, "hi");

    wait_until(|| st.log.lock().unwrap().iter().filter(|(m, _)| m == "email.ack").count() >= 2);
    let log = st.log.lock().unwrap();
    let acked: Vec<&str> = log
        .iter()
        .filter(|(m, _)| m == "email.ack")
        .map(|(_, p)| p["cursor"].as_str().unwrap())
        .collect();
    assert!(acked.contains(&"5"), "the real event's own ack_token must still be acked: {acked:?}");
    assert!(acked.contains(&"6"), "the skipped id must also be acked: {acked:?}");
}

#[test]
fn no_ack_method_means_skipped_ids_are_never_acked_either() {
    // Symmetry with `no_ack_method_means_no_ack_call`: a spec without
    // ack_method (Matrix's shape) must never call ack, regardless of whether
    // a `parse_ack_only` happens to be wired — the ack_method gate covers
    // both ack paths, not just the per-event one.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [],
        "skipped": [{"message_id": "99", "reason": "no usable From address"}]
    }));
    let (_driver, _identity) = PolledWorkerDriver::spawn(
        spec_without_ack(),
        calls,
        test_parse,
        test_encode,
        None,
        Some(test_parse_ack_only),
        DriverAudit::none(),
        ChannelId("matrix".into()),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")),
        "ack_method: None must suppress the skipped-id ack path too"
    );
}

#[test]
fn a_skipped_id_is_not_acked_when_the_same_batchs_events_fail_to_decode() {
    // The worker's ack cursor is ONE monotonic high-water mark shared by
    // `events` and `skipped` — not two independent counters. If `events`
    // fails to decode (a worker bug: `test_parse` errors on a non-string
    // `peer`) and the driver acked `skipped` anyway, that would silently
    // advance the shared cursor PAST whatever those undecoded events were,
    // permanently losing them (unlike a failed ack, an advanced cursor can
    // never be wound back). So this batch must produce NO ack call at all.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [{"peer": 42}], // malformed: peer must be a string — test_parse errors
        "skipped": [{"message_id": "77", "reason": "no usable From address"}]
    }));
    // A second, well-formed poll follows so the test can prove the driver is
    // still alive and polling (not wedged) without racing a fixed sleep.
    st.polls.lock().unwrap().push_back(json!({
        "events": [
            {"peer": "me@example.org", "conversation": "<a>", "body": "after-bad", "ack_token": "1"}
        ],
        "skipped": []
    }));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("the second batch's good event");
    assert_eq!(msg.body, "after-bad");

    // Give any (incorrect) ack attempt for "77" a chance to have happened.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !st.log.lock().unwrap().iter().any(|(m, p)| m == "email.ack" && p["cursor"] == "77"),
        "a skipped id from a batch whose events failed to decode must never be acked"
    );
}

#[test]
fn audit_ack_only_is_called_with_id_and_reason_for_every_acked_skipped_id() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [],
        "skipped": [
            {"message_id": "10", "reason": "no usable From address"},
            {"message_id": "11", "reason": "localmail 404: not found"}
        ]
    }));
    let audited: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let audited_cl = audited.clone();
    let audit: AckOnlyAudit = Box::new(move |id, reason| {
        audited_cl.lock().unwrap().push((id.to_string(), reason.to_string()));
    });
    let (_driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        DriverAudit { ack_only: Some(audit), ..DriverAudit::none() },
        ChannelId("email".into()),
    )
    .unwrap();

    wait_until(|| audited.lock().unwrap().len() >= 2);
    let got = audited.lock().unwrap();
    assert!(got.contains(&("10".to_string(), "no usable From address".to_string())), "{got:?}");
    assert!(got.contains(&("11".to_string(), "localmail 404: not found".to_string())), "{got:?}");
}

#[test]
fn audit_ack_only_is_not_called_when_the_same_batchs_events_fail_to_decode() {
    // Symmetric with the ack-suppression test above: the audit call sits in
    // the exact same gated block as the ack call (see `run`'s comment), so a
    // batch whose events fail to decode must produce no audit call either —
    // otherwise the audit trail would claim a message was "discarded" when
    // its cursor was never actually advanced (it will be redelivered).
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [{"peer": 42}], // malformed — test_parse errors
        "skipped": [{"message_id": "77", "reason": "no usable From address"}]
    }));
    st.polls.lock().unwrap().push_back(json!({
        "events": [
            {"peer": "me@example.org", "conversation": "<a>", "body": "after-bad", "ack_token": "1"}
        ],
        "skipped": []
    }));
    let audited: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let audited_cl = audited.clone();
    let audit: AckOnlyAudit = Box::new(move |id, reason| {
        audited_cl.lock().unwrap().push((id.to_string(), reason.to_string()));
    });
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        DriverAudit { ack_only: Some(audit), ..DriverAudit::none() },
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("the second batch's good event");
    assert_eq!(msg.body, "after-bad");

    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !audited.lock().unwrap().iter().any(|(id, _)| id == "77"),
        "audit must not fire for a skipped id whose batch's events failed to decode"
    );
}

/// A worker whose first poll is still out when the bus goes away — the
/// daemon's shutdown mid long-poll — and then answers it with skipped ids
/// only. Every later poll is an empty batch; every call is logged.
struct PollOutlivesTheBus {
    bus_gone: Arc<AtomicBool>,
    polls: AtomicUsize,
    log: Arc<Mutex<Vec<String>>>,
}

impl WorkerCalls for PollOutlivesTheBus {
    fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.log.lock().unwrap().push(format!("{method} {params}"));
        if method.ends_with(".poll") && self.polls.fetch_add(1, Ordering::SeqCst) == 0 {
            wait_until(|| self.bus_gone.load(Ordering::SeqCst));
            return Ok(json!({
                "events": [],
                "skipped": [{"message_id": "10", "reason": "no usable From address"}]
            }));
        }
        Ok(json!({"events": []}))
    }
}

/// #792: a poll that brought only skipped ids after the bus went away acks
/// none of them and audits none of them, and the driver exits. Acked then,
/// an id's audit row would be written into a shutting-down daemon (and lost
/// with it) while the id itself is never redelivered; not acked, the next
/// start redelivers it. The outbound endpoint is kept alive, so it is the
/// closed INBOUND side the driver must notice — the check a batch with no
/// event never reached (only an event's `blocking_send` could see it).
#[test]
fn skipped_ids_are_neither_acked_nor_audited_once_the_bus_has_gone() {
    let bus_gone = Arc::new(AtomicBool::new(false));
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let calls = PollOutlivesTheBus { bus_gone: bus_gone.clone(), polls: AtomicUsize::new(0), log: log.clone() };
    let audited: Arc<Mutex<Vec<String>>> = Arc::default();
    let audited_cl = audited.clone();
    let audit: AckOnlyAudit = Box::new(move |id, _| audited_cl.lock().unwrap().push(id.to_string()));
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        Box::new(calls),
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        DriverAudit { ack_only: Some(audit), ..DriverAudit::none() },
        ChannelId("email".into()),
    )
    .unwrap();
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;

    wait_until(|| log.lock().unwrap().iter().any(|c| c.starts_with("email.poll")));
    drop(inbound_rx);
    bus_gone.store(true, Ordering::SeqCst);

    wait_until(|| join.is_finished());
    assert!(
        !log.lock().unwrap().iter().any(|c| c.starts_with("email.ack")),
        "no skipped id may be acked once the bus has gone: {:?}",
        log.lock().unwrap()
    );
    assert!(audited.lock().unwrap().is_empty(), "nor audited: {:?}", audited.lock().unwrap());
    drop(outbound_tx);
}
