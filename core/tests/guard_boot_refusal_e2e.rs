//! A guard tier that refuses to boot leaves a `policy /
//! guard_tier.boot_refused` row, thinned across restarts (issue #906).
//!
//! Before #906 every refusal (D6) reached only stderr: the DGX refused 1500+
//! times over five days with nothing in `audit_log`. This boots the real
//! daemon against a real cluster three times:
//!
//! 1. τ = 2 — refused before any backend is dialled, so no guard mock is
//!    needed. One `tau` row.
//! 2. The same again — the crash loop. Still one row: the second refusal is
//!    within `REFUSAL_ROW_INTERVAL` of the first.
//! 3. `KASTELLAN_REQUIRE_GUARD=1` with no guard keys — a *different* refusal
//!    is written at once. Two rows.
//!
//! The daemon is spawned directly (not under the supervisor, which would
//! restart it): the binary is cargo's own build of this package's
//! `kastellan`, so it is never stale. Skip-as-pass without a supervisor/PG;
//! demanded by the `pg` profile of `scripts/run-e2e-gate.sh`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kastellan_tests_common::{
    bring_up_pg_cluster, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    unique_suffix, unique_temp_root, PathGuard,
};
use serde_json::{json, Value};

/// A refusal comes after migrations and the startup sweeps; generous for a
/// loaded full sweep, and a hang past it is itself the failure.
const EXIT_BUDGET: Duration = Duration::from_secs(90);

/// Run the daemon once with `env` on top of the cluster's, and require that
/// it refuses: a non-zero exit naming the guard tier.
fn run_refusing_daemon(data_dir: &Path, scratch: &Path, run: &str, env: &[(&str, &str)]) {
    let stdout_path = scratch.join(format!("{run}.out"));
    let stderr_path = scratch.join(format!("{run}.err"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kastellan"));
    cmd.env_clear()
        // A scratch HOME, so the operator's own config overlay is not read.
        .env("HOME", scratch)
        .env("USER", current_username())
        .env("PATH", "/usr/bin:/bin")
        .env("KASTELLAN_DATA_DIR", data_dir)
        .env("KASTELLAN_STATE_DIR", scratch.join("state"))
        .stdout(Stdio::from(std::fs::File::create(&stdout_path).expect("stdout file")))
        .stderr(Stdio::from(std::fs::File::create(&stderr_path).expect("stderr file")));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn kastellan");

    let deadline = Instant::now() + EXIT_BUDGET;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll kastellan") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let logs = || {
        format!(
            "\n--- stdout ---\n{}\n--- stderr ---\n{}",
            std::fs::read_to_string(&stdout_path).unwrap_or_default(),
            std::fs::read_to_string(&stderr_path).unwrap_or_default()
        )
    };
    let status = status.unwrap_or_else(|| panic!("{run}: no exit within {EXIT_BUDGET:?}{}", logs()));
    assert!(!status.success(), "{run}: the guard tier must refuse the boot{}", logs());
    assert!(
        std::fs::read_to_string(&stderr_path).unwrap_or_default().contains("guard tier:"),
        "{run}: the exit must be the guard tier's refusal, not an earlier failure{}",
        logs()
    );
}

async fn refusal_rows(pool: &sqlx::PgPool) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_log \
         WHERE actor = 'policy' AND action = 'guard_tier.boot_refused' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("select guard_tier.boot_refused rows")
}

#[test]
fn a_refused_boot_is_a_row_thinned_per_refusal_kind() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "gref-d",
        "gref-l",
        &format!("kastellan-supervisor-test-pg-gref-{suffix}"),
    );
    let scratch = unique_temp_root("gref");
    std::fs::create_dir_all(&scratch).expect("create scratch dir");
    let _scratch_guard = PathGuard { path: scratch.clone() };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Nothing listens on port 9; τ is refused before the guard is dialled.
    let bad_tau = [
        ("KASTELLAN_LLM_GUARD_URL", "http://127.0.0.1:9/v1"),
        ("KASTELLAN_LLM_GUARD_MODEL", "test-guard-model"),
        ("KASTELLAN_LLM_GUARD_TAU", "2"),
    ];
    let tau_row = json!({"refusal": "tau", "error_kind": null});

    run_refusing_daemon(&cluster.data_dir, &scratch, "tau-1", &bad_tau);
    let pool = rt.block_on(kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec))
        .expect("runtime pool");
    assert_eq!(rt.block_on(refusal_rows(&pool)), vec![tau_row.clone()]);

    // The crash loop's next restart: the same refusal writes nothing.
    run_refusing_daemon(&cluster.data_dir, &scratch, "tau-2", &bad_tau);
    assert_eq!(
        rt.block_on(refusal_rows(&pool)),
        vec![tau_row.clone()],
        "a repeat of the same refusal within REFUSAL_ROW_INTERVAL must be thinned"
    );

    // A different refusal is its own row, written at once.
    run_refusing_daemon(&cluster.data_dir, &scratch, "required", &[("KASTELLAN_REQUIRE_GUARD", "1")]);
    assert_eq!(
        rt.block_on(refusal_rows(&pool)),
        vec![tau_row, json!({"refusal": "required", "error_kind": null})]
    );
}
