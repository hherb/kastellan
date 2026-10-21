//! stdin↔vsock↔stdout bridge for the worker JSON-RPC channel.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Firecracker hybrid-vsock handshake line: after connecting the host-side UDS
/// the client announces the guest port with `CONNECT <port>\n`; firecracker
/// replies `OK <assigned_hostport>\n` once the guest listener is up.
pub fn firecracker_vsock_connect_line(port: u32) -> String {
    format!("CONNECT {port}\n")
}

/// Connect to the guest worker over Firecracker **host-initiated** hybrid vsock.
///
/// Our model is guest-listens / host-connects, so the correct direction is:
/// dial the *base* host UDS firecracker created (`uds_path`), send
/// `CONNECT <port>\n`, and proceed only on the `OK …` reply. (DGX-verified
/// 2026-06-27: the `<uds_path>_<port>` suffix is the opposite, *guest*-initiated
/// direction and does not reach a guest that is listening.)
///
/// Retries until `timeout` because the guest needs time to boot and bind its
/// vsock listener. Returns a connected, post-handshake, blocking stream ready
/// for JSON-RPC, or — if the guest never came up — **why the last attempt
/// failed**. That reason is the only thing separating "the guest port had no
/// listener yet" from a genuine vsock `EIO` (#754), so it goes in the report.
pub fn connect_hybrid_vsock(
    uds_path: &str,
    port: u32,
    timeout: Duration,
) -> Result<UnixStream, String> {
    let deadline = Instant::now() + timeout;
    let connect_line = firecracker_vsock_connect_line(port);
    loop {
        let last = match try_handshake(uds_path, connect_line.as_bytes()) {
            Ok(stream) => return Ok(stream),
            Err(why) => why,
        };
        if Instant::now() >= deadline {
            return Err(last);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One handshake attempt: connect, send `CONNECT <port>\n`, read the reply line,
/// succeed only on a leading `OK`. A 2 s read timeout bounds the wait when
/// firecracker accepts the UDS but the guest port has no listener yet; any
/// failure returns a description of it so the caller can retry — and, at its
/// deadline, say which failure it kept seeing. On success the read timeout is
/// cleared so [`pump`]'s blocking copy behaves normally.
fn try_handshake(uds_path: &str, connect_line: &[u8]) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(uds_path).map_err(|e| format!("connect {uds_path}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("set read timeout: {e}"))?;
    stream.write_all(connect_line).map_err(|e| format!("send CONNECT: {e}"))?;
    let mut byte = [0u8; 1];
    let mut line = Vec::with_capacity(32);
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err("closed before a full reply line".to_string()),
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > 64 {
                    return Err("reply line over 64 bytes: not a real \"OK <port>\"".to_string());
                }
            }
            // The read timeout surfaces as WouldBlock on Unix (TimedOut elsewhere).
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err("no reply within 2s (the guest port may have no listener yet)".to_string())
            }
            Err(e) => return Err(format!("read reply: {e}")),
        }
    }
    if line.starts_with(b"OK") {
        stream.set_read_timeout(None).map_err(|e| format!("clear read timeout: {e}"))?; // back to blocking for pump
        Ok(stream)
    } else {
        // Debug-escaped: these bytes came off the socket.
        Err(format!("refused: {:?}", String::from_utf8_lossy(&line)))
    }
}

/// Copy bytes both directions between this process's stdin/stdout and the
/// connected guest stream. Two threads: host→guest and guest→host, sharing the
/// one socket (via `try_clone`). JSON-RPC is line-framed but we copy raw bytes
/// (framing is the worker's concern).
///
/// Teardown is driven by the HOST side closing our stdin: when `tool_host`
/// finishes a dispatch and closes the worker, our stdin hits EOF, the host→guest
/// copy returns, and `pump` returns at once so the caller tears down the VM.
///
/// The guest→host relay runs on a **detached** thread that we deliberately do
/// NOT join: a guest worker that is blocked on `read` (the SingleUse case after
/// it has answered) never closes its side, so joining would block until the
/// wall-clock watchdog and add a ~30 s tail to every call. We instead
/// `shutdown(Both)` the shared socket (best-effort, to nudge the relay) and
/// return; process exit reaps the detached thread. This is safe because the
/// response has already flowed to our stdout and been read by the host *before*
/// it closes our stdin (the protocol reads the full response line, then closes),
/// so the relay has nothing left to deliver.
pub fn pump(stream: UnixStream) {
    let mut to_guest = stream.try_clone().expect("clone vsock stream");
    let mut from_guest = stream;
    // guest→host relays the worker's responses to our stdout; detached.
    //
    // We read+`write_all`+**flush** each chunk explicitly rather than
    // `io::copy(.., stdout().lock())`. `Stdout` is a `LineWriter`, and relaying
    // through it does NOT reliably push a complete JSON-RPC response line out to
    // the host pipe promptly — the dispatcher's blocking `read_line` then sees
    // nothing until the worker is wall-clock-killed (~30 s tail per call,
    // measured). Flushing every chunk delivers the response the instant the
    // guest emits it, so dispatch returns immediately and teardown follows.
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let stdout = std::io::stdout();
        loop {
            match from_guest.read(&mut buf) {
                Ok(0) | Err(_) => break, // guest closed or socket shut down
                Ok(n) => {
                    let mut lock = stdout.lock();
                    if lock.write_all(&buf[..n]).is_err() || lock.flush().is_err() {
                        break;
                    }
                }
            }
        }
    });
    // host→guest on the main thread; returns when the host closes our stdin.
    let mut stdin = std::io::stdin().lock();
    let _ = std::io::copy(&mut stdin, &mut to_guest);
    let _ = to_guest.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vsock_connect_line_is_connect_port_newline() {
        // Firecracker hybrid vsock: after connecting the host UDS, the client
        // sends "CONNECT <port>\n" and waits for "OK <hostport>\n".
        assert_eq!(firecracker_vsock_connect_line(1024), "CONNECT 1024\n");
    }

    /// A one-shot fake firecracker: accepts one connection, reads the CONNECT
    /// line, then answers `reply` and holds on (or just closes, for `None`).
    fn fake_vmm(reply: Option<&'static [u8]>) -> String {
        use std::io::BufRead;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "kastellan-bridge-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut line = String::new();
            std::io::BufReader::new(conn.try_clone().unwrap()).read_line(&mut line).unwrap();
            if let Some(reply) = reply {
                let _ = conn.write_all(reply);
                // Hold the connection open like a real VMM: macOS refuses
                // `setsockopt` (EINVAL) on a socket whose peer has gone.
                let _ = std::io::copy(&mut conn, &mut std::io::sink());
            }
        });
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_handshake_succeeds_only_on_ok() {
        let path = fake_vmm(Some(b"OK 52000\n"));
        if let Err(e) = try_handshake(&path, b"CONNECT 1024\n") {
            panic!("an OK reply must complete the handshake: {e}");
        }
    }

    #[test]
    fn each_handshake_failure_says_which_it_was() {
        // #754: these all collapsed to `None`, so a genuine vsock `EIO` read the
        // same as "no listener yet" and the boot report could only say "timeout".
        let path = fake_vmm(None);
        let closed = try_handshake(&path, b"CONNECT 1024\n").expect_err("closed");
        assert!(closed.contains("closed before a full reply line"), "{closed}");

        let path = fake_vmm(Some(b"FAILURE\n"));
        let refused = try_handshake(&path, b"CONNECT 1024\n").expect_err("refused");
        assert!(refused.contains("refused") && refused.contains("FAILURE"), "{refused}");

        let path = fake_vmm(Some(&[b'x'; 70]));
        let runaway = try_handshake(&path, b"CONNECT 1024\n").expect_err("runaway");
        assert!(runaway.starts_with("reply line over 64 bytes"), "{runaway}");

        let absent = try_handshake("/nonexistent/kastellan-v.sock", b"CONNECT 1024\n")
            .expect_err("absent");
        assert!(absent.starts_with("connect /nonexistent/kastellan-v.sock: "), "{absent}");
    }

    #[test]
    fn connecting_past_the_deadline_returns_the_last_failure() {
        let err = connect_hybrid_vsock("/nonexistent/kastellan-v.sock", 1024, Duration::ZERO)
            .expect_err("nothing listens");
        assert!(err.starts_with("connect /nonexistent/kastellan-v.sock: "), "{err}");
    }
}
