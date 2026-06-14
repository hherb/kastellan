//! End-to-end: the agent core spawns the `browser-driver` worker under the
//! platform sandbox and round-trips a `browser.render` call through
//! `tool_host::dispatch` (the sealed chokepoint — see `shell_exec_e2e.rs` for
//! why dispatch and not `worker.call`).
//!
//! Unlike `web_search_e2e`, browser-driver has **no hermetic (no-browser) test**:
//! its allowlist is enforced per-request *inside* a running browser, so any
//! end-to-end assertion needs a real Chromium. Both real tests are therefore
//! `#[ignore]` and operator-run after `scripts/workers/browser-driver/install.sh`.
//! The allowlist/route-handler logic is covered hermetically by the Python unit
//! tests (`workers/browser-driver/tests/test_render_drive.py`).
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the venv shim, or a working
//! sandbox is missing.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::browser_driver::{browser_driver_entry, BrowserDriverEnv};
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

/// Resolve the venv shim the way the host manifest does: the
/// `KASTELLAN_BROWSER_DRIVER_VENV_DIR` override wins, else
/// `$KASTELLAN_DATA_DIR/workers/browser-driver/.venv`, else
/// `$HOME/.local/share/kastellan/workers/browser-driver/.venv`. Returns the
/// resolved `BrowserDriverEnv`, or `None` (with a `[SKIP]` line) if the shim
/// isn't staged — run `scripts/workers/browser-driver/install.sh` first.
fn resolve_browser_env() -> Option<BrowserDriverEnv> {
    let venv_dir = std::env::var("KASTELLAN_BROWSER_DRIVER_VENV_DIR")
        .map(PathBuf::from)
        .or_else(|_| {
            std::env::var("KASTELLAN_DATA_DIR")
                .map(|d| PathBuf::from(d).join("workers/browser-driver/.venv"))
        })
        .or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".local/share/kastellan/workers/browser-driver/.venv"))
        })
        .ok()?;
    let script_path = venv_dir.join("bin").join("kastellan-worker-browser-driver");
    if !script_path.exists() {
        eprintln!(
            "\n[SKIP] browser-driver venv shim not found at {} — run scripts/workers/browser-driver/install.sh\n",
            script_path.display()
        );
        return None;
    }
    // Bind the real interpreter prefix when the venv's python lives outside the
    // venv (pyenv/uv), mirroring the manifest's resolve_interpreter_root.
    let interpreter_root = ["python3", "python"]
        .iter()
        .map(|n| venv_dir.join("bin").join(n))
        .find(|p| p.exists())
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .and_then(|real| real.parent().and_then(|b| b.parent()).map(PathBuf::from))
        .filter(|prefix| !prefix.starts_with(&venv_dir));
    // Operator escape hatch for host-specific deps (e.g. a pyenv interpreter's
    // /opt/homebrew libs) — same env the manifest reads.
    let extra_fs_read = std::env::var("KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ")
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect();
    // Mirror the manifest: bind the interpreter's out-of-prefix shared-lib dirs
    // (issue #284) so a pyenv/Homebrew-linked interpreter dyld-loads in the jail
    // without a manual KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ. Shares the
    // manifest's seed logic so the two can't drift (review M2).
    let interpreter_lib_dirs = kastellan_core::workers::interpreter_deps::interpreter_lib_dirs(
        &venv_dir,
        interpreter_root.as_deref(),
        &|p| p.exists(),
        &|p| std::fs::canonicalize(p).ok(),
        &|p| kastellan_core::workers::interpreter_deps::resolve_deps_via_tool(p),
    );
    Some(BrowserDriverEnv {
        script_path,
        venv_dir,
        interpreter_root,
        interpreter_lib_dirs,
        extra_fs_read,
    })
}

struct TestEnv {
    cluster: PgCluster,
    browser: BrowserDriverEnv,
}

fn ready_or_skip() -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let browser = resolve_browser_env()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "bd-d",
        "bd-l",
        &format!("kastellan-supervisor-test-pg-browserdriver-{suffix}"),
    );
    Some(TestEnv { cluster, browser })
}

fn dispatch_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "browser-driver-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Render `url` through the real jail with the given operator `allowlist`.
async fn render_in_jail(
    pool: &sqlx::PgPool,
    env: &TestEnv,
    allowlist: &[String],
    url: &str,
) -> Result<serde_json::Value, kastellan_core::tool_host::ToolHostError> {
    let entry = browser_driver_entry(&env.browser, allowlist);
    let backend = backend();
    let program = env.browser.script_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut sworker = spawn_worker(&*backend, &spec).expect("spawn browser-driver under sandbox");
    let result = dispatch(
        pool,
        &Vault::new(),
        &mut sworker,
        "browser-driver",
        "browser.render",
        serde_json::json!({ "url": url, "wait_until": "load", "timeout_ms": 10000 }),
    )
    .await;
    let _ = sworker.close();
    result
}

/// Locate the built proxy binary; `[SKIP]` if absent (mirrors `egress_proxy_e2e`).
fn proxy_binary_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    p.exists().then_some(p)
}

/// Create a short `/tmp`-based scratch root and return it. Short on purpose:
/// `spawn_forced_net_worker` nests `<root>/egress-<pid>-<seq>/egress.sock`, and
/// that projected UDS path must fit the 104-byte macOS `sockaddr_un.sun_path`
/// (the default `$TMPDIR` on macOS is ~50 chars deep and overflows once nested).
/// `/tmp` exists on both Linux and macOS.
fn short_scratch_root(tag: &str) -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("kfr-{tag}"));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Render `url` through the real jail **force-routed** through an egress-proxy
/// sidecar (the production posture). Mirrors `render_in_jail` but spawns via
/// `spawn_forced_net_worker` with `disable_mitm: true` (the browser tunnels TLS
/// end-to-end; the sidecar transparently tunnels).
/// One sidecar decision, flattened to the fields the acceptance asserts on:
/// `(action, host, port)` — e.g. `("egress.allowed", "127.0.0.1", 40787)`.
/// We capture tuples rather than `EgressAuditRow` (which isn't `Clone`) so the
/// test can snapshot them out of the decision-ingest thread.
type CapturedDecision = (String, String, u64);

/// Render `url` through the real jail **force-routed** through an egress-proxy
/// sidecar (the production posture). Mirrors `render_in_jail` but spawns via
/// `spawn_forced_net_worker` with `disable_mitm: true` (the browser does
/// end-to-end TLS; the sidecar transparently tunnels — no MITM).
///
/// Returns `(render_result, decisions)`. The **decisions** are the acceptance
/// signal: they are the egress sidecar's own allow/deny verdicts, captured from
/// its decision stream, proving egress is enforced at the netns boundary. The
/// `render_result` itself is NOT a reliable signal under a transparent tunnel —
/// to a hermetic loopback origin Chromium cannot complete real end-to-end TLS
/// (it would reject a self-signed cert; a trusted-cert render needs the deferred
/// MITM/NSS path), so a full 200 is not hermetically achievable. The sidecar
/// decision is the direct proof of the #280 property.
async fn render_in_jail_forced(
    pool: &sqlx::PgPool,
    env: &TestEnv,
    proxy_bin: &Path,
    scratch_root: &Path,
    worker_allowlist: &[String],
    sidecar_allowlist: &[String],
    url: &str,
) -> (
    Result<serde_json::Value, kastellan_core::tool_host::ToolHostError>,
    Vec<CapturedDecision>,
) {
    // The worker's in-process Playwright interception enforces `worker_allowlist`
    // (defense in depth); the egress sidecar enforces `sidecar_allowlist` at the
    // netns boundary. Passing DIFFERENT lists lets a test isolate the sidecar
    // boundary: allow in-process (so Chromium actually makes the request) while
    // the sidecar blocks it — proving the OS boundary, not just the app-layer
    // check. Production passes the same list to both.
    let entry = browser_driver_entry(&env.browser, worker_allowlist);
    let backend = backend();
    let program = env.browser.script_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let params = NetWorkerSpawn {
        backend: backend.as_ref(),
        proxy_bin,
        spec: &spec,
        allowlist: sidecar_allowlist,
        worker_name: "browser-driver",
        secret_fingerprints: &[],
        cert_pins_json: None,
        disable_mitm: true,
    };
    let decisions: std::sync::Arc<std::sync::Mutex<Vec<CapturedDecision>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_decisions = std::sync::Arc::clone(&decisions);
    let mut sworker = spawn_forced_net_worker(&params, scratch_root, move |row| {
        let host = row.payload["host"].as_str().unwrap_or_default().to_string();
        let port = row.payload["port"].as_u64().unwrap_or_default();
        sink_decisions.lock().unwrap().push((row.action, host, port));
    })
    .expect("force-route browser-driver under sidecar");
    let result = dispatch(
        pool,
        &Vault::new(),
        &mut sworker,
        "browser-driver",
        "browser.render",
        serde_json::json!({ "url": url, "wait_until": "load", "timeout_ms": 10000 }),
    )
    .await;
    let _ = sworker.close();
    // Decisions land on a detached ingest thread reading the proxy's stdout;
    // poll briefly (the CONNECT decision was emitted during the render, before
    // close()) so we don't race the thread.
    let mut snapshot = Vec::new();
    for _ in 0..60 {
        snapshot = decisions.lock().unwrap().clone();
        if !snapshot.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    (result, snapshot)
}

/// Split a `host:port` authority into `(host, port)` for asserting on a
/// captured sidecar decision.
fn split_authority(authority: &str) -> (String, u64) {
    let (h, p) = authority.rsplit_once(':').expect("authority has a port");
    (h.to_string(), p.parse().expect("numeric port"))
}

/// Spawn a one-shot loopback HTTP server that serves a JS-rendered page, and
/// return its `127.0.0.1:<port>` authority. The page injects a `js-ran` marker
/// via inline JS so the test can prove the DOM was rendered post-JS.
fn spawn_loopback_page() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let authority = format!("127.0.0.1:{}", addr.port());
    thread::spawn(move || {
        // Serve every connection the same fixture until the test thread exits.
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // drain the request line/headers
            let body = "<!doctype html><html><head><title>Fixture</title></head>\
                <body><article><p>static-content</p></article>\
                <script>var p=document.createElement('p');p.textContent='js-ran';\
                document.querySelector('article').appendChild(p);</script>\
                </body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    authority
}

/// Real render: a loopback page on the allowlist renders, JS runs, and the
/// post-JS text is returned. Cross-platform (Seatbelt + bwrap). Needs a staged
/// browser → `#[ignore]`.
#[test]
#[ignore = "requires a staged Chromium (scripts/workers/browser-driver/install.sh)"]
fn real_render_of_loopback_page() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    let authority = spawn_loopback_page();
    let url = format!("http://{authority}/");
    let allowlist = vec![authority.clone()];
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let r = render_in_jail(&pool, &env, &allowlist, &url)
            .await
            .expect("browser.render round trip");
        assert_eq!(r["status"], 200, "render result: {r}");
        let text = r["text"].as_str().unwrap_or("");
        assert!(text.contains("js-ran"), "post-JS marker missing from text: {r}");
        pool.close().await;
    });
}

/// Fail-closed: when the navigation host is NOT on the allowlist, the worker
/// aborts the request and the render fails (rather than reaching the network).
/// Needs a staged browser → `#[ignore]`.
#[test]
#[ignore = "requires a staged Chromium (scripts/workers/browser-driver/install.sh)"]
fn off_allowlist_navigation_fails_closed() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    let authority = spawn_loopback_page();
    let url = format!("http://{authority}/");
    // Allowlist a DIFFERENT host:port — the navigation host is not permitted.
    let allowlist = vec!["someother.test:443".to_string()];
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let r = render_in_jail(&pool, &env, &allowlist, &url).await;
        assert!(
            r.is_err(),
            "off-allowlist navigation must fail closed (RENDER_FAILED), got Ok: {r:?}"
        );
        pool.close().await;
    });
}

/// Acceptance (#280/#263): under force-routing, the browser's egress is enforced
/// at the **netns boundary** — a navigation to an allowlisted host reaches the
/// network ONLY via the per-worker egress sidecar, which ALLOWS it. We drive an
/// `https://` URL so Chromium uses the proxy's `CONNECT` protocol (it sends a
/// plain absolute-form GET for `http://`, which a CONNECT proxy rejects), and we
/// assert on the **sidecar's own decision** (the direct evidence of
/// egress-at-the-boundary) rather than a 200 render: a transparent tunnel can't
/// complete real TLS to a hermetic self-signed loopback origin (that needs the
/// deferred MITM/NSS path). Reaching an `egress.allowed` decision for the target
/// proves the full Chromium → in-jail shim → loopback-in-netns → UDS → sidecar →
/// origin path works. Needs a staged Chromium + the egress-proxy binary ->
/// #[ignore]. Cross-platform (Seatbelt + bwrap).
#[test]
#[ignore = "requires staged Chromium + egress-proxy binary"]
fn forced_render_of_loopback_page_through_sidecar() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    let Some(proxy) = proxy_binary_or_skip() else {
        return;
    };
    let scratch_root = short_scratch_root(&format!("bd-fr-{}", unique_suffix()));
    let authority = spawn_loopback_page();
    let (host, port) = split_authority(&authority);
    // https → Chromium uses CONNECT through the proxy; the host:port is on the
    // allowlist, so the sidecar must ALLOW + dial it.
    let url = format!("https://{authority}/");
    let allowlist = vec![authority.clone()];
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        // Both layers allow the target.
        let (_render, decisions) =
            render_in_jail_forced(&pool, &env, &proxy, &scratch_root, &allowlist, &allowlist, &url)
                .await;
        // The render result is not the signal (transparent tunnel can't complete
        // TLS to a hermetic origin). The sidecar decision is: it must have
        // ALLOWED the CONNECT to the allowlisted target — proving egress flowed
        // through the netns→shim→sidecar path and was permitted at the boundary.
        assert!(
            decisions
                .iter()
                .any(|(a, h, p)| a == "egress.allowed" && h == &host && *p == port),
            "expected an egress.allowed sidecar decision for {host}:{port}, got: {decisions:?}"
        );
        pool.close().await;
    });
    let _ = std::fs::remove_dir_all(&scratch_root);
}

/// Fail-closed AT THE SIDECAR (the #280 "not in-process-only" property): the
/// egress sidecar must block an off-allowlist target independently of the
/// worker's in-process interception. To isolate the OS boundary we deliberately
/// DIVERGE the two allowlists — the worker's in-process check ALLOWS the target
/// (so Chromium actually makes the request, rather than aborting it app-side),
/// while the sidecar's allowlist does NOT include it. The sidecar must then
/// block at the netns boundary: the render fails and a BLOCKED decision is
/// emitted, with NO allowed decision for the target. (In production both lists
/// are identical; this divergence exists only to prove the sidecar is a real,
/// independent boundary.)
#[test]
#[ignore = "requires staged Chromium + egress-proxy binary"]
fn forced_off_allowlist_fails_closed_at_sidecar() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    let Some(proxy) = proxy_binary_or_skip() else {
        return;
    };
    let scratch_root = short_scratch_root(&format!("bd-fr-deny-{}", unique_suffix()));
    let authority = spawn_loopback_page();
    let (host, port) = split_authority(&authority);
    let url = format!("https://{authority}/");
    // In-process check ALLOWS the target (so Chromium issues the CONNECT)...
    let worker_allowlist = vec![authority.clone()];
    // ...but the SIDECAR allowlist does not — it must block at the netns boundary.
    let sidecar_allowlist = vec!["someother.test:443".to_string()];
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let (render, decisions) = render_in_jail_forced(
            &pool,
            &env,
            &proxy,
            &scratch_root,
            &worker_allowlist,
            &sidecar_allowlist,
            &url,
        )
        .await;
        assert!(
            render.is_err(),
            "off-allowlist nav must fail closed (render error), got Ok: {render:?}"
        );
        // The sidecar must have BLOCKED the CONNECT at the netns boundary (and
        // must NOT have allowed the target).
        assert!(
            decisions
                .iter()
                .any(|(a, h, p)| a.starts_with("egress.blocked") && h == &host && *p == port),
            "expected a blocked sidecar decision for {host}:{port}, got: {decisions:?}"
        );
        assert!(
            !decisions
                .iter()
                .any(|(a, h, p)| a == "egress.allowed" && h == &host && *p == port),
            "off-allowlist target must never be allowed by the sidecar: {decisions:?}"
        );
        pool.close().await;
    });
    let _ = std::fs::remove_dir_all(&scratch_root);
}
