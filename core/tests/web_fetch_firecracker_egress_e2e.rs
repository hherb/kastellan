#![cfg(target_os = "linux")]
//! web-fetch × Firecracker micro-VM: the worker runs inside a VM and reaches the
//! host egress proxy over the slice-4a vsock channel.
//!
//! Both tiers drive the **production** entry through `WebFetchManifest::resolve`
//! under `KASTELLAN_WEB_FETCH_USE_MICROVM=1` (see [`web_fetch_vm_entry_for`]), so
//! the in-guest binary path, the rootfs filename, the memory budget and the whole
//! env set come from the code that runs in production rather than from literals
//! restated here.
//!
//! ## Tiers
//!
//! * `web_fetch_vm_reaches_proxy_with_ca_delivered` — the transport gate. A host
//!   `UnixListener` stub stands in for the egress proxy at the worker's
//!   `proxy_uds`; a force-routed web-fetch VM boots and one `web.fetch` is driven
//!   through it; we assert the stub RECEIVES the worker's `CONNECT <host>:443`
//!   line. The worker can only emit CONNECT after loading the in-guest CA
//!   (`make_get` fails closed on an unreadable `KASTELLAN_EGRESS_PROXY_CA`), so
//!   that single assertion proves VM boot + force-routing + the vsock relay + CA
//!   delivery. The stub then 503s and closes, so everything *after* the CONNECT
//!   — the upstream leg, the response body, the extracted text — is out of its
//!   reach by construction.
//!
//! * `real_web_fetch_through_sidecar` — the origin-validation tier: a real
//!   `web.fetch` completing through the **real** egress-proxy sidecar, in MITM
//!   mode, returning readable text. This is the last mile the stub cannot
//!   complete. See its doc comment for why the origin has to be a real public one
//!   and what MITM adds over browser-driver's transparent tunnel.
//!
//! ## Running them
//!
//! Both are `#[ignore]`d and DGX-only: they need `/dev/kvm`, `/dev/vhost-vsock`,
//! the web-fetch rootfs (REBUILD via `build-web-fetch-rootfs.sh`) and the
//! `kastellan-microvm-run` RELEASE launcher; the second also needs the
//! egress-proxy binary and outbound HTTPS. `--test-threads=1` matters — each
//! tier boots its own VM.
//!
//! ```sh
//! export PATH=$HOME/.local/bin:$PATH
//! cargo build --release -p kastellan-microvm-run
//! cargo build -p kastellan-worker-egress-proxy
//! bash scripts/workers/microvm/build-web-fetch-rootfs.sh
//! cargo test -p kastellan-core --test web_fetch_firecracker_egress_e2e -- \
//!     --test-threads=1 --ignored --nocapture
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kastellan_core::broker::BrokerConfigs;
use kastellan_core::egress::audit::EgressAuditRow;
use kastellan_core::scheduler::ToolEntry;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
use kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager};
use kastellan_core::worker_manifest::{Resolution, ResolveCtx, WorkerManifest};
use kastellan_core::workers::web_fetch::WebFetchManifest;
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};
use kastellan_tests_common::{
    bring_up_pg_cluster, egress_proxy_bin_or_skip, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_origin_unreachable, skip_if_sandbox_unavailable, unique_suffix,
};

/// The rootfs image both tiers boot. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "web-fetch.ext4";

/// The public origin both tiers fetch. Small and about as stable as the open
/// web gets — but see [`ORIGIN_TITLE`] for what "stable" is worth here.
const ORIGIN_HOST: &str = "example.com";
const ORIGIN_URL: &str = "https://example.com/";

/// The `<title>` the extracted document must carry — the page's identity.
///
/// Anchoring on the title rather than on a sentence from the body is a
/// deliberate correction, not a weakening. The first run of this tier asserted
/// the body contained `"Example Domain"` and failed: IANA has since reworded the
/// prose (it now reads "This domain is for use in documentation examples…"),
/// and readability puts the `<h1>` in `title`, not in `text`. Body copy on a
/// third-party page is content we do not control and cannot pin, so matching a
/// phrase from it would make an unrelated edit upstream look like an egress
/// regression. The `<title>` has outlived several such rewrites.
const ORIGIN_TITLE: &str = "Example Domain";

/// A floor on the extracted readable text. Proves a real body came back and
/// survived extraction — the thing the stub tier structurally cannot reach —
/// without pinning any particular wording of it.
const MIN_EXTRACTED_TEXT_BYTES: usize = 50;



/// The production micro-VM entry, resolved exactly as the daemon resolves it:
/// through `WebFetchManifest::resolve` with `KASTELLAN_WEB_FETCH_USE_MICROVM=1`.
///
/// Going through the manifest rather than calling `web_fetch_firecracker_entry`
/// with a hand-typed guest path is what binds these tests to production. The
/// failure it guards is the one that cost a debugging session on the
/// browser-driver arc: if the in-rootfs binary path drifts from what
/// `build-web-fetch-rootfs.sh` stages, PID1 `execv`s a path that does not exist
/// inside the guest, panics, the VM boot-loops, and the dispatch simply hangs to
/// wall-clock — presenting as a channel hang with no error naming the cause.
///
/// `exists` returns false throughout on purpose: the VM branch must need **no**
/// host-side worker binary on disk, because on a VM-only deployment there is
/// none. Host mode would return `Misconfigured` under the same probes.
///
/// `hosts` are handed over exactly as `tool_allowlists` supplies them — bare
/// hosts, which `allowlist_to_net_entries` maps to `{host}:443` (#469: a bare
/// host reaching the proxy unmapped is an all-port grant).
fn web_fetch_vm_entry_for(hosts: &[String]) -> ToolEntry {
    let dir = image_dir();
    let get_env = move |k: &str| match k {
        "KASTELLAN_WEB_FETCH_USE_MICROVM" => Some("1".to_string()),
        "KASTELLAN_MICROVM_DIR" => Some(dir.clone()),
        _ => None,
    };
    let exists = |_p: &std::path::Path| false;
    let hosts = hosts.to_vec();
    let allowlist = move |_t: &str| hosts.clone();
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &|_p| true,
        exe_dir: None,
        canonicalize: &|_p| None,
        allowlist: &allowlist,
    };
    match WebFetchManifest.resolve(&ctx) {
        Resolution::Register(entry) => entry,
        Resolution::Disabled { detail } => {
            panic!("VM branch must register, got Disabled: {detail}")
        }
        Resolution::Misconfigured { detail } => panic!(
            "VM branch must register without any host-side worker binary on disk, got \
             Misconfigured: {detail}"
        ),
    }
}




async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-fetch-firecracker-egress-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Mint a self-signed CA PEM the in-VM worker will trust as KASTELLAN_EGRESS_PROXY_CA.
/// The worker's make_get fails closed on an unreadable/invalid CA, so a parseable
/// cert is required for it to build ProxyConnectGet and emit CONNECT at all.
fn write_test_ca(path: &std::path::Path) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let key_pair = KeyPair::generate().expect("keypair");
    let mut params =
        CertificateParams::new(vec!["egress-proxy.test".to_string()]).expect("params");
    // Mint a proper CA (matches workers/egress-proxy/src/ca.rs) so the cert is a
    // valid rustls/webpki trust anchor — a NoCa leaf is latent fragility there.
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key_pair).expect("self-signed");
    std::fs::write(path, cert.pem()).expect("write ca.pem");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-fetch rootfs"]
async fn web_fetch_vm_reaches_proxy_with_ca_delivered() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }

    // Skip-as-pass without PG/supervisor/sandbox (dispatch needs a pool for audit).
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wf-d",
        "wf-l",
        &format!("kastellan-supervisor-test-pg-webfetch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Host scratch under /tmp (a share anchor); holds the stub proxy UDS + ca.pem.
    let dir = std::env::temp_dir().join(format!("kastellan-s4b-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("egress.sock");
    let ca_path = dir.join("ca.pem");
    let _ = std::fs::remove_file(&uds_path);
    write_test_ca(&ca_path);

    // Stub "proxy": on accept, read the first request line and report it back,
    // then send a fast 503 so the worker's fetch fails fast instead of blocking.
    let listener = UnixListener::bind(&uds_path).unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line.clone());
            }
            let mut w = stream;
            let _ = w.write_all(b"HTTP/1.1 503 stub\r\n\r\n");
        }
    });

    // Force-routed web-fetch VM entry: set proxy_uds + the CA env + CA in fs_read,
    // exactly as rewrite_worker_policy does on the production path.
    let mut entry = web_fetch_vm_entry_for(&[ORIGIN_HOST.to_string()]);
    entry.policy.proxy_uds = Some(uds_path.clone());
    entry.policy.env.push((
        "KASTELLAN_EGRESS_PROXY_CA".into(),
        ca_path.to_string_lossy().into_owned(),
    ));
    entry.policy.fs_read.push(ca_path.clone());

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn web-fetch in micro-VM");

    // Drive one web.fetch on a background task; we only need it to make the worker
    // attempt egress. The assertion is the stub receiving CONNECT.
    let fetch = tokio::spawn(async move {
        let _ = dispatch(
            &pool,
            &Vault::new(),
            &mut worker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({ "url": ORIGIN_URL }),
        )
        .await;
        (worker, pool)
    });

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("stub proxy never received the in-VM worker's CONNECT (transport or CA broken)");
    assert!(
        got.starts_with(&format!("CONNECT {ORIGIN_HOST}:443")),
        "expected CONNECT {ORIGIN_HOST}:443, got {got:?}"
    );

    let (worker, pool) = fetch.await.expect("fetch task joins");
    let _ = worker.close();
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// True iff `d` is the sidecar's allow verdict for `host:443`. Shared between
/// the ingest poll and the test's assertion so the two cannot drift — the poll
/// waits for exactly the row the assertion will then look for.
///
/// NB the action is `egress.allowed`, not `allowed` — `decision_to_audit`
/// namespaces every verdict under `egress.` (its siblings are
/// `egress.blocked.credential_leak` / `egress.blocked.tls_pin`). Host AND port
/// are matched: a bare-host check would pass on any decision mentioning the
/// host (#469's all-port-grant lesson applies to assertions too).
fn is_allowed_row_for(d: &str, host: &str) -> bool {
    d.starts_with("egress.allowed")
        && d.contains(&format!("\"host\":\"{host}\""))
        && d.contains("\"port\":443")
}

/// Drive one `web.fetch` through the production force-routing manager and return
/// `(dispatch result, sidecar decisions, wall clock)`.
///
/// Deliberately uses `SingleUseLifecycle::with_force_routing(...).acquire(...)`
/// rather than a hand-wired sidecar spawn. That is strictly stronger for a
/// specific reason: the manager derives `disable_mitm` by calling
/// `force_route::disable_mitm_for(worker_name)` itself, so whether this
/// connection is MITM'd is a **production** decision the test observes. A
/// hand-wired spawn would let the test pass `disable_mitm: false` to itself and
/// then assert it, consulting no production code at all.
async fn fetch_in_vm_through_real_sidecar(
    pool: &sqlx::PgPool,
    proxy_bin: PathBuf,
    hosts: &[String],
    url: &str,
) -> (
    Result<serde_json::Value, kastellan_core::tool_host::ToolHostError>,
    Vec<String>,
    Duration,
) {
    let entry = web_fetch_vm_entry_for(hosts);

    let decisions: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_src = Arc::clone(&decisions);
    let make_sink: DecisionSinkFactory = Box::new(move || {
        let d = Arc::clone(&sink_src);
        Box::new(move |row: EgressAuditRow| {
            d.lock().unwrap().push(format!("{} {}", row.action, row.payload));
        })
    });
    // Scratch under /tmp: a SHARE_ANCHOR, so the confined VMM jail can bind the
    // sidecar's UDS and the guest→host vsock relay can reach it.
    let force = Arc::new(ForceRoutingConfig::new(
        proxy_bin,
        std::env::temp_dir(),
        make_sink,
        None, // no cert pins
    ));
    let sandboxes = Arc::new(SandboxBackends::default_for_current_os());
    let mgr =
        SingleUseLifecycle::with_force_routing(sandboxes, Some(force), BrokerConfigs::default());

    let started = std::time::Instant::now();
    let mut handle = mgr
        .acquire("web-fetch", &entry)
        .await
        .expect("acquire a force-routed web-fetch VM worker through the manager");
    let result = dispatch(
        pool,
        &Vault::new(),
        handle.worker_mut(),
        "web-fetch",
        "web.fetch",
        serde_json::json!({ "url": url }),
    )
    .await;
    let elapsed = started.elapsed();

    // Decisions land on a detached ingest thread reading the proxy's stdout;
    // poll until the allowed row for this fetch is visible, not merely until
    // the vec is non-empty — were the fetch ever to produce more than one row
    // (a redirect, a connection retry), the needed row could land a beat after
    // the first and a first-row snapshot would miss it. Falls back to the
    // timeout so a blocked or decision-less run still returns what it saw for
    // the caller's failure message.
    let mut snapshot = Vec::new();
    for _ in 0..60 {
        snapshot = decisions.lock().unwrap().clone();
        if snapshot.iter().any(|d| hosts.iter().any(|h| is_allowed_row_for(d, h))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (result, snapshot, elapsed)
}

/// The origin-validation tier: a real `web.fetch` completing through the **real**
/// egress-proxy sidecar, in MITM mode, from a worker inside the micro-VM.
///
/// ## What the stub tier cannot reach
///
/// `web_fetch_vm_reaches_proxy_with_ca_delivered` stops at the `CONNECT` line —
/// its stub answers 503 and closes, so the fetch deliberately fails and its
/// *result* carries no information. Everything past that point is untested there:
/// the proxy's upstream leg to the origin, TLS termination and re-origination,
/// the leaf minted from the per-instance CA, the response body, the readability
/// extraction, and the JSON-RPC reply carrying real text.
///
/// ## Why the origin must be a real public one
///
/// A hermetic self-signed loopback origin does not work here, and the reason is
/// one hop further along than browser-driver's. web-fetch runs **with** MITM
/// (`force_route::disable_mitm_for` does not name it), so the *proxy* — not the
/// worker — validates the origin's certificate, and it does so against
/// `webpki_roots` only: `egress-proxy`'s `build_upstream_client_config` has no
/// extra-root knob, and a pin set only ever *narrows* that trust. Adding one to
/// make this test pass would widen a production trust store, which is the same
/// trade rejected on the browser-driver arc when `--ignore-certificate-errors-*`
/// was proposed. So this tier takes the real-network dependency instead, and
/// `skip_if_origin_unreachable` skips loudly when the network is absent.
///
/// (The worker side is already hermetic and stays that way: the in-guest
/// transport trusts **only** the sidecar's per-instance CA, delivered at spawn.
/// That is what makes assertion 3 below meaningful.)
///
/// ## What is asserted
///
/// 1. The dispatch **succeeds**, status is 200, the extracted document carries
///    [`ORIGIN_TITLE`], and its readable text clears
///    [`MIN_EXTRACTED_TEXT_BYTES`] — a completed fetch with a real body that
///    survived extraction, not merely an allowed connection.
/// 2. A sidecar decision `egress.allowed` for `example.com:443` — the fetch went
///    *through* the sidecar, not around it. Host **and** port are matched: a bare
///    host check would pass on any decision mentioning the host (#469's
///    all-port-grant lesson applies to assertions too).
/// 3. That decision carries `tls_intercepted: true` — the proxy really did
///    terminate and re-originate TLS, and the in-guest worker validated the
///    per-instance CA rather than a public one. This is the exact mirror of
///    browser-driver's `tls_intercepted:false` transparent-tunnel assertion, and
///    it is what makes this "a full MITM fetch". Without it a regression that
///    silently dropped web-fetch into transparent-tunnel mode would still pass:
///    the page would come back fine, and the proxy would have inspected nothing.
/// 4. Real wall-clock **headroom** under the entry's own `wall_clock_ms`.
///    Finishing is not enough — finishing at 29 s under a 30 s budget is a latent
///    failure, so the assertion is a fraction of budget, not a bare completion.
///
/// DGX-only. Needs outbound HTTPS. See the module doc for the run command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-fetch rootfs + egress proxy + outbound HTTPS"]
async fn real_web_fetch_through_sidecar() {
    if skip_if_no_microvm(VM_ROOTFS) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    if skip_if_origin_unreachable(ORIGIN_HOST) {
        return;
    }
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else {
        return;
    };
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wf-r",
        "wf-rl",
        &format!("kastellan-supervisor-test-pg-wfreal-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let hosts = vec![ORIGIN_HOST.to_string()];
    let (result, decisions, elapsed) =
        fetch_in_vm_through_real_sidecar(&pool, proxy_bin, &hosts, ORIGIN_URL).await;

    // Print the sidecar's own verdicts before asserting: on failure these say
    // whether the worker reached egress at all, and what the proxy decided.
    for line in &decisions {
        eprintln!("[egress-decision] {line}");
    }

    let value = result.expect(
        "web.fetch must SUCCEED against a real HTTPS origin through the real sidecar — this \
         is the whole point of this tier. A failure here means the VM booted and the worker \
         emitted CONNECT (the stub tier proves that separately) but the fetch did not \
         complete: check the egress decisions printed above for a block, and check that the \
         proxy's upstream leg validated the origin (it trusts webpki roots only)",
    );
    let text = value["text"].as_str().unwrap_or_default();
    let title = value["title"].as_str().unwrap_or_default();
    eprintln!(
        "[EVIDENCE] fetched {ORIGIN_URL} from inside the micro-VM in {elapsed:?}; \
         status={} title={title:?} text={} bytes",
        value["status"],
        text.len()
    );
    assert_eq!(value["status"], 200, "fetch result: {value}");
    assert!(
        title.contains(ORIGIN_TITLE),
        "expected {ORIGIN_TITLE:?} in the extracted title, got {title:?} — either the \
         proxy returned somebody else's page, or extraction lost the <title>"
    );
    assert!(
        text.len() >= MIN_EXTRACTED_TEXT_BYTES,
        "extracted only {} bytes of readable text (floor {MIN_EXTRACTED_TEXT_BYTES}): a 200 \
         with an empty body would mean the MITM leg completed but delivered nothing. Text: \
         {text:?}",
        text.len()
    );

    // The fetch must have gone THROUGH the sidecar. Same predicate the ingest
    // poll waited on (see `is_allowed_row_for` for the action-namespacing and
    // host-AND-port rationale), so a row the poll accepted is found here too.
    let allowed_row = decisions
        .iter()
        .find(|d| is_allowed_row_for(d, ORIGIN_HOST))
        .unwrap_or_else(|| {
            panic!(
                "no `egress.allowed {ORIGIN_HOST}:443` decision: the fetch returned text \
                 but not provably through the sidecar. Decisions: {decisions:?}"
            )
        });

    // MITM, not a transparent tunnel — scoped to the allowed row above, not to
    // any decision that happens to carry the flag. (NB `decision_to_audit`
    // defaults a MISSING `tls_intercepted` to false, so this catches both a
    // sidecar reporting `false` and one omitting the field.)
    assert!(
        allowed_row.contains("\"tls_intercepted\":true"),
        "the allowed decision for {ORIGIN_HOST}:443 is not a MITM one: the sidecar tunnelled \
         web-fetch's TLS through without terminating it, so it inspected nothing. That means \
         `disable_mitm_for` has started naming web-fetch, or the proxy's TLS sniff failed. \
         Row: {allowed_row}"
    );

    // Wall-clock headroom against the entry's OWN budget, so this measures the
    // production value rather than a test-local one.
    let budget = web_fetch_vm_entry_for(&hosts)
        .wall_clock_ms
        .expect("the VM entry sets a wall-clock budget");
    let used_pct = (elapsed.as_millis() as f64 / budget as f64) * 100.0;
    eprintln!("[EVIDENCE] wall clock: {elapsed:?} = {used_pct:.1}% of the {budget} ms budget");
    assert!(
        used_pct < 70.0,
        "fetch used {used_pct:.1}% of the {budget} ms wall-clock budget, leaving under 30% \
         headroom: the budget in web_fetch_firecracker_entry is too tight for a cold VM boot \
         plus a real HTTPS round trip"
    );

    pool.close().await;
}
