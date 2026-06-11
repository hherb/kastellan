//! Gating probe for egress slice #2 (macOS): a process under the force-routed
//! `Net::Allowlist` Seatbelt profile must (a) FAIL to connect any AF_INET
//! address and (b) SUCCEED connecting the proxy UDS. If (a) fails, the design
//! falls back to the `container` backend for net workers on darwin.
//!
//! This test mirrors the `macos_smoke.rs` harness: it invokes
//! `sandbox-exec -p <profile>` with small fixture binaries from
//! `target/debug/`. Run `cargo build -p kastellan-sandbox` first to
//! populate those binaries (or rely on the `cargo test` build step).
#![cfg(target_os = "macos")]

use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use kastellan_sandbox::{
    macos_seatbelt::{build_profile, MacosSeatbelt},
    Net, SandboxBackend, SandboxPolicy,
};

// ---- helpers mirrored from macos_smoke.rs --------------------------------

fn skip_if_no_seatbelt() -> bool {
    match MacosSeatbelt::probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
            true
        }
    }
}

fn read_to_string(handle: &mut Option<impl std::io::Read>) -> String {
    let mut s = String::new();
    if let Some(h) = handle.as_mut() {
        let _ = h.read_to_string(&mut s);
    }
    s
}

/// Resolve a fixture binary from `target/debug/` (the same convention as
/// `net_probe_binary`, `sid_probe_binary`, etc. in `macos_smoke.rs`).
fn fixture_binary(name: &str) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join(name)
}

// ---- the gating probe ----------------------------------------------------

/// The ONE gating probe for Seatbelt-based egress force-routing (slice #2):
///
/// A process running under the force-routed `Net::Allowlist` + `proxy_uds`
/// profile must:
///   (a) **FAIL** to TCP-connect any AF_INET address (`net_probe` to 1.1.1.1:443)
///   (b) **SUCCEED** connecting the proxy UDS (`uds_probe` to a locally bound socket)
///
/// If (a) passes (i.e. AF_INET is NOT denied by Seatbelt), this test fails
/// with a prominent message and Stage 4's macOS bring-up must select the
/// `MacosContainer` backend for net workers. Do NOT fake a pass.
#[test]
fn force_routed_profile_denies_inet_allows_uds() {
    if skip_if_no_seatbelt() {
        return;
    }

    let net_probe = fixture_binary("net_probe");
    let uds_probe = fixture_binary("uds_probe");

    for (name, bin) in [("net_probe", &net_probe), ("uds_probe", &uds_probe)] {
        if !bin.exists() {
            eprintln!(
                "[SKIP] {name} binary not built at {bin:?} — run `cargo build -p kastellan-sandbox` first"
            );
            return;
        }
    }

    // Bind a real UDS in /tmp so uds_probe has something to connect to.
    // Use a unique path per test run to avoid collisions.
    let uds_path = PathBuf::from(format!(
        "/tmp/kastellan_egress_probe_{}.sock",
        std::process::id()
    ));
    // Clean up any leftover from a previous crashed run.
    let _ = std::fs::remove_file(&uds_path);
    let _listener = UnixListener::bind(&uds_path)
        .expect("bind test UDS for gating probe");
    // Guard: remove the socket file on drop so we don't pollute /tmp.
    struct UdsGuard(PathBuf);
    impl Drop for UdsGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = UdsGuard(uds_path.clone());

    // Build the force-routed profile for a worker whose only allowed egress
    // is our test UDS. The profile is the unit-under-test here: we verified
    // its text in unit tests; now we verify Seatbelt ACTUALLY enforces it.
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["1.1.1.1:443".into()]),
        proxy_uds: Some(uds_path.clone()),
        cpu_ms: 5_000,
        ..SandboxPolicy::default()
    };

    let backend = MacosSeatbelt::new();

    // --- (a) inet MUST be denied ------------------------------------------
    // net_probe tries TcpStream::connect("1.1.1.1:443"); under the
    // force-routed profile the only network-outbound allowed is our UDS.
    let net_probe_str = net_probe.to_string_lossy().into_owned();
    let net_policy = SandboxPolicy {
        fs_read: vec![net_probe.clone()],
        ..policy.clone()
    };
    let mut net_child = backend
        .spawn_under_policy(&net_policy, &net_probe_str, &[])
        .expect("sandbox-exec should spawn net_probe");
    let net_status = net_child.wait().expect("wait net_probe");
    let net_stdout = read_to_string(&mut net_child.stdout);
    let net_stderr = read_to_string(&mut net_child.stderr);

    // The profile text for debugging if the assertion fires.
    let profile_text = build_profile(&policy);

    assert!(
        !net_status.success(),
        "\n\
         !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n\
         GATING PROBE FAILURE: Seatbelt does NOT deny AF_INET for the\n\
         force-routed profile on this Mac.\n\
         \n\
         Impact: egress force-routing via Seatbelt is NOT reliable here.\n\
         Stage 4 macOS bring-up MUST select MacosContainer for net workers\n\
         instead of relying on Seatbelt + proxy_uds for enforcement.\n\
         See ROADMAP:141 / Stage-4 open risk §1.\n\
         !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n\
         \n\
         net_probe status={net_status:?} stdout={net_stdout:?} stderr={net_stderr:?}\n\
         profile:\n{profile_text}"
    );

    // --- (b) UDS MUST be allowed ------------------------------------------
    // uds_probe takes the socket path as argv[1] and exits 0 on success.
    let uds_probe_str = uds_probe.to_string_lossy().into_owned();
    let uds_path_str = uds_path.to_string_lossy().into_owned();
    let uds_policy = SandboxPolicy {
        fs_read: vec![uds_probe.clone()],
        ..policy.clone()
    };
    let mut uds_child = backend
        .spawn_under_policy(&uds_policy, &uds_probe_str, &[uds_path_str.as_str()])
        .expect("sandbox-exec should spawn uds_probe");
    let uds_status = uds_child.wait().expect("wait uds_probe");
    let uds_stdout = read_to_string(&mut uds_child.stdout);
    let uds_stderr = read_to_string(&mut uds_child.stderr);

    assert!(
        uds_status.success(),
        "force-routed profile must ALLOW the proxy UDS; uds_probe failed.\n\
         uds_path={uds_path:?} status={uds_status:?} stdout={uds_stdout:?} stderr={uds_stderr:?}\n\
         profile:\n{profile_text}"
    );

    eprintln!("[PASS] force_routed_profile_denies_inet_allows_uds: AF_INET denied, proxy UDS allowed — Seatbelt primary path confirmed.");
}
