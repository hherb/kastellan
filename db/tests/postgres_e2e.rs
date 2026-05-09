//! End-to-end smoke for the per-user Postgres bring-up.
//!
//! This test exercises the full happy path from raw temp dir to a live
//! UDS-only Postgres that answers `SELECT 1`:
//!
//!   1. Locate Postgres binaries via [`hhagent_db::find_pg_bin_dir`]
//!      against the canonical PGDG / Homebrew candidates. Skip if none
//!      found.
//!   2. Skip if the user-level supervisor probe fails (headless Linux
//!      without `loginctl enable-linger`, SSH-only macOS).
//!   3. `initdb` a temp data dir using the helpers from `lib.rs`
//!      (writes `postgresql.auto.conf` with `listen_addresses=''`,
//!      socket dir inside the data dir, peer auth).
//!   4. Build the [`hhagent_supervisor::specs::postgres_service_spec`]
//!      spec and rename it `hhagent-postgres-test-{pid}-{nanos}` so
//!      concurrent runs don't collide and a real `hhagent-postgres`
//!      installed on the host is never clobbered.
//!   5. `install` → `start` → poll `status()` until Active → hold
//!      500 ms and re-check (rules out flapping under
//!      `Restart=on-failure`).
//!   6. Connect via `psql -h <socket_dir> -U <whoami>` over the UDS,
//!      run `SELECT 1` and assert the result. This is what proves the
//!      whole stack agrees: data dir, config overrides, peer auth,
//!      socket dir permissions, supervisor lifecycle.
//!   7. `stop` → poll `status()` until Inactive → `uninstall` → assert
//!      `NotInstalled`.
//!
//! RAII guards drop the test service, the temp data dir, and the per-test
//! log dir even if any assertion above panics, so a failed run cannot
//! leave a stale unit file or 200 MB of `pg_wal` behind.
//!
//! Skips silently with `[SKIP]` lines on hosts that can't run the test;
//! `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_supervisor::specs::postgres_service_spec;
use hhagent_supervisor::{
    default_probe, default_supervisor, ServiceStatus, Supervisor,
};

/// Skip if the supervisor can't reach its underlying service manager.
fn skip_if_no_supervisor() -> bool {
    match default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

/// Skip if no Postgres bin dir found on this host. Returns the dir on
/// success so the caller can construct paths to `postgres`, `initdb`,
/// `psql`.
fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match find_pg_bin_dir(&default_pg_bin_dir_candidates()) {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("\n[SKIP] no Postgres install found: {e}\n");
            None
        }
    }
}

/// Per-test unique name. The `hhagent-supervisor-test-` prefix matches
/// the supervisor smoke tests so a single
/// `find ~/.config/systemd/user/ -name 'hhagent-supervisor-test-*'`
/// cleans up post-crash residue from any of them.
fn unique_test_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("hhagent-supervisor-test-pg-{}-{}", std::process::id(), nanos)
}

/// Per-test temp data dir. Lives under `std::env::temp_dir()` so the
/// host's actual `~/.local/share/hhagent/pg/data` is never touched.
fn unique_temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hhagent-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ))
}

struct ServiceGuard {
    sup: Box<dyn Supervisor>,
    name: String,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        // Best-effort: if the test panicked between start and stop,
        // make sure the supervisor knows it shouldn't be running, then
        // remove the unit file. Both `stop` and `uninstall` are
        // documented as idempotent.
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
                "timed out after {:?} waiting for status; last={:?}",
                timeout, last
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
        last = sup
            .status(name)
            .map_err(|e| format!("status error: {e}"))?;
    }
}

/// Wait for the postgres listening socket to appear. Postgres creates
/// the file `<socket_dir>/.s.PGSQL.5432` only after it's ready to
/// accept connections, so this is the canonical "ready" signal — more
/// reliable than `psql` retry loops because it doesn't require a
/// successful TCP/UDS connect to detect "not ready yet".
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

/// Get the OS username so we can use it as the Postgres superuser via
/// `--username` (peer auth requires the OS uid match the PG role name).
fn current_username() -> String {
    // `whoami` is available on every supported platform; falls back to
    // a placeholder only if both `whoami` and `$USER` fail (which
    // would mean a deeply broken host where the test wouldn't work
    // anyway).
    if let Some(u) = std::env::var_os("USER") {
        return u.to_string_lossy().into_owned();
    }
    let out = Command::new("whoami").output();
    if let Ok(o) = out {
        if o.status.success() {
            return String::from_utf8_lossy(&o.stdout).trim().to_string();
        }
    }
    "hhagent".into()
}

#[test]
fn postgres_install_start_select_one_uninstall() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");
    let psql = bin_dir.join("psql");
    assert!(postgres.exists(), "postgres should exist at {}", postgres.display());
    assert!(initdb.exists(), "initdb should exist at {}", initdb.display());
    assert!(psql.exists(), "psql should exist at {}", psql.display());

    // ---------- temp dirs (data + logs) ----------
    let data_root = unique_temp_root("pg-e2e-data");
    let _data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);

    let log_dir = unique_temp_root("pg-e2e-logs");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let _log_guard = PathGuard { path: log_dir.clone() };

    // ---------- initdb ----------
    let user = current_username();
    let init_opts = InitDbOptions {
        data_dir: data_dir.clone(),
        username: user.clone(),
        ..InitDbOptions::default()
    };
    let argv = build_initdb_argv(&initdb, &init_opts);
    // initdb requires the data_dir parent to exist (it creates data_dir
    // itself) — `unique_temp_root` already created `data_root`.
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .output()
        .expect("spawn initdb");
    assert!(
        status.status.success(),
        "initdb failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr),
    );

    // Socket dir must exist with mode 0700 *before* postgres starts, or
    // it will refuse to create the socket file there.
    std::fs::create_dir(&socket_dir).expect("create socket dir");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&socket_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&socket_dir, perms).unwrap();
    }

    // postgresql.auto.conf overrides postgresql.conf at runtime; this
    // is what pins listen_addresses='' and unix_socket_directories.
    let conf = build_postgresql_auto_conf(&PgConfigOptions {
        socket_dir: socket_dir.clone(),
        ..PgConfigOptions::default()
    });
    std::fs::write(data_dir.join("postgresql.auto.conf"), conf)
        .expect("write postgresql.auto.conf");

    // ---------- supervisor spec ----------
    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = unique_test_name();
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let _service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };

    // ---------- install / start ----------
    sup.install(&spec).expect("install postgres service");
    assert_eq!(
        sup.status(&spec.name).expect("status pre-start"),
        ServiceStatus::Inactive,
    );

    sup.start(&spec.name).expect("start postgres service");

    // Postgres takes a few hundred ms to come up on a healthy host;
    // 15 s timeout accommodates a loaded CI box without masking a
    // real hang.
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("postgres should reach Active within 15s");

    // Active != accepting connections. Wait for the listening socket
    // to appear before psql — otherwise the first SELECT 1 races
    // postmaster startup and produces a flaky failure with a
    // non-obvious "could not connect" error.
    wait_for_socket(&socket_dir, Duration::from_secs(15))
        .expect("postgres socket should appear within 15s of Active");

    // Hold 500 ms and re-check; if we're flapping under
    // `Restart=on-failure` (e.g. config error), this catches it
    // before the SELECT 1 instead of after with a confusing connect
    // error.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).expect("stable-active recheck"),
        ServiceStatus::Active,
        "postgres should still be Active 500ms after start (no flapping); \
         check {}.err for the postmaster log",
        spec.name,
    );

    // ---------- SELECT 1 over UDS ----------
    let select_out = Command::new(&psql)
        .arg("-h")
        .arg(&socket_dir)
        .arg("-U")
        .arg(&user)
        .arg("-d")
        .arg("postgres")
        .arg("-At") // -A unaligned, -t tuples-only — output is just the value
        .arg("-c")
        .arg("SELECT 1")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        // Make sure no host PG config bleeds into the test.
        .env("PGPASSFILE", "/dev/null")
        .env("PGSERVICEFILE", "/dev/null")
        .env("PGSYSCONFDIR", "/dev/null")
        .output()
        .expect("spawn psql");
    assert!(
        select_out.status.success(),
        "psql SELECT 1 failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&select_out.stdout),
        String::from_utf8_lossy(&select_out.stderr),
    );
    let out = String::from_utf8_lossy(&select_out.stdout);
    assert_eq!(
        out.trim(),
        "1",
        "psql -At -c 'SELECT 1' should print exactly '1', got: {out}",
    );

    // ---------- stop / uninstall ----------
    sup.stop(&spec.name).expect("stop postgres service");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Inactive,
        Duration::from_secs(15),
    )
    .expect("postgres should reach Inactive within 15s of stop");

    sup.uninstall(&spec.name).expect("uninstall postgres service");
    assert_eq!(
        sup.status(&spec.name).expect("status post-uninstall"),
        ServiceStatus::NotInstalled,
    );

    // PathGuard drops handle the temp dirs.
}
