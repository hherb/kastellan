//! End-to-end integration test for the `hhagent-cli ask` happy path
//! and the plan-iteration-cap failure path (Task 4.4).
//!
//! This is the regression pin that none of the other scheduler tests
//! satisfy: it spawns the **real `hhagent-cli` subprocess**, which
//! INSERTs a `tasks` row and LISTENs for the completion NOTIFY, while
//! the **real `hhagent` daemon** runs under `systemd --user` (Linux)
//! or `launchctl` (macOS), the **real sandboxed worker** runs under
//! bwrap (Linux) or sandbox-exec (macOS), and only the **LLM is
//! mocked** behind a queued multi-shot HTTP listener.
//!
//! ## What this pins
//!
//! Two `#[test]` functions, each owning its per-test PG cluster +
//! per-test mock LLM. See `docs/superpowers/specs/2026-05-11-cli-ask-e2e-design.md`
//! for the design.
//!
//! 1. **`ask_subprocess_completes_planned_task_end_to_end`** —
//!    `hhagent-cli ask "say marker-<sfx>"`; mock serves
//!    `[non-terminal plan with echo step, terminal plan with text result]`;
//!    CLI exits 0; stdout is the marker; `tasks.state == "completed"`;
//!    `audit_log` has the canonical row multiset for a 2-iter task.
//!
//! 2. **`ask_subprocess_fails_after_plan_iteration_cap`** —
//!    `hhagent-cli ask "do the bad thing"`; mock serves three
//!    identical non-terminal plans with a non-allowlisted argv
//!    (`/bin/cat /etc/passwd`); each step fails POLICY_DENIED;
//!    inner-loop hits `DEFAULT_MAX_PLANS_FAST = 3`; CLI exits 1;
//!    `tasks.state == "failed"`; `audit_log` has 3× POLICY_DENIED
//!    rows.
//!
//! ## What this does NOT test
//!
//! - Constitutional-block paths (CASSANDRA stages still stub-Approve).
//! - Cancellation mid-step via CLI ctrl-C (timing-sensitive; needs a
//!   `BarrierDispatcher`-style hook).
//! - Long-lane scheduling (`scheduler_lanes_e2e` already pins it).
//! - Multiple concurrent CLI invocations.
//!
//! ## Skip behaviour
//!
//! Cleanly `[SKIP]`s with stderr explanation when the host is missing
//! Postgres, supervisor, sandbox, or the workspace binaries — same
//! pattern as the other supervisor-driven e2e tests. `cargo test --
//! --nocapture` to see the skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_supervisor::specs::{core_service_spec, postgres_service_spec};
use hhagent_supervisor::{
    default_probe, default_supervisor, ServiceStatus, Supervisor,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";
#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

// ---------------------------------------------------------------------------
// Skip / discovery helpers
// ---------------------------------------------------------------------------

fn skip_if_no_supervisor() -> bool {
    match default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

#[cfg(target_os = "linux")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::linux_bwrap::LinuxBwrap;
    if let Err(e) = LinuxBwrap::probe() {
        eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
        return true;
    }
    false
}

#[cfg(target_os = "macos")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::macos_seatbelt::MacosSeatbelt;
    if let Err(e) = MacosSeatbelt::probe() {
        eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
        return true;
    }
    false
}

fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match find_pg_bin_dir(&default_pg_bin_dir_candidates()) {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("\n[SKIP] no Postgres install found: {e}\n");
            None
        }
    }
}

/// Resolve `<workspace>/target/debug/<name>` the same way the other
/// e2e tests do. Honours `CARGO_TARGET_DIR` so out-of-tree builds work.
fn workspace_target_binary(name: &str) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join(name)
}

fn core_binary() -> PathBuf { workspace_target_binary("hhagent") }
fn cli_binary() -> PathBuf { workspace_target_binary("hhagent-cli") }
fn worker_binary() -> PathBuf { workspace_target_binary("hhagent-worker-shell-exec") }

/// Returns `true` (caller should `return` from its `#[test]`) when any
/// of the three workspace binaries this e2e exercises is missing —
/// almost always a clean dev-env build went stale. Logs a `[SKIP]`
/// line per missing binary so the operator running `cargo test --
/// --nocapture` sees which one to rebuild.
fn skip_if_any_binary_missing() -> bool {
    for (label, p) in &[("hhagent", core_binary()),
                        ("hhagent-cli", cli_binary()),
                        ("hhagent-worker-shell-exec", worker_binary())] {
        if !p.exists() {
            eprintln!(
                "\n[SKIP] {} binary missing at {}; run `cargo build --workspace`\n",
                label, p.display()
            );
            return true;
        }
    }
    false
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

fn unique_temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hhagent-{}-{}", label, unique_suffix()))
}

fn current_username() -> String {
    if let Some(u) = std::env::var_os("USER") {
        let s = u.to_string_lossy().into_owned();
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(out) = Command::new("whoami").output() {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    "hhagent".into()
}

// ---------------------------------------------------------------------------
// RAII guards
// ---------------------------------------------------------------------------

struct ServiceGuard {
    sup: Box<dyn Supervisor>,
    name: String,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = self.sup.stop(&self.name);
        let _ = self.sup.uninstall(&self.name);
    }
}

struct PathGuard { path: PathBuf }
impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Status / socket / log helpers — copies of the same helpers in
// supervisor_e2e.rs and scheduler_step_dispatch_e2e.rs. Issue #15 tracks
// the workspace-level `tests-common` refactor; this is the seventh
// duplication site.
// ---------------------------------------------------------------------------

fn wait_for_status<F: Fn(ServiceStatus) -> bool>(
    sup: &dyn Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<ServiceStatus, String> {
    let start = Instant::now();
    let mut last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    loop {
        if predicate(last) {
            return Ok(last);
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout {:?}; last={last:?}", timeout));
        }
        std::thread::sleep(Duration::from_millis(50));
        last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    }
}

fn wait_for_socket(socket_dir: &Path, timeout: Duration) -> Result<(), String> {
    let target = socket_dir.join(".s.PGSQL.5432");
    let start = Instant::now();
    loop {
        if target.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout {:?} waiting for {}", timeout, target.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_log_match<F: Fn(&str) -> bool>(
    path: &Path,
    predicate: F,
    timeout: Duration,
) -> Result<String, String> {
    let start = Instant::now();
    loop {
        if let Ok(body) = std::fs::read_to_string(path) {
            if predicate(&body) {
                return Ok(body);
            }
        }
        if start.elapsed() > timeout {
            let observed = std::fs::read_to_string(path).unwrap_or_default();
            return Err(format!(
                "timed out after {:?}; log body:\n---\n{}\n---",
                timeout, observed
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// macOS-only static mutex (launchd GUI domain is a shared global).
// Matches the pattern in supervisor_e2e.rs.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// Mock LLM — queued multi-shot HTTP listener.
// ---------------------------------------------------------------------------

/// Hard cap on inbound request bytes the mock will buffer before
/// giving up. Real chat-completion requests are a few KiB; 1 MiB is
/// generous headroom that defends against a buggy client pinning the
/// mock task in an unbounded read.
const MOCK_MAX_REQUEST_BYTES: usize = 1 << 20;

/// Multi-shot HTTP mock for the LLM router.
///
/// Serves canned 200-OK JSON bodies from a queue in FIFO order. Once
/// the queue is exhausted, every subsequent request gets a `503
/// Service Unavailable` so an unexpected extra LLM call surfaces as
/// `RouterError::HttpStatus` in the daemon log AND as a `tasks.state
/// = "failed"` row in the test's final assertion — i.e. loud, not
/// silent.
///
/// The accept loop runs forever (one connection at a time) until the
/// `JoinHandle` is aborted. `Drop` aborts it for us so the mock cannot
/// leak past the test boundary.
struct MockLlm {
    base_url: String,
    /// Captured request bodies in arrival order. Useful for asserting
    /// the daemon dialed N times.
    requests: Arc<Mutex<Vec<String>>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

async fn spawn_queued_mock(responses: Vec<String>) -> MockLlm {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let queue = Arc::new(Mutex::new(responses));
    let queue_for_task = queue.clone();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests_for_task = requests.clone();

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                // Listener closed (e.g. JoinHandle aborted just as a
                // peer was connecting). Exit the loop cleanly rather
                // than panicking.
                Err(_) => return,
            };

            let mut buf = Vec::with_capacity(4096);
            let mut tmp = [0u8; 1024];
            let req_body: Option<String> = loop {
                let n = match sock.read(&mut tmp).await {
                    Ok(n) => n,
                    Err(_) => break None,
                };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(headers_end) = find_double_crlf(&buf) {
                    let header_str = match std::str::from_utf8(&buf[..headers_end]) {
                        Ok(s) => s,
                        Err(_) => break None,
                    };
                    let content_length = header_content_length(header_str).unwrap_or(0);
                    let body_start = headers_end + 4;
                    let total_needed = body_start + content_length;
                    if buf.len() >= total_needed {
                        match String::from_utf8(buf[body_start..total_needed].to_vec()) {
                            Ok(b) => break Some(b),
                            Err(_) => break None,
                        }
                    }
                }
                if buf.len() > MOCK_MAX_REQUEST_BYTES {
                    break None;
                }
            };

            if let Some(body) = req_body {
                requests_for_task.lock().unwrap().push(body);
            }

            // FIFO dequeue: the daemon dials the LLM once per plan
            // iteration; the test plants the i-th expected response in
            // queue position i. `Vec::remove(0)` is O(n) but n is at
            // most a handful here.
            let next: Option<String> = {
                let mut q = queue_for_task.lock().unwrap();
                if q.is_empty() { None } else { Some(q.remove(0)) }
            };

            let resp = match next {
                Some(body) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                ),
                None => {
                    let empty = "{}";
                    format!(
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        empty.len(),
                        empty,
                    )
                }
            };

            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    MockLlm {
        base_url,
        requests,
        join: Some(join),
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 { return None; }
    for i in 0..(buf.len() - 3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

fn header_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let mut parts = line.splitn(2, ':');
        let Some(name) = parts.next() else { continue };
        let Some(value) = parts.next() else { continue };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Wrap `plan_json` in an OpenAI-compatible chat-completion envelope
/// so the mock can return it as the backend's response body. Matches
/// the shape `RouterAgent::formulate_plan` decodes via
/// `ChatResponse::choices[0].message.content`.
fn envelope_for(plan_json_string: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 1_700_000_000_u64,
        "model": "test-local-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": plan_json_string},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    }).to_string()
}

// ---------------------------------------------------------------------------
// PG cluster bring-up — copied verbatim from scheduler_step_dispatch_e2e.rs.
// Issue #15 tracks the workspace-level `tests-common` refactor.
// ---------------------------------------------------------------------------

fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    PathBuf, // data_dir
    PathBuf, // socket_dir
    (ServiceGuard, PathGuard, PathGuard),
) {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    let data_root = unique_temp_root("cli-d");
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("cli-l");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_guard = PathGuard { path: log_dir.clone() };

    let user = current_username();
    let argv = build_initdb_argv(
        &initdb,
        &InitDbOptions {
            data_dir: data_dir.clone(),
            username: user.clone(),
            ..InitDbOptions::default()
        },
    );
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .output()
        .expect("spawn initdb");
    assert!(
        out.status.success(),
        "initdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::create_dir(&socket_dir).expect("create socket dir");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&socket_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&socket_dir, perms).unwrap();
    }
    std::fs::write(
        data_dir.join("postgresql.auto.conf"),
        build_postgresql_auto_conf(&PgConfigOptions {
            socket_dir: socket_dir.clone(),
            ..PgConfigOptions::default()
        }),
    )
    .expect("write postgresql.auto.conf");

    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = format!("hhagent-supervisor-test-pg-cliask-{suffix}");
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install pg");
    sup.start(&spec.name).expect("start pg");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("pg active");
    wait_for_socket(&socket_dir, Duration::from_secs(15)).expect("pg socket");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).unwrap(),
        ServiceStatus::Active,
        "pg flap"
    );

    let conn_spec = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };
    (conn_spec, data_dir, socket_dir, (service_guard, data_guard, log_guard))
}

// ---------------------------------------------------------------------------
// Daemon bring-up — wires the `hhagent` core service to the per-test
// PG cluster + mock LLM + workspace prompts + per-test allowlist.
// ---------------------------------------------------------------------------

/// Returned by [`bring_up_daemon`]. Used to surface the daemon's
/// stdout/stderr log files in test-failure messages so a flaky run
/// shows what the daemon was doing without a re-run. The audit-mirror
/// JSONL under `<state_dir>` is intentionally not exposed here — the
/// audit assertions in each test go through the DB directly, and the
/// JSONL-mirror integration is already pinned by `supervisor_e2e`.
struct Daemon {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

fn bring_up_daemon(
    suffix: &str,
    data_dir: &Path,
    mock_base_url: &str,
    user: &str,
) -> (Daemon, (ServiceGuard, PathGuard, PathGuard)) {
    let core_log_dir = unique_temp_root("cli-clog");
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let core_log_guard = PathGuard { path: core_log_dir.clone() };

    let state_dir = unique_temp_root("cli-state");
    let state_guard = PathGuard { path: state_dir.clone() };

    let binary = core_binary();
    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("hhagent-supervisor-test-core-cliask-{suffix}");
    assert!(spec.name.len() <= 200);
    let stdout_path = core_log_dir.join(format!("{}.out", spec.name));
    let stderr_path = core_log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path.clone());

    // Required env — see the spec doc for why each one is needed.
    spec.env.push((
        "HHAGENT_DATA_DIR".into(),
        data_dir.to_string_lossy().into_owned(),
    ));
    spec.env.push(("USER".into(), user.to_string()));
    spec.env.push((
        "HHAGENT_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));

    // Prompts: the daemon's prompt loader fails closed if the dir is
    // missing. Point at the in-tree `prompts/` (read-only access).
    let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("prompts");
    spec.env.push((
        "HHAGENT_PROMPTS_DIR".into(),
        workspace_prompts.to_string_lossy().into_owned(),
    ));

    // LLM router → mock. The router's `compose_url` appends
    // `/chat/completions` to the base. Use `<mock>/v1` so the on-wire
    // URL matches the production OpenAI-compat shape.
    spec.env.push((
        "HHAGENT_LLM_LOCAL_URL".into(),
        format!("{mock_base_url}/v1"),
    ));
    spec.env.push((
        "HHAGENT_LLM_LOCAL_MODEL".into(),
        "test-local-model".into(),
    ));
    // 5 s is loose enough for slow CI runners — the mock responds
    // synchronously on accept, so on a healthy host this is sub-ms.
    // Production default is 30 s; we tighten so a mock bug surfaces
    // fast.
    spec.env.push(("HHAGENT_LLM_TIMEOUT_MS".into(), "5000".into()));

    // Tool registry: register shell-exec with only ECHO_PATH
    // allowlisted. Plan A's echo step succeeds; the failure path's
    // `/bin/cat` step deliberately is NOT in the allowlist and will
    // return POLICY_DENIED at the worker.
    spec.env.push((
        "HHAGENT_SHELL_EXEC_BIN".into(),
        worker_binary().to_string_lossy().into_owned(),
    ));
    spec.env.push((
        "HHAGENT_SHELL_EXEC_ALLOWLIST".into(),
        ECHO_PATH.into(),
    ));

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install core");
    sup.start(&spec.name).expect("start core");

    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(10),
    )
    .expect("core active");

    // Wait for the database probe to log success — that's our cue
    // that the daemon has finished bring-up and is ready to claim
    // tasks. Same approach as supervisor_e2e.rs.
    wait_for_log_match(
        &stdout_path,
        |s| s.contains("scheduler spawned"),
        Duration::from_secs(10),
    )
    .expect("daemon should log 'scheduler spawned' within 10s");

    (
        Daemon { stdout_path, stderr_path },
        (service_guard, core_log_guard, state_guard),
    )
}

/// Convenience JSON builder for the plan body the planner emits as the
/// assistant message content.
fn plan_json(
    decision: &str,
    steps: serde_json::Value,
    result: Option<serde_json::Value>,
) -> String {
    let mut obj = serde_json::json!({
        "context":      "test context",
        "decision":     decision,
        "rationale":    "test rationale",
        "steps":        steps,
        "data_ceiling": "Public",
    });
    if let Some(r) = result {
        obj.as_object_mut().unwrap().insert("result".into(), r);
    } else {
        obj.as_object_mut().unwrap().insert("result".into(), serde_json::Value::Null);
    }
    obj.to_string()
}

fn echo_step(text: &str) -> serde_json::Value {
    serde_json::json!([{
        "tool":           "shell-exec",
        "method":         "shell.exec",
        "parameters":     {"argv": [ECHO_PATH, text]},
        "returns":        "stdout",
        "done_when":      "exit_code == 0",
        "classification": "Public",
    }])
}

fn cat_passwd_step() -> serde_json::Value {
    serde_json::json!([{
        "tool":           "shell-exec",
        "method":         "shell.exec",
        "parameters":     {"argv": ["/bin/cat", "/etc/passwd"]},
        "returns":        "stdout",
        "done_when":      "exit_code == 0",
        "classification": "Public",
    }])
}

/// Build the `audit_log` (actor, action) → count multiset.
async fn audit_multiset(pool: &sqlx::PgPool) -> HashMap<(String, String), usize> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM audit_log ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("select audit_log");
    let mut m = HashMap::new();
    for r in rows {
        *m.entry(r).or_insert(0_usize) += 1;
    }
    m
}

// ---------------------------------------------------------------------------
// Test 1 — happy path
// ---------------------------------------------------------------------------

#[test]
fn ask_subprocess_completes_planned_task_end_to_end() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() { return; }
    if skip_if_sandbox_unavailable() { return; }
    let bin_dir = match pg_bin_dir_or_skip() { Some(d) => d, None => return };
    if skip_if_any_binary_missing() { return; }

    // Scope-end drop order is reverse declaration order, so the
    // bindings below resolve to:
    //   1. `_daemon_guards`  → stops + uninstalls the daemon service
    //   2. `mock`            → aborts the listener accept-task
    //   3. `_pg_guards`      → stops PG, wipes data + log dirs
    // The daemon stops dialing before the mock dies; the mock
    // releases its ephemeral port before PG goes down. No explicit
    // `drop(...)` calls needed.
    let suffix = unique_suffix();
    let marker = format!("marker-{suffix}");
    let user = current_username();

    let (conn_spec, data_dir, _socket_dir, _pg_guards) =
        bring_up_pg_cluster(&bin_dir, &suffix);

    // worker_threads(1): `tool_host::dispatch` uses `block_in_place`,
    // which needs at least one spare worker. One is enough — there is
    // no concurrent in-test work the runtime needs to schedule.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    let mock = rt.block_on(spawn_queued_mock(vec![
        envelope_for(&plan_json(
            "act",
            echo_step(&marker),
            None,
        )),
        envelope_for(&plan_json(
            "task_complete",
            serde_json::json!([]),
            Some(serde_json::json!({"kind": "text", "body": &marker})),
        )),
    ]));

    let (daemon, _daemon_guards) = bring_up_daemon(&suffix, &data_dir, &mock.base_url, &user);

    // ---------- Spawn the real CLI subprocess ----------
    let output = Command::new(cli_binary())
        .arg("ask")
        .arg(format!("say {marker}"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("HHAGENT_DATA_DIR", data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn hhagent-cli ask");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "CLI must exit 0 in the happy path; got {:?}\n\
         --- CLI stdout ---\n{}\n--- CLI stderr ---\n{}\n\
         --- daemon stdout ({}) ---\n{}\n\
         --- daemon stderr ({}) ---\n{}\n",
        output.status, stdout, stderr,
        daemon.stdout_path.display(),
        std::fs::read_to_string(&daemon.stdout_path).unwrap_or_default(),
        daemon.stderr_path.display(),
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    assert_eq!(
        stdout.trim_end(),
        marker,
        "CLI stdout must echo the marker verbatim; got:\n{stdout}\n--- stderr ---\n{stderr}\n"
    );

    // ---------- DB assertions ----------
    rt.block_on(async {
        let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
            .await
            .expect("connect runtime pool");

        // The CLI inserted exactly one task; the daemon ran it to completion.
        let rows: Vec<(i64, String, i32, Option<serde_json::Value>)> = sqlx::query_as(
            "SELECT id, state, plan_count, result FROM tasks ORDER BY id"
        )
        .fetch_all(&pool)
        .await
        .expect("select tasks");
        assert_eq!(rows.len(), 1, "expected exactly one task row, got {rows:?}");
        let (_, state, plan_count, result) = &rows[0];
        assert_eq!(state, "completed", "task state must be 'completed'; got {state}");
        assert_eq!(*plan_count, 2, "expected plan_count == 2 (two LLM rounds); got {plan_count}");
        let result_body = result.as_ref()
            .and_then(|v| v.get("body"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(result_body, marker, "result.body must equal marker; got {result_body:?}");

        // Audit-log multiset. Approve verdict + non-terminal plan A's
        // outcome row, plus the canonical bring-up.
        let m = audit_multiset(&pool).await;
        assert_eq!(m.get(&("core".into(), "startup".into())), Some(&1),
                   "expected 1× core/startup; multiset = {m:?}");
        assert_eq!(m.get(&("agent".into(), "plan.formulate".into())), Some(&2),
                   "expected 2× agent/plan.formulate (one per LLM call); multiset = {m:?}");
        assert_eq!(m.get(&("cassandra:chain".into(), "verdict".into())), Some(&2),
                   "expected 2× cassandra:chain/verdict (one per plan); multiset = {m:?}");
        assert_eq!(m.get(&("tool:shell-exec".into(), "shell.exec".into())), Some(&1),
                   "expected 1× tool:shell-exec/shell.exec (the echo step); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "plan.outcome".into())), Some(&1),
                   "expected 1× scheduler/plan.outcome (only plan A executed steps); multiset = {m:?}");

        // Total row count. The multiset above pins individual (actor,
        // action) presence and count, but does NOT catch *unexpected*
        // additional rows from a future audit-emitting refactor (e.g.
        // a `scheduler/task.<state>` lifecycle row, spec §7). When
        // such a row lands, the developer will see this exact
        // assertion fail and update both the multiset and the total.
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        let expected_total: i64 = 1 + 2 + 2 + 1 + 1; // = 7
        assert_eq!(
            total.0, expected_total,
            "audit_log row count mismatch (expected {expected_total}, got {}); multiset = {m:?}",
            total.0
        );

        pool.close().await;
    });

    // Mock dial count: 2 — happy path is exactly 2 LLM calls.
    let captured = mock.requests.lock().unwrap();
    assert_eq!(
        captured.len(), 2,
        "expected daemon to dial mock exactly 2× in happy path; got {}",
        captured.len()
    );
    // The first request's body should carry the cached
    // `agent_planner` system prompt verbatim. We pin the distinctive
    // heading `Constitutional Principles` from `prompts/agent_planner.md`
    // — distinctive enough that a regression accidentally swapping
    // which prompt the daemon caches and sends would surface here.
    // `router_agent_mock_e2e.rs` does the analogous check at the
    // dispatcher layer; this lifts the same regression-pin to the
    // full subprocess path.
    let first_body = &captured[0];
    assert!(
        first_body.contains("Constitutional Principles"),
        "first request must carry the cached planner prompt; got first {} chars:\n{}",
        first_body.len().min(800),
        &first_body[..first_body.len().min(800)],
    );
}

// ---------------------------------------------------------------------------
// Test 2 — plan-iteration-cap failure path
// ---------------------------------------------------------------------------

#[test]
fn ask_subprocess_fails_after_plan_iteration_cap() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() { return; }
    if skip_if_sandbox_unavailable() { return; }
    let bin_dir = match pg_bin_dir_or_skip() { Some(d) => d, None => return };
    if skip_if_any_binary_missing() { return; }

    // Scope-end drop order: `_daemon_guards` → `mock` → `_pg_guards`.
    // See the matching comment in the happy-path test for why no
    // explicit `drop` calls are needed.
    let suffix = unique_suffix();
    let user = current_username();

    let (conn_spec, data_dir, _socket_dir, _pg_guards) =
        bring_up_pg_cluster(&bin_dir, &suffix);

    // worker_threads(1): see happy-path test for the rationale.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    // Same non-terminal plan three times — every iteration the agent
    // tries to cat /etc/passwd, every iteration the worker denies it,
    // every iteration the inner-loop replans. On the fourth would-be
    // iteration the plan-iter cap kicks in (DEFAULT_MAX_PLANS_FAST=3)
    // and we return Outcome::Failed.
    let denied_plan = envelope_for(&plan_json("act", cat_passwd_step(), None));
    let mock = rt.block_on(spawn_queued_mock(vec![
        denied_plan.clone(),
        denied_plan.clone(),
        denied_plan,
    ]));

    let (daemon, _daemon_guards) = bring_up_daemon(&suffix, &data_dir, &mock.base_url, &user);

    let output = Command::new(cli_binary())
        .arg("ask")
        .arg("do the bad thing")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("HHAGENT_DATA_DIR", data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn hhagent-cli ask");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "CLI must exit non-zero in the failure path; got {:?}\n\
         --- CLI stdout ---\n{}\n--- CLI stderr ---\n{}\n\
         --- daemon stdout ({}) ---\n{}\n\
         --- daemon stderr ({}) ---\n{}\n",
        output.status, stdout, stderr,
        daemon.stdout_path.display(),
        std::fs::read_to_string(&daemon.stdout_path).unwrap_or_default(),
        daemon.stderr_path.display(),
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    assert!(
        stderr.contains("failed"),
        "CLI stderr must mention 'failed'; got:\n{stderr}"
    );

    rt.block_on(async {
        let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
            .await
            .expect("connect runtime pool");

        let rows: Vec<(i64, String, i32, Option<serde_json::Value>)> = sqlx::query_as(
            "SELECT id, state, plan_count, result FROM tasks ORDER BY id"
        )
        .fetch_all(&pool)
        .await
        .expect("select tasks");
        assert_eq!(rows.len(), 1, "expected exactly one task row, got {rows:?}");
        let (_, state, plan_count, _) = &rows[0];
        assert_eq!(state, "failed", "task state must be 'failed'; got {state}");
        assert_eq!(*plan_count, 3, "plan_count must equal cap (3); got {plan_count}");

        // Each of the three iterations dispatched the denied step, so
        // we expect 3 tool:shell-exec rows whose payload carries an
        // `err` string mentioning the JSON-RPC POLICY_DENIED code
        // (`-32001`). The audit envelope `err` is the worker's
        // protocol-error string captured by `tool_host::dispatch`, not
        // a structured object — the rpc_code → mnemonic mapping
        // happens one layer up in `ToolHostStepDispatcher`.
        let denied_rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT payload FROM audit_log \
             WHERE actor = 'tool:shell-exec' AND action = 'shell.exec' \
             ORDER BY id"
        )
        .fetch_all(&pool)
        .await
        .expect("select shell-exec audit rows");
        assert_eq!(
            denied_rows.len(), 3,
            "expected 3 tool:shell-exec rows (one per iter); got {}",
            denied_rows.len()
        );
        for (i, (payload,)) in denied_rows.iter().enumerate() {
            let err_str = payload.get("err")
                .and_then(|e| e.as_str())
                .unwrap_or("");
            assert!(
                err_str.contains("-32001"),
                "iter {i}: expected err string to carry POLICY_DENIED code -32001; \
                 got err={err_str:?}; payload={payload}"
            );
            assert!(
                !payload.as_object().map(|o| o.contains_key("result")).unwrap_or(true),
                "iter {i}: denied row must not carry a `result` key; payload={payload}"
            );
        }

        let m = audit_multiset(&pool).await;
        assert_eq!(m.get(&("core".into(), "startup".into())), Some(&1),
                   "expected 1× core/startup; multiset = {m:?}");
        assert_eq!(m.get(&("agent".into(), "plan.formulate".into())), Some(&3),
                   "expected 3× agent/plan.formulate (one per LLM call before cap); multiset = {m:?}");
        assert_eq!(m.get(&("cassandra:chain".into(), "verdict".into())), Some(&3),
                   "expected 3× cassandra:chain/verdict (one per plan); multiset = {m:?}");
        assert_eq!(m.get(&("tool:shell-exec".into(), "shell.exec".into())), Some(&3),
                   "expected 3× tool:shell-exec/shell.exec (one per denied dispatch); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "plan.outcome".into())), Some(&3),
                   "expected 3× scheduler/plan.outcome (one per non-terminal plan); multiset = {m:?}");

        // Total row count — catches unexpected additional audit rows.
        // See the matching comment in the happy-path test.
        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        let expected_total: i64 = 1 + 3 + 3 + 3 + 3; // = 13
        assert_eq!(
            total.0, expected_total,
            "audit_log row count mismatch (expected {expected_total}, got {}); multiset = {m:?}",
            total.0
        );

        pool.close().await;
    });

    let dialed = mock.requests.lock().unwrap().len();
    assert_eq!(dialed, 3, "expected daemon to dial mock exactly 3× before cap; got {dialed}");
}
