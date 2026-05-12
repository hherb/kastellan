//! Per-test Postgres cluster bring-up.
//!
//! `bring_up_pg_cluster` is the consolidation of the
//! initdb-then-`auto.conf`-then-install-then-start dance that was
//! byte-duplicated across seven integration tests before issue #15.
//!
//! # The single struct return type
//!
//! Pre-hoist, each test's local `bring_up_pg_cluster` returned a
//! slightly different tuple (`ConnectSpec` only, or `(ConnectSpec,
//! guards)`, or `(data_dir, socket_dir, sup, name, guards)`).
//! Consolidating those into one struct ([`PgCluster`]) means every
//! caller pays for the same fields but only reads what it needs —
//! which is cheap because the unread fields are just pointers and a
//! short string, and the alternative (a builder + multiple return
//! shapes) would re-introduce the per-call-site fork the hoist is
//! meant to eliminate.
//!
//! # The `_guards` field is private
//!
//! [`PgCluster::_guards`] is a `(ServiceGuard, PathGuard, PathGuard)`
//! triple kept private so callers cannot move it out and drop it
//! early. When `PgCluster` itself drops at end-of-scope, the guards
//! drop in tuple-element order: service stops + uninstalls first,
//! then the data + log directories wipe. A panicking test gets the
//! same cleanup path because `Drop` runs during unwind.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_socket_dir, InitDbOptions,
    PgConfigOptions,
};
use hhagent_supervisor::specs::postgres_service_spec;
use hhagent_supervisor::{default_supervisor, ServiceStatus, Supervisor};

use crate::guards::{PathGuard, ServiceGuard};
use crate::temp::{current_username, unique_temp_root};
use crate::wait::{wait_for_socket, wait_for_status};

/// Handle returned by [`bring_up_pg_cluster`]. All fields needed by
/// downstream tests are public; the cleanup guards are kept private
/// so they cannot be dropped early.
///
/// Drop runs in declaration order, so `sup` (which references the
/// running service) is left intact while the field-level destructors
/// run — only the trailing `_guards` triple actually performs cleanup,
/// in tuple order (service stop+uninstall first, then directory wipes).
pub struct PgCluster {
    pub conn_spec: hhagent_db::conn::ConnectSpec,
    pub data_dir: PathBuf,
    pub socket_dir: PathBuf,
    pub sup: Box<dyn Supervisor>,
    pub service_name: String,
    _guards: (ServiceGuard, PathGuard, PathGuard),
}

/// Bring up a per-test Postgres cluster end-to-end:
///
///   1. `initdb` a temp data dir under `std::env::temp_dir()`.
///   2. Create the socket dir with mode 0700.
///   3. Write `postgresql.auto.conf` to lock the cluster to UDS only.
///   4. Install + start the supervisor service.
///   5. Wait for `Active` then for the listening socket.
///   6. 500 ms hold + re-check to rule out a `Restart=on-failure`
///      flap masking a config error.
///
/// # Parameters
///
/// * `bin_dir` — path to a Postgres `bin/` directory (caller usually
///   gets this from [`crate::skip::pg_bin_dir_or_skip`]).
/// * `data_label` — short label appended to the temp data root, e.g.
///   `"disp-d"` or `"pg-data"`. Keep this **short** (≤ 8 chars
///   ideally) because the full socket path
///   `<tmp>/<label>-<pid>-<nanos>/data/sockets/.s.PGSQL.5432` must
///   fit in `sockaddr_un.sun_path` (108 bytes on Linux).
/// * `log_label` — short label for the per-test log dir.
/// * `service_name` — full systemd unit / launchd label, e.g.
///   `"hhagent-supervisor-test-pg-dispatch-<suffix>"`. Asserted ≤
///   200 chars. Caller constructs this with whatever uniqueness suffix
///   it likes (typically via [`crate::temp::unique_suffix`]).
///
/// # Panics
///
/// Panics with a descriptive message on any failure in the bring-up
/// sequence (the test would have failed anyway, and a panic stops
/// the test from racing further on a half-up cluster).
pub fn bring_up_pg_cluster(
    bin_dir: &Path,
    data_label: &str,
    log_label: &str,
    service_name: &str,
) -> PgCluster {
    assert!(
        service_name.len() <= 200,
        "service_name too long ({} bytes)",
        service_name.len()
    );

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");
    assert!(postgres.exists(), "postgres at {}", postgres.display());
    assert!(initdb.exists(), "initdb at {}", initdb.display());

    let data_root = unique_temp_root(data_label);
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);

    let log_dir = unique_temp_root(log_label);
    std::fs::create_dir_all(&log_dir).expect("create pg log dir");
    let log_guard = PathGuard { path: log_dir.clone() };

    // initdb. env_clear so the test process's locale/encoding settings
    // don't leak into the cluster's defaults (initdb honours LANG,
    // LC_*, etc.); LC_ALL=C ensures a deterministic collation that
    // matches what the production migration runs against.
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

    // Socket dir must exist mode 0700 before postgres starts, or it
    // refuses to create the socket file.
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

    // Supervisor spec. We use a fresh default_supervisor() for the
    // guard so the test's `sup` handle stays usable for the test body
    // without aliasing the guard's drop path.
    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = service_name.to_string();
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
        "postgres flapped within 500ms of start; check {}.err for the postmaster log",
        spec.name,
    );

    let conn_spec = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };

    PgCluster {
        conn_spec,
        data_dir,
        socket_dir,
        sup,
        service_name: spec.name,
        _guards: (service_guard, data_guard, log_guard),
    }
}
