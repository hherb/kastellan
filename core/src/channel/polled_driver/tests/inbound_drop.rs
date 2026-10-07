//! #826: inbound messages the driver could not hand to a closed bus, through
//! the real `run` loop. On a channel that does not redeliver (Matrix) they are
//! counted, said and audited; on an ack channel (email) they are redelivered,
//! so nothing is reported.
//!
//! Each test that reads the emitted lines uses its own channel label, because
//! the emitter's test record is shared by every test in the binary.
use super::*;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// What the hook was handed, per call: `(channel, dropped)`.
type Dropped = Arc<Mutex<Vec<(String, usize)>>>;

/// A worker whose FIRST poll is held until the test closes the bus, then
/// answers with `batch`; every later poll is empty. So the batch is in the
/// driver's hands exactly when the bus has gone.
struct BatchOutlivesTheBus {
    batch: Value,
    polling: Arc<AtomicBool>,
    bus_gone: Arc<AtomicBool>,
    polls: AtomicUsize,
    log: Arc<Mutex<Vec<String>>>,
}

impl WorkerCalls for BatchOutlivesTheBus {
    fn call(&self, method: &str, _params: Value) -> anyhow::Result<Value> {
        self.log.lock().unwrap().push(method.to_string());
        if method.ends_with(".init") {
            return Ok(json!({"user_id": "@fake:srv"}));
        }
        if method.ends_with(".poll") && self.polls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.polling.store(true, Ordering::SeqCst);
            wait_until(|| self.bus_gone.load(Ordering::SeqCst));
            return Ok(self.batch.clone());
        }
        Ok(json!({"events": []}))
    }
}

fn events(n: usize, with_ack_token: bool) -> Value {
    let evs: Vec<Value> = (0..n)
        .map(|i| {
            let mut e = json!({"peer": "@me:srv", "conversation": "!room:srv", "body": format!("m{i}")});
            if with_ack_token {
                e["ack_token"] = json!(format!("{i}"));
            }
            e
        })
        .collect();
    json!({ "events": evs })
}

fn hook(dropped: &Dropped) -> InboundDroppedAudit {
    let sink = dropped.clone();
    Box::new(move |d: InboundDropped<'_>| sink.lock().unwrap().push((d.channel.0.clone(), d.dropped)))
}

/// Spawn a driver over [`BatchOutlivesTheBus`] answering with `batch`, close
/// the bus while its first poll is out, release the poll, and wait for the
/// driver to exit. Returns what the hook was handed and the worker's calls.
fn run_until_the_bus_closes_mid_poll(
    spec: PolledWorkerSpec,
    batch: Value,
    encode_ack: Option<EncodeAck>,
) -> (Vec<(String, usize)>, Vec<String>) {
    let polling = Arc::new(AtomicBool::new(false));
    let bus_gone = Arc::new(AtomicBool::new(false));
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let calls = BatchOutlivesTheBus {
        batch,
        polling: polling.clone(),
        bus_gone: bus_gone.clone(),
        polls: AtomicUsize::new(0),
        log: log.clone(),
    };
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, Box::new(calls), test_parse, test_encode, encode_ack, None, audit,
        ChannelId(spec.label.into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    wait_until(|| polling.load(Ordering::SeqCst));
    drop(inbound_rx);
    bus_gone.store(true, Ordering::SeqCst);
    join.join().expect("the driver exits cleanly");
    drop(outbound_tx);
    let dropped = dropped.lock().unwrap().clone();
    let log = log.lock().unwrap().clone();
    (dropped, log)
}

/// The headline: on Matrix's shape (no ack method), a batch the worker had
/// already passed when the bus closed is counted, said once at WARN, and
/// handed to the hook — channel and count only.
#[test]
fn a_batch_that_outlives_the_bus_is_counted_said_and_audited_when_nothing_redelivers_it() {
    let spec = PolledWorkerSpec { label: "t-drop-batch", ..spec_without_ack() };
    let (dropped, _log) = run_until_the_bus_closes_mid_poll(spec, events(3, false), None);
    assert_eq!(dropped, [("t-drop-batch".to_string(), 3)]);
    let lines = emitted_refusal_lines_for("t-drop-batch");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("dropped 3 inbound messages"), "{lines:?}");
    assert!(lines[0].0.contains("recording it as channel.inbound_dropped"), "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Warn);
    assert!(!lines[0].0.contains("m0"), "never a body: {lines:?}");
}

/// The common case at a shutdown: the long-poll timed out empty. Nothing was
/// dropped, so nothing is said or recorded.
#[test]
fn an_empty_poll_that_outlives_the_bus_reports_nothing() {
    let spec = PolledWorkerSpec { label: "t-drop-empty", ..spec_without_ack() };
    let (dropped, log) = run_until_the_bus_closes_mid_poll(spec, events(0, false), None);
    assert!(log.iter().any(|m| m.ends_with(".poll")), "POSITIVE CONTROL: the poll ran: {log:?}");
    assert!(dropped.is_empty(), "{dropped:?}");
    assert!(emitted_refusal_lines_for("t-drop-empty").is_empty());
}

/// An ack channel's batch is not acked, so it is redelivered on the next
/// start: not a loss, and not reported as one.
#[test]
fn an_ack_channel_s_batch_is_redelivered_so_not_reported() {
    let spec = PolledWorkerSpec { label: "email-drop", ..spec_with_ack() };
    let (dropped, log) =
        run_until_the_bus_closes_mid_poll(spec, events(2, true), Some(encode_test_ack));
    assert!(!log.iter().any(|m| m.ends_with(".ack")), "nothing acked: {log:?}");
    assert!(dropped.is_empty(), "{dropped:?}");
    assert!(emitted_refusal_lines_for("email-drop").is_empty());
}

/// The bus closes MID-batch: the driver has handed over what fits in the
/// inbound buffer and is blocked on the next send when the receiver drops.
/// That message and the one after it never reached the bus: 2 are dropped.
/// (The buffered ones were handed over; losing them unread is the bus's
/// stop, #832.)
#[test]
fn a_batch_cut_off_mid_send_counts_only_what_was_never_handed_over() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(events(INBOUND_BUFFER + 2, false));
    let spec = PolledWorkerSpec { label: "t-drop-mid", ..spec_without_ack() };
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, None, None, audit, ChannelId("t-drop-mid".into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    wait_until(|| inbound_rx.len() == INBOUND_BUFFER);
    drop(inbound_rx);
    join.join().expect("the driver exits cleanly");
    drop(outbound_tx);
    assert_eq!(*dropped.lock().unwrap(), [("t-drop-mid".to_string(), 2)]);
    let lines = emitted_refusal_lines_for("t-drop-mid");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("dropped 2 inbound messages"), "{lines:?}");
}

/// The mid-batch twin of the ack-channel test above: an email-shaped channel
/// cut off mid-send does not ack what the bus never took, so it is
/// redelivered — no drop line (it would falsely say "does not redeliver") and
/// no hook call. The buffered messages WERE acked, which is the bus's stop
/// (#832), not this path.
#[test]
fn an_ack_channel_cut_off_mid_send_is_redelivered_so_not_reported() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(events(INBOUND_BUFFER + 2, true));
    let spec = PolledWorkerSpec { label: "email-drop-mid", ..spec_with_ack() };
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, Some(encode_test_ack), None, audit,
        ChannelId("email-drop-mid".into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    wait_until(|| inbound_rx.len() == INBOUND_BUFFER);
    drop(inbound_rx);
    join.join().expect("the driver exits cleanly");
    drop(outbound_tx);
    let acks = st.log.lock().unwrap().iter().filter(|(m, _)| m.ends_with(".ack")).count();
    assert_eq!(acks, INBOUND_BUFFER, "POSITIVE CONTROL: only what the bus took was acked");
    assert!(dropped.lock().unwrap().is_empty(), "{:?}", dropped.lock().unwrap());
    assert!(emitted_refusal_lines_for("email-drop-mid").is_empty());
}

/// Pure: the line names the count, agrees in number, and says whether the
/// channel records it — a channel with no sink must not claim a row.
#[test]
fn the_drop_line_counts_and_says_whether_it_is_recorded() {
    use super::super::inbound_drop::format_inbound_drop_report as line;
    assert_eq!(
        line(1, true),
        "dropped 1 inbound message: the bus closed (the channel was restarted or shut down) \
         after the worker had passed it, and this channel does not redeliver; recording it as \
         channel.inbound_dropped"
    );
    assert!(line(4, true).starts_with("dropped 4 inbound messages: "), "{}", line(4, true));
    assert!(line(4, true).contains("had passed them"), "{}", line(4, true));
    assert!(
        line(2, false).ends_with(
            "NOT recorded as channel.inbound_dropped: this channel has no audit sink"
        ),
        "{}",
        line(2, false)
    );
}

/// The row is channel + count + when, and the action is pinned literally:
/// operators query `audit_log` by that string.
#[test]
fn the_inbound_dropped_row_is_channel_count_and_when_only() {
    let v = InboundDropped {
        channel: &ChannelId("matrix".into()),
        dropped: 3,
        observed_at: time::macros::datetime!(2026-10-07 12:34:56 UTC),
    }
    .payload();
    assert_eq!(
        v,
        json!({"channel": "matrix", "dropped": 3, "observed_at": "2026-10-07T12:34:56Z"})
    );
    assert_eq!(crate::channel::actions::INBOUND_DROPPED, "channel.inbound_dropped");
}
