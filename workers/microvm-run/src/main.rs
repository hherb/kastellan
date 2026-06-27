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
    // child instead of orphaning it (holding KVM/vsock). It also removes the
    // firecracker-created base UDS. The guard owns a clone; the outer scope
    // keeps `vsock_uds` for the connect borrow below.
    let uds_for_guard = vsock_uds.clone();
    let run_dir_for_guard = run_dir.clone();
    let teardown = scopeguard(move || {
        let _ = fc.kill();
        // Remove the whole per-spawn run-dir when we know it (#362); this
        // subsumes the base-UDS removal since the UDS lives inside it. When the
        // flag is absent (older caller / a direct test), fall back to the UDS.
        match run_dir_for_guard {
            Some(dir) => remove_run_dir(&dir),
            None => {
                let _ = std::fs::remove_file(&uds_for_guard);
            }
        }
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

/// Best-effort removal of the per-spawn run-dir on launcher exit. Separated from
/// the teardown closure so it is unit-testable without booting a VM. Removing
/// the whole dir subsumes removing the base vsock UDS (which lives inside it).
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
}
