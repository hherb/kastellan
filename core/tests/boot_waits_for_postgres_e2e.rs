//! The daemon waits for Postgres at boot instead of exiting (issue #909).
//!
//! `kastellan_db::boot_wait` has its own unit tests. This file proves the
//! daemon actually calls it: started against a socket directory with no socket
//! in it yet (a Postgres that has not started), `kastellan` must log the wait
//! more than once and still be running, instead of exiting on the first
//! refused dial as it did before #909.
//!
//! No Postgres is needed. The binary is cargo's own build of this package's
//! `kastellan` bin, so it is always present and never stale.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use kastellan_tests_common::{current_username, unique_temp_root, PathGuard};

/// The daemon's per-attempt log message (`kastellan_db::boot_wait`).
const WAIT_LINE: &str = "postgres not accepting connections yet; waiting";

#[test]
fn daemon_waits_for_a_postgres_that_has_not_started() {
    let root = unique_temp_root("boot-wait");
    let data_dir = root.join("d");
    // The socket directory exists (as `kastellan-db-init` leaves it) but holds
    // no socket yet: a missing directory is refused at once, not waited for.
    std::fs::create_dir_all(data_dir.join("sockets")).expect("create socket dir");
    let _guard = PathGuard { path: root.clone() };

    let mut child = Command::new(env!("CARGO_BIN_EXE_kastellan"))
        .env_clear()
        // A scratch HOME, so the operator's own config overlay is not read.
        .env("HOME", &root)
        .env("USER", current_username())
        .env("PATH", "/usr/bin:/bin")
        .env("KASTELLAN_DATA_DIR", &data_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kastellan");

    // Read both streams on threads so the wait below has a deadline. The
    // JSON log is on stdout; a boot refusal (`Error: ...`) is on stderr, and
    // a failure message without it would not say why the daemon exited.
    let (tx, rx) = mpsc::channel();
    let streams: [Box<dyn std::io::Read + Send>; 2] = [
        Box::new(child.stdout.take().expect("piped stdout")),
        Box::new(child.stderr.take().expect("piped stderr")),
    ];
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx); // the channel closes once both streams end

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
