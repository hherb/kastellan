//! Real-daemon e2e for the python-exec skill-catalog invocation path (slice 2).
//!
//! Mirrors `cli_memory_l3_run_daemon_e2e.rs` one payload over: an approved
//! **Python** skill, submitted via `kastellan-cli memory l3 run <id> --execute`,
//! is executed by the **daemon** against ITS OWN live registry under the real
//! python-exec jail, and the snippet's stdout comes back in the `InvokeReport`.
//!
//! What this pins:
//!  1. **`python_run_succeeds_against_daemon_registry`** — the happy path AND
//!     the #179 invariant: the operator CLI subprocess carries NO
//!     `KASTELLAN_PYTHON_EXEC_BIN`; the daemon (which DOES, plus
//!     `KASTELLAN_PYTHON_EXEC_ENABLE=1`) runs the skill against its own
//!     registry. Asserts exit 0, stdout `"executed skill"` + the snippet's
//!     output, and an `l3.invoke_outcome` audit row carrying `kind:"python"`.
//!  2. **`python_run_fails_closed_when_python_exec_disabled`** — the design's
//!     fail-closed contract: a daemon WITHOUT `KASTELLAN_PYTHON_EXEC_ENABLE=1`
//!     has no `python-exec` in its registry, so the single dispatch returns a
//!     tool-not-registered error (exit non-zero) — never a silent no-op.
//!
//! ## Skip semantics
//!
//! Every daemon scenario short-circuits with a `[SKIP]` print when the host
//! lacks a supervisor, a sandbox backend, `pg_ctl`, the workspace binaries, or
//! a usable python3 interpreter. On the DGX (live PG + bwrap) and the Mac
//! (live PG + Seatbelt) they RUN. Cross-platform (Linux + macOS).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kastellan_core::cassandra::types::PythonSkillCandidate;
use kastellan_core::memory::l3_crystallise::L3Source;
use kastellan_core::memory::l3py_crystallise::crystallise_python_skill;
use kastellan_core::workers::python_exec::PYTHON_CANDIDATES;
use kastellan_supervisor::specs::core_service_spec;
use kastellan_supervisor::{default_supervisor, ServiceStatus};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, core_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, unique_temp_root,
    wait_for_log_match, wait_for_status, workspace_target_binary, PathGuard, PgCluster,
    ServiceGuard,
};
#[cfg(target_os = "macos")]
use kastellan_tests_common::serial_lock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The skill's snippet prints this marker; it must round-trip back through the
/// worker's `stdout` capture into the rendered `InvokeReport`.
const SNIPPET_MARKER: &str = "hi-from-py-skill";

/// The manifest's own per-OS candidate cascade (single source of truth — on
/// macOS that list excludes the `/usr/bin/python3` xcrun shim, which cannot run
/// inside the jail). `None` ⇒ no usable interpreter ⇒ skip.
fn find_python() -> Option<PathBuf> {
    for c in PYTHON_CANDIDATES {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(std::fs::canonicalize(&p).unwrap_or(p));
        }
    }
    eprintln!("\n[SKIP] no python3 interpreter on this host\n");
    None
}

// ---------------------------------------------------------------------------
// Minimal LLM mock — the l3_run path NEVER calls the LLM (the daemon executes
// the approved skill directly). It exists only so the daemon's router config
// points at a live socket and the daemon boots cleanly; every request gets 503.
// ---------------------------------------------------------------------------

struct MockLlm {
    base_url: String,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

async fn spawn_inert_mock() -> MockLlm {
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
// Daemon bring-up — mirrors cli_memory_l3_run_daemon_e2e.rs, but registers the
// python-exec worker (opt-in `KASTELLAN_PYTHON_EXEC_ENABLE=1` + the worker bin
// + the interpreter) on the *daemon*. The operator CLI subprocess will NOT
// carry the worker bin — the #179 invariant.
// ---------------------------------------------------------------------------

struct Daemon {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

/// `enable_python`: when false the daemon boots with python-exec UNregistered,
/// exercising the fail-closed contract.
fn bring_up_daemon(
    suffix: &str,
    data_dir: &Path,
    mock_base_url: &str,
    user: &str,
    worker_bin: &Path,
    python: &Path,
    enable_python: bool,
) -> (Daemon, (ServiceGuard, PathGuard, PathGuard)) {
    let core_log_dir = unique_temp_root("cli-l3pyrun-clog");
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let core_log_guard = PathGuard {
        path: core_log_dir.clone(),
    };

    let state_dir = unique_temp_root("cli-l3pyrun-state");
    let state_guard = PathGuard {
        path: state_dir.clone(),
    };

    let binary = core_binary();
    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("kastellan-supervisor-test-core-l3pyrun-{suffix}");
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

    let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("prompts");
    spec.env.push((
        "KASTELLAN_PROMPTS_DIR".into(),
        workspace_prompts.to_string_lossy().into_owned(),
    ));

    spec.env.push((
        "KASTELLAN_LLM_LOCAL_URL".into(),
        format!("{mock_base_url}/v1"),
    ));
    spec.env
        .push(("KASTELLAN_LLM_LOCAL_MODEL".into(), "test-local-model".into()));
    spec.env.push(("KASTELLAN_LLM_TIMEOUT_MS".into(), "5000".into()));

    // The python-exec worker is opt-in. Register it on the DAEMON (the operator
    // CLI subprocess deliberately omits the bin — the #179 invariant). When
    // `enable_python` is false we leave the worker unregistered to exercise the
    // fail-closed path; the worker bin + interpreter are still passed so the
    // ONLY difference is the enable flag.
    if enable_python {
        spec.env
            .push(("KASTELLAN_PYTHON_EXEC_ENABLE".into(), "1".into()));
    }
    spec.env.push((
        "KASTELLAN_PYTHON_EXEC_BIN".into(),
        worker_bin.to_string_lossy().into_owned(),
    ));
    spec.env.push((
        "KASTELLAN_PYTHON_EXEC_PYTHON".into(),
        python.to_string_lossy().into_owned(),
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

    wait_for_log_match(
        &stdout_path,
        |s| s.contains("scheduler spawned"),
        Duration::from_secs(10),
    )
    .expect("daemon should log 'scheduler spawned' within 10s");

    (
        Daemon {
            stdout_path,
            stderr_path,
        },
        (service_guard, core_log_guard, state_guard),
    )
}

/// Build the per-test PG cluster.
fn cluster_for(suffix: &str) -> PgCluster {
    let bin_dir = pg_bin_dir_or_skip().expect("caller already short-circuited on missing pg");
    bring_up_pg_cluster(
        &bin_dir,
        "l3pyd-d",
        "l3pyd-l",
        &format!("kastellan-supervisor-test-pg-l3pyrun-{suffix}"),
    )
}

/// A Python skill whose verbatim source prints [`SNIPPET_MARKER`].
fn hello_python_skill() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "say_hi_py".into(),
        description: "Print a greeting from a python skill".into(),
        code: format!("print('{SNIPPET_MARKER}')\n"),
    }
}

/// A Python skill that reads runtime params and echoes the `greeting` key,
/// prefixed with `"GOT:"` so the assertion can distinguish it from other output.
fn param_echo_skill() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "param_echo_py".into(),
        description: "Echo the greeting runtime param".into(),
        code: "import os, json\np = json.loads(os.environ['KASTELLAN_PYTHON_PARAMS'])\nprint('GOT:' + p['greeting'])\n".into(),
    }
}

/// Crystallise → approve a Python skill via the CLI; returns its memory id.
/// Unlike the templated path, the Python approve gate needs NO `registry.loaded`
/// snapshot (a python skill dispatches no tools — the jail is its ceiling), so
/// none is seeded.
async fn seed_and_approve_python_skill(pool: &sqlx::PgPool, data_dir: &Path, user: &str) -> i64 {
    seed_and_approve_skill(pool, &hello_python_skill(), data_dir, user).await
}

/// Generic crystallise → approve helper for any [`PythonSkillCandidate`].
async fn seed_and_approve_skill(
    pool: &sqlx::PgPool,
    skill: &PythonSkillCandidate,
    data_dir: &Path,
    user: &str,
) -> i64 {
    let outcome = crystallise_python_skill(
        pool,
        skill,
        L3Source::AgentRaised { task_id: 1 },
    )
    .await
    .expect("crystallise_python_skill");
    let id = outcome.memory_id();

    let approve = Command::new(cli_binary())
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", user)
        .env("KASTELLAN_DATA_DIR", data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn cli memory l3 approve");
    assert!(
        approve.status.success(),
        "approve must succeed before run; stdout={}\nstderr={}",
        String::from_utf8_lossy(&approve.stdout),
        String::from_utf8_lossy(&approve.stderr),
    );
    id
}

/// Apply migrations + connect a runtime pool. No tool allowlist seeding — a
/// python skill needs none.
async fn prepare_db(cluster: &PgCluster) -> sqlx::PgPool {
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "test",
        "setup",
        serde_json::json!({"test": "cli_memory_l3py_run_daemon_e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool")
}

/// True iff some host prerequisite is missing — caller returns early on `true`.
fn missing_prereqs(check_sandbox: bool) -> bool {
    if skip_if_no_supervisor() {
        return true;
    }
    if check_sandbox && skip_if_sandbox_unavailable() {
        return true;
    }
    if pg_bin_dir_or_skip().is_none() {
        return true;
    }
    for (label, p) in &[
        ("kastellan", core_binary()),
        ("kastellan-cli", cli_binary()),
        (
            "kastellan-worker-python-exec",
            workspace_target_binary("kastellan-worker-python-exec"),
        ),
    ] {
        if !p.exists() {
            eprintln!("\n[SKIP] {label} binary missing at {}\n", p.display());
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Scenario 1 — happy path + the #179 invariant + the kind:"python" audit row.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_run_succeeds_against_daemon_registry() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if missing_prereqs(true) {
        return;
    }
    let Some(python) = find_python() else {
        return;
    };
    let worker_bin = workspace_target_binary("kastellan-worker-python-exec");

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    let pool = prepare_db(&cluster).await;
    let id = seed_and_approve_python_skill(&pool, &cluster.data_dir, &user).await;

    let mock = spawn_inert_mock().await;
    let (daemon, _daemon_guards) = bring_up_daemon(
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        &worker_bin,
        &python,
        true, // python-exec enabled
    );

    // Operator CLI subprocess: NO KASTELLAN_PYTHON_EXEC_BIN. The daemon executes
    // against its own registry (the #179 invariant).
    let output = Command::new(cli_binary())
        .args([
            "memory",
            "l3",
            "run",
            &id.to_string(),
            "--execute",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --execute");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "run --execute must exit 0; got {:?}\n\
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
    assert!(
        stdout.contains("executed skill"),
        "stdout must report 'executed skill'; got:\n{stdout}\n--- stderr ---\n{stderr}",
    );
    assert!(
        stdout.contains(SNIPPET_MARKER),
        "stdout must carry the skill's snippet output '{SNIPPET_MARKER}'; got:\n{stdout}",
    );

    // The lifecycle stream must distinguish python: an l3.invoke_outcome row
    // tagged kind:"python".
    let outcome_kind: Option<String> = sqlx::query_scalar(
        "SELECT payload->>'kind' FROM audit_log \
         WHERE action = 'l3.invoke_outcome' ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(&pool)
    .await
    .expect("query l3.invoke_outcome audit row")
    .flatten();
    assert_eq!(
        outcome_kind.as_deref(),
        Some("python"),
        "the l3.invoke_outcome audit row must carry kind:\"python\"",
    );

    pool.close().await;
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 2 — fail-closed: a daemon without python-exec enabled refuses the
// run with a tool-not-registered error rather than silently doing nothing.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_run_fails_closed_when_python_exec_disabled() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if missing_prereqs(true) {
        return;
    }
    let Some(python) = find_python() else {
        return;
    };
    let worker_bin = workspace_target_binary("kastellan-worker-python-exec");

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    let pool = prepare_db(&cluster).await;
    let id = seed_and_approve_python_skill(&pool, &cluster.data_dir, &user).await;

    let mock = spawn_inert_mock().await;
    let (_daemon, _daemon_guards) = bring_up_daemon(
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        &worker_bin,
        &python,
        false, // python-exec NOT enabled — fail-closed
    );

    let output = Command::new(cli_binary())
        .args(["memory", "l3", "run", &id.to_string(), "--execute"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --execute (no python-exec)");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // Fail-closed: the single python.exec dispatch hits an unregistered tool, so
    // the run reports a step error and exits non-zero — never a silent success.
    assert!(
        !output.status.success(),
        "run against a daemon without python-exec must exit non-zero; got {:?}\n\
         stdout={stdout}\nstderr={stderr}",
        output.status,
    );
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("python-exec") || combined.to_lowercase().contains("not registered"),
        "the failure must name the missing python-exec tool; got:\nstdout={stdout}\nstderr={stderr}",
    );

    pool.close().await;
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 3 — runtime params round-trip: a skill that reads
// KASTELLAN_PYTHON_PARAMS receives the submitted {"greeting": "hi"} object
// and its stdout ("GOT:hi") flows back through the real jail into the
// InvokeReport rendered by the CLI.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_skill_params_round_trip_through_jail() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if missing_prereqs(true) {
        return;
    }
    let Some(python) = find_python() else {
        return;
    };
    let worker_bin = workspace_target_binary("kastellan-worker-python-exec");

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    let pool = prepare_db(&cluster).await;
    let id = seed_and_approve_skill(&pool, &param_echo_skill(), &cluster.data_dir, &user).await;

    let mock = spawn_inert_mock().await;
    let (daemon, _daemon_guards) = bring_up_daemon(
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        &worker_bin,
        &python,
        true, // python-exec enabled
    );

    // Pass {"greeting": "hi"} as runtime params. The daemon builds a
    // python.exec step with parameters: {code, params: {"greeting": "hi"}},
    // which the worker serialises and exposes as KASTELLAN_PYTHON_PARAMS.
    let output = Command::new(cli_binary())
        .args([
            "memory",
            "l3",
            "run",
            &id.to_string(),
            "--param",
            "greeting=hi",
            "--execute",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --param greeting=hi --execute");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "param round-trip run must exit 0; got {:?}\n\
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
    assert!(
        stdout.contains("executed skill"),
        "stdout must report 'executed skill'; got:\n{stdout}\n--- stderr ---\n{stderr}",
    );
    // The skill printed "GOT:hi\n"; the InvokeReport embeds the worker's
    // stdout in the step result JSON, so the rendered CLI output carries it.
    assert!(
        stdout.contains("GOT:hi"),
        "stdout must carry the param-echo marker 'GOT:hi'; got:\n{stdout}",
    );

    // TODO(params-e2e): secret-param coverage needs the vault harness (deferred).
    // The recursive substitute_refs_in_params walker is unit-tested in
    // core/src/secrets/substitute.rs; the e2e confirmation is a nice-to-have.

    pool.close().await;
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 4 — over-cap params rejection: params serialising to >64 KiB are
// rejected by the core gate (validate_python_params) before dispatch, and the
// CLI renders a REFUSED outcome (exit non-zero). Asserted at the CLI output
// layer, matching the fail-closed assertion style of scenario 2.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_skill_over_cap_params_refused() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if missing_prereqs(true) {
        return;
    }
    let Some(python) = find_python() else {
        return;
    };
    let worker_bin = workspace_target_binary("kastellan-worker-python-exec");

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    let pool = prepare_db(&cluster).await;
    // Any approved skill works — the gate fires before execution.
    let id = seed_and_approve_python_skill(&pool, &cluster.data_dir, &user).await;

    let mock = spawn_inert_mock().await;
    let (_daemon, _daemon_guards) = bring_up_daemon(
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        &worker_bin,
        &python,
        true, // python-exec enabled
    );

    // Build a params JSON object whose serialised form exceeds the 64 KiB cap.
    // {"greeting": "xxx…"} with 64*1024 x's serialises to ~65551 bytes > 65536.
    let big_value = "x".repeat(64 * 1024);
    let big_params = serde_json::json!({ "greeting": big_value });
    let params_json_str = serde_json::to_string(&big_params).expect("serialise big params");

    let output = Command::new(cli_binary())
        .args([
            "memory",
            "l3",
            "run",
            &id.to_string(),
            "--params-json",
            &params_json_str,
            "--execute",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run (over-cap params)");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // The core gate (validate_python_params) fires in the daemon's l3_run
    // handler before dispatch. It returns InvokeReport::Refused, which the CLI
    // renders as "REFUSED …" and exits non-zero.
    assert!(
        !output.status.success(),
        "over-cap params must cause a non-zero exit; got {:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.to_lowercase().contains("cap") || combined.to_lowercase().contains("refused"),
        "the failure must mention the cap or REFUSED; got:\nstdout={stdout}\nstderr={stderr}",
    );

    pool.close().await;
    drop(cluster);
}
