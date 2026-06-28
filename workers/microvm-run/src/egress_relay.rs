//! Host-side egress reverse-relay (slice 4a). Firecracker delivers a
//! guest-initiated vsock connection on port P to the host UDS `<base>_P`; this
//! module listens there and pipes every such connection to the real host egress
//! proxy UDS, so an in-VM worker reaches the proxy with unchanged code. Detached
//! threads die on launcher exit (VM teardown); the listener socket lives in the
//! run-dir, so the launcher's RAII teardown reclaims it.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

/// Host-side path firecracker connects to for a guest-initiated vsock connection
/// on `port`: the base UDS with a `_<port>` suffix.
pub fn guest_initiated_uds_path(base_uds: &str, port: u32) -> String {
    format!("{base_uds}_{port}")
}

/// Parse the optional egress reverse-relay args; `Some((proxy_uds, port))` only
/// when both `--egress-uds` and a parseable `--egress-vsock-port` are present.
pub fn parse_egress_relay_args(uds: Option<String>, port: Option<String>) -> Option<(String, u32)> {
    let uds = uds?;
    let port = port?.parse().ok()?;
    Some((uds, port))
}

/// Bind the reverse-relay listener at `<base_uds>_<port>` and spawn a detached
/// accept loop that pipes each accepted connection to `proxy_uds`. Returns the
/// bound path.
pub fn spawn_egress_relay(
    base_uds: &str,
    port: u32,
    proxy_uds: String,
) -> std::io::Result<String> {
    let path = guest_initiated_uds_path(base_uds, port);
    let _ = std::fs::remove_file(&path); // clear a stale socket so bind() succeeds
    let listener = UnixListener::bind(&path)?;
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(guest) = conn else { continue };
            let proxy_uds = proxy_uds.clone();
            thread::spawn(move || match UnixStream::connect(&proxy_uds) {
                Ok(proxy) => relay_bidirectional(guest, proxy),
                Err(e) => eprintln!("microvm-run egress: dial proxy {proxy_uds} failed: {e}"),
            });
        }
    });
    Ok(path)
}

/// Pipe bytes both directions between two connected streams until either closes.
fn relay_bidirectional(left: UnixStream, right: UnixStream) {
    let (Ok(left_rd), Ok(right_rd)) = (left.try_clone(), right.try_clone()) else {
        return;
    };
    let up = thread::spawn(move || pipe(left_rd, right)); // left -> right
    pipe(right_rd, left); // right -> left
    let _ = up.join();
}

/// One-direction byte copy with per-chunk flush; shuts the writer down on EOF.
fn pipe(mut src: UnixStream, mut dst: UnixStream) {
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = dst.shutdown(Shutdown::Write);
                break;
            }
            Ok(n) => {
                if dst.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = dst.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn guest_initiated_uds_path_appends_port_suffix() {
        assert_eq!(guest_initiated_uds_path("/run/vsock.sock", 1025), "/run/vsock.sock_1025");
    }

    #[test]
    fn parse_egress_relay_args_requires_both() {
        assert_eq!(
            parse_egress_relay_args(Some("/p.sock".into()), Some("1025".into())),
            Some(("/p.sock".to_string(), 1025))
        );
        assert_eq!(parse_egress_relay_args(None, Some("1025".into())), None);
        assert_eq!(parse_egress_relay_args(Some("/p.sock".into()), None), None);
        assert_eq!(parse_egress_relay_args(Some("/p.sock".into()), Some("nope".into())), None);
    }

    #[test]
    fn relay_pipes_guest_connection_to_proxy_uds_and_back() {
        let dir = std::env::temp_dir().join(format!("kastellan-egressrelay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proxy_path = dir.join("proxy.sock");
        let _ = std::fs::remove_file(&proxy_path);
        // Echo "proxy": read 5 bytes, reply PONG.
        let proxy = UnixListener::bind(&proxy_path).unwrap();
        thread::spawn(move || {
            if let Ok((mut c, _)) = proxy.accept() {
                let mut buf = [0u8; 5];
                if c.read_exact(&mut buf).is_ok() {
                    let _ = c.write_all(b"PONG\n");
                }
            }
        });
        let base = dir.join("vsock.sock");
        let bound = spawn_egress_relay(
            &base.to_string_lossy(),
            1025,
            proxy_path.to_string_lossy().into_owned(),
        )
        .unwrap();
        assert_eq!(bound, format!("{}_1025", base.to_string_lossy()));
        // Simulate firecracker delivering a guest-initiated connection.
        let mut c = UnixStream::connect(&bound).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(b"PING\n").unwrap();
        let mut buf = [0u8; 5];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"PONG\n", "relay forwarded PING to the proxy and PONG back");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
