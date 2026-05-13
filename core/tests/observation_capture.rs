//! Observation-phase orchestrator (#[ignore]-flagged).
//!
//! Brings up a per-test PG cluster + real `hhagent` daemon under
//! `systemd --user` / `launchctl` + sandboxed worker, points the daemon
//! at the **real local LLM** (operator's HHAGENT_LLM_LOCAL_URL), iterates
//! every fixture under `tests/observation/fixtures/`, runs each through
//! `hhagent-cli ask`, queries `audit_log` for the task's rows, and
//! writes one capture JSON per fixture under
//! `tests/observation/captures/<id>/<date>_<model_slug>.json`.
//!
//! ## Invocation
//!
//! ```sh
//! cargo test -p hhagent-core --test observation_capture \
//!     -- --ignored --nocapture
//! ```
//!
//! Env knobs:
//! - `HHAGENT_LLM_LOCAL_URL` (required) — operator's local LLM endpoint
//! - `HHAGENT_LLM_LOCAL_MODEL` (default: "gemma4:26b-a4b-it-q8_0")
//! - `HHAGENT_OBSERVATION_DRY_RUN=1` — walk fixtures + print work plan,
//!   no LLM dial, no file write
//!
//! ## Why #[ignore]
//!
//! The live-LLM dependency is not CI-friendly. Operators invoke this
//! manually after authoring or revising a fixture.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_core::observation::capture::{
    capture_filename, extract_plans_from_audit_rows, fetch_audit_rows_for_task,
    parse_fixture_prompt, slug_model, write_capture_to_dir, CaptureJson, SCHEMA_VERSION,
};
use hhagent_db::{conn::ConnectSpec, pool::connect_runtime_pool};
use hhagent_supervisor::specs::core_service_spec;
use hhagent_supervisor::{default_supervisor, ServiceStatus};
use hhagent_tests_common::{
    bring_up_pg_cluster, cli_binary, core_binary, current_username, pg_bin_dir_or_skip,
    shell_exec_worker_binary, skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix,
    unique_temp_root, wait_for_log_match, wait_for_status, PathGuard, PgCluster, ServiceGuard,
};
#[cfg(target_os = "macos")]
use hhagent_tests_common::serial_lock;

const ECHO_PATH_LINUX: &str = "/usr/bin/echo";
const ECHO_PATH_MACOS: &str = "/bin/echo";
const DATE_PATH_LINUX: &str = "/usr/bin/date";
const DATE_PATH_MACOS: &str = "/bin/date";
const LS_PATH_LINUX: &str = "/usr/bin/ls";
const LS_PATH_MACOS: &str = "/bin/ls";

const DEFAULT_LLM_MODEL: &str = "gemma4:26b-a4b-it-q8_0";

/// 120 s per fixture is generous: real warm capture against a loaded
/// model is 5-15 s on the operator's DGX. The slack is for cold-start.
const PER_FIXTURE_TIMEOUT: Duration = Duration::from_secs(120);

/// Locate `tests/observation/` relative to the workspace root.
fn observation_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("tests")
        .join("observation")
}

#[derive(Debug)]
struct FixtureMeta {
    fixture_id: String,
    summary: String,
    prompt: String,
}

/// Walk every subdirectory of `tests/observation/fixtures/`, parse its
/// prompt.md and meta.toml, return a sorted list (fixture_id ascending).
fn load_fixtures() -> Vec<FixtureMeta> {
    let fixtures_root = observation_root().join("fixtures");
    if !fixtures_root.exists() {
        panic!("missing fixtures dir: {}", fixtures_root.display());
    }
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&fixtures_root)
        .expect("read_dir fixtures")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let dir = entry.path();
        let id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .expect("fixture dir name")
            .to_string();
        let prompt_md = std::fs::read_to_string(dir.join("prompt.md"))
            .unwrap_or_else(|e| panic!("read prompt.md for {id}: {e}"));
        let (summary, prompt) = parse_fixture_prompt(&prompt_md)
            .unwrap_or_else(|e| panic!("parse prompt.md for {id}: {e}"));
        // meta.toml is parsed but not retained — its fields are
        // informational for the rule-iteration follow-up, not used by
        // the orchestrator. We still read it to enforce it parses.
        let meta_toml = std::fs::read_to_string(dir.join("meta.toml"))
            .unwrap_or_else(|e| panic!("read meta.toml for {id}: {e}"));
        let _: toml::Value = toml::from_str(&meta_toml)
            .unwrap_or_else(|e| panic!("parse meta.toml for {id}: {e}"));
        out.push(FixtureMeta {
            fixture_id: id,
            summary,
            prompt,
        });
    }
    out
}

/// Try to dial `<base_url>/v1/models` (OpenAI-compat health endpoint).
/// Returns Ok if the server accepts our request and replies with at
/// least one byte within 5 s. On failure, returns a string suitable for
/// inclusion in the test's panic message.
///
/// We require a non-zero read so a stale listener that accepts and
/// immediately closes (zero-byte read) does not pass the check —
/// otherwise the orchestrator would race the LLM and surface confusing
/// errors deep in the capture loop.
fn check_llm_reachable(base_url: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::str::FromStr;

    // Parse base_url into host:port + path.
    let stripped = base_url.trim_end_matches('/');
    let after_scheme = stripped
        .strip_prefix("http://")
        .or_else(|| stripped.strip_prefix("https://"))
        .ok_or_else(|| format!("base_url must start with http:// or https://: {base_url}"))?;
    let (authority, _path) = match after_scheme.find('/') {
        Some(i) => after_scheme.split_at(i),
        None => (after_scheme, ""),
    };
    let (host, port_str) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => return Err(format!("base_url is missing port: {base_url}")),
    };
    let port = u16::from_str(port_str).map_err(|e| format!("port parse: {e}"))?;

    let addr = format!("{host}:{port}");
    let stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("addr parse: {e}"))?,
        Duration::from_secs(5),
    )
    .map_err(|e| format!("tcp connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok();
    // Send a minimal HTTP GET; we don't validate the response shape,
    // just that the server speaks HTTP. /v1/models on a healthy LLM
    // returns 200; some return 401; both prove the server is up and
    // both write a status line that contains > 0 bytes.
    let mut s = stream;
    let req = format!("GET /v1/models HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).map_err(|e| format!("read: {e}"))?;
    if n == 0 {
        return Err(format!(
            "server at {addr} accepted the TCP connection but closed without writing a byte"
        ));
    }
    Ok(())
}

struct DaemonHandles {
    _service: ServiceGuard,
    _core_log: PathGuard,
    _state: PathGuard,
}

fn bring_up_daemon(
    suffix: &str,
    data_dir: &Path,
    llm_base_url: &str,
    llm_model: &str,
    user: &str,
) -> DaemonHandles {
    let core_log_dir = unique_temp_root("obs-clog");
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let core_log = PathGuard {
        path: core_log_dir.clone(),
    };

    let state_dir = unique_temp_root("obs-state");
    let state_guard = PathGuard {
        path: state_dir.clone(),
    };

    let binary = core_binary();
    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("hhagent-supervisor-test-core-obs-{suffix}");
    assert!(spec.name.len() <= 200);
    let stdout_path = core_log_dir.join(format!("{}.out", spec.name));
    let stderr_path = core_log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path);

    spec.env.push((
        "HHAGENT_DATA_DIR".into(),
        data_dir.to_string_lossy().into_owned(),
    ));
    spec.env.push(("USER".into(), user.to_string()));
    spec.env.push((
        "HHAGENT_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));

    let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("prompts");
    spec.env.push((
        "HHAGENT_PROMPTS_DIR".into(),
        workspace_prompts.to_string_lossy().into_owned(),
    ));

    spec.env.push((
        "HHAGENT_LLM_LOCAL_URL".into(),
        llm_base_url.to_string(),
    ));
    spec.env.push((
        "HHAGENT_LLM_LOCAL_MODEL".into(),
        llm_model.to_string(),
    ));
    spec.env.push(("HHAGENT_LLM_TIMEOUT_MS".into(), "120000".into()));

    spec.env.push((
        "HHAGENT_SHELL_EXEC_BIN".into(),
        shell_exec_worker_binary().to_string_lossy().into_owned(),
    ));
    // Permissive allowlist for observation: echo, date, ls, cat (read-only).
    let allowlist = if cfg!(target_os = "linux") {
        format!(
            "{}:{}:{}:{}",
            ECHO_PATH_LINUX, DATE_PATH_LINUX, LS_PATH_LINUX, "/bin/cat"
        )
    } else {
        format!(
            "{}:{}:{}:{}",
            ECHO_PATH_MACOS, DATE_PATH_MACOS, LS_PATH_MACOS, "/bin/cat"
        )
    };
    spec.env
        .push(("HHAGENT_SHELL_EXEC_ALLOWLIST".into(), allowlist));

    let sup = default_supervisor();
    let service = ServiceGuard {
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
        Duration::from_secs(15),
    )
    .expect("daemon should log 'scheduler spawned' within 15s");

    DaemonHandles {
        _service: service,
        _core_log: core_log,
        _state: state_guard,
    }
}

/// Submit one prompt via `hhagent-cli ask`, then capture the audit-log
/// stream for the resulting task. Returns the constructed CaptureJson.
async fn capture_one_fixture(
    pool: &sqlx::PgPool,
    data_dir: &Path,
    user: &str,
    fixture: &FixtureMeta,
    llm_backend: &str,
    llm_model: &str,
    llm_base_url: &str,
    captured_at: &str,
) -> CaptureJson {
    // Snapshot max(id) so we can identify the new task after the CLI
    // returns. Serial submission means exactly one row will appear.
    let prior_max: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM tasks")
        .fetch_one(pool)
        .await
        .expect("snapshot max id");

    let start = Instant::now();
    let output = Command::new(cli_binary())
        .arg("ask")
        .arg(&fixture.prompt)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", user)
        .env("HHAGENT_DATA_DIR", data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn hhagent-cli ask");
    let elapsed = start.elapsed();
    assert!(
        elapsed < PER_FIXTURE_TIMEOUT,
        "fixture {} exceeded {:?}; CLI elapsed {:?}",
        fixture.fixture_id,
        PER_FIXTURE_TIMEOUT,
        elapsed
    );
    let _ = output; // exit code and stdout body are informational
                    // (some fixtures intentionally fail); the capture
                    // is in the audit log either way.

    // Identify the new task.
    let task_id: i64 =
        sqlx::query_scalar("SELECT id FROM tasks WHERE id > $1 ORDER BY id ASC LIMIT 1")
            .bind(prior_max)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| {
                panic!("no task appeared for fixture {}: {e}", fixture.fixture_id)
            });

    let task_state: String = sqlx::query_scalar("SELECT state FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(pool)
        .await
        .expect("read tasks.state");

    let audit_rows = fetch_audit_rows_for_task(pool, task_id)
        .await
        .expect("fetch audit rows");
    let plans = extract_plans_from_audit_rows(&audit_rows);

    CaptureJson {
        schema_version: SCHEMA_VERSION,
        fixture_id: fixture.fixture_id.clone(),
        fixture_summary: fixture.summary.clone(),
        captured_at: captured_at.to_string(),
        llm_backend: llm_backend.to_string(),
        llm_model: llm_model.to_string(),
        llm_base_url: llm_base_url.to_string(),
        prompt: fixture.prompt.clone(),
        task_id,
        task_state,
        plan_iterations: plans.len() as u32,
        plans,
        audit_rows,
    }
}

fn dry_run_enabled() -> bool {
    std::env::var("HHAGENT_OBSERVATION_DRY_RUN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn dry_run_report(fixtures: &[FixtureMeta]) {
    eprintln!(
        "\n[DRY RUN] would capture {} fixtures (HHAGENT_OBSERVATION_DRY_RUN=1):",
        fixtures.len()
    );
    for f in fixtures {
        eprintln!(
            "  - id={}  summary={:?}  prompt_chars={}",
            f.fixture_id,
            f.summary,
            f.prompt.chars().count()
        );
    }
    eprintln!("[DRY RUN] no LLM dial; no file writes.\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "operator-run: needs real local LLM at HHAGENT_LLM_LOCAL_URL"]
async fn capture_all_fixtures_against_live_llm() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    let fixtures = load_fixtures();
    assert!(
        !fixtures.is_empty(),
        "expected at least one fixture under tests/observation/fixtures/"
    );

    if dry_run_enabled() {
        dry_run_report(&fixtures);
        return;
    }

    // Skip the same things cli_ask_e2e skips — operator does not lose
    // data because we never fired the LLM call.
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    // LLM env. Fail loudly on missing URL or unreachable backend —
    // operators ran this explicitly; a silent skip would produce no
    // captures and waste their time.
    let llm_base_url = std::env::var("HHAGENT_LLM_LOCAL_URL").unwrap_or_else(|_| {
        panic!(
            "HHAGENT_LLM_LOCAL_URL is required; set it to your local LLM \
             OpenAI-compat base URL (e.g. http://127.0.0.1:11434/v1)"
        )
    });
    let llm_model = std::env::var("HHAGENT_LLM_LOCAL_MODEL")
        .unwrap_or_else(|_| DEFAULT_LLM_MODEL.to_string());
    if let Err(why) = check_llm_reachable(&llm_base_url) {
        panic!(
            "LLM at {} unreachable: {}. Start your local LLM before running this test.",
            llm_base_url, why
        );
    }

    let suffix = unique_suffix();
    let user = current_username();
    let cluster: PgCluster = bring_up_pg_cluster(
        &bin_dir,
        "obs-cap-d",
        "obs-cap-l",
        &format!("hhagent-supervisor-test-pg-obs-{suffix}"),
    );

    let _daemon = bring_up_daemon(&suffix, &cluster.data_dir, &llm_base_url, &llm_model, &user);

    let spec = ConnectSpec::default_for(&cluster.data_dir).expect("spec");
    let pool = connect_runtime_pool(&spec).await.expect("pool");

    // RFC 3339 timestamp once at the top so all per-fixture captures
    // share a single date prefix in their filenames.
    let captured_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("rfc 3339");

    let captures_root = observation_root().join("captures");
    std::fs::create_dir_all(&captures_root).expect("create captures root");

    let mut summary: BTreeMap<String, String> = BTreeMap::new();
    for fixture in &fixtures {
        eprintln!("\n[obs] capturing fixture {}", fixture.fixture_id);
        let cap = capture_one_fixture(
            &pool,
            &cluster.data_dir,
            &user,
            fixture,
            "local",
            &llm_model,
            &llm_base_url,
            &captured_at,
        )
        .await;
        let dest = write_capture_to_dir(&captures_root, &cap)
            .unwrap_or_else(|e| panic!("write capture for {}: {e}", fixture.fixture_id));
        eprintln!(
            "[obs]   → {} (task_state={}, plan_iters={})",
            dest.display(),
            cap.task_state,
            cap.plan_iterations
        );
        summary.insert(fixture.fixture_id.clone(), cap.task_state);
    }

    eprintln!("\n[obs] capture summary:");
    for (id, state) in &summary {
        eprintln!("  {} → {}", id, state);
    }
    eprintln!(
        "[obs] {} captures written under {}",
        summary.len(),
        captures_root.display()
    );

    // Pin the on-disk slug shape so a slug_model regression surfaces here too.
    let slug = slug_model(&llm_model);
    assert!(!slug.is_empty(), "llm_model must slug to non-empty");
    let fname = capture_filename(&captured_at[..10], &slug);
    assert!(fname.ends_with(".json"));

    // Teardown is intentionally LEFT to scope-end RAII so the daemon
    // (_daemon, declared before `pool`) drops AFTER pool but BEFORE
    // cluster — the correct order: daemon stops while PG is still alive,
    // then PG tears down. Explicit `drop(pool); drop(cluster);` would
    // tear PG down first and force the daemon to shut down against a
    // missing DB.
}
