//! Host-side reverse-relay (slice 4a egress + VM × broker). Firecracker delivers
//! a guest-initiated vsock connection on port P to the host UDS `<base>_P`; this
//! module listens there and pipes every such connection to a real host-side
//! target UDS, so an in-VM worker reaches the target with unchanged code. Two
//! channels ride it: egress (port 1025 → the host egress proxy) and the embed
//! broker (port 1026 → the host broker); both share the generic
//! [`spawn_reverse_relay`]. Detached threads die on launcher exit (VM teardown);
//! the listener socket lives in the run-dir, so the launcher's RAII teardown
//! reclaims it.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

/// Host-side path firecracker connects to for a guest-initiated vsock connection
/// on `port`: the base UDS with a `_<port>` suffix.
pub fn guest_initiated_uds_path(base_uds: &str, port: u32) -> String {
    format!("{base_uds}_{port}")
}

/// Parse the optional reverse-relay args (a `--*-uds` + `--*-vsock-port` pair);
/// `Some((target_uds, port))` only when both a UDS and a parseable port are
/// present. Shared by the egress and broker channels.
pub fn parse_reverse_relay_args(uds: Option<String>, port: Option<String>) -> Option<(String, u32)> {
    let uds = uds?;
    let port = port?.parse().ok()?;
    Some((uds, port))
}

/// Bind the reverse-relay listener at `<base_uds>_<port>` and spawn a detached
/// accept loop that pipes each accepted connection to `target_uds` (the host
/// egress proxy for the egress channel, the host broker for the broker channel).
/// Returns the bound path.
pub fn spawn_reverse_relay(
    base_uds: &str,
    port: u32,
    target_uds: String,
) -> std::io::Result<String> {
    let path = guest_initiated_uds_path(base_uds, port);
    let _ = std::fs::remove_file(&path); // clear a stale socket so bind() succeeds
    let listener = UnixListener::bind(&path)?;
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(guest) = conn else { continue };
            let target_uds = target_uds.clone();
            thread::spawn(move || match UnixStream::connect(&target_uds) {
                Ok(target) => relay_bidirectional(guest, target),
                Err(e) => eprintln!("microvm-run relay: dial target {target_uds} failed: {e}"),
            });
        }
    });
    Ok(path)
}

/// Pipe bytes both directions between two connected streams until either closes.
fn relay_bidirectional(left: UnixStream, right: UnixStream) {
    let (Ok(left_rd), Ok(right_rd)) = (left.try_clone(), right.try_clone()) else {
        // Likely fd exhaustion (EMFILE/ENFILE) under many concurrent relay
        // connections; log it so a dropped connection is diagnosable rather than
        // surfacing in-guest as a phantom intermittent network error.
        eprintln!("microvm-run relay: try_clone failed; dropping connection");
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
    fn parse_reverse_relay_args_requires_both() {
        assert_eq!(
            parse_reverse_relay_args(Some("/p.sock".into()), Some("1025".into())),
            Some(("/p.sock".to_string(), 1025))
        );
        assert_eq!(parse_reverse_relay_args(None, Some("1025".into())), None);
        assert_eq!(parse_reverse_relay_args(Some("/p.sock".into()), None), None);
        assert_eq!(parse_reverse_relay_args(Some("/p.sock".into()), Some("nope".into())), None);
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
        let bound = spawn_reverse_relay(
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

    #[test]
    fn two_relays_bind_distinct_suffix_paths() {
        // Egress (1025) and broker (1026) reverse-relays share the vsock base UDS
        // but must bind DISTINCT host listener paths (`<base>_<port>`), so neither
        // hides the other. Proves the generic relay supports a second channel.
        let dir = std::env::temp_dir().join(format!("kastellan-tworelay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("vsock.sock");
        let egress_target = dir.join("egress-proxy.sock");
        let broker_target = dir.join("broker.sock");
        let e = spawn_reverse_relay(
            &base.to_string_lossy(),
            1025,
            egress_target.to_string_lossy().into_owned(),
        )
        .unwrap();
        let b = spawn_reverse_relay(
            &base.to_string_lossy(),
            1026,
            broker_target.to_string_lossy().into_owned(),
        )
        .unwrap();
        assert_eq!(e, format!("{}_1025", base.to_string_lossy()));
        assert_eq!(b, format!("{}_1026", base.to_string_lossy()));
        assert_ne!(e, b, "the two channels must bind distinct listener paths");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
