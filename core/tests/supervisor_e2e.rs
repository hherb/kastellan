//! End-to-end smoke for the cross-platform supervisor wiring +
//! database probe.
//!
//! The daemon's hard dependency on a live Postgres (added in C2.2 —
//! see `core/src/main.rs::bring_up_database`) means this test brings
//! up *two* services in sequence and verifies the daemon's bring-up
//! contract end-to-end:
//!
//!   1. `initdb` a per-test temp cluster (peer-auth, UDS only).
//!   2. Install + start `kastellan-postgres` via `default_supervisor()`.
//!      Wait for Active and the listening socket.
//!   3. Build the `core_service_spec` for the freshly-built `kastellan`
//!      binary, override `KASTELLAN_DATA_DIR` to point at the temp
//!      cluster, install + start the service, wait for Active, hold
//!      500 ms and re-check (no flapping under `Restart=on-failure`).
//!   4. Sanity-check the daemon's stdout log for the startup JSON
//!      line and the "database probe succeeded" follow-up.
//!   5. Connect via `psql` and assert the bring-up `audit_log` row
//!      (actor=`core`, action=`startup`) is present — proves the
//!      probe ran end-to-end through migrations.
//!   6. Stop kastellan → wait Inactive → uninstall.
//!
//! Bring-up scaffolding + skip helpers + RAII guards now live in
//! `kastellan-tests-common` (issue #15).
//!
//! Skips silently with `[SKIP]` lines on hosts where any precondition
//! is missing; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kastellan_core::STARTUP_READY_MSG;
use kastellan_supervisor::specs::core_service_spec;
use kastellan_supervisor::{default_supervisor, ServiceStatus};
use kastellan_tests_common::{
    bring_up_pg_cluster, core_binary, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    unique_suffix, unique_temp_root, wait_for_log_match, wait_for_status, PathGuard, ServiceGuard,
};
#[cfg(target_os = "macos")]
use kastellan_tests_common::serial_lock;

/// Read every `audit-*.jsonl` file under `state_dir` (concatenated)
/// and return the body once `predicate(&body)` is true, or fail with
/// a verbose error if `timeout` elapses first.
///
/// Used to assert that the audit-mirror task has picked up a freshly
/// written row. The audit-mirror writes to a date-named file inside
/// `state_dir`, so we don't know the exact filename a priori — but
/// every existing audit file under the dir is fair game. Kept here
/// (not in `kastellan-tests-common`) because no other test reads the
/// state-dir mirror today.
fn wait_for_state_dir_match<F: Fn(&str) -> bool>(
    state_dir: &Path,
    predicate: F,
    timeout: Duration,
) -> Result<String, String> {
    let start = Instant::now();
    loop {
        let body = read_state_dir_jsonl(state_dir);
        if predicate(&body) {
            return Ok(body);
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {:?}; concatenated JSONL body:\n---\n{}\n---",
                timeout, body
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read_state_dir_jsonl(state_dir: &Path) -> String {
    let mut out = String::new();
    let entries = match std::fs::read_dir(state_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("audit-") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    for p in paths {
        if let Ok(body) = std::fs::read_to_string(&p) {
            out.push_str(&body);
        }
    }
    out
}

#[test]
fn core_starts_runs_db_probe_writes_audit_row_and_shuts_down_cleanly() {
    // Hold the macOS-only mutex for the full body so the launchd
    // domain isn't touched concurrently by other launchd-using tests.
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let binary = core_binary();
    if !binary.exists() {
        eprintln!(
            "\n[SKIP] kastellan binary not found at {}; run `cargo build --workspace` first\n",
            binary.display()
        );
        return;
    }

    let suffix = unique_suffix();

    // ---------- step 1: bring up the per-test PG cluster ----------
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pg-d",
        "pg-l",
        &format!("kastellan-supervisor-test-pg-{suffix}"),
    );

    // ---------- step 2: build the core service spec ----------
    let core_log_dir = unique_temp_root(&format!("core-l-{suffix}"));
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let _core_log_guard = PathGuard {
        path: core_log_dir.clone(),
    };

    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("kastellan-supervisor-test-core-{suffix}");
    assert!(spec.name.len() <= 200);
    let stdout_path = core_log_dir.join(format!("{}.out", spec.name));
    let stderr_path = core_log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path.clone());

    // The daemon resolves its data dir from `KASTELLAN_DATA_DIR` before
    // falling back to `default_data_dir()`. Pointing it at our temp
    // cluster avoids touching the operator's installed cluster.
    spec.env.push((
        "KASTELLAN_DATA_DIR".to_string(),
        cluster.data_dir.to_string_lossy().into_owned(),
    ));
    // `$USER` is what `ConnectSpec::default_for` reads to assemble the
    // peer-auth identity. The unit/agent file only carries env vars the
    // spec lists explicitly, so forward the test process's `$USER`.
    spec.env.push(("USER".to_string(), current_username()));

    // Per-test state dir for the audit-mirror's JSONL output.
    let state_dir = unique_temp_root(&format!("core-state-{suffix}"));
    let _state_guard = PathGuard {
        path: state_dir.clone(),
    };
    spec.env.push((
        "KASTELLAN_STATE_DIR".to_string(),
        state_dir.to_string_lossy().into_owned(),
    ));

    // The daemon's prompt loader resolves its directory from
    // `KASTELLAN_PROMPTS_DIR` and falls back to a cwd-relative `prompts/`.
    // systemd's working directory is not the workspace root, so without
    // this override the daemon exits before the audit-mirror would have
    // written its bring-up row.
    let workspace_prompts = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("prompts");
    spec.env.push((
        "KASTELLAN_PROMPTS_DIR".to_string(),
        workspace_prompts.to_string_lossy().into_owned(),
    ));

    let sup_core = default_supervisor();
    let _core_service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };

    // ---------- step 3: install + start core ----------
    sup_core.install(&spec).expect("install kastellan core service");
    assert_eq!(
        sup_core.status(&spec.name).expect("status pre-start"),
        ServiceStatus::Inactive,
    );
    sup_core.start(&spec.name).expect("start kastellan core");

    wait_for_status(
        sup_core.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(10),
    )
    .expect("core should reach Active within 10s");

    // The daemon does an async DB probe (connect + ensure DB +
    // migrate + insert audit row) before announcing readiness. On a
    // healthy host this is sub-second; the 500 ms hold + re-check
    // is long enough to catch a probe failure that exits non-zero
    // and triggers `Restart=on-failure`.
    std::thread::sleep(Duration::from_millis(500));
    let status_check = sup_core
        .status(&spec.name)
        .expect("core stable-active recheck");
    if status_check != ServiceStatus::Active {
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        panic!(
            "core daemon should still be Active 500ms after start (no flapping); \
             observed {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status_check, stdout, stderr,
        );
    }

    // ---------- step 4: sanity-check log lines ----------
    // Pin the readiness signal to the constant exported by kastellan-core so
    // a future rename fails to compile rather than silently timing out.
    wait_for_log_match(
        &stdout_path,
        |s| s.contains(STARTUP_READY_MSG),
        Duration::from_secs(10),
    )
    .unwrap_or_else(|_| panic!("daemon should log {STARTUP_READY_MSG:?} within 10s"));

    // ---------- step 5: read the audit_log row ----------
    let psql = bin_dir.join("psql");
    let user = current_username();
    let select_out = Command::new(&psql)
        .arg("-h")
        .arg(&cluster.socket_dir)
        .arg("-U")
        .arg(&user)
        .arg("-d")
        .arg("kastellan")
        .arg("-At")
        .arg("-c")
        .arg("SELECT count(*) FROM audit_log WHERE actor = 'core' AND action = 'startup'")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("PGPASSFILE", "/dev/null")
        .env("PGSERVICEFILE", "/dev/null")
        .env("PGSYSCONFDIR", "/dev/null")
        .output()
        .expect("spawn psql for audit_log read");
    assert!(
        select_out.status.success(),
        "psql audit_log read failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&select_out.stdout),
        String::from_utf8_lossy(&select_out.stderr),
    );
    let count_str = String::from_utf8_lossy(&select_out.stdout);
    let count: u64 = count_str
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("audit_log count parse: {e}; raw: {count_str}"));
    assert!(
        count >= 1,
        "audit_log should have at least one core/startup row, got {count}",
    );

    // ---------- step 5b: assert audit-mirror picked up the row ----------
    let mirror_body = wait_for_state_dir_match(
        &state_dir,
        |body| body.contains("\"actor\":\"core\"") && body.contains("\"action\":\"startup\""),
        Duration::from_secs(5),
    )
    .expect("audit_mirror JSONL should contain the bring-up row within 5 s");
    // Sanity: every line in the JSONL file must be valid JSON.
    for (i, line) in mirror_body.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("JSONL mirror line {i} not valid JSON: {e}\n{line}"));
    }

    // ---------- step 6: stop + uninstall core ----------
    sup_core.stop(&spec.name).expect("stop core");
    wait_for_status(
        sup_core.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Inactive,
        Duration::from_secs(10),
    )
    .expect("core should reach Inactive within 10s of stop");
    sup_core.uninstall(&spec.name).expect("uninstall core");
    assert_eq!(
        sup_core.status(&spec.name).expect("status post-uninstall"),
        ServiceStatus::NotInstalled,
    );

    // PgCluster + PathGuard + ServiceGuard drops clean up everything else.
}
