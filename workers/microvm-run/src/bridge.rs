//! stdin↔vsock↔stdout bridge for the worker JSON-RPC channel.

use std::io::Write;
use std::os::unix::net::UnixStream;

/// Firecracker hybrid-vsock handshake: after connecting the host-side UDS the
/// client must announce the guest port with `CONNECT <port>\n`; the guest's
/// listener replies `OK <assigned_hostport>\n` before bytes flow.
///
/// `#[allow(dead_code)]`: slice 1's `main.rs` takes the per-port `_<port>`
/// suffix path, so this helper is unreferenced in the (non-test) bin build and
/// would trip `dead_code` under `-D warnings`. It is retained deliberately
/// because Task 7 Step 2 resolves the connect direction live on the DGX; the
/// losing branch is deleted then (and this `allow` with it).
#[allow(dead_code)]
pub fn firecracker_vsock_connect_line(port: u32) -> String {
    format!("CONNECT {port}\n")
}

/// Copy bytes both directions between this process's stdin/stdout and the
/// connected guest stream until either side closes. Two threads: host→guest
/// and guest→host. JSON-RPC is line-framed but we copy raw bytes (framing is
/// the worker's concern).
pub fn pump(stream: UnixStream) {
    let mut to_guest = stream.try_clone().expect("clone vsock stream");
    let from_guest = stream;
    let h = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let _ = std::io::copy(&mut stdin, &mut to_guest);
        let _ = to_guest.shutdown(std::net::Shutdown::Write);
    });
    let mut from_guest = from_guest;
    let mut stdout = std::io::stdout().lock();
    let _ = std::io::copy(&mut from_guest, &mut stdout);
    let _ = stdout.flush();
    let _ = h.join();
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
