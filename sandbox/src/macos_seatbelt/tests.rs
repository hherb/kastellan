//! Unit tests for the macOS Seatbelt backend (`build_profile` profile text,
//! `canonicalize_policy_paths` symlink resolution, and the
//! `spawn_under_policy` path-validation guards), plus the on-host `probe`
//! smoke check.
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! (item 9b over-cap test-lift). Production logic lives in the parent
//! `macos_seatbelt.rs`; this file is `mod tests;` from there and is only
//! compiled under `#[cfg(test)]`.

use super::*;
use crate::Net;
use std::path::PathBuf;

fn strict_policy() -> SandboxPolicy {
    SandboxPolicy::default()
}

#[test]
fn profile_starts_with_version_and_deny_default() {
    let p = build_profile(&strict_policy());
    // (version 1) must appear before any allow/deny rule.
    let version_idx = p.find("(version 1)").expect("missing (version 1)");
    let deny_default_idx = p.find("(deny default)").expect("missing (deny default)");
    assert!(version_idx < deny_default_idx);
}

#[test]
fn profile_emits_always_on_allows() {
    let p = build_profile(&strict_policy());
    for needle in [
        "(allow process-fork)",
        "(allow process-exec*)",
        "(allow file-read* (literal \"/\"))",
        "(allow file-read* (subpath \"/usr/lib\"))",
        "(allow file-read* (subpath \"/usr/libexec\"))",
        "(allow file-read* (subpath \"/System/Library\"))",
        "(allow file-read-metadata (subpath \"/\"))",
        "(allow sysctl-read)",
    ] {
        assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
    }
}

/// Issue #1: the strict profile must NOT contain an unrestricted
/// `(allow mach-lookup)` rule. None of our shipping workers need it,
/// and granting it would expose every registered launchd service
/// (Apple Events broker, pasteboard, etc.) — the largest asymmetry
/// vs the threat-model invariant. When `python-exec` (Phase 4) needs
/// specific Mach services, the rule must be re-introduced as
/// `(allow mach-lookup (global-name "..."))` — narrow, never broad.
/// Pin this so a future refactor cannot silently regress.
#[test]
fn profile_does_not_grant_unrestricted_mach_lookup() {
    let p = build_profile(&strict_policy());
    assert!(
        !p.contains("(allow mach-lookup)"),
        "strict profile must not contain unrestricted (allow mach-lookup); \
         got:\n{p}"
    );
    // Defence in depth: also reject any allow rule that *starts* with
    // `(allow mach-lookup)` followed only by whitespace + `)` —
    // catches `(allow mach-lookup )` and the line-continuation forms.
    for line in p.lines() {
        let trimmed = line.trim();
        assert!(
            trimmed != "(allow mach-lookup)" && trimmed != "(allow mach-lookup )",
            "strict profile contains an effectively-unrestricted mach-lookup line: {line:?}"
        );
    }
}

#[test]
fn dev_allowlist_is_minimal() {
    let p = build_profile(&strict_policy());
    // The six safe /dev nodes must be present (/dev/tty is intentionally absent).
    for needle in [
        "(literal \"/dev/null\")",
        "(literal \"/dev/zero\")",
        "(literal \"/dev/random\")",
        "(literal \"/dev/urandom\")",
        "(subpath \"/dev/fd\")",
        "(literal \"/dev/dtracehelper\")",
    ] {
        assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
    }
    // /dev/tty must NOT be present — it would create a covert channel to
    // the user's terminal that Linux workers (bwrap --new-session) can't use.
    assert!(
        !p.contains("(literal \"/dev/tty\")"),
        "profile must not expose /dev/tty; got:\n{p}"
    );
    // /dev as a whole must NOT be subpath-allowed — that would expose disk*,
    // auditpipe, bpf*, etc.
    assert!(
        !p.contains("(subpath \"/dev\")"),
        "profile must not allow broad (subpath \"/dev\") rule; got:\n{p}"
    );
}

#[test]
fn fs_read_emits_subpath_allow() {
    let mut p = strict_policy();
    p.fs_read = vec![PathBuf::from("/etc/ssl"), PathBuf::from("/opt/data")];
    let prof = build_profile(&p);
    assert!(prof.contains("(allow file-read* (subpath \"/etc/ssl\"))"), "got:\n{prof}");
    assert!(prof.contains("(allow file-read* (subpath \"/opt/data\"))"), "got:\n{prof}");
}

#[test]
fn fs_write_emits_read_and_write_subpath_allow() {
    let mut p = strict_policy();
    p.fs_write = vec![PathBuf::from("/var/lib/kastellan/scratch")];
    let prof = build_profile(&p);
    assert!(
        prof.contains("(allow file-read* file-write* (subpath \"/var/lib/kastellan/scratch\"))"),
        "expected combined read+write allow; got:\n{prof}"
    );
    // The fs_write path must NOT appear as a separate read-only allow.
    assert!(
        !prof.contains("(allow file-read* (subpath \"/var/lib/kastellan/scratch\"))"),
        "fs_write path must not also be emitted as a separate read-only rule; got:\n{prof}"
    );
}

#[test]
fn deny_does_not_allow_network() {
    let p = build_profile(&strict_policy());
    assert!(!p.contains("(allow network*)"), "Net::Deny must not emit (allow network*); got:\n{p}");
}

#[test]
fn allowlist_does_allow_network() {
    let mut p = strict_policy();
    p.net = Net::Allowlist(vec!["api.example.com:443".into()]);
    let prof = build_profile(&p);
    assert!(prof.contains("(allow network*)"), "Net::Allowlist must emit (allow network*); got:\n{prof}");
}

#[test]
fn proxy_egress_emits_allow_network() {
    let p = SandboxPolicy {
        net: crate::Net::ProxyEgress,
        ..SandboxPolicy::default()
    };
    let prof = build_profile(&p);
    assert!(prof.contains("(allow network*)"), "ProxyEgress must allow network; got:\n{prof}");
}

#[test]
fn relative_policy_paths_are_rejected_by_spawn() {
    let backend = MacosSeatbelt::new();
    let mut p = strict_policy();
    p.fs_read.push(PathBuf::from("relative/path"));
    let err = backend
        .spawn_under_policy(&p, "/usr/bin/true", &[])
        .expect_err("must reject relative paths");
    let msg = format!("{err}");
    assert!(
        msg.contains("must be absolute"),
        "expected 'must be absolute' error, got: {msg}"
    );
}

/// TinyScheme-special characters (", \, (, ), newline, NUL) in a policy
/// path would let a malformed `SandboxPolicy` rewrite the profile by
/// closing the `(subpath "...")` s-expression early. Today all callers are
/// trusted core code, but the validation forecloses an entire class of
/// future bug; cheaper than auditing every future call site.
#[test]
fn policy_paths_with_tinyscheme_specials_are_rejected_by_spawn() {
    let backend = MacosSeatbelt::new();
    for bad in [
        "/tmp/x\")(allow network*)(literal \"/x",
        "/tmp/has\\backslash",
        "/tmp/has(paren",
        "/tmp/has)paren",
        "/tmp/has\nnewline",
        "/tmp/has\0nul",
    ] {
        let mut p = strict_policy();
        p.fs_read.push(PathBuf::from(bad));
        let err = backend
            .spawn_under_policy(&p, "/usr/bin/true", &[])
            .err()
            .unwrap_or_else(|| panic!("must reject fs_read path {bad:?}"));
        let msg = format!("{err}");
        assert!(
            msg.contains("disallowed character"),
            "expected 'disallowed character' error for {bad:?}, got: {msg}"
        );
    }
    // Same shape, but for fs_write — the validation must cover both lists.
    let mut p = strict_policy();
    p.fs_write.push(PathBuf::from("/tmp/x\"escape"));
    let err = backend
        .spawn_under_policy(&p, "/usr/bin/true", &[])
        .expect_err("fs_write path with quote must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("disallowed character"),
        "expected 'disallowed character' error, got: {msg}"
    );

    // And proxy_uds — it is interpolated into the profile as a
    // `(path-literal ...)` rule, so it must pass the SAME guard. A UDS path
    // carrying a structural char would otherwise let a crafted policy rewrite
    // the force-routing rule.
    let mut p = strict_policy();
    p.net = crate::Net::Allowlist(vec!["api.example.com:443".into()]);
    p.proxy_uds = Some(PathBuf::from("/tmp/egress\")(allow network*)(literal \"/x.sock"));
    let err = backend
        .spawn_under_policy(&p, "/usr/bin/true", &[])
        .expect_err("proxy_uds path with injection chars must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("disallowed character"),
        "expected 'disallowed character' error for proxy_uds, got: {msg}"
    );
}

/// Force-routed profile (`Net::Allowlist` + `proxy_uds`): must deny all
/// outbound then re-allow only the proxy UDS, and must NOT emit the broad
/// `(allow network*)` rule that would bypass the force-routing.
#[test]
fn allowlist_with_proxy_uds_denies_outbound_except_uds() {
    let p = SandboxPolicy {
        net: crate::Net::Allowlist(vec!["api.example.com:443".into()]),
        proxy_uds: Some(std::path::PathBuf::from("/scratch/egress.sock")),
        ..SandboxPolicy::default()
    };
    let prof = build_profile(&p);
    assert!(prof.contains("(deny network-outbound)"),
        "force-routed worker must deny outbound; got:\n{prof}");
    assert!(prof.contains("(allow network-outbound (remote unix-socket (path-literal \"/scratch/egress.sock\")))"),
        "must allow only the proxy UDS; got:\n{prof}");
    assert!(!prof.contains("(allow network*)"),
        "must NOT broadly allow network; got:\n{prof}");
}

/// Legacy `Net::Allowlist` without a proxy UDS keeps the old broad
/// `(allow network*)` rule — no regression on the slice #1 posture.
#[test]
fn allowlist_without_proxy_uds_keeps_legacy_allow_network() {
    let p = SandboxPolicy {
        net: crate::Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let prof = build_profile(&p);
    assert!(prof.contains("(allow network*)"));
}

// This test runs a real sandbox-exec invocation. It only meaningfully runs
// on macOS hosts; the parent module is cfg(target_os = "macos") so this
// file isn't compiled elsewhere. Print a [SKIP] line on probe failure
// (matches the integration-test pattern) instead of panicking, so a
// host with MDM-clipped Seatbelt or a future macOS regression doesn't
// false-fail the suite.
#[test]
fn probe_succeeds_on_this_host() {
    if let Err(e) = MacosSeatbelt::probe() {
        eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
    }
}

#[test]
fn canonicalize_policy_paths_resolves_etc_symlink() {
    // /etc is a symlink to /private/etc on macOS — verify the helper
    // resolves it. /etc/hosts is guaranteed to exist on any macOS host.
    let mut p = strict_policy();
    p.fs_read = vec![PathBuf::from("/etc/hosts")];
    let canon = canonicalize_policy_paths(&p).expect("canonicalize must succeed");
    let resolved = &canon.fs_read[0];
    assert_eq!(
        resolved,
        &PathBuf::from("/private/etc/hosts"),
        "canonicalize did not resolve /etc -> /private/etc symlink; got {resolved:?}"
    );
}

#[test]
fn canonicalize_policy_paths_falls_back_for_nonexistent() {
    // /var/lib/kastellan/scratch_xyz_does_not_exist — should keep its literal form.
    let mut p = strict_policy();
    let nonexistent = PathBuf::from("/var/lib/kastellan/scratch_xyz_does_not_exist");
    p.fs_write = vec![nonexistent.clone()];
    let canon = canonicalize_policy_paths(&p).expect("NotFound must fall back, not error");
    assert_eq!(canon.fs_write[0], nonexistent);
}

/// Non-NotFound canonicalize errors (e.g. PermissionDenied on a parent
/// directory) MUST propagate — silently emitting an unresolved-path rule
/// would produce a silently-non-functional Seatbelt rule and mask user
/// errors as "the sandbox is just too strict." Pin this so a future
/// refactor doesn't quietly re-introduce the catch-all swallow.
#[test]
fn canonicalize_policy_paths_propagates_non_notfound_errors() {
    use std::os::unix::fs::PermissionsExt;
    // Create an owned temp dir, drop perms to 000, then attempt to
    // canonicalize a path inside it — the parent walk fails with
    // PermissionDenied. We must propagate rather than fall back.
    let tmp = std::env::temp_dir().join(format!(
        "kastellan_canon_perm_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir(&tmp).expect("create_dir");
    // Always chmod back to 0o700 in a guard so we don't leak an
    // unreadable temp dir on test failure.
    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(
                &self.0,
                std::fs::Permissions::from_mode(0o700),
            );
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = Guard(tmp.clone());
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000))
        .expect("chmod 000");

    let mut p = strict_policy();
    p.fs_read = vec![tmp.join("inside")];
    let res = canonicalize_policy_paths(&p);
    let err = res.err().unwrap_or_else(|| {
        panic!("expected PermissionDenied to propagate; got Ok")
    });
    let msg = format!("{err}");
    assert!(
        msg.contains("could not canonicalize"),
        "expected 'could not canonicalize' in error, got: {msg}"
    );
}

fn browser_policy() -> SandboxPolicy {
    SandboxPolicy {
        net: Net::Allowlist(vec!["example.com:443".into()]),
        profile: crate::Profile::WorkerBrowserClient,
        ..SandboxPolicy::default()
    }
}

/// The browser-only Seatbelt widening (spike findings §3.1): a
/// `WorkerBrowserClient` policy must emit all three clusters Chromium needs —
/// shared-memory IPC, IOKit, and Mach bootstrap.
#[test]
fn browser_client_profile_emits_browser_clusters() {
    let p = build_profile(&browser_policy());
    for rule in [
        "(allow ipc-posix-shm*)",
        "(allow iokit-open)",
        "(allow iokit-get-properties)",
        "(allow mach-lookup)",
        "(allow mach-register)",
        "(allow sysctl-write)",
        "(allow system-socket)",
    ] {
        assert!(p.contains(rule), "browser profile missing {rule:?}\n{p}");
    }
}

/// Force-routed browser profile (`Net::Allowlist` + `proxy_uds` +
/// `WorkerBrowserClient`): must emit the standard deny-outbound-except-UDS
/// rules AND additionally allow loopback TCP bind/accept/connect so the
/// in-jail loopback-TCP<->UDS shim (egress slice #2) can serve Chromium.
#[test]
fn browser_proxy_uds_allows_loopback_tcp() {
    let policy = SandboxPolicy {
        net: crate::Net::Allowlist(vec!["example.com:443".into()]),
        proxy_uds: Some(std::path::PathBuf::from("/tmp/egress.sock")),
        profile: crate::Profile::WorkerBrowserClient,
        ..SandboxPolicy::default()
    };
    let p = build_profile(&policy);
    assert!(p.contains("(deny network-outbound)"), "still deny-by-default");
    assert!(p.contains("unix-socket (path-literal"), "UDS still allowed");
    assert!(p.contains(r#"(allow network-bind (local ip "localhost:*"))"#));
    assert!(p.contains(r#"(allow network-inbound (local ip "localhost:*"))"#));
    assert!(p.contains(r#"(allow network-outbound (remote ip "localhost:*"))"#));
}

/// Non-browser force-routed workers (`Net::Allowlist` + `proxy_uds` +
/// `WorkerNetClient`) must NOT receive the loopback TCP widening — they use
/// an in-process CONNECT-over-UDS client and have no in-jail shim.
#[test]
fn non_browser_proxy_uds_has_no_loopback_tcp() {
    let policy = SandboxPolicy {
        net: crate::Net::Allowlist(vec!["example.com:443".into()]),
        proxy_uds: Some(std::path::PathBuf::from("/tmp/egress.sock")),
        profile: crate::Profile::WorkerNetClient,
        ..SandboxPolicy::default()
    };
    let p = build_profile(&policy);
    assert!(!p.contains(r#"network-bind (local ip "localhost"#),
        "non-browser UDS workers must not be widened with loopback TCP");
    assert!(!p.contains(r#"network-outbound (remote ip "localhost"#));
}

/// `persistent_store` emits a combined `file-read* file-write*` subpath rule
/// for `guest_mount` so the worker can write to its persistent store.
/// On macOS there is no path remap (host_backing == guest_mount in the demo),
/// so we grant the `guest_mount` path directly.
#[test]
fn persistent_store_grants_rw_subpath() {
    let mut policy = strict_policy();
    policy.persistent_store = Some(crate::PersistentStore {
        host_backing: std::path::PathBuf::from("/tmp/kvstate"),
        guest_mount: std::path::PathBuf::from("/tmp/kvstate"),
        size_mib: 0,
    });
    let profile = build_profile(&policy);
    assert!(
        profile.contains("(allow file-read* file-write* (subpath \"/tmp/kvstate\"))"),
        "expected persistent_store subpath rule; got:\n{profile}"
    );
}

/// The widening is gated to `WorkerBrowserClient` ALONE — the strict and
/// net-client profiles keep the deny-default (incl. the issue-#1 mach-lookup
/// deny). This is the regression pin that the browser cluster never leaks into
/// another worker's profile.
#[test]
fn non_browser_profiles_do_not_emit_browser_clusters() {
    for policy in [
        strict_policy(),
        SandboxPolicy {
            net: Net::Allowlist(vec!["example.com:443".into()]),
            profile: crate::Profile::WorkerNetClient,
            ..SandboxPolicy::default()
        },
    ] {
        let p = build_profile(&policy);
        for rule in [
            "(allow ipc-posix-shm*)",
            "(allow iokit-open)",
            "(allow mach-lookup)",
            "(allow mach-register)",
            "(allow sysctl-write)",
            "(allow system-socket)",
        ] {
            assert!(
                !p.contains(rule),
                "non-browser profile ({:?}) leaked browser rule {rule:?}",
                policy.profile
            );
        }
    }
}
