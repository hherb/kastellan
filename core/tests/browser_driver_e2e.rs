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
use std::path::PathBuf;
use std::thread;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::browser_driver::{browser_driver_entry, BrowserDriverEnv};
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, PgCluster,
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
    Some(BrowserDriverEnv {
        script_path,
        venv_dir,
        interpreter_root,
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
