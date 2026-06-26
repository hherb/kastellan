//! `kastellan-microvm-run`: the process the sandbox backend spawns as the
//! worker `Child`. Boots a Firecracker micro-VM and bridges the worker's
//! JSON-RPC stdio over hybrid vsock. Kernel logs go to `--log`, never stdout.

mod boot;
mod bridge;

use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    let port: u32 = arg("--vsock-port").expect("--vsock-port required").parse().unwrap();
    let log = arg("--log").unwrap_or_else(|| "/dev/null".into());

    // Boot firecracker as our child; it creates the vsock UDS once the guest
    // is up. Its stdout/stderr go to the log path via --log-path, so we keep
    // our own stdout pristine for JSON-RPC.
    let fc_argv = boot::firecracker_argv(&config, &log);
    let mut fc = Command::new(&fc_argv[0])
        .args(&fc_argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // The guest's init listens on `port`; firecracker exposes it as
    // "<uds_path>_<port>" for host-initiated connections (hybrid vsock).
    let conn_path = format!("{vsock_uds}_{port}");
    let stream = connect_with_retry(&conn_path, Duration::from_secs(20))
        .expect("guest vsock did not come up within 20s");

    // Hybrid-vsock handshake on a plain connect to the per-port socket is not
    // required (the _<port> suffix encodes it); the worker speaks JSON-RPC now.
    let teardown = scopeguard(move || { let _ = fc.kill(); let _ = std::fs::remove_file(&conn_path); });
    bridge::pump(stream);
    drop(teardown);
    Ok(())
}

/// Retry connecting to the per-port vsock UDS until the guest listener is up.
fn connect_with_retry(path: &str, timeout: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = UnixStream::connect(path) { return Some(s); }
        if Instant::now() >= deadline { return None; }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Minimal RAII guard (avoid a dep; teardown must run on every exit path).
fn scopeguard<F: FnOnce()>(f: F) -> impl Drop {
    struct G<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for G<F> { fn drop(&mut self) { if let Some(f) = self.0.take() { f(); } } }
    G(Some(f))
}
