//! The daemon waits for Postgres at boot instead of exiting (issue #909).
//!
//! `kastellan_db::boot_wait` has its own unit tests. This file proves the
//! daemon actually calls it, both ways round:
//!
//! - **It waits.** Started against a socket directory with no socket in it
//!   yet (a Postgres that has not started), `kastellan` must log the wait more
//!   than once and still be running, instead of exiting on the first refused
//!   dial as it did before #909. No Postgres needed.
//! - **It recovers** (#911). Started against an `initdb`'d cluster that is not
//!   running, then the cluster is started under the daemon's feet: the wait
//!   must end in `postgres is accepting connections` (after more than one
//!   attempt) and the database probe must succeed (`STARTUP_READY_MSG`) — on a
//!   real fresh cluster, so the production dial is pinned to a database that
//!   exists before migrations run. Skip-as-pass without a supervisor/PG;
//!   demanded by the `pg` profile of `scripts/run-e2e-gate.sh`.
//!
//! The binary is cargo's own build of this package's `kastellan` bin, so it
//! is always present and never stale.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kastellan_core::STARTUP_READY_MSG;
use kastellan_tests_common::{
    current_username, init_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
    unique_temp_root, PathGuard, PG_BRING_UP_TIMEOUT_SECS,
};

/// The daemon's per-attempt log message (`kastellan_db::boot_wait`).
const WAIT_LINE: &str = "postgres not accepting connections yet; waiting";

/// The daemon's line when a wait ends in a connection (`kastellan_db::boot_wait`).
const ACCEPTING_LINE: &str = "postgres is accepting connections";

/// Spawn `kastellan` against `data_dir` with `home` as its scratch HOME
/// (`home/state` its state dir), and return it with a channel carrying every
/// line of its stdout and stderr.
///
/// Both streams are read on threads so a wait on the channel has a deadline.
/// The JSON log is on stdout; a boot refusal (`Error: ...`) is on stderr, and
/// a failure message without it would not say why the daemon exited.
fn spawn_daemon(data_dir: &Path, home: &Path) -> (Child, mpsc::Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kastellan"))
        .env_clear()
        // A scratch HOME, so the operator's own config overlay is not read.
        .env("HOME", home)
        .env("USER", current_username())
        .env("PATH", "/usr/bin:/bin")
        .env("KASTELLAN_DATA_DIR", data_dir)
        .env("KASTELLAN_STATE_DIR", home.join("state"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kastellan");

    let (tx, rx) = mpsc::channel();
    let streams: [Box<dyn std::io::Read + Send>; 2] = [
        Box::new(child.stdout.take().expect("piped stdout")),
        Box::new(child.stderr.take().expect("piped stderr")),
    ];
    for stream in streams {
        let tx = tx.clone();
        // Lossy, so one non-UTF-8 line cannot end the reader and leave the
        // daemon blocked on a full pipe.
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            while matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
                buf.clear();
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    // `tx` drops here, so the channel closes once both streams end.
    (child, rx)
}

/// Read lines into `seen` until one contains `needle` or `deadline`
/// passes; whether it was found.
fn read_until(rx: &mpsc::Receiver<String>, seen: &mut Vec<String>, needle: &str, deadline: Instant) -> bool {
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(line) => {
                let hit = line.contains(needle);
                seen.push(line);
                if hit {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
    false
}

#[test]
fn daemon_waits_for_a_postgres_that_has_not_started() {
    let root = unique_temp_root("boot-wait");
    let data_dir = root.join("d");
    // The socket directory exists (as `kastellan-db-init` leaves it) but holds
    // no socket yet: a missing directory is refused at once, not waited for.
    std::fs::create_dir_all(data_dir.join("sockets")).expect("create socket dir");
    let _guard = PathGuard { path: root.clone() };

    let (mut child, rx) = spawn_daemon(&data_dir, &root);

    // Two wait lines prove a retry happened. The interval is 1 s, so 15 s is
    // plenty even on a loaded host.
    let mut seen = Vec::new();
    let mut waits = 0;
    while waits < 2 {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(line) => {
                if line.contains(WAIT_LINE) {
                    waits += 1;
                }
                seen.push(line);
            }
            Err(_) => break,
        }
    }
    let still_running = child.try_wait().expect("poll kastellan").is_none();
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(waits, 2, "the daemon should log {WAIT_LINE:?} twice; its log:\n{}", seen.join("\n"));
    assert!(still_running, "the daemon must still be waiting, not exited; its log:\n{}", seen.join("\n"));
}

#[test]
fn daemon_boots_once_a_late_postgres_starts() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let init = init_pg_cluster(&bin_dir, "bwr-d", "bwr-l");
    let home = unique_temp_root("bwr");
    std::fs::create_dir_all(&home).expect("create scratch HOME");
    let _home_guard = PathGuard { path: home.clone() };

    let (child, rx) = spawn_daemon(&init.data_dir, &home);
    // A `Child` is not killed on drop; this one is, so a failed assertion
    // below does not leave the daemon running.
    struct KillOnDrop(Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = KillOnDrop(child);
    let mut seen = Vec::new();
    let log = |seen: &[String]| seen.join("\n");

    // The daemon is waiting on a cluster that is not running...
    let waiting = read_until(&rx, &mut seen, WAIT_LINE, Instant::now() + Duration::from_secs(15));
    assert!(waiting, "the daemon should log {WAIT_LINE:?}; its log:\n{}", log(&seen));

    // ...until it starts. The boot wait's budget is 60 s from the first dial.
    // `start` waits up to `PG_BRING_UP_TIMEOUT_SECS` (30 s) for Active and
    // again for the socket, so a launchd slow enough to use both would
    // outlast the daemon — a red run naming the budget, never a false pass.
    // A healthy cluster comes up in about a second.
    let _cluster = init.start(
        &format!("kastellan-supervisor-test-pg-bwr-{}", unique_suffix()),
        Duration::from_secs(PG_BRING_UP_TIMEOUT_SECS),
    );
    // Migrations run before the ready line; generous for a loaded sweep.
    let deadline = Instant::now() + Duration::from_secs(60);
    let ready = read_until(&rx, &mut seen, STARTUP_READY_MSG, deadline);
    let exited = child.0.try_wait().expect("poll kastellan");
    drop(child);
    if !ready {
        // What the daemon said between the deadline and the kill.
        while let Ok(line) = rx.recv_timeout(Duration::from_secs(1)) {
            seen.push(line);
        }
    }
    assert!(
        ready,
        "the daemon should log {STARTUP_READY_MSG:?} once Postgres is up (exited: {exited:?}); \
         its log:\n{}",
        log(&seen)
    );

    // The wait ended in a connection, after more than one attempt, and
    // said so.
    let accepted: serde_json::Value = seen
        .iter()
        .find(|l| l.contains(ACCEPTING_LINE))
        .and_then(|l| serde_json::from_str(l).ok())
        .unwrap_or_else(|| panic!("no {ACCEPTING_LINE:?} JSON line; the log:\n{}", log(&seen)));
    let attempts = accepted["fields"]["attempts"].as_u64();
    assert!(attempts.is_some_and(|n| n > 1), "attempts must be > 1: {accepted}");
}
