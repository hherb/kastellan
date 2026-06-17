//! Real-daemon e2e for the python-exec skill-catalog invocation path (slice 2).
//!
//! Mirrors `cli_memory_l3_run_daemon_e2e.rs` one payload over: an approved
//! **Python** skill, submitted via `kastellan-cli memory l3 run <id> --execute`,
//! is executed by the **daemon** against ITS OWN live registry under the real
//! python-exec jail, and the snippet's stdout comes back in the `InvokeReport`.
//!
//! The shared daemon bring-up + inert mock LLM + operator-CLI command builder
//! live in `kastellan_tests_common` (`daemon` + `binaries` modules); only the
//! python-specific bits (the interpreter cascade, the skill factories) are
//! local here.
//!
//! What this pins (one scenario each, fully described at its banner below): the
//! happy path + the #179 invariant + the `kind:"python"` audit row; the
//! fail-closed contract when python-exec is unregistered; the runtime-params
//! round-trip through the jail; the clobber-proof child env; and >64 KiB params
//! refused by the core gate before dispatch.
//!
//! ## Skip semantics
//!
//! Every daemon scenario short-circuits with a `[SKIP]` print when the host
//! lacks a supervisor, a sandbox backend, `pg_ctl`, the workspace binaries, or
//! a usable python3 interpreter. On the DGX (live PG + bwrap) and the Mac
//! (live PG + Seatbelt) they RUN. Cross-platform (Linux + macOS).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};

use kastellan_core::cassandra::types::PythonSkillCandidate;
use kastellan_core::memory::l3_crystallise::L3Source;
use kastellan_core::memory::l3py_crystallise::crystallise_python_skill;
use kastellan_core::workers::python_exec::PYTHON_CANDIDATES;
use kastellan_tests_common::{
    assert_cli_failure, assert_cli_success, bring_up_daemon, bring_up_pg_cluster, cli_binary,
    cli_command, core_binary, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, spawn_inert_mock, unique_suffix, workspace_target_binary,
    DaemonGuards, DaemonHandle, MockLlm, PgCluster,
};
#[cfg(target_os = "macos")]
use kastellan_tests_common::serial_lock;

/// The skill's snippet prints this marker; it must round-trip back through the
/// worker's `stdout` capture into the rendered `InvokeReport`.
const SNIPPET_MARKER: &str = "hi-from-py-skill";

/// The manifest's own per-OS interpreter cascade (it excludes macOS's
/// `/usr/bin/python3` xcrun shim, unusable in the jail). `None` ⇒ skip.
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

/// The python-exec worker env vars to register on the daemon. The worker bin +
/// interpreter are always passed; only `KASTELLAN_PYTHON_EXEC_ENABLE` is gated,
/// so the ONLY difference between the happy and fail-closed daemons is the flag.
fn python_env(worker_bin: &Path, python: &Path, enable: bool) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if enable {
        env.push(("KASTELLAN_PYTHON_EXEC_ENABLE".into(), "1".into()));
    }
    env.push((
        "KASTELLAN_PYTHON_EXEC_BIN".into(),
        worker_bin.to_string_lossy().into_owned(),
    ));
    env.push((
        "KASTELLAN_PYTHON_EXEC_PYTHON".into(),
        python.to_string_lossy().into_owned(),
    ));
    env
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

/// A Python skill that prints the SORTED keys of its own process environment,
/// so the test can pin EXACTLY which env vars the python-exec child sees —
/// proving runtime params (JSON inside `KASTELLAN_PYTHON_PARAMS`) never become
/// env vars and that no host lockdown env var (`KASTELLAN_LANDLOCK_*`, `PATH`,
/// `KASTELLAN_PYTHON_EXEC_PYTHON`, …) leaks into the child.
fn env_keys_skill() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "env_keys_py".into(),
        description: "Print the python-exec child's environment keys".into(),
        code: "import os\nprint('ENVKEYS:' + ','.join(sorted(os.environ.keys())))\n".into(),
    }
}

/// Crystallise → approve a [`PythonSkillCandidate`] via the CLI; returns its
/// memory id. Unlike the templated path, the Python approve gate needs NO
/// `registry.loaded` snapshot (a python skill dispatches no tools — the jail is
/// its ceiling), so none is seeded.
async fn seed_and_approve_skill(
    pool: &sqlx::PgPool,
    skill: &PythonSkillCandidate,
    data_dir: &Path,
    user: &str,
) -> i64 {
    let outcome = crystallise_python_skill(pool, skill, L3Source::AgentRaised { task_id: 1 })
        .await
        .expect("crystallise_python_skill");
    let id = outcome.memory_id();

    let approve = cli_command(data_dir, user)
        .args(["memory", "l3", "approve", &id.to_string()])
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
fn missing_prereqs() -> bool {
    if skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
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

/// Everything a python l3_run scenario needs after bring-up. The `_mock` and
/// `_guards` fields are held only for their `Drop` (socket + service cleanup).
struct Fixture {
    cluster: PgCluster,
    pool: sqlx::PgPool,
    daemon: DaemonHandle,
    user: String,
    id: i64,
    _mock: MockLlm,
    _guards: DaemonGuards,
}

/// Common bring-up for every scenario: skip-guards + interpreter probe, a fresh
/// PG cluster, an approved copy of `skill`, and a booted daemon with python-exec
/// registered iff `enable_python`. Returns `None` (already `[SKIP]`-printed)
/// when a host prerequisite is missing. The caller holds the macOS serial lock.
async fn setup(skill: &PythonSkillCandidate, enable_python: bool) -> Option<Fixture> {
    if missing_prereqs() {
        return None;
    }
    let python = find_python()?;
    let worker_bin = workspace_target_binary("kastellan-worker-python-exec");

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    let pool = prepare_db(&cluster).await;
    let id = seed_and_approve_skill(&pool, skill, &cluster.data_dir, &user).await;

    let mock = spawn_inert_mock().await;
    let (daemon, guards) = bring_up_daemon(
        "l3pyrun",
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        python_env(&worker_bin, &python, enable_python),
    );

    Some(Fixture {
        cluster,
        pool,
        daemon,
        user,
        id,
        _mock: mock,
        _guards: guards,
    })
}

// ---------------------------------------------------------------------------
// Scenario 1 — happy path + the #179 invariant + the kind:"python" audit row.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_run_succeeds_against_daemon_registry() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    let Some(fx) = setup(&hello_python_skill(), true).await else {
        return;
    };

    // Operator CLI subprocess: NO KASTELLAN_PYTHON_EXEC_BIN. The daemon executes
    // against its own registry (the #179 invariant).
    let output = cli_command(&fx.cluster.data_dir, &fx.user)
        .args(["memory", "l3", "run", &fx.id.to_string(), "--execute"])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --execute");

    let (stdout, stderr) = assert_cli_success(&output, &fx.daemon, "run --execute");
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
    .fetch_optional(&fx.pool)
    .await
    .expect("query l3.invoke_outcome audit row")
    .flatten();
    assert_eq!(
        outcome_kind.as_deref(),
        Some("python"),
        "the l3.invoke_outcome audit row must carry kind:\"python\"",
    );

    fx.pool.close().await;
    drop(fx.cluster);
}

// ---------------------------------------------------------------------------
// Scenario 2 — fail-closed: a daemon without python-exec enabled refuses the
// run with a tool-not-registered error rather than silently doing nothing.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_run_fails_closed_when_python_exec_disabled() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    let Some(fx) = setup(&hello_python_skill(), false).await else {
        return;
    };

    let output = cli_command(&fx.cluster.data_dir, &fx.user)
        .args(["memory", "l3", "run", &fx.id.to_string(), "--execute"])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --execute (no python-exec)");

    // Fail-closed: the single python.exec dispatch hits an unregistered tool, so
    // the run reports a step error and exits non-zero — never a silent success.
    let (stdout, stderr) =
        assert_cli_failure(&output, "run against a daemon without python-exec");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("python-exec") || combined.to_lowercase().contains("not registered"),
        "the failure must name the missing python-exec tool; got:\nstdout={stdout}\nstderr={stderr}",
    );

    fx.pool.close().await;
    drop(fx.cluster);
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

    let Some(fx) = setup(&param_echo_skill(), true).await else {
        return;
    };

    // Pass {"greeting": "hi"} as runtime params. The daemon builds a
    // python.exec step with parameters: {code, params: {"greeting": "hi"}},
    // which the worker serialises and exposes as KASTELLAN_PYTHON_PARAMS.
    let output = cli_command(&fx.cluster.data_dir, &fx.user)
        .args([
            "memory",
            "l3",
            "run",
            &fx.id.to_string(),
            "--param",
            "greeting=hi",
            "--execute",
        ])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --param greeting=hi --execute");

    let (stdout, stderr) = assert_cli_success(&output, &fx.daemon, "param round-trip run");
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

    // Secret-param coverage lives elsewhere: the substitute-in + output scrub is
    // proven end-to-end by python_exec_e2e::materialized_secret_param_is_scrubbed_from_output
    // (same dispatch chokepoint); the full-DAEMON secret e2e is deferred to #298
    // (the secret:// ref is minted randomly + never logged, so the separate CLI
    // process needs a security-sensitive Vault-ref seam in main.rs).

    fx.pool.close().await;
    drop(fx.cluster);
}

// ---------------------------------------------------------------------------
// Scenario 4 — env clobber proof: the python-exec child env is clobber-proof.
// Runtime params are JSON inside KASTELLAN_PYTHON_PARAMS, never separate env
// vars, and the worker's env_clear() keeps every host lockdown var out of the
// child. We pass params named like dangerous env vars (`path`, `ld_preload`)
// and assert the child's env keys are EXACTLY {HOME, KASTELLAN_PYTHON_PARAMS,
// TMPDIR}.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_exec_child_env_is_clobber_proof() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    let Some(fx) = setup(&env_keys_skill(), true).await else {
        return;
    };

    let output = cli_command(&fx.cluster.data_dir, &fx.user)
        .args([
            "memory",
            "l3",
            "run",
            &fx.id.to_string(),
            "--param",
            "path=/evil/bin",
            "--param",
            "ld_preload=/evil.so",
            "--execute",
        ])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run env_keys --execute");

    let (stdout, _stderr) = assert_cli_success(&output, &fx.daemon, "env-keys run");
    // We assert the FULL sorted key list exactly: the `path`/`ld_preload` params
    // live INSIDE KASTELLAN_PYTHON_PARAMS as JSON keys, never as env vars, and a
    // substring check would miss a leaked var sorting after TMPDIR (e.g. the
    // lowercase params we pass) — precisely the leak this test exists to catch.
    //
    // The CLI renders the worker's stdout inside a JSON step-result object on one
    // line, so we slice from the ENVKEYS token up to the next `"` (the JSON value
    // terminator) and strip the encoded `\n` / surrounding whitespace.
    let env_token_start = stdout
        .find("ENVKEYS:")
        .unwrap_or_else(|| panic!("worker stdout must contain an ENVKEYS: token; got:\n{stdout}"));
    let env_value_start = env_token_start + "ENVKEYS:".len();
    let env_keys = stdout[env_value_start..]
        .split('"')
        .next()
        .unwrap_or("")
        .trim_matches(|c| c == '\n' || c == '\r' || c == '\\' || c == 'n')
        .trim();
    assert_eq!(
        env_keys, "HOME,KASTELLAN_PYTHON_PARAMS,TMPDIR",
        "python-exec child env must be EXACTLY {{HOME, KASTELLAN_PYTHON_PARAMS, TMPDIR}}; \
         a differing set means a runtime param leaked as an env var or a host var leaked into the child; \
         got full stdout:\n{stdout}",
    );

    fx.pool.close().await;
    drop(fx.cluster);
}

// ---------------------------------------------------------------------------
// Scenario 5 — over-cap params rejection: params serialising to >64 KiB are
// rejected by the core gate (validate_python_params) before dispatch, and the
// CLI renders a REFUSED outcome (exit non-zero). Asserted at the CLI output
// layer, matching the fail-closed assertion style of scenario 2.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_skill_over_cap_params_refused() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    // Any approved skill works — the gate fires before execution.
    let Some(fx) = setup(&hello_python_skill(), true).await else {
        return;
    };

    // Build a params JSON object whose serialised form exceeds the 64 KiB cap.
    // {"greeting": "xxx…"} with 64*1024 x's serialises to ~65551 bytes > 65536.
    let big_value = "x".repeat(64 * 1024);
    let big_params = serde_json::json!({ "greeting": big_value });
    let params_json_str = serde_json::to_string(&big_params).expect("serialise big params");

    let output = cli_command(&fx.cluster.data_dir, &fx.user)
        .args([
            "memory",
            "l3",
            "run",
            &fx.id.to_string(),
            "--params-json",
            &params_json_str,
            "--execute",
        ])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run (over-cap params)");

    // The core gate (validate_python_params) fires in the daemon's l3_run
    // handler before dispatch. It returns InvokeReport::Refused, which the CLI
    // renders as "REFUSED …" and exits non-zero.
    let (stdout, stderr) = assert_cli_failure(&output, "over-cap params run");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.to_lowercase().contains("cap") || combined.to_lowercase().contains("refused"),
        "the failure must mention the cap or REFUSED; got:\nstdout={stdout}\nstderr={stderr}",
    );

    fx.pool.close().await;
    drop(fx.cluster);
}
