//! `kastellan-microvm-run`: the process the sandbox backend spawns as the
//! worker `Child`. Boots a Firecracker micro-VM and bridges the worker's
//! JSON-RPC stdio over hybrid vsock. Kernel logs go to `--log`, never stdout.

mod boot;
mod bridge;

use std::process::{Command, Stdio};
use std::time::Duration;

fn arg(flag: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == flag { return it.next(); }
    }
    None
}

fn main() -> std::io::Result<()> {
    let config = arg("--config-file").expect("--config-file required");
    let vsock_uds = arg("--vsock-uds").expect("--vsock-uds required");
    let port: u32 = arg("--vsock-port")
        .expect("--vsock-port required")
        .parse()
        .expect("--vsock-port must be a u32");
    let log = arg("--log").unwrap_or_else(|| "/dev/null".into());

    // Per-spawn run-dir to remove on exit (#362). Optional for backward
    // compatibility with callers that don't pass it; when absent we fall back
    // to removing just the base vsock UDS, as before.
    let run_dir = arg("--run-dir");

    // Boot firecracker as our child; it creates the base vsock UDS once it is
    // up. Its stdout/stderr go to the log path via --log-path, so we keep our
    // own stdout pristine for JSON-RPC.
    let fc_argv = boot::firecracker_argv(&config, &log);
    let mut fc = Command::new(&fc_argv[0])
        .args(&fc_argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Build the teardown guard BEFORE connecting so a panic (or early return) in
    // the connect unwinds through it and kills the already-spawned firecracker
    // child instead of orphaning it (holding KVM/vsock). The run-dir disposition
    // (remove vs keep-for-diagnostics) is decided by `teardown_run_dir`. The
    // guard owns a clone; the outer scope keeps `vsock_uds` for the connect
    // borrow below.
    let uds_for_guard = vsock_uds.clone();
    let run_dir_for_guard = run_dir.clone();
    let teardown = scopeguard(move || {
        let _ = fc.kill();
        // `fc.kill()` always runs (never orphan firecracker holding KVM/vsock);
        // the run-dir disposition depends on whether we are unwinding a panic.
        teardown_run_dir(
            run_dir_for_guard.as_deref(),
            &uds_for_guard,
            std::thread::panicking(),
        );
    });

    // Host-initiated hybrid-vsock connect: dial the base UDS and `CONNECT` to
    // the guest's listening port (DGX-verified direction). Retries while the
    // guest boots and binds its listener.
    let stream = bridge::connect_hybrid_vsock(&vsock_uds, port, Duration::from_secs(20))
        .expect("guest vsock did not come up within 20s");
    bridge::pump(stream);
    drop(teardown);
    Ok(())
}

/// Minimal RAII guard (avoid a dep; teardown must run on every exit path).
fn scopeguard<F: FnOnce()>(f: F) -> impl Drop {
    struct G<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for G<F> { fn drop(&mut self) { if let Some(f) = self.0.take() { f(); } } }
    G(Some(f))
}

/// Decide what to clean up on launcher exit, given whether we are unwinding a
/// panic. Separated from the teardown closure so it is unit-testable without
/// booting a VM.
///
/// - Graceful exit, `--run-dir` known: remove the whole run-dir (#362). This
///   subsumes the base-UDS removal since the UDS lives inside it.
/// - **Panic** (firecracker/connect boot failure), `--run-dir` known: KEEP the
///   run-dir so firecracker's `fc.log` survives for post-mortem (#367 review).
///   The orphan sweep in the next `spawn_under_policy` reclaims it once this
///   launcher's now-dead pid is observed — so this is a deferred clean, not a
///   leak.
/// - No `--run-dir` (legacy caller / direct test): fall back to removing just
///   the base vsock UDS, as before — on both graceful and panic paths.
fn teardown_run_dir(run_dir: Option<&str>, base_uds: &str, panicking: bool) {
    match run_dir {
        Some(dir) if !panicking => remove_run_dir(dir),
        Some(_) => {} // panic path: keep the run-dir for diagnostics.
        None => {
            let _ = std::fs::remove_file(base_uds);
        }
    }
}

/// Best-effort removal of the per-spawn run-dir on launcher exit. Removing the
/// whole dir subsumes removing the base vsock UDS (which lives inside it).
fn remove_run_dir(run_dir: &str) {
    let _ = std::fs::remove_dir_all(run_dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_run_dir_deletes_the_directory_tree() {
        let dir = std::env::temp_dir().join(format!(
            "kastellan-microvm-runtest-{}-{}",
            std::process::id(),
            "a"
        ));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("fc.json"), "{}").unwrap();
        assert!(dir.exists());

        remove_run_dir(&dir.to_string_lossy());

        assert!(!dir.exists(), "remove_run_dir must delete the whole tree");
    }

    #[test]
    fn remove_run_dir_is_noop_on_missing_dir() {
        // Must not panic when the dir is already gone.
        remove_run_dir("/tmp/kastellan-microvm-runtest-definitely-absent-zzz");
    }

    fn fresh_run_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kastellan-microvm-runtest-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fc.log"), "boot log").unwrap();
        dir
    }

    #[test]
    fn teardown_removes_run_dir_on_graceful_exit() {
        let dir = fresh_run_dir("graceful");
        teardown_run_dir(Some(&dir.to_string_lossy()), "/unused", false);
        assert!(!dir.exists(), "graceful exit must remove the run-dir");
    }

    #[test]
    fn teardown_keeps_run_dir_on_panic_for_diagnostics() {
        // #367: a boot failure (panic) must KEEP the run-dir so fc.log survives;
        // the orphan sweep reclaims it later once the launcher pid is dead.
        let dir = fresh_run_dir("panic");
        teardown_run_dir(Some(&dir.to_string_lossy()), "/unused", true);
        assert!(dir.exists(), "panic must keep the run-dir for post-mortem");
        assert!(dir.join("fc.log").exists(), "fc.log must survive a panic exit");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn teardown_removes_base_uds_when_no_run_dir() {
        // Legacy caller (no --run-dir): fall back to removing just the base UDS,
        // on both graceful and panic paths.
        for panicking in [false, true] {
            let uds = std::env::temp_dir().join(format!(
                "kastellan-microvm-runtest-{}-uds-{panicking}.sock",
                std::process::id()
            ));
            std::fs::write(&uds, "").unwrap();
            teardown_run_dir(None, &uds.to_string_lossy(), panicking);
            assert!(!uds.exists(), "legacy path must remove the base UDS");
        }
    }
}
