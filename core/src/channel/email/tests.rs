use std::path::PathBuf;
use std::sync::Mutex;

use kastellan_sandbox::{Net, SandboxError, SandboxPolicy};

use super::*;

/// Records every policy it's asked to spawn under, then always refuses —
/// same pattern as `worker_lifecycle::force_route::tests::PolicyCapturingBackend`.
/// `PersistentWorker::spawn_with_backoff` calls the factory synchronously
/// once before returning, so a refusal here surfaces as `spawn_email_worker`
/// returning `Err` — no real process, no sandbox, fully hermetic.
struct PolicyCapturingBackend {
    policies: Mutex<Vec<SandboxPolicy>>,
}
impl SandboxBackend for PolicyCapturingBackend {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<std::process::Child, SandboxError> {
        self.policies.lock().expect("capture mutex poisoned").push(policy.clone());
        Err(SandboxError::Backend("test: spawn refused".into()))
    }
}

fn test_cfg() -> EmailConfig {
    EmailConfig {
        endpoint: "https://127.0.0.1:8443".into(),
        subscription: "agent-inbox".into(),
        address: "agent@example.org".into(),
        authserv_id: "mx.example.net".into(),
        token_file: PathBuf::from("/etc/kastellan/email.token"),
        worker_bin: PathBuf::from("/bin/kastellan-worker-email-in"),
    }
}

#[test]
fn egress_none_calls_the_worker_backend_directly_with_the_expected_allowlist() {
    let backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
    let cfg = test_cfg();
    let err = match spawn_email_worker(backend.clone(), ChannelId("email".into()), &cfg, None, EmailDriverAudit::none()) {
        Err(e) => e,
        Ok(_) => panic!("PolicyCapturingBackend always refuses to spawn"),
    };
    assert!(err.to_string().contains("test: spawn refused"), "{err}");

    let policies = backend.policies.lock().unwrap();
    assert_eq!(
        policies.len(),
        1,
        "egress: None must call the worker backend directly, exactly once, \
         never a sidecar's backend"
    );
    let p = &policies[0];
    assert!(
        matches!(&p.net, Net::Allowlist(v) if v == &["127.0.0.1:8443".to_string()]),
        "net = {:?}",
        p.net
    );
    assert!(
        p.proxy_uds.is_none(),
        "the direct (egress: None) path must never force-route through a proxy UDS"
    );
}

/// A no-op [`crate::worker_lifecycle::force_route::DecisionSinkFactory`] —
/// proves the wiring without a live audit sink, same convention as
/// `force_route::tests::noop_sink_factory`.
fn noop_sink() -> crate::worker_lifecycle::force_route::DecisionSinkFactory {
    Box::new(|| Box::new(|_row: crate::egress::audit::EgressAuditRow| {}))
}

/// The positive half of Task 3's wiring, exercised end to end through the
/// public entry point (not just `upstream_ca::select_ca_for_allowlist` in
/// isolation, which only proves the first link in the chain and is already
/// covered by `egress::upstream_ca`'s own unit tests): a configured anchor
/// for this worker's own endpoint must reach the intercepting SIDECAR's
/// actual `SandboxPolicy` env, built by `proxy_policy` several calls deep
/// (`spawn_email_worker` → factory → `spawn_net_transport` →
/// `spawn_sidecar` → `proxy_policy`). Both backends refuse to spawn
/// (hermetic, no real process), but the sidecar backend still captures the
/// policy it was handed before refusing — same technique as
/// `force_route::tests::a_selected_extra_ca_reaches_the_sidecar_policy_env_and_fs_read`.
#[test]
fn egress_some_intercepts_and_the_selected_anchor_reaches_the_sidecar_policy() {
    let worker_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
    let sidecar_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
    let cfg = test_cfg(); // endpoint https://127.0.0.1:8443 — host is 127.0.0.1
    let ca_map = crate::egress::upstream_ca::parse_upstream_cas(
        r#"{"127.0.0.1":"/etc/kastellan/localmail.pem"}"#,
    )
    .expect("valid config");
    let scratch_root = tempfile::tempdir().expect("scratch root");
    let routing = ForceRoutingConfig::new(
        PathBuf::from("/bin/kastellan-worker-egress-proxy"),
        scratch_root.path().to_path_buf(),
        noop_sink(),
        None,
    )
    .with_upstream_cas(Some(ca_map));
    let egress = EmailEgress {
        sidecar_backend: sidecar_backend.clone() as Arc<dyn SandboxBackend>,
        routing: Arc::new(routing),
    };

    // The sidecar backend always refuses, so this always errors — the
    // point is what got CAPTURED before the refusal, not the outcome.
    let _ = spawn_email_worker(
        worker_backend.clone() as Arc<dyn SandboxBackend>,
        ChannelId("email".into()),
        &cfg,
        Some(egress),
        EmailDriverAudit::none(),
    );

    let sidecar_policies = sidecar_backend.policies.lock().expect("capture mutex poisoned");
    let sidecar_policy =
        sidecar_policies.first().expect("the sidecar spawn must have been attempted");
    assert!(
        sidecar_policy.env.iter().any(|(k, v)| k
            == "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA"
            && v == "/etc/kastellan/localmail.pem"),
        "the selected anchor must reach the sidecar's env: {:?}",
        sidecar_policy.env
    );
    assert!(
        !sidecar_policy.env.iter().any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_DISABLE_MITM"),
        "an intercepting sidecar must NOT carry the disable-MITM key: {:?}",
        sidecar_policy.env
    );
}

/// `upstream_ca::select_ca_for_allowlist`'s two `Err` arms
/// (`MixedAllowlist`, `MultipleKeyedHosts`) both require the worker's OWN
/// allowlist to name at least two distinct hosts: `matched` is bounded by
/// the size of that host set, and once it has exactly one member `others`
/// is always empty (see that function's three-way match on `matched.as_slice()`).
/// `spawn_email_worker` always builds a single-entry allowlist
/// (`vec![format!("{host}:{port}")]`, derived once from `cfg.endpoint`), so
/// for THIS call site `upstream_ca_for` can only ever return `Ok(None)` or
/// `Ok(Some(_))` — never `Err` — no matter what else is in the operator's
/// CA map. This is not a gap: the refusal arms ARE reachable and tested
/// where a worker's allowlist genuinely has more than one host
/// (`egress::upstream_ca`'s own unit tests select the map in isolation;
/// `force_route::tests` exercises the full multi-host spawn path for the
/// general-purpose net-worker case). This test pins the email-specific
/// half of that invariant: a CA entry for a host this worker never dials
/// is simply irrelevant to it, never a refusal.
#[test]
fn an_unrelated_configured_origin_does_not_reach_or_block_the_sidecar() {
    let worker_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
    let sidecar_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
    let cfg = test_cfg(); // endpoint host is 127.0.0.1
    let ca_map = crate::egress::upstream_ca::parse_upstream_cas(
        r#"{"10.0.0.3":"/etc/kastellan/unrelated.pem"}"#, // a different host
    )
    .expect("valid config");
    let scratch_root = tempfile::tempdir().expect("scratch root");
    let routing = ForceRoutingConfig::new(
        PathBuf::from("/bin/kastellan-worker-egress-proxy"),
        scratch_root.path().to_path_buf(),
        noop_sink(),
        None,
    )
    .with_upstream_cas(Some(ca_map));
    let egress = EmailEgress {
        sidecar_backend: sidecar_backend.clone() as Arc<dyn SandboxBackend>,
        routing: Arc::new(routing),
    };

    let _ = spawn_email_worker(
        worker_backend.clone() as Arc<dyn SandboxBackend>,
        ChannelId("email".into()),
        &cfg,
        Some(egress),
        EmailDriverAudit::none(),
    );

    let sidecar_policies = sidecar_backend.policies.lock().expect("capture mutex poisoned");
    let sidecar_policy = sidecar_policies.first().expect(
        "an unrelated CA entry must not block the spawn from reaching the sidecar attempt",
    );
    assert!(
        !sidecar_policy
            .env
            .iter()
            .any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA"),
        "a CA keyed to a host this worker never dials must not be handed to its sidecar: {:?}",
        sidecar_policy.env
    );
    // Load-bearing design decision, pinned by mutation: this channel
    // intercepts UNCONDITIONALLY, not "when an anchor happens to be
    // configured". This call resolves to NO anchor for this worker (the
    // map has only an unrelated host), so if `spawn_email_worker` ever
    // switched to picking the posture from `upstream_extra_ca.is_some()`
    // (i.e. `Mitm::Transparent` when anchorless), this is exactly the
    // case that would flip — and `KASTELLAN_EGRESS_PROXY_DISABLE_MITM` is
    // pushed only for the transparent posture (see `proxy_policy`), so
    // its absence here is the proof the sidecar still intercepts.
    assert!(
        !sidecar_policy
            .env
            .iter()
            .any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_DISABLE_MITM"),
        "an anchorless email channel spawn must still intercept, never fall back to \
         transparent: {:?}",
        sidecar_policy.env
    );
}
