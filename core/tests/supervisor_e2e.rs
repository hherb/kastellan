//! End-to-end smoke for the cross-platform supervisor wiring +
//! database probe.
//!
//! The daemon's hard dependency on a live Postgres (added in C2.2 —
//! see `core/src/main.rs::bring_up_database`) means this test brings
//! up *two* services in sequence and verifies the daemon's bring-up
//! contract end-to-end:
//!
//!   1. `initdb` a per-test temp cluster (peer-auth, UDS only).
//!   2. Install + start `hhagent-postgres` via `default_supervisor()`.
//!      Wait for Active and the listening socket.
//!   3. Build the `core_service_spec` for the freshly-built `hhagent`
//!      binary, override `HHAGENT_DATA_DIR` to point at the temp
//!      cluster, install + start the service, wait for Active, hold
//!      500 ms and re-check (no flapping under `Restart=on-failure`).
//!   4. Sanity-check the daemon's stdout log for the startup JSON
//!      line and the "database probe succeeded" follow-up.
//!   5. Connect via `psql` and assert the bring-up `audit_log` row
//!      (actor=`core`, action=`startup`) is present — proves the
//!      probe ran end-to-end through migrations.
//!   6. Stop hhagent → wait Inactive → uninstall.
//!   7. Stop postgres → wait Inactive → uninstall.
//!   8. RAII guards wipe data dir, log dir, and any leftover service
//!      registrations on panic.
//!
//! Skips silently on:
//!   - Hosts where the supervisor probe fails (headless Linux without
//!     `loginctl enable-linger`, SSH-only macOS).
//!   - Hosts where no Postgres binaries are found in the canonical
//!     PGDG / Homebrew locations (`hhagent_db::find_pg_bin_dir`).
//!   - Hosts where the freshly-built `hhagent` binary is missing from
//!     `target/debug/`.
//! Skipped runs print `[SKIP]` to stderr; `cargo test -- --nocapture`
//! to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_supervisor::specs::{core_service_spec, postgres_service_spec};
use hhagent_supervisor::{
    default_probe, default_supervisor, ServiceStatus, Supervisor,
};

/// On macOS, `~/Library/LaunchAgents/` and the GUI launchd domain are
/// shared global resources. The supervisor crate's launchd smoke test
/// uses an intra-binary static mutex; we mirror that here so tests
/// don't race when run together via `cargo test --workspace`. Linux
/// uses unique-per-test names instead, so the lock is only held on
/// macOS.
#[cfg(target_os = "macos")]
fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn skip_if_no_supervisor() -> bool {
    match default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
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

fn core_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("hhagent")
}

/// Process+timestamp-unique label, used as both the service-name
/// suffix and (with a different prefix) the temp-dir suffix. Same
/// shape as `db/tests/postgres_e2e.rs::unique_test_name`.
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

/// Resolve the OS user. Peer auth requires the connecting OS uid to
/// match the Postgres role, so we pass this both to `initdb
/// --username` (cluster superuser) and to `psql -U` (audit-row read).
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

struct PathGuard {
    path: PathBuf,
}
impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn wait_for_status<F: Fn(ServiceStatus) -> bool>(
    sup: &dyn Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<ServiceStatus, String> {
    let start = Instant::now();
    let mut last = sup
        .status(name)
        .map_err(|e| format!("status error: {e}"))?;
    loop {
        if predicate(last) {
            return Ok(last);
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {:?}; last={:?}",
                timeout, last
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
        last = sup
            .status(name)
            .map_err(|e| format!("status error: {e}"))?;
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
            return Err(format!(
                "timed out after {:?} waiting for {} to appear",
                timeout,
                target.display()
            ));
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

/// Bring up a Postgres cluster under the test supervisor and return
/// `(data_dir, socket_dir, supervisor, service_name, guards)`.
///
/// Once `guards` go out of scope, the cluster is stopped, the unit
/// is uninstalled, and the data + log dirs are wiped — even on panic.
/// The caller binds the entire tuple so all four guards live for the
/// scope of the test body.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    PathBuf,
    PathBuf,
    Box<dyn Supervisor>,
    String,
    (ServiceGuard, PathGuard, PathGuard),
) {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");
    assert!(postgres.exists(), "postgres at {}", postgres.display());
    assert!(initdb.exists(), "initdb at {}", initdb.display());

    let data_root = unique_temp_root(&format!("e2e-pg-data-{suffix}"));
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);

    let log_dir = unique_temp_root(&format!("e2e-pg-logs-{suffix}"));
    std::fs::create_dir_all(&log_dir).expect("create pg log dir");
    let log_guard = PathGuard { path: log_dir.clone() };

    // ---------- initdb ----------
    let user = current_username();
    let init_opts = InitDbOptions {
        data_dir: data_dir.clone(),
        username: user.clone(),
        ..InitDbOptions::default()
    };
    let argv = build_initdb_argv(&initdb, &init_opts);
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .output()
        .expect("spawn initdb");
    assert!(
        out.status.success(),
        "initdb failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Socket dir must exist mode 0700 before postgres starts.
    std::fs::create_dir(&socket_dir).expect("create socket dir");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&socket_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&socket_dir, perms).unwrap();
    }
    let conf = build_postgresql_auto_conf(&PgConfigOptions {
        socket_dir: socket_dir.clone(),
        ..PgConfigOptions::default()
    });
    std::fs::write(data_dir.join("postgresql.auto.conf"), conf)
        .expect("write postgresql.auto.conf");

    // ---------- supervise ----------
    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    let pg_name = format!("hhagent-supervisor-test-pg-{suffix}");
    spec.name = pg_name.clone();
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install postgres service");
    sup.start(&spec.name).expect("start postgres service");

    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("postgres should reach Active within 15s");
    wait_for_socket(&socket_dir, Duration::from_secs(15))
        .expect("postgres socket should appear within 15s");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).expect("pg stable-active recheck"),
        ServiceStatus::Active,
        "postgres should still be Active 500ms after start"
    );

    (data_dir, socket_dir, sup, pg_name, (service_guard, data_guard, log_guard))
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
            "\n[SKIP] hhagent binary not found at {}; run `cargo build --workspace` first\n",
            binary.display()
        );
        return;
    }

    let suffix = unique_suffix();

    // ---------- step 1: bring up the per-test PG cluster ----------
    let (data_dir, socket_dir, _sup_pg, _pg_name, _pg_guards) =
        bring_up_pg_cluster(&bin_dir, &suffix);

    // ---------- step 2: build the core service spec ----------
    let core_log_dir = unique_temp_root(&format!("e2e-core-logs-{suffix}"));
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let _core_log_guard = PathGuard { path: core_log_dir.clone() };

    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("hhagent-supervisor-test-core-{suffix}");
    assert!(spec.name.len() <= 200);
    let stdout_path = core_log_dir.join(format!("{}.out", spec.name));
    let stderr_path = core_log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path.clone());

    // The daemon resolves its data dir from `HHAGENT_DATA_DIR` before
    // falling back to `default_data_dir()` (see
    // `core/src/main.rs::bring_up_database`). Pointing it at our
    // temp cluster avoids touching the operator's installed cluster
    // and lets concurrent tests coexist.
    spec.env.push((
        "HHAGENT_DATA_DIR".to_string(),
        data_dir.to_string_lossy().into_owned(),
    ));
    // `$USER` is what `ConnectSpec::default_for` reads to assemble
    // the peer-auth identity. systemd's user manager and macOS
    // launchd both inherit it from the operator's login record, but
    // the unit/agent file built by `build_unit_file` /
    // `build_plist` only carries env vars the spec lists explicitly.
    // Forward the test process's `$USER` so the daemon connects as
    // the same role that ran `initdb --username=$USER` above.
    spec.env.push(("USER".to_string(), current_username()));

    let sup_core = default_supervisor();
    let _core_service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };

    // ---------- step 3: install + start core ----------
    sup_core.install(&spec).expect("install hhagent core service");
    assert_eq!(
        sup_core.status(&spec.name).expect("status pre-start"),
        ServiceStatus::Inactive,
    );
    sup_core.start(&spec.name).expect("start hhagent core");

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
    wait_for_log_match(
        &stdout_path,
        |s| s.contains("database probe succeeded"),
        Duration::from_secs(10),
    )
    .expect("daemon should log 'database probe succeeded' within 10s");

    // ---------- step 5: read the audit_log row ----------
    let psql = bin_dir.join("psql");
    let user = current_username();
    let select_out = Command::new(&psql)
        .arg("-h")
        .arg(&socket_dir)
        .arg("-U")
        .arg(&user)
        .arg("-d")
        .arg("hhagent")
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

    // PG service + data dir + log dirs cleaned up via guards as the
    // tuple bound above goes out of scope.
}
