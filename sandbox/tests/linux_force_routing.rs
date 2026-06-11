//! Gating kernel-barrier proof for egress slice #2 (Linux): a worker under the
//! force-routed `Net::Allowlist` + `proxy_uds` policy is placed in a **private
//! network namespace** whose only egress is the bound proxy UDS. It must have
//! **no direct route** off the allowlist — a DNS lookup (which needs the
//! network) MUST fail, even though the host is nominally "allowlisted" (the
//! allowlist is enforced by the proxy, never by handing the worker a socket).
//!
//! This is the Linux twin of `sandbox/tests/seatbelt_uds_probe.rs` (the macOS
//! Seatbelt probe). It actually invokes `bwrap`, so it runs only on Linux with
//! `bwrap` on `$PATH` + unprivileged user-ns enabled — **run natively on the
//! DGX over WireGuard SSH** (the in-band acceptance gate, ROADMAP:141). It
//! `[SKIP]`s cleanly otherwise.
//!
//! It also exercises the one bwrap concern the spec flags (#243 / host↔jail
//! path identity): the proxy UDS is `--bind`-mounted into the jail at an
//! identical path. If the bind fails the spawn errors *before* `getent` runs,
//! which the test distinguishes from the netns barrier by binding a real UDS up
//! front.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use kastellan_sandbox::{linux_bwrap::LinuxBwrap, Net, SandboxBackend, SandboxPolicy};

/// Skip if this host's kernel won't let us create an unprivileged user
/// namespace (Ubuntu 24.04+ needs the bwrap AppArmor profile — see
/// `scripts/linux/install-bwrap-apparmor-profile.sh`).
fn skip_if_no_userns() -> bool {
    match LinuxBwrap::probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
            true
        }
    }
}

fn read_to_string(handle: &mut Option<impl Read>) -> String {
    let mut s = String::new();
    if let Some(h) = handle.as_mut() {
        let _ = h.read_to_string(&mut s);
    }
    s
}

/// The gating kernel-barrier proof: a force-routed `Net::Allowlist` worker has
/// **no direct network route**. Mirrors `linux_smoke::net_is_unreachable_under_deny`
/// but for `Net::Allowlist` **with** `proxy_uds` set (the slice-#2 force-routed
/// shape) — which previously shared the host netns (`--share-net`) and so could
/// resolve DNS. Force-routing must close that: private netns, UDS-only egress.
#[test]
fn force_routed_allowlist_worker_has_no_direct_route() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();

    // Bind a REAL UDS up front so the worker's `--bind <uds> <uds>` succeeds —
    // otherwise the spawn would fail at the bind step and `getent` would never
    // run, masking the netns barrier behind an unrelated failure.
    let scratch = std::env::temp_dir().join(format!("kastellan-force-route-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let uds = scratch.join("egress.sock");
    let _ = std::fs::remove_file(&uds);
    let _listener = UnixListener::bind(&uds).expect("bind the stand-in proxy UDS");

    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["example.com:443".into()]),
        // proxy_uds set → bwrap uses a private netns (NO --share-net) and binds
        // the UDS in. This is the force-routed shape Stage 4 spawns workers with.
        proxy_uds: Some(uds.clone()),
        // The scratch dir holds the bound UDS; bwrap binds the socket itself rw.
        fs_write: vec![scratch.clone()],
        cpu_ms: 5_000,
        ..SandboxPolicy::default()
    };

    // `getent hosts example.com` performs a DNS lookup, which requires a
    // network route. In the worker's private netns there is none — the only
    // egress is the proxy UDS — so this MUST fail.
    let mut child = backend
        .spawn_under_policy(&policy, "/usr/bin/getent", &["hosts", "example.com"])
        .expect("bwrap should spawn getent (UDS bind must succeed)");
    let status = child.wait().expect("wait getent");
    let stderr = read_to_string(&mut child.stderr);

    let _ = std::fs::remove_file(&uds);
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        !status.success(),
        "FORCE-ROUTING LEAK: a force-routed Net::Allowlist worker reached DNS \
         directly — the private-netns barrier is not effective. The worker must \
         have NO route except the proxy UDS. status={status:?} stderr={stderr:?}"
    );
}

/// Contrast pin: the *legacy* `Net::Allowlist` (no `proxy_uds`) still shares the
/// host network namespace (`--share-net`), so DNS resolves. This guards against
/// a refactor that accidentally force-routes EVERY allowlist worker (breaking
/// the slice-#1 posture for workers that haven't opted into force-routing).
/// Requires real network; `#[ignore]` by default (run on the DGX with egress).
#[test]
#[ignore = "real network: legacy Net::Allowlist shares host netns and resolves DNS"]
fn legacy_allowlist_without_proxy_uds_can_resolve() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["example.com:443".into()]),
        proxy_uds: None, // legacy: --share-net
        fs_read: vec![
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        cpu_ms: 5_000,
        ..SandboxPolicy::default()
    };
    let mut child = backend
        .spawn_under_policy(&policy, "/usr/bin/getent", &["hosts", "example.com"])
        .expect("bwrap should spawn getent");
    let status = child.wait().expect("wait getent");
    assert!(
        status.success(),
        "legacy Net::Allowlist (no proxy_uds) should keep host-netns DNS; \
         stderr={}",
        read_to_string(&mut child.stderr)
    );
}
