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
//! round-trip through the jail; the clobber-proof child env; and large (>64 KiB)
//! params delivered to the skill via the scratch-file channel.
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

/// A Python skill that reads runtime params via the FILE channel when present
/// (params >64 KiB), falling back to the inline env var, and prints the byte
/// length of the `greeting` key — used to prove a large param survives the
/// scratch-file channel end-to-end through the daemon path.
fn large_param_echo_skill() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "large_param_echo_py".into(),
        description: "Echo the length of the greeting runtime param (file channel)".into(),
        code: "import os, json\np = os.environ.get('KASTELLAN_PYTHON_PARAMS_FILE')\nif p:\n    with open(p) as f:\n        params = json.load(f)\nelse:\n    params = json.loads(os.environ.get('KASTELLAN_PYTHON_PARAMS', '{}'))\nprint('GOT:' + str(len(params['greeting'])))\n".into(),
    }
}

/// A Python skill that echoes the `token` runtime param prefixed with `TOKEN:`.
/// Used by the secret-scrub scenario: the daemon substitutes a seeded
/// `secret://` ref in `token` to plaintext before the worker runs, the worker
/// prints it, and the output scrub must redact it on the way back out.
fn secret_param_echo_skill() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "secret_param_echo_py".into(),
        description: "Echo the token runtime param".into(),
        code: "import os, json\np = json.loads(os.environ['KASTELLAN_PYTHON_PARAMS'])\nprint('TOKEN:' + p['token'])\n".into(),
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
    setup_with_env(skill, enable_python, Vec::new()).await
}

/// Like [`setup`], but folds `extra_env` into the daemon's environment on top of
/// the python-exec worker registration. Used by the secret-scrub scenario to
/// inject the test-only `KASTELLAN_TEST_VAULT_SEED` seam (#298).
async fn setup_with_env(
    skill: &PythonSkillCandidate,
    enable_python: bool,
    extra_env: Vec<(String, String)>,
) -> Option<Fixture> {
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

    let mut env = python_env(&worker_bin, &python, enable_python);
    env.extend(extra_env);

    let mock = spawn_inert_mock().await;
    let (daemon, guards) = bring_up_daemon(
        "l3pyrun",
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        env,
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

    // Secret-param scrub through the FULL DAEMON is covered by
    // `secret_param_round_trips_and_is_scrubbed_through_daemon` below (#298). The
    // in-process complement is
    // python_exec_e2e::materialized_secret_param_is_scrubbed_from_output.

    fx.pool.close().await;
    drop(fx.cluster);
}

// ---------------------------------------------------------------------------
// Scenario 4 — env clobber proof: the python-exec child env is clobber-proof.
// Runtime params are JSON inside KASTELLAN_PYTHON_PARAMS, never separate env
// vars, and the worker's env_clear() keeps every host lockdown var out of the
// child. We pass params named like dangerous env vars (`path`, `ld_preload`)
// and assert the child's env keys are EXACTLY {HOME, KASTELLAN_PYTHON_PARAMS,
// TMPDIR} (plus LC_CTYPE on Linux — a benign CPython PEP 538 locale artifact,
// see the per-platform expected set in the assertion below).
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
    // CPython's PEP 538 C-locale coercion injects LC_CTYPE=C.UTF-8 into the
    // child on Linux, because the worker's env_clear() leaves no LANG/LC_* ⇒ the
    // interpreter sees the C locale and coerces. It is the interpreter's OWN
    // benign artifact — not a runtime-param or host-var leak — so the exact set
    // includes it on Linux. macOS framework python does not coerce, so the set
    // stays the original three there. The guarantee this test exists to catch (no
    // param/host-var leak) is preserved exactly on each platform.
    let expected = if cfg!(target_os = "linux") {
        "HOME,KASTELLAN_PYTHON_PARAMS,LC_CTYPE,TMPDIR"
    } else {
        "HOME,KASTELLAN_PYTHON_PARAMS,TMPDIR"
    };
    assert_eq!(
        env_keys, expected,
        "python-exec child env must be EXACTLY {expected:?}; \
         a differing set means a runtime param leaked as an env var or a host var leaked into the child; \
         got full stdout:\n{stdout}",
    );

    fx.pool.close().await;
    drop(fx.cluster);
}

// ---------------------------------------------------------------------------
// Scenario 5 — large (>64 KiB) params via the scratch-file channel: a param
// that exceeds the 64 KiB inline-env threshold (but is under the 1 MiB worker
// file ceiling) is delivered to the skill through <scratch>/params.json +
// KASTELLAN_PYTHON_PARAMS_FILE, end-to-end through the daemon's l3_run path.
// (The CLI argv channel cannot carry an over-ceiling payload — MAX_ARG_STRLEN
// is 128 KiB on Linux — so over-cap REFUSAL is covered by the worker/host unit
// tests, not here. This scenario proves the positive file-channel delivery.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_skill_large_params_via_file_channel() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    let Some(fx) = setup(&large_param_echo_skill(), true).await else {
        return;
    };

    // 80 KiB greeting: > the 64 KiB inline threshold (forces the file channel)
    // and < the 128 KiB Linux per-arg argv limit (so --params-json can carry it).
    let big_value = "x".repeat(80 * 1024);
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
        .expect("spawn kastellan-cli memory l3 run (large params)");

    // The param exceeds the inline env threshold, so the worker writes it to
    // <scratch>/params.json and the skill reads it via the file channel; the run
    // succeeds and echoes the full 80 KiB length (81920) — a broken file channel
    // would leave the inline env as "{}" → KeyError → non-zero exit.
    let (stdout, _stderr) = assert_cli_success(&output, &fx.daemon, "large params run");
    assert!(
        stdout.contains("GOT:81920"),
        "skill must echo the full 80 KiB greeting length via the file channel; got:\n{stdout}",
    );

    fx.pool.close().await;
    drop(fx.cluster);
}

// ---------------------------------------------------------------------------
// Scenario 6 — full-daemon secret output-scrub (#298). A secret materialized
// into the DAEMON's in-process Vault under a test-known `secret://` ref (the
// `#[cfg(debug_assertions)]` `KASTELLAN_TEST_VAULT_SEED` seam) is passed by the
// separate CLI process as the `token` param. The daemon's `dispatch`
// substitutes the ref to plaintext before the worker runs; the skill echoes it;
// and the output scrub redacts it before the CLI renders the InvokeReport.
//
// This closes the gap the in-process `python_exec_e2e` test cannot reach: the
// real CLI → tasks queue → scheduler → l3py_invoke → ToolHostStepDispatcher →
// dispatch routing, end-to-end through the live daemon subprocess.
//
// The two assertions are jointly non-vacuous: if substitution failed the output
// would carry the literal `secret://` ref and the `[redacted:` marker would be
// absent (FAIL); if the scrub failed the plaintext would survive (FAIL).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_param_round_trips_and_is_scrubbed_through_daemon() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    // A distinctive plaintext well over the leak scanner's MIN_SECRET_LEN (8
    // bytes) so an un-scrubbed leak would be unmistakable in the failure output.
    const PLAINTEXT: &str = "SCRUBME-7c1f9a2b-daemon-secret-do-not-leak";
    const REF_HEX: &str = "deadbe01";

    let Some(fx) = setup_with_env(
        &secret_param_echo_skill(),
        true,
        vec![(
            "KASTELLAN_TEST_VAULT_SEED".into(),
            format!("{REF_HEX}={PLAINTEXT}"),
        )],
    )
    .await
    else {
        return;
    };

    // The CLI (a separate process) passes the ref it knows up front. The daemon
    // substitutes `secret://deadbe01` → PLAINTEXT inside `dispatch`.
    let output = cli_command(&fx.cluster.data_dir, &fx.user)
        .args([
            "memory",
            "l3",
            "run",
            &fx.id.to_string(),
            "--param",
            &format!("token=secret://{REF_HEX}"),
            "--execute",
        ])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --param token=secret://… --execute");

    let (stdout, stderr) = assert_cli_success(&output, &fx.daemon, "secret-scrub run");
    assert!(
        stdout.contains("executed skill"),
        "stdout must report 'executed skill'; got:\n{stdout}\n--- stderr ---\n{stderr}",
    );
    // The plaintext must NOT survive into the rendered InvokeReport...
    assert!(
        !stdout.contains(PLAINTEXT),
        "materialized secret plaintext leaked through the daemon's rendered output:\n{stdout}",
    );
    // ...and the scrub must have replaced it with the redaction marker.
    assert!(
        stdout.contains("[redacted:"),
        "expected a [redacted:<hex>] marker in the scrubbed output; got:\n{stdout}",
    );

    // The redacted scrub audit row landed (hash/offset/len only — never
    // plaintext; that shape is unit-pinned in tool_host::secret_scrub).
    let scrub_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor='policy' AND action='secret.output_scrubbed'",
    )
    .fetch_one(&fx.pool)
    .await
    .expect("count scrub rows");
    assert!(
        scrub_rows >= 1,
        "expected at least one secret.output_scrubbed audit row, got {scrub_rows}",
    );

    fx.pool.close().await;
    drop(fx.cluster);
}
