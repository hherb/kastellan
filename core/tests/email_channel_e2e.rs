//! Hermetic email-channel e2e: a fake worker process feeds canned
//! `email.poll` results through the real driver stack —
//! `PersistentWorker` (supervision) → `PolledWorkerDriver` (poll/ack loop) →
//! `EmailChannel` → real `ChannelBus` — into a fake events sink. No
//! localmail, no sandbox, no Postgres, no network. Modelled on
//! `core/tests/matrix_channel_e2e.rs`.
//!
//! Unlike Matrix, email cannot authenticate its own senders, so every event
//! carries [`kastellan_core::channel::PeerEvidence`] and a fake
//! `TokenAuthorizer` here mirrors `DbPeerAuthorizer`'s real rule (see
//! `core/src/channel/auth.rs`): evidence must be present, DMARC must have
//! passed, and the presented token must match. This file proves the whole
//! pipe end to end, including two fixes found while building this slice:
//! `auth_results_order_known: false` must fail closed even over a
//! passing-looking header (case 5), and ids in a poll result's `skipped`
//! list must be acked even though they never become a task, or the
//! localmail cursor wedges forever (case 6).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use kastellan_core::channel::auth::{AuthDecision, PeerAuthorizer, UnauthenticReason};
use kastellan_core::channel::bus::{
    ChannelBus, ChannelEvents, ClaimedReply, CompletedTasks, ReplyDisposition,
};
use kastellan_core::channel::email::{wire, EmailChannel};
use kastellan_core::channel::polled_driver::{DriverAudit, PolledWorkerDriver};
use kastellan_core::channel::{actions, ChannelId, PeerEvidence};
use kastellan_core::worker_lifecycle::persistent::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use kastellan_db::tasks::Lane;
use kastellan_protocol::client::Client;

/// Deadline for every poll-until-condition wait below. Generous relative to
/// the fake worker's near-instant replies, so this never flakes on a loaded
/// CI box, but still fails a genuinely broken wire-up in reasonable time.
const WAIT: Duration = Duration::from_secs(5);

/// Fixed authserv-id every test in this file uses: `wire::set_authserv_id`
/// writes a process-global `OnceLock`, so a second, different value would be
/// silently ignored — all tests here MUST agree (see that fn's docs).
const AUTHSERV_ID: &str = "mx.example.net";

/// Locate the `fake_email_worker` example binary
/// (`<target>/debug/examples/…`), same discovery `matrix_channel_e2e.rs`
/// uses for its fixture.
fn fixture_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // core/
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("examples").join("fake_email_worker")
}

/// One canned `email.poll` event, in the exact shape `email-in` produces
/// (`workers/email-in/src/handler.rs::build_event`) — including the two
/// fields (`auth_results_order_known`, plus `subject`/`date` for realism)
/// that an earlier draft of this harness omitted.
fn event_json(peer: &str, auth_results: Vec<&str>, order_known: bool, body: &str, ack: &str) -> Value {
    json!({
        "peer": peer,
        "conversation": "<mid-1@example.org>",
        "subject": "test",
        "date": "2026-07-28T00:00:00Z",
        "body": body,
        "ack_token": ack,
        "auth_results": auth_results,
        "auth_results_order_known": order_known,
    })
}

/// One canned `skipped` entry (a message `email-in` could not turn into an
/// event at all — no usable `From`, a failed detail fetch).
fn skipped_json(message_id: &str, reason: &str) -> Value {
    json!({ "message_id": message_id, "reason": reason })
}

/// Bus-side recorder: captures enqueued payloads and audited actions.
#[derive(Default, Clone)]
struct RecordingEvents {
    enqueued: Arc<Mutex<Vec<Value>>>,
    audited: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl ChannelEvents for RecordingEvents {
    async fn enqueue(&self, _lane: Lane, payload: Value) -> anyhow::Result<i64> {
        self.enqueued.lock().unwrap().push(payload);
        Ok(1)
    }
    async fn audit(&self, action: &str, _payload: Value) {
        self.audited.lock().unwrap().push(action.to_string());
    }
}

/// Outbound source seam: email has no outbound worker in this slice
/// (`EmailChannel::send` always bails), so this just never yields — the
/// bus's outbound pump idles for the lifetime of the test.
struct NeverCompleted;
#[async_trait::async_trait]
impl CompletedTasks for NeverCompleted {
    async fn next_completed(&mut self) -> Option<i64> {
        std::future::pending::<()>().await;
        unreachable!("NeverCompleted never yields")
    }
    async fn load(&self, _id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        Ok(None)
    }
    /// Claims always succeed: this fake predates the reply claim (#825),
    /// whose own semantics are tested against `bus/tests/catch_up.rs`'s
    /// `Backlog` and real Postgres.
    async fn claim(&self, _id: i64, _d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        let now = time::OffsetDateTime::now_utc();
        Ok(Some(ClaimedReply { created_at: now, finished_at: Some(now) }))
    }
    async fn unsettled(&self, _after_id: i64, _limit: i64) -> anyhow::Result<Vec<i64>> {
        tokio::task::yield_now().await; // a real backlog read yields: no hang on a looping sweep
        Ok(Vec::new())
    }
}

/// Mirrors `DbPeerAuthorizer`'s rule (`core/src/channel/auth.rs`) without a
/// DB: the pairing requires a token, so evidence must be present, DMARC must
/// pass, and the token must match `good-token`.
struct TokenAuthorizer;
#[async_trait::async_trait]
impl PeerAuthorizer for TokenAuthorizer {
    async fn authorize(
        &self,
        _c: &ChannelId,
        _p: &kastellan_core::channel::PeerId,
        evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        let Some(e) = evidence else {
            return AuthDecision::Rejected;
        };
        if !e.dmarc_pass {
            return AuthDecision::RejectedUnauthentic(UnauthenticReason::DmarcFail);
        }
        match e.presented_token.as_deref() {
            Some("good-token") => AuthDecision::Recognised,
            Some(_) => AuthDecision::RejectedUnauthentic(UnauthenticReason::TokenMismatch),
            None => AuthDecision::RejectedUnauthentic(UnauthenticReason::NoToken),
        }
    }
}

/// A running email channel + the seams needed to assert on it.
struct Handle {
    events: RecordingEvents,
    bus: ChannelBus,
    ack_log: PathBuf,
}

impl Handle {
    /// Poll until an enqueued task payload appears, or time out.
    async fn next_enqueued(&self) -> Option<Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            {
                let mut q = self.events.enqueued.lock().unwrap();
                if !q.is_empty() {
                    return Some(q.remove(0));
                }
            }
            if Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// True iff no task is enqueued within `dur` — used for the negative
    /// (rejected) cases, where we must positively wait out the window rather
    /// than racing a single check against the driver's async delivery.
    async fn no_task_within(&self, dur: Duration) -> bool {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            if !self.events.enqueued.lock().unwrap().is_empty() {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.events.enqueued.lock().unwrap().is_empty()
    }

    /// Poll until `action` appears in the audit log, or time out.
    async fn audited(&self, action: &str) -> bool {
        let deadline = Instant::now() + WAIT;
        loop {
            if self.events.audited.lock().unwrap().iter().any(|a| a == action) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Poll the ack log until it has at least one line, then return every
    /// cursor acked so far (one per line, in the order the fake worker
    /// appended them).
    async fn acked_cursors(&self) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(s) = std::fs::read_to_string(&self.ack_log) {
                if !s.trim().is_empty() {
                    return s.lines().map(String::from).collect();
                }
            }
            if Instant::now() > deadline {
                return Vec::new();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// Spawn the fixture worker (plain child, piped stdio, no sandbox) through
/// the PRODUCTION stack: `PersistentWorker` (supervision) +
/// `PolledWorkerDriver` (poll/ack loop) + `EmailChannel`, then start a real
/// `ChannelBus` over it. `events` becomes the first (and only) `email.poll`
/// batch's `events` array; `skipped` becomes that same batch's `skipped`
/// array. Every subsequent poll gets an empty batch, so the driver keeps
/// polling without redelivering (`fake_email_worker`'s contract).
async fn spawn_email_channel(events: Vec<Value>, skipped: Vec<Value>) -> Handle {
    let bin = fixture_bin();
    assert!(
        bin.exists(),
        "fixture not built: {} — run `cargo build -p kastellan-core --example fake_email_worker`",
        bin.display()
    );

    let ack_log = std::env::temp_dir().join(format!(
        "kastellan-email-ack-{}-{}.log",
        std::process::id(),
        kastellan_tests_common::unique_suffix(),
    ));
    let _ = std::fs::remove_file(&ack_log);

    let poll_result = json!({ "events": events, "skipped": skipped }).to_string();
    let ack_log_env = ack_log.display().to_string();

    // Process-global; idempotent — see this const's own docs above.
    wire::set_authserv_id(AUTHSERV_ID);

    let factory: PersistentFactory = Box::new(move || {
        let child = Command::new(&bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("KASTELLAN_FAKE_EMAIL_POLL_RESULT", &poll_result)
            .env("KASTELLAN_FAKE_EMAIL_ACK_LOG", &ack_log_env)
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn fake email worker: {e}"))?;
        let client = Client::from_child(child)
            .map_err(|e| anyhow::anyhow!("connect to fake worker: {e}"))?;
        Ok(Box::new(ClientTransport::from_client(client)) as Box<dyn PersistentTransport>)
    });
    let handle = PersistentWorker::spawn("email-e2e", factory).expect("persistent spawn");

    let (driver, identity) = PolledWorkerDriver::spawn(
        wire::EMAIL_POLLED_SPEC,
        Box::new(handle),
        wire::parse_email_poll,
        wire::encode_email_send,
        Some(wire::encode_email_ack),
        Some(wire::parse_email_skipped),
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .expect("polled driver spawn");
    assert_eq!(identity["address"], "kastellan@example.org", "email.init identity must surface");

    let channel = EmailChannel::from_driver(ChannelId("email".into()), driver);
    let events_sink = RecordingEvents::default();
    let bus = ChannelBus::spawn(
        vec![Box::new(channel)],
        Arc::new(TokenAuthorizer),
        None, // no pairing carve-out: email always carries evidence, see bus::handle_inbound.
        Arc::new(events_sink.clone()),
        Box::new(NeverCompleted),
        None,
    );

    Handle { events: events_sink, bus, ack_log }
}

// ── Case 1: a gated email becomes a task; the token never reaches it ──────

#[tokio::test(flavor = "multi_thread")]
async fn gated_email_becomes_a_task_and_the_token_never_reaches_it() {
    let h = spawn_email_channel(
        vec![event_json(
            "me@example.org",
            vec!["mx.example.net; dmarc=pass"],
            true,
            "kastellan-token: good-token\nwhat is 17*23?",
            "7",
        )],
        vec![],
    )
    .await;

    let payload = h.next_enqueued().await.expect("a gated email must enqueue a task");
    let instruction = payload["instruction"].as_str().unwrap();
    assert_eq!(instruction, "what is 17*23?");
    assert!(!instruction.contains("good-token"), "the token must never reach the task");
    assert_eq!(payload["kind"], "channel");

    h.bus.shutdown().await;
    let _ = std::fs::remove_file(&h.ack_log);
}

// ── Case 2: a forged Authentication-Results header is rejected ────────────

#[tokio::test(flavor = "multi_thread")]
async fn email_with_a_forged_auth_results_header_is_rejected_unauthentic() {
    let h = spawn_email_channel(
        vec![event_json(
            "me@example.org",
            vec!["mx.example.net; dmarc=fail", "evil.example.com; dmarc=pass"],
            true,
            "kastellan-token: good-token\nwhat is 17*23?",
            "7",
        )],
        vec![],
    )
    .await;

    assert!(
        h.no_task_within(Duration::from_secs(2)).await,
        "a forged pass must not enqueue"
    );
    assert!(h.audited(actions::REJECTED_UNAUTHENTIC).await);

    h.bus.shutdown().await;
    let _ = std::fs::remove_file(&h.ack_log);
}

// ── Case 3: a wrong token is rejected ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn email_with_a_wrong_token_is_rejected_unauthentic() {
    let h = spawn_email_channel(
        vec![event_json(
            "me@example.org",
            vec!["mx.example.net; dmarc=pass"],
            true,
            "kastellan-token: WRONG\nwhat is 17*23?",
            "7",
        )],
        vec![],
    )
    .await;

    assert!(h.no_task_within(Duration::from_secs(2)).await);
    assert!(h.audited(actions::REJECTED_UNAUTHENTIC).await);

    h.bus.shutdown().await;
    let _ = std::fs::remove_file(&h.ack_log);
}

// ── Case 4: a delivered event is acked with its own cursor ────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_delivered_event_is_acked() {
    let h = spawn_email_channel(
        vec![event_json(
            "me@example.org",
            vec!["mx.example.net; dmarc=pass"],
            true,
            "kastellan-token: good-token\nhi",
            "7",
        )],
        vec![],
    )
    .await;

    h.next_enqueued().await.expect("task enqueued");
    assert_eq!(h.acked_cursors().await, vec!["7".to_string()]);

    h.bus.shutdown().await;
    let _ = std::fs::remove_file(&h.ack_log);
}

// ── Case 5 (NEW): auth_results_order_known: false fails closed ────────────
//
// Closes a confirmed bypass: localmail groups headers by exact case and
// serde_json's Map is byte-ordered, so an attacker's
// `AUTHENTICATION-RESULTS:` can sort before the operator's genuine
// `Authentication-Results:` header. `wire::parse_email_poll_with` never
// calls `trusted_dmarc_pass` at all when the order isn't confirmed known —
// prove that end to end: a header that WOULD pass on its own must still be
// rejected once `auth_results_order_known` is `false`.

#[tokio::test(flavor = "multi_thread")]
async fn order_unknown_is_rejected_even_though_the_header_looks_like_a_pass() {
    let h = spawn_email_channel(
        vec![event_json(
            "me@example.org",
            vec!["mx.example.net; dmarc=pass"], // would pass the bare header check alone
            false,                              // ...but the order is NOT known.
            "kastellan-token: good-token\nwhat is 17*23?",
            "7",
        )],
        vec![],
    )
    .await;

    assert!(
        h.no_task_within(Duration::from_secs(2)).await,
        "an order-unknown batch must not enqueue even with a passing-looking header"
    );
    assert!(h.audited(actions::REJECTED_UNAUTHENTIC).await);

    h.bus.shutdown().await;
    let _ = std::fs::remove_file(&h.ack_log);
}

// ── Case 6 (NEW): a skipped id is acked even though it never became a task ─
//
// If nothing acks a `skipped` id, the localmail cursor pins on it forever
// and the whole channel wedges permanently — `PolledWorkerDriver` acks these
// via `wire::parse_email_skipped_ids` (a second, ack-only extractor over the
// same raw poll result), independent of the events path. Prove the cursor
// really gets acked end to end, not just that the pure extractor returns the
// right ids.

#[tokio::test(flavor = "multi_thread")]
async fn skipped_ids_are_acked_even_though_they_never_become_a_task() {
    let h = spawn_email_channel(vec![], vec![skipped_json("10", "no usable From address")]).await;

    let acked = h.acked_cursors().await;
    assert!(
        acked.contains(&"10".to_string()),
        "the skipped id must be acked or the cursor wedges forever; acked={acked:?}"
    );
    assert!(
        h.no_task_within(Duration::from_millis(200)).await,
        "a skipped id never becomes a task"
    );

    h.bus.shutdown().await;
    let _ = std::fs::remove_file(&h.ack_log);
}

// ── Task 10: daemon config gate ────────────────────────────────────────────
//
// The whole byte-identical-when-unset guarantee, plus the "partial config is
// an error, never a silent skip" rule — see
// `core/src/channel/email/config.rs`'s module docs for why a half-configured
// channel is worse than no channel at all (a missing authserv-id would fail
// every message closed, which looks exactly like a delivery bug).
//
// What that error DOES is scoped: `main::email_boot::spawn_email_channel`
// turns it into a loud `error!` and returns `None`, so the EMAIL CHANNEL does
// not start but the daemon does (design §6; final whole-branch review,
// Important 5). It no longer aborts startup — the fallback channel must never
// be able to take Matrix, the scheduler, or the graceful-shutdown path down
// with it. `spawn_email_channel` itself needs the real daemon pool + sandbox
// backends, so it is not directly unit-testable here; what IS pinned here is
// the contract it consumes (`Ok(None)` vs `Err`), and `email_boot.rs`'s own
// signature (`-> Option<ChannelBus>`, no `Result`) is what makes the abort
// unreachable by construction.

use kastellan_tests_common::env::{env_lock, EnvVarGuard};

#[test]
fn unset_email_config_yields_no_channel() {
    let _lock = env_lock();
    let _e = EnvVarGuard::unset("KASTELLAN_EMAIL_ENDPOINT");
    let _s = EnvVarGuard::unset("KASTELLAN_EMAIL_SUBSCRIPTION");
    let _a = EnvVarGuard::unset("KASTELLAN_EMAIL_ADDRESS");
    let _i = EnvVarGuard::unset("KASTELLAN_EMAIL_AUTHSERV_ID");
    let _t = EnvVarGuard::unset("KASTELLAN_EMAIL_TOKEN_FILE");
    let cfg = kastellan_core::channel::email::config::EmailConfig::from_env().unwrap();
    assert!(cfg.is_none(), "no email env must mean no email channel");
}

#[test]
fn partial_email_config_is_an_error_not_a_silent_skip() {
    let _lock = env_lock();
    let _s = EnvVarGuard::unset("KASTELLAN_EMAIL_SUBSCRIPTION");
    let _a = EnvVarGuard::unset("KASTELLAN_EMAIL_ADDRESS");
    // authserv-id missing: starting without it would fail every message closed
    // and look like a delivery bug rather than a misconfiguration.
    let _i = EnvVarGuard::unset("KASTELLAN_EMAIL_AUTHSERV_ID");
    let _t = EnvVarGuard::unset("KASTELLAN_EMAIL_TOKEN_FILE");
    let _e = EnvVarGuard::set("KASTELLAN_EMAIL_ENDPOINT", "https://10.0.0.3:8443");
    let err = kastellan_core::channel::email::config::EmailConfig::from_env()
        .expect_err("a partial config must be an error, not a silent partial start")
        .to_string();
    // The daemon no longer aborts on this, so the message IS the remedy: it
    // must name EVERY missing variable, or the operator restarts once per typo.
    for key in [
        "KASTELLAN_EMAIL_SUBSCRIPTION",
        "KASTELLAN_EMAIL_ADDRESS",
        "KASTELLAN_EMAIL_AUTHSERV_ID",
        "KASTELLAN_EMAIL_TOKEN_FILE",
    ] {
        assert!(err.contains(key), "{key} must be named in the operator-facing error: {err}");
    }
}
