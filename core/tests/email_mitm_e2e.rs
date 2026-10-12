//! Hermetic MITM round-trip for the **email fallback channel** (#494 slice 1):
//! the real `kastellan-worker-email-in` binary, under the real platform jail,
//! force-routed through a real intercepting egress sidecar, polling a
//! **self-signed HTTPS** localmail mock.
//!
//! This is the leg that did not exist before this branch. Slice 1's sidecar was
//! a transparent tunnel: the worker terminated TLS itself against the origin,
//! its `web-common` client trusts webpki roots only, and so a self-signed
//! localmail — the only kind a personal deployment has — was flatly unreachable
//! by this channel. Interception moves the origin handshake into the sidecar,
//! where the operator's anchor (`KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA`, #492) can
//! widen trust for exactly that one private origin.
//!
//! Everything earlier in the branch is proved at the policy-construction level
//! (`channel::email`'s own tests capture the sidecar `SandboxPolicy` and assert
//! the anchor reached its env). Those prove the *wiring*. This file is the only
//! place that proves the wiring **works** — that bytes actually cross a
//! re-originated TLS connection the anchor validated.
//!
//! Skips as-pass when the sandbox or a required binary is missing; no Postgres
//! is involved (the channel path is DB-free — `audit_ack_only` is `None` here).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kastellan_core::channel::email::config::EmailConfig;
use kastellan_core::channel::email::{spawn_email_worker, EmailDriverAudit, EmailEgress};
use kastellan_core::channel::{Channel, ChannelId, IncomingMessage};
use kastellan_core::egress::audit::EgressAuditRow;
use kastellan_core::egress::upstream_ca::parse_upstream_cas;
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
use kastellan_sandbox::SandboxBackend;
use kastellan_tests_common::egress_forcing::short_scratch_root;
use kastellan_tests_common::mock_localmail::{
    spawn_mock_localmail_tls, CANNED_AUTHSERV_ID, CANNED_BODY_TEXT, CANNED_FROM_ADDRESS,
    CANNED_MESSAGE_ID_HEADER,
};
use kastellan_tests_common::{
    backend, egress_proxy_bin_or_skip, skip_if_sandbox_unavailable, unique_suffix,
    workspace_target_binary,
};

/// The email worker binary this file drives.
const EMAIL_WORKER: &str = "kastellan-worker-email-in";

/// Loopback literal the mock always binds, the `upstream_cas` key below, and the
/// `host` every decision about this origin carries.
const ORIGIN_HOST: &str = "127.0.0.1";

/// How long the positive case waits for the first inbound event. Generous: it
/// covers sidecar bring-up (up to a 5s readiness budget), the jailed worker
/// spawn, `email.init`, and one `GET /v1/changes` + `GET /v1/messages/{id}`
/// pair through the tunnel — all of which are sub-second in practice.
const POSITIVE_WAIT: Duration = Duration::from_secs(30);

/// How long the negative case waits before concluding no event will arrive. The
/// worker's poll fails on the sidecar's upstream handshake and the driver
/// retries every 200ms, so the `mitm_failed` decision lands within the first
/// second; the rest of this window is margin against a loaded box.
const NEGATIVE_WAIT: Duration = Duration::from_secs(8);

/// Multi-threaded so the mock's accept/serve tasks keep running while
/// `spawn_email_worker` (a synchronous call: supervisor spawn + sidecar
/// readiness wait + the blocking `email.init`) occupies a worker thread.
fn driver_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

/// Common skip gate. Returns the egress-proxy binary path, or `None` after
/// printing the `[SKIP]` line for whichever prerequisite is missing.
fn proxy_or_skip() -> Option<std::path::PathBuf> {
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let proxy = egress_proxy_bin_or_skip()?;
    if !workspace_target_binary(EMAIL_WORKER).exists() {
        eprintln!("\n[SKIP] {EMAIL_WORKER} not built; run cargo build --workspace\n");
        return None;
    }
    Some(proxy)
}

/// Drive the real email channel against a self-signed HTTPS localmail mock
/// through a real intercepting sidecar. `with_extra_ca` toggles the ONE variable
/// under test: whether the operator configured the mock's cert as the upstream
/// extra CA for this origin. Everything else — the worker binary, the jail, the
/// force-routing coupling, the mock — is identical between the two callers, so a
/// difference in outcome can only be attributed to the anchor.
///
/// Returns the first inbound message (if any arrived within `wait`), every
/// egress decision the sidecar emitted, and the mock's ephemeral port — the
/// callers need that last one to scope their decision assertions to this origin
/// rather than to any decision that happens to carry the right flag.
async fn run_forced_email_poll_over_tls(
    proxy: &Path,
    with_extra_ca: bool,
    wait: Duration,
) -> (Option<IncomingMessage>, Vec<EgressAuditRow>, u16) {
    let (mock, cert_pem) = spawn_mock_localmail_tls().await;
    let origin_port = mock
        .base_url
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("mock base_url should end in :<port>; got {}", mock.base_url));

    // The anchor and the bearer token both need absolute paths readable from
    // inside their respective jails (`proxy_policy` fs_reads the CA;
    // `build_email_policy` fs_reads the token file).
    let files = tempfile::tempdir().expect("anchor/token tempdir");
    let ca_path = files.path().join("localmail-ca.pem");
    std::fs::write(&ca_path, &cert_pem).expect("write localmail ca pem");
    let token_file = files.path().join("email-token");
    std::fs::write(&token_file, b"test-bearer-token").expect("write token");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
        .expect("chmod token 0600");

    // Short root: `spawn_email_worker` nests `<root>/email-<pid>-<seq>/egress.sock`
    // and that must fit macOS's 104-byte `sun_path`.
    let scratch_root = short_scratch_root(&format!("emitm-{}", unique_suffix()));

    let rows: Arc<Mutex<Vec<EgressAuditRow>>> = Arc::new(Mutex::new(Vec::new()));
    let make_sink: DecisionSinkFactory = {
        let rows = Arc::clone(&rows);
        Box::new(move || {
            let rows = Arc::clone(&rows);
            Box::new(move |row: EgressAuditRow| rows.lock().expect("rows mutex").push(row))
        })
    };

    // The mock binds loopback, so the endpoint host is the IP literal `127.0.0.1`
    // — which is also the key `upstream_ca`'s single-private-origin rule expects.
    let upstream_cas = with_extra_ca.then(|| {
        parse_upstream_cas(&format!(r#"{{"{ORIGIN_HOST}": "{}"}}"#, ca_path.display()))
            .expect("valid upstream extra-CA config")
    });

    let routing = ForceRoutingConfig::new(proxy.to_path_buf(), scratch_root.clone(), make_sink, None)
        .with_upstream_cas(upstream_cas);

    let cfg = EmailConfig {
        endpoint: mock.base_url.clone(),
        subscription: "agent-inbox".into(),
        address: "kastellan@example.org".into(),
        // Matches the mock's canned `Authentication-Results` stamp, so a header
        // that survives the tunnel produces `dmarc_pass: true` (see the positive
        // test's assertions for why that is a second, independent round-trip
        // signal rather than a gate test).
        authserv_id: CANNED_AUTHSERV_ID.into(),
        token_file,
        worker_bin: workspace_target_binary(EMAIL_WORKER),
    };

    let worker_backend: Arc<dyn SandboxBackend> = Arc::from(backend());
    let sidecar_backend: Arc<dyn SandboxBackend> = Arc::from(backend());
    let egress = EmailEgress { sidecar_backend, routing: Arc::new(routing) };

    // `email.init` is answered from the worker's own env — no network — so this
    // succeeds in BOTH cases. The anchor's absence surfaces later, on the first
    // poll, which is exactly where the negative control asserts.
    let spawned = spawn_email_worker(worker_backend, ChannelId("email".into()), &cfg, Some(egress), EmailDriverAudit::none())
        .expect("email channel bring-up (sidecar + jailed worker + email.init)");
    let mut channel = spawned.channel;

    let received = tokio::time::timeout(wait, channel.recv()).await.ok().flatten();

    // Tear down FIRST: dropping the channel drops both driver endpoints, so the
    // driver thread exits, drops its `PersistentHandle`, and the supervisor reaps
    // the worker and its sidecar.
    drop(channel);

    // The sidecar's decision-ingest thread is deliberately DETACHED (see
    // `EgressSidecar`), so the teardown above only *starts* its drain: the proxy
    // dies, the thread sees EOF on its stdout, flushes the decision lines still
    // buffered, and exits. Reading `rows` immediately would race that drain —
    // worst for a connection's LAST decision, which is precisely the negative
    // control's `mitm_failed: …` (emitted only after the upstream handshake
    // fails). Poll to quiescence instead: the count must hold steady across two
    // consecutive polls.
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut last_len, mut stable) = (usize::MAX, 0u8);
    while Instant::now() < deadline && stable < 2 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let len = rows.lock().expect("rows mutex").len();
        if len == last_len && len > 0 {
            stable += 1;
        } else {
            (last_len, stable) = (len, 0);
        }
    }

    let _ = std::fs::remove_dir_all(&scratch_root);
    let captured = std::mem::take(&mut *rows.lock().expect("rows mutex"));
    (received, captured, origin_port)
}

/// Does this decision concern the mock origin **specifically** — matching `host`
/// *and* `port`, never just one?
///
/// Both halves are load-bearing, and the host half is the weaker one here: the
/// mock binds the bare loopback literal, the least discriminating host string in
/// the suite. On a real box `127.0.0.1` is also SearxNG, the embed broker and
/// localmail proper, and #448 already cost a round-trip to a bare-host egress
/// assertion that false-positived on a co-resident service. Matching the port too
/// is the convention `is_allowed_row_for` in
/// `web_fetch_firecracker_egress_e2e.rs` set after #469: "a bare-host check would
/// pass on any decision mentioning the host" — that all-port-grant lesson applies
/// to assertions, not only to allowlists.
///
/// The sidecar's allowlist here happens to hold exactly one `host:port`, so today
/// this filter cannot reject a row the unscoped check would have accepted — it is
/// a no-op. That is precisely why it is worth pinning while it is free: it keeps
/// the assertion honest the day the allowlist grows a second entry, a retry lands
/// a decision for another origin, or this file is copied as the template for the
/// next channel's MITM e2e.
fn is_for_origin(r: &EgressAuditRow, port: u16) -> bool {
    r.payload["host"].as_str() == Some(ORIGIN_HOST)
        && r.payload["port"].as_u64() == Some(u64::from(port))
}

/// Hermetic full round-trip: the REAL email-in worker, force-routed in MITM
/// mode, polls a self-signed HTTPS localmail mock; the proxy MITM-terminates the
/// worker's TLS and re-originates upstream, validating the origin against webpki
/// **plus** the operator-provided extra CA.
///
/// **The load-bearing assertions are the polled data.** Bytes only reach the
/// worker if the proxy's upstream handshake against the self-signed origin
/// validated, and each asserted field pins a different hop:
///
/// * `peer` / `body` come from `GET /v1/messages/{id}`'s `from.address` and
///   `body_text` — so they prove the *second* request round-tripped, which in
///   turn required the *first* (`GET /v1/changes`) to have yielded the id.
/// * `conversation` is the `Message-ID` **header**, served only under
///   `?headers=full`. Without it `build_event` falls back to the synthetic
///   `localmail:<id>`, so asserting the real value proves the header block
///   survived the tunnel rather than being silently absent.
/// * `dmarc_pass` needs `Authentication-Results` from that same header block and
///   is computed in core, so it is a second, independent witness of the same
///   thing. It is asserted here as a round-trip signal, not as a gate test —
///   the gate itself is covered by `channel/email/wire.rs` and
///   `email_channel_e2e.rs`.
///
/// `tls_intercepted: true` is asserted too, but note it is a *weaker* signal
/// than it reads: the proxy emits that decision when it takes the MITM branch,
/// BEFORE `run_mitm` performs the upstream handshake, so on its own it proves
/// only "not transparently tunnelled", never "re-origination succeeded". A test
/// that asserted only this would pass with re-origination completely broken.
/// It is scoped to a decision for the mock origin's own `host:port` (see
/// `is_for_origin`) rather than to any row carrying the flag.
#[test]
fn force_routed_email_poll_round_trips_through_mitm_sidecar() {
    let Some(proxy) = proxy_or_skip() else {
        return;
    };

    driver_runtime().block_on(async {
        let (received, rows, origin_port) =
            run_forced_email_poll_over_tls(&proxy, true, POSITIVE_WAIT).await;

        let msg = received.unwrap_or_else(|| {
            panic!(
                "no inbound event arrived within {POSITIVE_WAIT:?}: the worker's poll never \
                 round-tripped through the MITM sidecar to the self-signed origin. \
                 Decisions: {:?}",
                decision_summary(&rows)
            )
        });
        assert_eq!(msg.peer.0, CANNED_FROM_ADDRESS, "peer comes from the origin's from.address");
        assert_eq!(msg.body, CANNED_BODY_TEXT, "body comes from the origin's body_text");
        assert_eq!(
            msg.conversation.0, CANNED_MESSAGE_ID_HEADER,
            "conversation is the origin's Message-ID header — the synthetic `localmail:<id>` \
             fallback here would mean the ?headers=full detail fetch came back header-less"
        );
        let evidence = msg.evidence.expect("email always supplies evidence, never None");
        assert!(
            evidence.dmarc_pass,
            "the Authentication-Results header must have survived the tunnel too"
        );

        assert!(
            rows.iter().any(|r| is_for_origin(r, origin_port)
                && r.action == "egress.allowed"
                && r.payload["tls_intercepted"] == serde_json::Value::Bool(true)),
            "expected an MITM-intercepted allow decision (tls_intercepted: true) for the mock \
             origin {ORIGIN_HOST}:{origin_port}; got {:?}",
            decision_summary(&rows)
        );
    });
}

/// Negative control: the identical setup with NO operator anchor must FAIL, and
/// fail **on the upstream handshake** — the proxy re-originates against webpki
/// roots only and rejects the self-signed origin.
///
/// Without this, the positive test cannot distinguish "the anchor worked" from
/// "TLS was never actually verified": a proxy that skipped origin verification
/// entirely would pass the positive test just as happily.
///
/// "No event arrived" alone would be satisfied by ANY failure (worker crash, a
/// mock that never bound, a jail refusal), so the control would silently stop
/// being a control the day something upstream of TLS broke. The decision
/// assertion is what pins it to the re-origination leg.
///
/// That assertion matches `mitm_failed: origin TLS handshake` and **not** the
/// bare `mitm_failed:` prefix, which would be far too broad to mean what this
/// test claims. `classify_mitm_error`
/// (`workers/egress-proxy/src/proxy.rs`) stamps that prefix onto *every*
/// non-pin intercept failure, and `mitm.rs` reaches it from five sites —
/// including `worker TLS handshake: …`, which fires BEFORE the origin is ever
/// dialled, and `dial origin …: connection refused`. So a bare-prefix match
/// would be satisfied by broken per-instance CA provisioning or by an origin
/// that never bound, neither of which has anything to do with the missing
/// operator anchor. Only `origin TLS handshake: …` is the re-origination leg
/// this test exists to exercise (observed in full:
/// `mitm_failed: origin TLS handshake: invalid peer certificate: UnknownIssuer`).
/// It is `host:port`-scoped for the same reason the positive assertion is (see
/// `is_for_origin`): the reason string alone would not say *which* origin failed.
#[test]
fn without_the_operator_anchor_the_mitm_leg_fails_closed() {
    let Some(proxy) = proxy_or_skip() else {
        return;
    };

    driver_runtime().block_on(async {
        let (received, rows, origin_port) =
            run_forced_email_poll_over_tls(&proxy, false, NEGATIVE_WAIT).await;

        assert!(
            received.is_none(),
            "without the upstream extra CA the MITM re-origination must reject the self-signed \
             origin, so no event can round-trip; got {received:?}"
        );
        assert!(
            rows.iter().any(|r| is_for_origin(r, origin_port)
                && r.payload["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("mitm_failed: origin TLS handshake"))),
            "the failure must be the proxy's UPSTREAM handshake rejecting the self-signed origin \
             at {ORIGIN_HOST}:{origin_port} (a `mitm_failed: origin TLS handshake: …` decision) — \
             a bare `mitm_failed:` would also match a worker-side handshake or a refused dial, \
             which this test is not about; got {:?}",
            decision_summary(&rows)
        );
    });
}

/// Deduplicated `host:port / action / reason / tls_intercepted × count` view of
/// the captured decisions, for assertion failure messages.
///
/// Deliberately not a raw `{:?}` over the rows: the driver retries a failing
/// poll every 200ms, so a failure prints ~40 near-identical multi-field JSON
/// objects and the one line that explains it (`mitm_failed: …`) is invisible in
/// the noise. Collapsing to the fields that discriminate keeps the message
/// diagnostic. `tls_intercepted` is kept because it is exactly the field a
/// reader will want to check against the weaker-signal caveat above — it is
/// `true` on the failing rows too.
///
/// `host:port` is kept because both assertions are scoped by it (`is_for_origin`).
/// Without it here, the one failure mode the scoping introduces — decisions
/// arrived, but for some other origin — would print a summary that looks like it
/// should have matched, and the reader would have no way to see why it did not.
fn decision_summary(rows: &[EgressAuditRow]) -> Vec<String> {
    let mut seen: Vec<(String, usize)> = Vec::new();
    for r in rows {
        let key = format!(
            "{}:{} {} reason={:?} tls_intercepted={}",
            r.payload["host"].as_str().unwrap_or("<no host>"),
            r.payload["port"],
            r.action,
            r.payload["reason"],
            r.payload["tls_intercepted"]
        );
        match seen.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => seen.push((key, 1)),
        }
    }
    seen.into_iter().map(|(k, n)| format!("{k} ×{n}")).collect()
}
