//! Unit tests for the parent [`super`] seccomp module.
//!
//! Lifted out of `seccomp_lock.rs` (sibling test-module split,
//! 2026-07-06) to keep the production file under the 500-LOC cap. The
//! `use super::*` below resolves to the parent `seccomp_lock` module,
//! reaching [`super::Profile`], the BPF builders, and every allow-list
//! table the parent re-exports from its `allow_lists` sibling. Test
//! bodies are verbatim moves.

use super::*;

#[test]
fn profile_parse_recognises_known_values() {
    assert_eq!(Profile::parse("strict").unwrap(), Some(Profile::Strict));
    assert_eq!(
        Profile::parse("net_client").unwrap(),
        Some(Profile::NetClient)
    );
    assert_eq!(
        Profile::parse("browser_client").unwrap(),
        Some(Profile::BrowserClient)
    );
    assert_eq!(Profile::parse("none").unwrap(), None);
    assert_eq!(Profile::parse("").unwrap(), None);
}

#[test]
fn profile_parse_rejects_unknown() {
    assert!(Profile::parse("garbage").is_err());
}

#[test]
fn build_bpf_strict_succeeds() {
    // Just verifies the rule construction + BPF compilation works on
    // the test host's arch. Doesn't actually load the filter (which
    // would poison subsequent tests).
    let bpf = build_bpf(Profile::Strict).expect("strict bpf must build");
    assert!(!bpf.is_empty(), "expected non-empty BPF program");
}

#[test]
fn build_bpf_net_client_succeeds() {
    let bpf = build_bpf(Profile::NetClient).expect("net_client bpf must build");
    assert!(!bpf.is_empty(), "expected non-empty BPF program");
}

#[test]
fn unshare_is_not_in_allow_list() {
    // The most important syscall in our threat model — escape into a
    // fresh user namespace — must NOT appear in any profile's
    // allow-list. If this regresses, the worker can re-enter
    // unshare(CLONE_NEWUSER) and bypass the namespace boundary.
    for profile in [Profile::Strict, Profile::NetClient] {
        let allow = allow_list_for(profile);
        assert!(
            !allow.contains(&libc::SYS_unshare),
            "unshare must never be allow-listed (profile {profile:?})"
        );
        assert!(
            !allow.contains(&libc::SYS_mount),
            "mount must never be allow-listed (profile {profile:?})"
        );
        assert!(
            !allow.contains(&libc::SYS_ptrace),
            "ptrace must never be allow-listed (profile {profile:?})"
        );
        assert!(
            !allow.contains(&libc::SYS_bpf),
            "bpf must never be allow-listed (profile {profile:?})"
        );
    }
}

#[test]
fn socket_is_only_in_net_client_profile() {
    // The hard line between Strict and NetClient: socket() and the
    // BSD-socket family must be allowed under NetClient and killed
    // under Strict. This is the test that proves the two profiles
    // differ — if it ever regresses, NetClient and Strict have
    // collapsed back into the same set.
    let strict = allow_list_for(Profile::Strict);
    let net_client = allow_list_for(Profile::NetClient);

    assert!(
        !strict.contains(&libc::SYS_socket),
        "Strict must not allow socket()"
    );
    assert!(
        net_client.contains(&libc::SYS_socket),
        "NetClient must allow socket()"
    );

    // Sanity: the difference is exactly NET_CLIENT_ADDITIONS.
    for nr in NET_CLIENT_ADDITIONS {
        assert!(
            !strict.contains(nr),
            "syscall {nr} present in Strict but should be NetClient-only"
        );
        assert!(
            net_client.contains(nr),
            "syscall {nr} missing from NetClient"
        );
    }
}

#[test]
fn essentials_are_in_base_allow_list() {
    // Smoke test: a handful of syscalls that *every* worker hits
    // during normal operation must be in the base list. If one of
    // these regresses, the worker dies in a confusing way (SIGSYS at
    // startup with no obvious cause) — surface the failure here
    // instead.
    for nr in [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_openat,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_brk,
        libc::SYS_futex,
        libc::SYS_clone3,
        libc::SYS_execve,
        libc::SYS_wait4,
        libc::SYS_exit_group,
        libc::SYS_rt_sigreturn,
    ] {
        assert!(
            BASE_ALLOW.contains(&nr),
            "essential syscall {nr} missing from BASE_ALLOW"
        );
    }
}

#[test]
fn build_bpf_browser_client_succeeds() {
    let bpf = build_bpf(Profile::BrowserClient).expect("browser_client bpf must build");
    assert!(!bpf.is_empty(), "browser_client filter must emit instructions");
}

#[test]
fn io_uring_eperm_filter_builds() {
    let bpf = build_io_uring_eperm_bpf().expect("io_uring EPERM filter must build");
    assert!(!bpf.is_empty(), "io_uring EPERM filter must emit instructions");
}

#[test]
fn browser_client_is_a_superset_of_net_client() {
    // BrowserClient must allow everything NetClient does (it's net_client +
    // the browser additions), so a browser worker is never *more* restricted
    // on the socket family than the egress proxy.
    let net_client = allow_list_for(Profile::NetClient);
    let browser = allow_list_for(Profile::BrowserClient);
    for nr in net_client {
        assert!(
            browser.contains(&nr),
            "BrowserClient missing NetClient syscall {nr}"
        );
    }
    // socket() in particular (the net/strict dividing line).
    assert!(browser.contains(&libc::SYS_socket));
}

#[test]
fn browser_client_includes_the_spike_additions() {
    let browser = allow_list_for(Profile::BrowserClient);
    let strict = allow_list_for(Profile::Strict);
    for nr in BROWSER_CLIENT_ADDITIONS {
        assert!(
            browser.contains(nr),
            "BrowserClient missing spike syscall {nr}"
        );
        assert!(
            !strict.contains(nr),
            "browser syscall {nr} leaked into Strict"
        );
    }
}

#[test]
fn io_uring_is_allowed_in_the_main_filter_but_eperm_listed_separately() {
    // io_uring MUST be in the main allow-list (so the main filter returns
    // Allow, not Kill) — the second filter then downgrades it to EPERM.
    // Neither Strict nor NetClient list io_uring at all.
    let browser = allow_list_for(Profile::BrowserClient);
    for nr in BROWSER_IO_URING {
        assert!(
            browser.contains(nr),
            "io_uring {nr} must be in the BrowserClient main allow-list"
        );
        assert!(
            !allow_list_for(Profile::NetClient).contains(nr),
            "io_uring {nr} must NOT be in NetClient"
        );
        assert!(
            !allow_list_for(Profile::Strict).contains(nr),
            "io_uring {nr} must NOT be in Strict"
        );
    }
}

#[test]
fn profile_parse_recognises_ml_client() {
    assert_eq!(Profile::parse("ml_client").unwrap(), Some(Profile::MlClient));
}

#[test]
fn build_bpf_ml_client_succeeds() {
    let bpf = build_bpf(Profile::MlClient).expect("ml_client bpf must build");
    assert!(!bpf.is_empty(), "ml_client filter must emit instructions");
}

#[test]
fn ml_client_is_a_superset_of_net_client() {
    // ml_client = net_client + ML additions, so it must allow everything
    // net_client does (notably the socket family torch needs even offline).
    let net_client = allow_list_for(Profile::NetClient);
    let ml = allow_list_for(Profile::MlClient);
    for nr in net_client {
        assert!(ml.contains(&nr), "MlClient missing NetClient syscall {nr}");
    }
    assert!(ml.contains(&libc::SYS_socket), "MlClient must allow socket()");
}

#[test]
fn ml_client_excludes_escape_primitives() {
    // The threat-model invariant: even a torch-tier worker must never be able
    // to escape its namespace / inspect other processes / load BPF.
    let ml = allow_list_for(Profile::MlClient);
    for nr in [
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
    ] {
        assert!(!ml.contains(&nr), "MlClient must never allow {nr}");
    }
}

#[test]
fn ml_client_includes_enumerated_numa_additions() {
    // The DGX-enumerated torch additions (NUMA memory-policy syscalls) must
    // be present in MlClient and ML-specific — i.e. NOT already granted by
    // the Strict base (otherwise they'd belong in BASE_ALLOW, not here).
    let ml = allow_list_for(Profile::MlClient);
    let strict = allow_list_for(Profile::Strict);
    assert!(
        !ML_CLIENT_ADDITIONS.is_empty(),
        "ML_CLIENT_ADDITIONS was populated by the DGX enumeration"
    );
    for nr in ML_CLIENT_ADDITIONS {
        assert!(ml.contains(nr), "MlClient missing enumerated syscall {nr}");
        assert!(
            !strict.contains(nr),
            "enumerated syscall {nr} is already in Strict — move it to BASE_ALLOW"
        );
    }
}

#[test]
fn profile_parse_recognises_matrix_client() {
    assert_eq!(
        Profile::parse("matrix_client").unwrap(),
        Some(Profile::MatrixClient)
    );
}

#[test]
fn build_bpf_matrix_client_succeeds() {
    let bpf = build_bpf(Profile::MatrixClient).expect("matrix_client bpf must build");
    assert!(!bpf.is_empty(), "matrix_client filter must emit instructions");
}

#[test]
fn matrix_client_is_a_superset_of_net_client() {
    // matrix_client = net_client + MATRIX additions, so it must allow
    // everything net_client does (the socket family matrix-sdk needs for
    // homeserver I/O + reconnects).
    let net_client = allow_list_for(Profile::NetClient);
    let mx = allow_list_for(Profile::MatrixClient);
    for nr in net_client {
        assert!(mx.contains(&nr), "MatrixClient missing NetClient syscall {nr}");
    }
    assert!(mx.contains(&libc::SYS_socket), "MatrixClient must allow socket()");
}

#[test]
fn matrix_client_excludes_escape_primitives() {
    // Threat-model invariant: the worker with the largest external attack
    // surface must never be able to escape its namespace / inspect other
    // processes / load BPF.
    let mx = allow_list_for(Profile::MatrixClient);
    for nr in [
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
    ] {
        assert!(!mx.contains(&nr), "MatrixClient must never allow {nr}");
    }
}

#[test]
fn matrix_client_includes_enumerated_additions() {
    // The DGX-enumerated matrix-sdk additions (SQLite ftruncate today) must
    // be present in MatrixClient and matrix-specific — i.e. NOT already
    // granted by the Strict/net_client base (else they'd belong there).
    let mx = allow_list_for(Profile::MatrixClient);
    let net_client = allow_list_for(Profile::NetClient);
    assert!(
        !MATRIX_CLIENT_ADDITIONS.is_empty(),
        "MATRIX_CLIENT_ADDITIONS was populated by the DGX enumeration"
    );
    for nr in MATRIX_CLIENT_ADDITIONS {
        assert!(mx.contains(nr), "MatrixClient missing enumerated syscall {nr}");
        assert!(
            !net_client.contains(nr),
            "enumerated syscall {nr} is already in net_client — drop it from MATRIX_CLIENT_ADDITIONS"
        );
    }
}
