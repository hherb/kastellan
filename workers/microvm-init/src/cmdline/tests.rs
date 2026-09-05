//! Unit tests for the pure cmdline parsers/types in [`super`]. Moved verbatim
//! from the former single `main.rs` `mod tests` during the Item 9b prod-split
//! (2026-07-06); `super` now resolves to `crate::cmdline` (where every item
//! under test lives) rather than the crate root, so the bodies are unchanged.

use super::*;
#[test]
fn vsock_listen_addr_uses_any_cid_and_worker_port() {
    // Guest listens on VMADDR_CID_ANY:1024. Assert the helper builds the
    // right (cid, port) pair.
    assert_eq!(vsock_listen_cid_port(), (0xffffffff, 1024));
}

#[test]
fn parse_worker_cmdline_decodes_fixture() {
    // Same hex the sandbox build_launch_plan_appends_worker_token fixture emits.
    let hex = "2f7573722f6c6f63616c2f62696e2f6b617374656c6c616e2d776f726b65722d7765622d6665746368";
    let cmdline = format!("console=ttyS0 kastellan.worker={hex} panic=1");
    assert_eq!(
        super::parse_worker_cmdline(&cmdline),
        Some("/usr/local/bin/kastellan-worker-web-fetch".to_string())
    );
}

#[test]
fn parse_worker_cmdline_missing_or_bad_is_none() {
    assert_eq!(super::parse_worker_cmdline("console=ttyS0 panic=1"), None);
    assert_eq!(super::parse_worker_cmdline("kastellan.worker=zz"), None); // bad hex
}

#[test]
fn parse_worker_args_cmdline_decodes_fixture() {
    // Cross-crate sync guard: `kastellan-sandbox`'s encode_worker_args_cmdline
    // emits this exact token for args ["/bin/x", "y"] — each arg hex-encoded
    // independently, joined with ','. Keep this fixture identical in both
    // crates' tests. "/bin/x" = 2f62696e2f78, "y" = 79.
    let cmdline = "console=ttyS0 kastellan.worker.args=2f62696e2f78,79 panic=1";
    assert_eq!(
        super::parse_worker_args_cmdline(cmdline),
        vec!["/bin/x".to_string(), "y".to_string()]
    );
}

#[test]
fn parse_worker_args_cmdline_missing_token_is_empty() {
    // No token → no extra args (the common case: every lockdown_shim:None
    // worker forwards just `program`).
    assert!(super::parse_worker_args_cmdline("console=ttyS0 panic=1").is_empty());
}

#[test]
fn parse_worker_args_cmdline_malformed_is_empty() {
    // Any malformed component fails the WHOLE list closed (never a partial,
    // positionally-shifted argv that would misfeed the lockdown-exec shim).
    assert!(super::parse_worker_args_cmdline("kastellan.worker.args=zz").is_empty());
    assert!(super::parse_worker_args_cmdline("kastellan.worker.args=2f62,zz").is_empty());
    // An empty token decodes to no args (treated as "no extra args").
    assert!(super::parse_worker_args_cmdline("kastellan.worker.args=").is_empty());
}

#[test]
fn parse_env_cmdline_decodes_host_fixture() {
    // Cross-crate sync guard: `kastellan-sandbox`'s `hex_encode` emits this
    // exact hex for env [("A","1"),("B","2")] (block "A=1\nB=2"). Keep this
    // fixture identical in both crates' tests.
    let cmdline = "console=ttyS0 panic=1 kastellan.env=413d310a423d32";
    assert_eq!(
        parse_env_cmdline(cmdline).unwrap(),
        vec![("A".to_string(), "1".to_string()), ("B".to_string(), "2".to_string())]
    );
}

#[test]
fn parse_env_cmdline_missing_token_is_empty() {
    assert!(parse_env_cmdline("console=ttyS0 panic=1").unwrap().is_empty());
}

#[test]
fn parse_env_cmdline_malformed_hex_is_an_error() {
    // Odd length and non-hex are ERRORS (audit 2026-09-02, F2): an empty env
    // would boot a worker with its lockdown missing, which is the one thing
    // "fail-safe" must not mean here.
    assert!(parse_env_cmdline("kastellan.env=abc").is_err());
    assert!(parse_env_cmdline("kastellan.env=zz").is_err());
    // Valid hex that is not UTF-8 is an error too.
    assert!(parse_env_cmdline("kastellan.env=ff").is_err());
}

#[test]
fn parse_env_cmdline_value_may_contain_equals() {
    // Split on the FIRST '=' so a JSON-ish value survives. Block `K=["a=b"]`
    // = bytes 4b 3d 5b 22 61 3d 62 22 5d → one whitespace-free token.
    let cmdline = "console=ttyS0 kastellan.env=4b3d5b22613d62225d";
    assert_eq!(
        parse_env_cmdline(cmdline).unwrap(),
        vec![("K".to_string(), "[\"a=b\"]".to_string())]
    );
}

#[test]
fn hex_decode_rejects_odd_and_non_hex() {
    assert_eq!(hex_decode("abc"), None);
    assert_eq!(hex_decode("zz"), None);
    assert_eq!(hex_decode("00ff"), Some(vec![0x00, 0xff]));
}

#[test]
fn parse_mount_manifest_decodes_ro_fixture() {
    // Cross-crate sync guard: kastellan-sandbox's encoder emits this exact hex
    // for RoShare{sources:[/opt/a], guest_dev:/dev/vdb}. Block "ro\t/dev/vdb\t/opt/a".
    let cmdline = "console=ttyS0 kastellan.mounts=726f092f6465762f766462092f6f70742f61";
    let m = parse_mount_manifest(cmdline);
    let ro = m.ro.expect("ro mount");
    assert_eq!(ro.dev, "/dev/vdb");
    assert_eq!(ro.targets, vec!["/opt/a".to_string()]);
    assert!(m.rw.is_empty());
}

#[test]
fn parse_mount_manifest_decodes_ro_and_rw() {
    // Block "ro\t/dev/vdb\t/opt/a\nrw\t/dev/vdc\t/tmp/s".
    // Build the hex from the bytes to avoid a hand-typo; assert structure.
    let block = "ro\t/dev/vdb\t/opt/a\nrw\t/dev/vdc\t/tmp/s";
    let hex: String = block.bytes().map(|b| format!("{b:02x}")).collect();
    let cmdline = format!("console=ttyS0 kastellan.mounts={hex}");
    let m = parse_mount_manifest(&cmdline);
    assert_eq!(m.ro.unwrap().dev, "/dev/vdb");
    assert_eq!(m.rw.len(), 1);
    assert_eq!(m.rw[0].dev, "/dev/vdc");
    assert_eq!(m.rw[0].mountpoint, "/tmp/s");
}

#[test]
fn parse_mount_manifest_missing_or_garbled_is_empty() {
    let m = parse_mount_manifest("console=ttyS0 panic=1");
    assert!(m.ro.is_none() && m.rw.is_empty());
    let bad = parse_mount_manifest("kastellan.mounts=zz");
    assert!(bad.ro.is_none() && bad.rw.is_empty());
}

#[test]
fn parse_mount_manifest_decodes_two_rw_lines() {
    // Slice 5b-2: a scratch drive + a persistent drive both appear as `rw`
    // lines. The guest must mount EVERY rw entry, not just the first.
    let block = "rw\t/dev/vdc\t/tmp\nrw\t/dev/vdd\t/data";
    let hex: String = block.bytes().map(|b| format!("{b:02x}")).collect();
    let cmdline = format!("console=ttyS0 kastellan.mounts={hex}");
    let m = parse_mount_manifest(&cmdline);
    assert!(m.ro.is_none());
    assert_eq!(m.rw.len(), 2);
    assert_eq!(m.rw[0].dev, "/dev/vdc");
    assert_eq!(m.rw[0].mountpoint, "/tmp");
    assert_eq!(m.rw[1].dev, "/dev/vdd");
    assert_eq!(m.rw[1].mountpoint, "/data");
}

#[test]
fn anchor_of_skips_tmp_and_takes_top_level() {
    assert_eq!(anchor_of("/opt/venv/lib"), Some("/opt".to_string()));
    assert_eq!(anchor_of("/work/scratch"), Some("/work".to_string()));
    // /tmp is already a writable tmpfs → no anchor needed.
    assert_eq!(anchor_of("/tmp/x"), None);
    assert_eq!(anchor_of("/"), None);
}

#[test]
fn parse_egress_config_reads_tokens() {
    assert_eq!(parse_egress_config("console=ttyS0 panic=1"), EgressConfig::default());
    assert_eq!(
        parse_egress_config("console=ttyS0 kastellan.egress=1"),
        EgressConfig { enabled: true, selftest: false }
    );
    assert_eq!(
        parse_egress_config("kastellan.egress=1 kastellan.egress.selftest=1"),
        EgressConfig { enabled: true, selftest: true }
    );
}

#[test]
fn parse_broker_config_enabled_from_token() {
    let c = parse_broker_config("console=ttyS0 kastellan.broker=1 kastellan.egress=1");
    assert!(c.enabled, "kastellan.broker=1 must enable the broker channel");
}

#[test]
fn parse_broker_config_disabled_when_token_absent() {
    let c = parse_broker_config("console=ttyS0 kastellan.egress=1");
    assert!(!c.enabled, "no kastellan.broker token => disabled");
}

#[test]
fn bind_prep_directory_source() {
    assert_eq!(super::bind_prep(true, false), super::BindPrep::Dir);
}

#[test]
fn bind_prep_file_source() {
    assert_eq!(super::bind_prep(false, true), super::BindPrep::File);
}

#[test]
fn bind_prep_missing_source_skips() {
    // Neither dir nor file (missing / socket / fifo) → skip the bind entirely.
    assert_eq!(super::bind_prep(false, false), super::BindPrep::Skip);
}

#[test]
fn pack_ifname_lo_is_nul_padded() {
    let n = super::pack_ifname("lo");
    assert_eq!(n[0], b'l' as libc::c_char);
    assert_eq!(n[1], b'o' as libc::c_char);
    assert_eq!(n[2], 0);
    assert_eq!(n[15], 0);
}

#[test]
fn pack_ifname_truncates_to_15_and_nul_terminates() {
    // 20-char name → 15 bytes kept, index 15 stays NUL.
    let n = super::pack_ifname("0123456789abcdefGHIJ");
    assert_eq!(n[14], b'e' as libc::c_char); // 15th kept char (index 14)
    assert_eq!(n[15], 0);
}

/// Build a `kastellan.mounts=` token from the plaintext manifest.
///
/// Fixtures go through this rather than being pasted as hex, because a
/// **non-hex** fixture is not a loud failure: `hex_decode` returns `None`,
/// `parse_mount_manifest` falls back to the DEFAULT (empty) manifest, and any
/// loop over `m.rw` silently iterates nothing. The first version of
/// `worker_owned_paths_keeps_rw_mountpoints_and_their_anchors` was written with
/// `"rw:vdb:/data/scratch"` — colon-separated and not hex, so doubly wrong —
/// and asserted precisely nothing: deleting the whole RW/anchor block from
/// `worker_owned_paths` left it green. Note the real manifest is TAB-separated.
fn mounts_cmdline(manifest: &str) -> String {
    let hex: String = manifest.bytes().map(|b| format!("{b:02x}")).collect();
    format!("kastellan.mounts={hex}")
}

/// The whole chown set for a plain worker: `/tmp` and nothing else.
///
/// Pinned as the COMPLETE vector rather than probed with `contains`. Every
/// assertion below is an `assert_eq!` on the whole set for the same reason —
/// membership, conditionality, ordering and the absence of `/run` are one
/// property, and five independent `contains` probes could not see a guard that
/// was too broad (see `..._keeps_each_socket_to_its_own_relay`).
#[test]
fn worker_owned_paths_without_a_relay_is_tmp_alone() {
    assert_eq!(worker_owned_paths(""), [OwnedPath::writable("/tmp")]);
}

/// The relay socket must be in the chown set when its relay is enabled.
///
/// This is the assertion the whole networked half of the Firecracker suite
/// turned on: `connect(2)` to an `AF_UNIX` socket needs write permission on the
/// socket file, and the init binds these as root before it drops to the worker
/// uid. Without the chown the worker's first dial fails `EACCES` and every
/// egress/broker VM worker is dead, with a message that names the proxy rather
/// than the permission.
///
/// `/run` is deliberately absent — see `worker_owned_paths`' doc comment: the
/// tmpfs is already 1777, so chowning it granted nothing the worker lacked and
/// handed it ownership of a sticky directory. Pinning the whole vector is what
/// stops it being re-added on a plausible-sounding hunch.
#[test]
fn worker_owned_paths_for_an_egress_worker_is_tmp_and_its_socket() {
    assert_eq!(
        worker_owned_paths("kastellan.egress=1"),
        [
            OwnedPath::writable("/tmp"),
            OwnedPath::relay_socket(GUEST_EGRESS_UDS),
        ],
    );
}

/// Each socket is conditional on ITS OWN relay, not on "any relay".
///
/// The mutation this exists to kill is the tidy-up that collapses the two
/// conditionals into `if egress || broker` — tempting because `/run` genuinely
/// *was* `egress || broker` one line above until this branch removed it. A
/// broker-only worker would then chown an egress socket that was never bound,
/// logging an error on every boot of that worker class. Neither single-relay
/// test could see it before: each asserted only that its own socket was
/// present, and the only negative test used the both-disabled cmdline, the one
/// configuration where the over-broad guard is indistinguishable from this one.
#[test]
fn worker_owned_paths_keeps_each_socket_to_its_own_relay() {
    assert_eq!(
        worker_owned_paths("kastellan.broker=1"),
        [
            OwnedPath::writable("/tmp"),
            OwnedPath::relay_socket(GUEST_BROKER_UDS),
        ],
    );
    assert_eq!(
        worker_owned_paths("kastellan.egress=1 kastellan.broker=1"),
        [
            OwnedPath::writable("/tmp"),
            OwnedPath::relay_socket(GUEST_EGRESS_UDS),
            OwnedPath::relay_socket(GUEST_BROKER_UDS),
        ],
    );
}

/// The RW mountpoints and their share anchors still come through, so the
/// relay-socket fix ADDED to the pre-existing set rather than replacing it.
///
/// This is the only coverage that half of `worker_owned_paths` has: it moved
/// verbatim out of `drop_privileges_for_worker`, which is all syscalls and so
/// unreachable from any unit test, and the e2e that would notice its loss is
/// `#[ignore]`d and DGX-only.
#[test]
fn worker_owned_paths_keeps_rw_mountpoints_and_their_anchors() {
    let cmdline = format!(
        "{} kastellan.egress=1",
        mounts_cmdline("rw\tvdb\t/data/scratch")
    );
    assert_eq!(
        worker_owned_paths(&cmdline),
        [
            OwnedPath::writable("/data/scratch"),
            OwnedPath::writable("/tmp"),
            OwnedPath::writable("/data"),
            OwnedPath::relay_socket(GUEST_EGRESS_UDS),
        ],
    );
}

/// Duplicate anchors are expected, not a bug — two RW mounts under one
/// top-level directory push `/data` twice. Pinned so the documented "no
/// deduplication" contract is a fact about the code rather than a claim in a
/// comment, and so a future `dedup()` has to change a test that says why.
#[test]
fn worker_owned_paths_repeats_a_shared_anchor_rather_than_deduplicating() {
    let cmdline = mounts_cmdline("rw\tvdb\t/data/scratch\nrw\tvdc\t/data/store");
    assert_eq!(
        worker_owned_paths(&cmdline),
        [
            OwnedPath::writable("/data/scratch"),
            OwnedPath::writable("/data/store"),
            OwnedPath::writable("/tmp"),
            OwnedPath::writable("/data"),
            OwnedPath::writable("/data"),
        ],
    );
}

/// A failed `chown` means something different for each role, and this is the
/// single place that decides which.
///
/// The mutation it kills is a collapse to a constant — `false` restores the
/// pre-#670 warn-only behaviour (a networked worker that dies on every dial
/// while the VM reports a clean boot), and `true` makes a mountpoint that
/// cannot be chowned refuse the whole VM, which is a far worse trade than the
/// degradation it replaces. Both directions are asserted for that reason.
#[test]
fn only_a_relay_socket_makes_a_failed_chown_fatal() {
    assert!(
        OwnedPathRole::RelaySocket.chown_failure_is_fatal(),
        "a socket the worker cannot connect to is a dead worker, not a degraded one"
    );
    assert!(
        !OwnedPathRole::Writable.chown_failure_is_fatal(),
        "a mountpoint the worker cannot own must NOT halt the VM — the worker runs \
         and fails on the specific write, which is more useful than refusing to boot"
    );
}

/// The role is derived from what the path IS, not from where it sits in the
/// vector — so a reordering of `worker_owned_paths` cannot silently turn a
/// socket into a directory.
#[test]
fn roles_follow_the_path_kind_not_its_position() {
    let paths = worker_owned_paths("kastellan.egress=1 kastellan.broker=1");
    for p in &paths {
        let expected = if p.path == GUEST_EGRESS_UDS || p.path == GUEST_BROKER_UDS {
            OwnedPathRole::RelaySocket
        } else {
            OwnedPathRole::Writable
        };
        assert_eq!(p.role, expected, "wrong role for {}", p.path);
    }
}

/// `/run` is mounted with an EXPLICIT mode, not the kernel's 1777 default.
///
/// A weak-looking assertion that is doing real work: the whole of #672 is that
/// the previous value was nobody's decision, and the failure mode it guards
/// against is someone dropping the option again while every test stays green
/// (the guest works fine at 1777 — that is exactly why it survived). The live
/// proof is the in-guest e2e that stats `/run`; this is the cheap regression
/// guard beside the constant.
#[test]
fn run_tmpfs_is_mounted_with_an_explicit_mode() {
    assert!(
        RUN_TMPFS_MOUNT_OPTS.contains("mode="),
        "the /run tmpfs must name its mode; without one the kernel gives 1777: \
         {RUN_TMPFS_MOUNT_OPTS}"
    );
    assert!(
        !RUN_TMPFS_MOUNT_OPTS.contains("1777") && !RUN_TMPFS_MOUNT_OPTS.contains("0777"),
        "world-writable is the default this constant exists to replace: {RUN_TMPFS_MOUNT_OPTS}"
    );
}
