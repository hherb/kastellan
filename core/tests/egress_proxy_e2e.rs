//! End-to-end: `core::egress::spawn_sidecar` brings up the egress-proxy under
//! the real platform sandbox; a test CONNECT client over the UDS exercises the
//! allowed / blocked paths; `decision_to_audit` maps the proxy's stdout stream.
//!
//! Hermetic test drives a localhost origin via a literal-allowlisted CONNECT.
//! `[SKIP]`s cleanly when the sandbox or the worker binary is missing — same
//! posture as `web_fetch_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;

use hhagent_core::egress::audit::decision_to_audit;
use hhagent_core::egress::spawn::spawn_sidecar;
use hhagent_tests_common::{
    backend, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
};

/// Locate the built proxy binary; `[SKIP]` if absent.
fn proxy_binary_or_skip() -> Option<std::path::PathBuf> {
    let p = workspace_target_binary("hhagent-worker-egress-proxy");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[test]
fn allowed_literal_origin_round_trips_and_blocks_off_allowlist() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(binary) = proxy_binary_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };

    // A localhost origin that echoes a token after the client writes.
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let origin_thread = std::thread::spawn(move || {
        if let Ok((mut s, _)) = origin.accept() {
            let mut buf = [0u8; 8];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"PONG");
        }
    });

    // Scratch dir (must be writable by the sandboxed proxy to create the UDS).
    let scratch = std::env::temp_dir().join(format!("egress-e2e-{}", unique_suffix()));
    std::fs::create_dir_all(&scratch).unwrap();

    // Allowlist: the literal loopback origin (the local-SearxNG carve-out shape).
    let allowlist = vec!["127.0.0.1".to_string()];
    let backend = backend();
    let mut handle = spawn_sidecar(backend.as_ref(), &binary, &allowlist, &scratch, "web-fetch")
        .expect("sidecar spawns and binds UDS");
    let stdout = handle.stdout().expect("child stdout piped");

    // Allowed round-trip via CONNECT to the literal-allowlisted origin.
    let mut client = UnixStream::connect(&handle.uds_path).unwrap();
    write!(client, "CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").unwrap();
    let mut head = [0u8; 39];
    client.read_exact(&mut head).unwrap();
    assert!(std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"));
    client.write_all(b"ping").unwrap();
    let mut echo = [0u8; 4];
    client.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"PONG");
    drop(client);
    origin_thread.join().unwrap();

    // Off-allowlist CONNECT is blocked at the boundary.
    let mut bad = UnixStream::connect(&handle.uds_path).unwrap();
    write!(bad, "CONNECT evil.test:443 HTTP/1.1\r\n\r\n").unwrap();
    let mut resp = String::new();
    let _ = bad.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 403"), "got {resp:?}");

    // Drain the decision stream and map to audit rows.
    let reader = BufReader::new(stdout);
    let mut actions = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        if let Some(row) = decision_to_audit(&line) {
            actions.push(row.action);
        }
        if actions.len() >= 2 {
            break;
        }
    }
    handle.shutdown();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(actions.contains(&"egress.allowed".to_string()), "actions: {actions:?}");
    assert!(actions.contains(&"egress.blocked.allowlist".to_string()), "actions: {actions:?}");
}

/// Real-network: a test CONNECT to a real public host round-trips through the
/// sandboxed proxy (validates DNS + IP-pinning + tunnel + TLS-in-jail end to
/// end). Run with `--ignored` and network access.
#[test]
#[ignore = "real network: validates DNS + pinning + tunnel through the sandboxed proxy"]
fn real_host_round_trips_through_sidecar() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(binary) = proxy_binary_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };
    let scratch = std::env::temp_dir().join(format!("egress-e2e-real-{}", unique_suffix()));
    std::fs::create_dir_all(&scratch).unwrap();
    let allowlist = vec!["example.com".to_string()];
    let backend = backend();
    let handle = spawn_sidecar(backend.as_ref(), &binary, &allowlist, &scratch, "web-fetch")
        .expect("sidecar spawns");

    let mut client = UnixStream::connect(&handle.uds_path).unwrap();
    write!(client, "CONNECT example.com:443 HTTP/1.1\r\n\r\n").unwrap();
    let mut head = [0u8; 39];
    client.read_exact(&mut head).unwrap();
    assert!(
        std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"),
        "expected a tunnel to a real allowlisted public host"
    );

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&scratch);
}
