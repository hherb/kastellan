//! Shared real-daemon bring-up for the CLI e2e tests.
//!
//! Several integration tests (`cli_memory_l3_run_daemon_e2e`,
//! `cli_memory_l3py_run_daemon_e2e`, …) drive a *real* `kastellan` daemon under
//! the supervisor against a per-test Postgres cluster, then exercise it through
//! the `kastellan-cli` operator subprocess. They previously each carried a
//! byte-duplicated `MockLlm` + `bring_up_daemon` pair that drifted apart over
//! time; this module is the single source of truth (issue #15 spirit).
//!
//! What is *not* here: anything that depends on `kastellan-core` types
//! (skill factories, the per-OS python interpreter cascade). `tests-common`
//! is deliberately core-free — those stay private to the individual test file.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use kastellan_supervisor::specs::core_service_spec;
use kastellan_supervisor::{default_supervisor, ServiceStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::guards::{PathGuard, ServiceGuard};
use crate::{core_binary, unique_temp_root, wait_for_log_match, wait_for_status};

// ---------------------------------------------------------------------------
// Inert LLM mock — the `l3_run` paths NEVER call the LLM (the daemon executes
// the approved skill directly, no planner / CASSANDRA). It exists only so the
// daemon's router config points at a live socket and the daemon boots cleanly;
// every request gets a 503. If an l3_run path ever did dial the LLM, that 503
// would surface loudly as a task failure rather than hang.
// ---------------------------------------------------------------------------

/// A live-but-inert local-LLM endpoint. Holds the listener task; aborts it on
/// drop so no socket leaks between tests.
pub struct MockLlm {
    /// `http://127.0.0.1:<ephemeral-port>` — feed this to the daemon's
    /// `KASTELLAN_LLM_LOCAL_URL` (the caller appends `/v1`).
    pub base_url: String,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

/// Bind an ephemeral loopback port and serve `503 Service Unavailable` to every
/// connection. Returns once the listener is bound and accepting.
pub async fn spawn_inert_mock() -> MockLlm {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            // Drain whatever the client sent (best-effort) then 503.
            let mut tmp = [0u8; 1024];
            let _ = sock.read(&mut tmp).await;
            let body = "{}";
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    MockLlm {
        base_url,
        join: Some(join),
    }
}

// ---------------------------------------------------------------------------
// Daemon bring-up.
// ---------------------------------------------------------------------------

/// The log file paths of a booted daemon — used by callers to dump the daemon's
/// stdout/stderr into assertion-failure messages.
pub struct DaemonHandle {
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

/// The RAII guards a booted daemon owns: the service (stopped + uninstalled on
/// drop), the core log dir, and the state dir.
pub type DaemonGuards = (ServiceGuard, PathGuard, PathGuard);

/// Install + start a real `kastellan` daemon under the supervisor and wait for
/// it to log `"scheduler spawned"`.
///
/// `label` distinguishes co-running tests' temp dirs + service names (e.g.
/// `"l3run"` → `kastellan-supervisor-test-core-l3run-<suffix>`). `extra_env`
/// carries the *test-specific* worker registration (e.g.
/// `KASTELLAN_SHELL_EXEC_BIN` or the `KASTELLAN_PYTHON_EXEC_*` trio) on top of
/// the common data-dir / prompts / inert-LLM config every daemon needs.
///
/// Panics (rather than skips) on failure: callers are expected to have already
/// short-circuited on missing host prerequisites.
pub fn bring_up_daemon(
    label: &str,
    suffix: &str,
    data_dir: &Path,
    mock_base_url: &str,
    user: &str,
    extra_env: Vec<(String, String)>,
) -> (DaemonHandle, DaemonGuards) {
    let core_log_dir = unique_temp_root(&format!("cli-{label}-clog"));
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let core_log_guard = PathGuard {
        path: core_log_dir.clone(),
    };

    let state_dir = unique_temp_root(&format!("cli-{label}-state"));
    let state_guard = PathGuard {
        path: state_dir.clone(),
    };

    let binary = core_binary();
    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("kastellan-supervisor-test-core-{label}-{suffix}");
    assert!(spec.name.len() <= 200);
    let stdout_path = core_log_dir.join(format!("{}.out", spec.name));
    let stderr_path = core_log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path.clone());

    spec.env.push((
        "KASTELLAN_DATA_DIR".into(),
        data_dir.to_string_lossy().into_owned(),
    ));
    spec.env.push(("USER".into(), user.to_string()));
    spec.env.push((
        "KASTELLAN_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));

    // Prompts: the daemon's prompt loader fails closed if the dir is missing.
    // `CARGO_MANIFEST_DIR` is `tests-common/` here, whose parent is the
    // workspace root — the same `<root>/prompts` a test crate would resolve.
    let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("prompts");
    spec.env.push((
        "KASTELLAN_PROMPTS_DIR".into(),
        workspace_prompts.to_string_lossy().into_owned(),
    ));

    // LLM router → inert mock. The l3_run path never dials it, but the daemon
    // needs a valid-looking config to construct its router at startup.
    spec.env.push((
        "KASTELLAN_LLM_LOCAL_URL".into(),
        format!("{mock_base_url}/v1"),
    ));
    spec.env
        .push(("KASTELLAN_LLM_LOCAL_MODEL".into(), "test-local-model".into()));
    spec.env.push(("KASTELLAN_LLM_TIMEOUT_MS".into(), "5000".into()));

    // Test-specific worker registration (the daemon's own registry — the
    // operator CLI subprocess deliberately omits these; the #179 invariant).
    spec.env.extend(extra_env);

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

    wait_for_log_match(
        &stdout_path,
        |s| s.contains("scheduler spawned"),
        Duration::from_secs(10),
    )
    .expect("daemon should log 'scheduler spawned' within 10s");

    (
        DaemonHandle {
            stdout_path,
            stderr_path,
        },
        (service_guard, core_log_guard, state_guard),
    )
}

// ---------------------------------------------------------------------------
// CLI-output assertions.
// ---------------------------------------------------------------------------

/// Assert the operator CLI subprocess exited 0 and return its decoded
/// `(stdout, stderr)` for further content checks. On failure the panic message
/// dumps BOTH the CLI streams and the daemon's log files — the only way to
/// diagnose a daemon-side error from a CI log. `what` names the invocation.
pub fn assert_cli_success(output: &Output, daemon: &DaemonHandle, what: &str) -> (String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "{what} must exit 0; got {:?}\n\
         --- CLI stdout ---\n{}\n--- CLI stderr ---\n{}\n\
         --- daemon stdout ({}) ---\n{}\n--- daemon stderr ({}) ---\n{}\n",
        output.status,
        stdout,
        stderr,
        daemon.stdout_path.display(),
        std::fs::read_to_string(&daemon.stdout_path).unwrap_or_default(),
        daemon.stderr_path.display(),
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    (stdout, stderr)
}

/// Assert the operator CLI subprocess exited NON-zero (the fail-closed contract)
/// and return its decoded `(stdout, stderr)`. `what` names the invocation.
pub fn assert_cli_failure(output: &Output, what: &str) -> (String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "{what} must exit non-zero; got {:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    (stdout, stderr)
}
