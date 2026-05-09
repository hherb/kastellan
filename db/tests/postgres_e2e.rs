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

/// End-to-end smoke for the runtime probe and the `Graph` trait.
///
/// Pipeline (mirrors what `core/src/main.rs::bring_up_database` does
/// every time the daemon starts, plus a Graph round-trip):
///
///   1. Bring up a per-test PG cluster (same shape as
///      `postgres_install_start_select_one_uninstall` above — kept
///      separate so a regression in the runtime probe never masks a
///      regression in the supervisor lifecycle, and vice versa).
///   2. Run `db::probe::run` once. This exercises:
///        * The maintenance-DB connect.
///        * `CREATE DATABASE hhagent` (first-boot branch).
///        * Reconnect to `hhagent`.
///        * `MIGRATOR.run` — pulls in `0001_init.sql`.
///        * The `audit_log` insert.
///   3. Run `db::probe::run` a *second* time. The CREATE DATABASE
///      branch must short-circuit (the lookup already finds the row),
///      and migrations must be a no-op (sqlx's `_sqlx_migrations`
///      checksum check). A fresh `audit_log` row appears, proving
///      idempotency without rewriting state.
///   4. Connect with sqlx and exercise `PgGraph`:
///        * `upsert_entity` two nodes (kind=`person`).
///        * Re-`upsert_entity` the first node — id stays stable
///          (ON CONFLICT (kind,name) DO UPDATE).
///        * `upsert_relation` one edge.
///        * `get_entity` round-trip.
///        * `neighbors` returns the second node.
///        * `path` finds the 1-hop path.
///   5. Sanity-check the `audit_log` row count is exactly 2 (the
///      two probe runs above), so no spurious writes are happening
///      in either probe path or the graph round-trip.
///   6. Tear down: stop / uninstall the PG service, RAII guards wipe
///      data + log dirs.
///
/// Skips silently with `[SKIP]` lines on the same hosts as the
/// supervisor-lifecycle test above (no PG, no supervisor probe).
#[test]
fn probe_runs_migrations_and_graph_happy_path() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // ---------- temp dirs ----------
    let data_root = unique_temp_root("probe-data");
    let _data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("probe-logs");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let _log_guard = PathGuard { path: log_dir.clone() };

    // ---------- initdb ----------
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
        "initdb failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
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

    // ---------- supervisor spec ----------
    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = format!(
        "hhagent-supervisor-test-pg-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let _service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install postgres");
    sup.start(&spec.name).expect("start postgres");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("postgres reaches Active");
    wait_for_socket(&socket_dir, Duration::from_secs(15))
        .expect("postgres socket appears");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).unwrap(),
        ServiceStatus::Active,
        "postgres flapping during stable-active window"
    );

    // ---------- run the probe twice (idempotency) ----------
    // New binding (`conn`) so we don't shadow the supervisor `spec`
    // we still need for stop/uninstall below.
    let conn = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        // First run — exercises the CREATE DATABASE + migrations branches.
        hhagent_db::probe::run(
            &conn,
            "core",
            "startup",
            serde_json::json!({"version": "test", "run": 1}),
        )
        .await
        .expect("first probe run");

        // Second run — must be a no-op except for the audit row.
        hhagent_db::probe::run(
            &conn,
            "core",
            "startup",
            serde_json::json!({"version": "test", "run": 2}),
        )
        .await
        .expect("second probe run (idempotency)");

        // ---------- Graph trait round-trip ----------
        use hhagent_db::graph::{Graph, PgGraph};
        let pool = sqlx::postgres::PgPool::connect_with(conn.to_pg_connect_options())
            .await
            .expect("pool connect");
        let g = PgGraph::new(&pool);

        // Upsert two entities.
        let alice = g
            .upsert_entity("person", "alice", &serde_json::json!({"role": "engineer"}))
            .await
            .expect("upsert alice");
        let bob = g
            .upsert_entity("person", "bob", &serde_json::json!({}))
            .await
            .expect("upsert bob");
        assert!(alice > 0 && bob > 0 && alice != bob);

        // Re-upsert alice — id must stay stable (ON CONFLICT key is
        // (kind,name); a regression that flipped to INSERT-only or
        // changed the key would change the id).
        let alice_again = g
            .upsert_entity("person", "alice", &serde_json::json!({"role": "tlm"}))
            .await
            .expect("upsert alice again");
        assert_eq!(alice, alice_again);

        // Edge alice --knows--> bob.
        let edge_id = g
            .upsert_relation(alice, bob, "knows", &serde_json::json!({}))
            .await
            .expect("upsert relation");
        assert!(edge_id > 0);

        // get_entity round-trip — attrs must reflect the second upsert.
        let fetched = g.get_entity("person", "alice").await.expect("get alice");
        let fetched = fetched.expect("alice should exist");
        assert_eq!(fetched.id, alice);
        assert_eq!(fetched.kind, "person");
        assert_eq!(fetched.name, "alice");
        assert_eq!(fetched.attrs["role"], "tlm");

        // neighbors(alice, knows) returns bob.
        let neighbors = g
            .neighbors(alice, Some("knows"), 100)
            .await
            .expect("neighbors");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].id, bob);

        // neighbors(alice, None) — same result with the unfiltered
        // query path (different SQL, same answer).
        let neighbors_unfiltered = g
            .neighbors(alice, None, 100)
            .await
            .expect("neighbors unfiltered");
        assert_eq!(neighbors_unfiltered.len(), 1);
        assert_eq!(neighbors_unfiltered[0].id, bob);

        // path(alice, bob, 5) returns [alice, bob].
        let path = g.path(alice, bob, 5).await.expect("path");
        let path = path.expect("path should exist");
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].id, alice);
        assert_eq!(path[1].id, bob);

        // path(bob, alice) returns None — relations are directed,
        // and we wrote only alice->bob, not the reverse.
        let no_path = g.path(bob, alice, 5).await.expect("path bob->alice");
        assert!(no_path.is_none(), "path should not exist in reverse direction");

        // ---------- audit_log row count ----------
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        assert_eq!(row.0, 2, "expected exactly 2 audit_log rows (one per probe run)");

        pool.close().await;
    });

    // ---------- teardown ----------
    sup.stop(&spec.name).expect("stop postgres");
    let _ = sup.uninstall(&spec.name);
    // Guards wipe data + log dirs on drop.
    let _ = (data_root, log_dir, socket_dir);
}

/// Verify the runtime-role split from migration `0002_runtime_role.sql`.
///
/// The migration creates `hhagent_runtime` (NOSUPERUSER, NOCREATEROLE,
/// NOCREATEDB, NOLOGIN, NOINHERIT), grants membership to the OS user,
/// grants `SELECT, INSERT` on `audit_log`, and explicitly REVOKEs
/// `UPDATE, DELETE, TRUNCATE` from it. After `db::probe::run` applies
/// the migration and switches into the runtime role for its own
/// `audit_log` insert, this test connects on a fresh connection,
/// `SET ROLE`s, and proves the contract:
///
///   * `audit_log` INSERT succeeds under the runtime role.
///   * `audit_log` UPDATE fails with `permission denied` (SQLSTATE 42501).
///   * `audit_log` DELETE fails with `permission denied`.
///   * `memories` full CRUD succeeds (sanity that the GRANT block's
///     CRUD line is in fact wired).
///   * The role exists with the expected `pg_roles` flags and the OS
///     user is recorded in `pg_auth_members` as a member.
///   * Final `audit_log` row count is exactly 2 (probe row + our test
///     INSERT) — no UPDATE silently rewrote the probe row, no DELETE
///     vanished it.
///
/// Skips silently with `[SKIP]` lines on hosts without Postgres or a
/// reachable supervisor (same as the other tests in this file).
#[test]
fn runtime_role_audit_log_revoke_is_enforced() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // ---------- temp dirs ----------
    let data_root = unique_temp_root("runtime-role-data");
    let _data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("runtime-role-logs");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let _log_guard = PathGuard { path: log_dir.clone() };

    // ---------- initdb + auto.conf ----------
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
        "initdb failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
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

    // ---------- supervisor spec ----------
    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = format!(
        "hhagent-supervisor-test-pg-runtime-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let _service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install postgres");
    sup.start(&spec.name).expect("start postgres");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("postgres reaches Active");
    wait_for_socket(&socket_dir, Duration::from_secs(15))
        .expect("postgres socket appears");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).unwrap(),
        ServiceStatus::Active,
        "postgres flapping during stable-active window"
    );

    // ---------- probe + revoke checks ----------
    let conn_spec = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        // Probe applies migrations 0001 + 0002 and writes one audit row
        // already under SET ROLE. The role + grants now exist.
        hhagent_db::probe::run(
            &conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "runtime-role-revoke"}),
        )
        .await
        .expect("probe run");

        // Pool connects as the OS user (= cluster superuser). We then
        // SET ROLE on a single acquired connection so all subsequent
        // statements run as the runtime role for that connection only.
        let pool = sqlx::postgres::PgPool::connect_with(conn_spec.to_pg_connect_options())
            .await
            .expect("pool connect");

        // ---------- role shape pin ----------
        // The four boolean flags here pin the contract from
        // `0002_runtime_role.sql`'s CREATE ROLE statement. A regression
        // that flipped any of these (e.g. accidentally adding LOGIN)
        // would silently weaken the boundary; the test is louder than
        // a code-review catch.
        let row: (String, bool, bool, bool, bool, bool) = sqlx::query_as(
            "SELECT rolname, rolcanlogin, rolsuper, rolinherit, rolcreaterole, rolcreatedb \
             FROM pg_roles WHERE rolname = 'hhagent_runtime'",
        )
        .fetch_one(&pool)
        .await
        .expect("hhagent_runtime row in pg_roles");
        assert_eq!(row.0, "hhagent_runtime");
        assert!(!row.1, "hhagent_runtime must be NOLOGIN (rolcanlogin=false)");
        assert!(!row.2, "hhagent_runtime must be NOSUPERUSER (rolsuper=false)");
        assert!(!row.3, "hhagent_runtime must be NOINHERIT (rolinherit=false)");
        assert!(!row.4, "hhagent_runtime must be NOCREATEROLE (rolcreaterole=false)");
        assert!(!row.5, "hhagent_runtime must be NOCREATEDB (rolcreatedb=false)");

        // The OS user (cluster superuser) MUST be a member of the
        // runtime role — otherwise SET ROLE fails for the daemon. The
        // join walks the role-membership graph: r1 = role being granted
        // (hhagent_runtime), r2 = role receiving the grant (= current_user
        // in our setup, which is the OS user under peer auth).
        let (member_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM pg_auth_members am \
             JOIN pg_roles r1 ON am.roleid = r1.oid \
             JOIN pg_roles r2 ON am.member = r2.oid \
             WHERE r1.rolname = 'hhagent_runtime' \
               AND r2.rolname = current_user",
        )
        .fetch_one(&pool)
        .await
        .expect("pg_auth_members lookup");
        assert_eq!(
            member_count, 1,
            "OS user must be a member of hhagent_runtime so SET ROLE works"
        );

        // ---------- SET ROLE on a held connection ----------
        // Pool acquire returns a connection from the pool (or opens a
        // fresh one). SET ROLE is a session setting, so it persists for
        // the lifetime of *this* connection only. Holding the
        // connection out across all the subsequent queries ensures every
        // statement runs under hhagent_runtime.
        let mut held = pool.acquire().await.expect("acquire connection");
        sqlx::query(&hhagent_db::conn::set_role_runtime_statement())
            .execute(&mut *held)
            .await
            .expect("SET ROLE hhagent_runtime");

        // ---------- positive path: INSERT into audit_log ----------
        let inserted: (i64,) = sqlx::query_as(
            "INSERT INTO audit_log (actor, action, payload) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind("test")
        .bind("revoke-check")
        .bind(serde_json::json!({"phase": "positive"}))
        .fetch_one(&mut *held)
        .await
        .expect("INSERT audit_log under runtime role");
        let row_id = inserted.0;

        // ---------- negative path 1: UPDATE rejected ----------
        // Postgres rejects with SQLSTATE 42501 ("permission denied for
        // table audit_log"). Matching on the substring "permission
        // denied" is portable across PG major versions and across the
        // sqlx error wrapper (which formats the underlying message).
        let upd_err = sqlx::query("UPDATE audit_log SET payload = $1 WHERE id = $2")
            .bind(serde_json::json!({"tampered": true}))
            .bind(row_id)
            .execute(&mut *held)
            .await
            .expect_err("UPDATE audit_log must be rejected under runtime role");
        let upd_msg = upd_err.to_string();
        assert!(
            upd_msg.contains("permission denied"),
            "expected 'permission denied' in error, got: {upd_msg}"
        );

        // ---------- negative path 2: DELETE rejected ----------
        let del_err = sqlx::query("DELETE FROM audit_log WHERE id = $1")
            .bind(row_id)
            .execute(&mut *held)
            .await
            .expect_err("DELETE audit_log must be rejected under runtime role");
        let del_msg = del_err.to_string();
        assert!(
            del_msg.contains("permission denied"),
            "expected 'permission denied' in error, got: {del_msg}"
        );

        // ---------- positive path: full CRUD on memories ----------
        // Sanity-pin that the bulk GRANT in 0002 actually wires
        // SELECT/INSERT/UPDATE/DELETE for the application tables; a
        // typo there (e.g. accidental `INSERT, UPDATE` only) would not
        // be caught by the audit_log assertions above. `body` is the
        // only NOT NULL column without a default; the rest are
        // generated/defaulted, so this minimal INSERT exercises the
        // sequence USAGE grant on memories_id_seq too.
        let mem: (i64,) = sqlx::query_as(
            "INSERT INTO memories (body) VALUES ($1) RETURNING id",
        )
        .bind("hello")
        .fetch_one(&mut *held)
        .await
        .expect("INSERT memories under runtime role");
        let mem_id = mem.0;

        sqlx::query("UPDATE memories SET body = $1 WHERE id = $2")
            .bind("world")
            .bind(mem_id)
            .execute(&mut *held)
            .await
            .expect("UPDATE memories under runtime role");

        let body: (String,) = sqlx::query_as("SELECT body FROM memories WHERE id = $1")
            .bind(mem_id)
            .fetch_one(&mut *held)
            .await
            .expect("SELECT memories under runtime role");
        assert_eq!(body.0, "world");

        sqlx::query("DELETE FROM memories WHERE id = $1")
            .bind(mem_id)
            .execute(&mut *held)
            .await
            .expect("DELETE memories under runtime role");

        // ---------- final audit row count ----------
        // Probe inserted 1 row, our positive INSERT inserted 1 more.
        // UPDATE and DELETE both failed at the auth layer so neither
        // mutated the table. Anything other than 2 means either a
        // bookkeeping bug or — much worse — an UPDATE/DELETE that was
        // *not* rejected.
        drop(held);
        let (audit_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        assert_eq!(
            audit_count, 2,
            "expected exactly 2 audit_log rows (probe row + test INSERT); \
             a different number means UPDATE/DELETE may have leaked through"
        );

        pool.close().await;
    });

    // ---------- teardown ----------
    sup.stop(&spec.name).expect("stop postgres");
    let _ = sup.uninstall(&spec.name);
    let _ = (data_root, log_dir, socket_dir);
}

/// Verify the runtime pool, the `audit::insert` helper, and the
/// `audit_log_inserted` NOTIFY trigger from migration `0003`.
///
/// What this proves end-to-end:
///   * `pool::connect_runtime_pool` opens a pool whose `after_connect`
///     hook runs `SET ROLE hhagent_runtime`. UPDATE/DELETE on
///     `audit_log` via the pool fail with `permission denied` —
///     proof that role drop actually happened (would succeed under
///     superuser).
///   * The 0003 trigger fires AFTER INSERT and emits a NOTIFY on
///     channel `audit_log_inserted` carrying the new row's `id`.
///   * `PgListener` on a separate dedicated connection receives the
///     NOTIFY within ≤ 2 s of the INSERT.
///   * `audit::fetch_by_id` round-trips the inserted row.
///   * `audit::truncate_payload` is wired into `audit::insert`: an
///     8 KiB payload is replaced with the `_truncated` envelope before
///     storage, and `fetch_by_id` returns the envelope (not the
///     original).
///
/// Skips silently with `[SKIP]` lines on hosts without Postgres or a
/// reachable supervisor.
#[test]
fn audit_helpers_pool_and_notify_round_trip() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // ---------- temp dirs ----------
    let data_root = unique_temp_root("audit-pool-data");
    let _data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("audit-pool-logs");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let _log_guard = PathGuard { path: log_dir.clone() };

    // ---------- initdb + auto.conf ----------
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
        "initdb failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
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

    // ---------- supervisor spec ----------
    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = format!(
        "hhagent-supervisor-test-pg-audit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let _service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install postgres");
    sup.start(&spec.name).expect("start postgres");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("postgres reaches Active");
    wait_for_socket(&socket_dir, Duration::from_secs(15))
        .expect("postgres socket appears");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).unwrap(),
        ServiceStatus::Active,
        "postgres flapping during stable-active window"
    );

    // ---------- exercise the new modules ----------
    let conn_spec = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        // Probe runs migrations 0001, 0002, 0003 and writes the
        // bring-up audit row. The bring-up NOTIFY happens before the
        // listener attaches — covered separately by the catch-up
        // `fetch_since` path; we don't require it to surface here.
        hhagent_db::probe::run(
            &conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "audit-helpers"}),
        )
        .await
        .expect("probe run");

        // Pool with after_connect SET ROLE.
        let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
            .await
            .expect("connect runtime pool");

        // Negative-path proof that pool connections run as the runtime
        // role: UPDATE on `audit_log` must fail. Under the bootstrap
        // superuser this would succeed; the failure is what tells us
        // SET ROLE actually ran in `after_connect`.
        let upd_err = sqlx::query(
            "UPDATE audit_log SET payload = $1 \
             WHERE id = (SELECT min(id) FROM audit_log)",
        )
        .bind(serde_json::json!({"tampered": true}))
        .execute(&pool)
        .await
        .expect_err("UPDATE under runtime-role pool must be rejected");
        assert!(
            upd_err.to_string().contains("permission denied"),
            "expected 'permission denied' from runtime-role pool: {upd_err}"
        );

        // ---------- attach listener BEFORE the watched insert ----------
        // PgListener holds its own dedicated connection (LISTEN binds
        // the channel to the physical connection — it cannot use pool
        // connections that get returned to the pool). The listener's
        // connection comes from the same options as the pool but does
        // NOT go through after_connect, so it stays as the OS user.
        // LISTEN works for any role, so this is fine.
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
            .await
            .expect("PgListener connect");
        listener
            .listen("audit_log_inserted")
            .await
            .expect("LISTEN audit_log_inserted");

        // ---------- write a row via audit::insert ----------
        let inserted_id = hhagent_db::audit::insert(
            &pool,
            "tool:test",
            "call",
            serde_json::json!({"req": {"argv": ["echo", "hi"]}, "ms": 7}),
        )
        .await
        .expect("audit::insert under runtime-role pool");
        assert!(inserted_id > 0);

        // ---------- listener receives the NOTIFY ----------
        // The trigger fires synchronously inside the INSERT
        // transaction; NOTIFY queue drain is microseconds on a
        // healthy host. 2 s is robust against a paused container or a
        // busy CI without masking a real bug.
        let notif = tokio::time::timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("NOTIFY must arrive within 2 s of audit_log INSERT")
            .expect("recv() returned a notification, not an error");
        assert_eq!(notif.channel(), "audit_log_inserted");
        let payload_id: i64 = notif
            .payload()
            .parse()
            .expect("NOTIFY payload must be a parseable i64 row id");
        assert_eq!(
            payload_id, inserted_id,
            "NOTIFY payload must equal the inserted row's id"
        );

        // ---------- fetch_by_id round-trip ----------
        let row = hhagent_db::audit::fetch_by_id(&pool, inserted_id)
            .await
            .expect("fetch_by_id");
        assert_eq!(row.id, inserted_id);
        assert_eq!(row.actor, "tool:test");
        assert_eq!(row.action, "call");
        assert_eq!(
            row.payload,
            serde_json::json!({"req": {"argv": ["echo", "hi"]}, "ms": 7})
        );

        // ---------- truncation: an 8 KiB payload is replaced ----------
        // 8 KiB of `x` serialises to 8 KiB + 2 (the surrounding `"`s),
        // comfortably above the 4 KiB threshold. The stored row must
        // carry the `_truncated` envelope, not the original string.
        let big = "x".repeat(8192);
        let truncated_id = hhagent_db::audit::insert(
            &pool,
            "tool:test",
            "call",
            serde_json::Value::String(big),
        )
        .await
        .expect("audit::insert with big payload");
        let truncated = hhagent_db::audit::fetch_by_id(&pool, truncated_id)
            .await
            .expect("fetch_by_id for truncated row");
        let env = truncated
            .payload
            .as_object()
            .expect("truncated payload must be an object");
        assert_eq!(env.get("_truncated"), Some(&serde_json::Value::Bool(true)));
        assert!(env.contains_key("sha256"));
        assert!(env.contains_key("len"));

        pool.close().await;
    });

    // ---------- teardown ----------
    sup.stop(&spec.name).expect("stop postgres");
    let _ = sup.uninstall(&spec.name);
    let _ = (data_root, log_dir, socket_dir);
}

