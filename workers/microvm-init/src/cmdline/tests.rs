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
        parse_env_cmdline(cmdline),
        vec![("A".to_string(), "1".to_string()), ("B".to_string(), "2".to_string())]
    );
}

#[test]
fn parse_env_cmdline_missing_token_is_empty() {
    assert!(parse_env_cmdline("console=ttyS0 panic=1").is_empty());
}

#[test]
fn parse_env_cmdline_malformed_hex_is_empty() {
    // Odd length and non-hex both fail closed to no env (fail-safe → caller
    // keeps the baked defaults).
    assert!(parse_env_cmdline("kastellan.env=abc").is_empty());
    assert!(parse_env_cmdline("kastellan.env=zz").is_empty());
}

#[test]
fn parse_env_cmdline_value_may_contain_equals() {
    // Split on the FIRST '=' so a JSON-ish value survives. Block `K=["a=b"]`
    // = bytes 4b 3d 5b 22 61 3d 62 22 5d → one whitespace-free token.
    let cmdline = "console=ttyS0 kastellan.env=4b3d5b22613d62225d";
    assert_eq!(
        parse_env_cmdline(cmdline),
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
