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
/// for JSON-RPC, or `None` if the guest never came up.
pub fn connect_hybrid_vsock(uds_path: &str, port: u32, timeout: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + timeout;
    let connect_line = firecracker_vsock_connect_line(port);
    loop {
        if let Some(stream) = try_handshake(uds_path, connect_line.as_bytes()) {
            return Some(stream);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One handshake attempt: connect, send `CONNECT <port>\n`, read the reply line,
/// succeed only on a leading `OK`. A 2 s read timeout bounds the wait when
/// firecracker accepts the UDS but the guest port has no listener yet; any error
/// or non-`OK` reply yields `None` so the caller retries. On success the read
/// timeout is cleared so [`pump`]'s blocking copy behaves normally.
fn try_handshake(uds_path: &str, connect_line: &[u8]) -> Option<UnixStream> {
    let mut stream = UnixStream::connect(uds_path).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(connect_line).ok()?;
    let mut byte = [0u8; 1];
    let mut line = Vec::with_capacity(32);
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return None, // closed before a full line
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > 64 {
                    return None; // runaway: not a real "OK <port>" line
                }
            }
            Err(_) => return None, // timeout / error → retry
        }
    }
    if line.starts_with(b"OK") {
        stream.set_read_timeout(None).ok()?; // back to blocking for pump
        Some(stream)
    } else {
        None
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
}
