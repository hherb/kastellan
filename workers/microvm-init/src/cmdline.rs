//! Pure cross-platform "wire contract" layer for the microvm-init guest PID1:
//! the kernel-cmdline token constants shared (manually) with
//! `kastellan-sandbox::linux_firecracker::plan`, the fail-safe parsers that
//! decode them, and the small value types they yield.
//!
//! Everything here is a pure function with no syscalls, so its RED→GREEN TDD
//! cycle and unit tests run on the macOS dev box as well as the Linux guest —
//! the tests live in the sibling [`tests`] module.
//!
//! Provenance: the bodies below were lifted verbatim from the former single
//! `main.rs` when it was split by concern (Item 9b prod-split, 2026-07-06). The
//! only change is that each item was widened from module-private to
//! `pub(crate)` so the Linux mechanism (`crate::guest`) and the entry point
//! (`crate::main`) — now siblings rather than same-module neighbours — can still
//! reach them. The `#[allow(dead_code)]` attributes are kept because on macOS
//! `crate::guest` is `cfg`'d out, so these helpers have no non-test caller there.

#[cfg(test)]
mod tests;

/// WORKER_VSOCK_PORT is the vsock port the guest listens on. The value is shared
/// with `kastellan-sandbox::linux_firecracker::WORKER_VSOCK_PORT` (kept in sync
/// manually; the guest crate must not depend on the sandbox crate).
// Used on Linux (in accept_host_bridge via vsock_listen_cid_port) and in tests
// on all platforms. The Linux-gated path is not visible to the macOS compiler.
#[allow(dead_code)]
pub(crate) const WORKER_VSOCK_PORT: u32 = 1024;

/// VMADDR_CID_ANY mirrors `libc::VMADDR_CID_ANY` on Linux (0xffffffff). Defined
/// here as a plain u32 literal so the pure helper and its test compile on macOS
/// without the Linux-only libc items.
#[allow(dead_code)]
pub(crate) const VMADDR_CID_ANY: u32 = 0xffff_ffff;

/// Kernel-cmdline token carrying the host-forwarded worker env (#360). Must stay
/// in sync with `kastellan-sandbox::linux_firecracker::plan::ENV_CMDLINE_KEY`
/// (this crate must not depend on the sandbox crate — same constraint as
/// [`WORKER_VSOCK_PORT`]).
#[allow(dead_code)]
pub(crate) const ENV_CMDLINE_KEY: &str = "kastellan.env";

/// Decode lowercase/uppercase hex to bytes. Pure; `None` on odd length or any
/// non-hex digit (fail-safe — a garbled token yields no env rather than partial
/// junk). Mirrors `kastellan-sandbox`'s `hex_encode`.
#[allow(dead_code)]
pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let nibble = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Some(out)
}

/// Parse host-forwarded env out of the kernel cmdline (#360). Finds the
/// whitespace-delimited `kastellan.env=<hex>` token, hex-decodes it, and splits
/// the `K1=V1\nK2=V2\n…` block into pairs (split on the FIRST `=` so values may
/// contain `=`). Pure → unit-testable on any platform.
///
/// Fail-safe: a missing token, bad hex, non-UTF-8 bytes, or a line without `=`
/// all yield no (or fewer) pairs rather than an error — the caller falls back to
/// the baked defaults and still boots a working worker.
#[allow(dead_code)]
pub(crate) fn parse_env_cmdline(cmdline: &str) -> Vec<(String, String)> {
    let prefix = format!("{ENV_CMDLINE_KEY}=");
    let Some(token) = cmdline.split_whitespace().find_map(|t| t.strip_prefix(&prefix)) else {
        return Vec::new();
    };
    let Some(bytes) = hex_decode(token) else {
        return Vec::new();
    };
    let Ok(block) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    block
        .split('\n')
        .filter_map(|line| {
            line.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Cmdline token carrying the hex-encoded worker program path to exec (slice 4b).
/// Must stay in sync with `kastellan-sandbox::linux_firecracker::plan`'s
/// WORKER_CMDLINE_KEY.
#[allow(dead_code)]
pub(crate) const WORKER_CMDLINE_KEY: &str = "kastellan.worker";

/// Parse the host-forwarded worker program path out of the kernel cmdline
/// (slice 4b). Fail-safe: a missing token, bad hex, non-UTF-8, or empty value
/// all yield `None`, so `exec_worker` falls back to the baked path. Pure.
#[allow(dead_code)]
pub(crate) fn parse_worker_cmdline(cmdline: &str) -> Option<String> {
    let prefix = format!("{WORKER_CMDLINE_KEY}=");
    let token = cmdline.split_whitespace().find_map(|t| t.strip_prefix(&prefix))?;
    let bytes = hex_decode(token)?;
    let s = String::from_utf8(bytes).ok()?;
    (!s.is_empty()).then_some(s)
}

/// Cmdline token carrying the host-forwarded worker argv (#374). Each arg is
/// hex-encoded independently and the list joined with ','. Must stay in sync
/// with `kastellan-sandbox::linux_firecracker::plan`'s WORKER_ARGS_CMDLINE_KEY.
#[allow(dead_code)]
pub(crate) const WORKER_ARGS_CMDLINE_KEY: &str = "kastellan.worker.args";

/// Parse the host-forwarded worker argv out of the kernel cmdline (#374). The
/// token is `<hex0>,<hex1>,…`, each component the hex of one argv entry (the
/// ',' separator can never collide with the hex alphabet `[0-9a-f]`).
///
/// Fail-safe AND all-or-nothing: a missing token yields an empty `Vec` (no extra
/// args — the common `lockdown_shim:None` case). A token that is present but has
/// ANY malformed component (bad hex or non-UTF-8) also yields empty rather than a
/// partial list — a positionally-shifted argv would misfeed the lockdown-exec
/// shim (which reads its target from argv[1]), so dropping the whole list and
/// running the program bare is the safe degradation. Pure.
#[allow(dead_code)]
pub(crate) fn parse_worker_args_cmdline(cmdline: &str) -> Vec<String> {
    let prefix = format!("{WORKER_ARGS_CMDLINE_KEY}=");
    let Some(token) = cmdline.split_whitespace().find_map(|t| t.strip_prefix(&prefix)) else {
        return Vec::new();
    };
    // An empty token means no args were forwarded (the host emits no token at all
    // for empty argv, so this only guards a hand-crafted cmdline).
    if token.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in token.split(',') {
        let Some(bytes) = hex_decode(part) else {
            return Vec::new();
        };
        let Ok(s) = String::from_utf8(bytes) else {
            return Vec::new();
        };
        out.push(s);
    }
    out
}

/// Cmdline token carrying the hex-encoded mount manifest (slice 3). Must stay in
/// sync with `kastellan-sandbox::linux_firecracker::plan::MOUNTS_CMDLINE_KEY`.
#[allow(dead_code)]
pub(crate) const MOUNTS_CMDLINE_KEY: &str = "kastellan.mounts";

/// Egress vsock port (slice 4a). Shared with
/// `kastellan-sandbox::linux_firecracker::plan::EGRESS_VSOCK_PORT` (kept in sync
/// manually; this crate must not depend on the sandbox crate).
#[allow(dead_code)]
pub(crate) const EGRESS_VSOCK_PORT: u32 = 1025;
/// In-guest UDS the worker dials and the relay binds. Shared with the sandbox
/// crate's `GUEST_EGRESS_UDS`.
#[allow(dead_code)]
pub(crate) const GUEST_EGRESS_UDS: &str = "/run/kastellan-egress.sock";
/// The host's vsock CID from inside the guest (mirrors `libc::VMADDR_CID_HOST`).
/// Plain literal so the parser/tests compile on macOS without the libc item.
#[allow(dead_code)]
pub(crate) const VMADDR_CID_HOST: u32 = 2;

/// Egress channel config parsed from the kernel cmdline (slice 4a). Pure.
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct EgressConfig {
    pub(crate) enabled: bool,
    pub(crate) selftest: bool,
}

/// Parse the egress tokens out of the kernel cmdline. `enabled` from
/// `kastellan.egress=1`, `selftest` from `kastellan.egress.selftest=1`. Pure →
/// unit-testable on any platform.
#[allow(dead_code)]
pub(crate) fn parse_egress_config(cmdline: &str) -> EgressConfig {
    let mut c = EgressConfig::default();
    for t in cmdline.split_whitespace() {
        match t {
            "kastellan.egress=1" => c.enabled = true,
            "kastellan.egress.selftest=1" => c.selftest = true,
            _ => {}
        }
    }
    c
}

/// Broker vsock port (VM × broker). Shared with the sandbox crate's
/// `BROKER_VSOCK_PORT` (kept in sync manually; this crate must not depend on the
/// sandbox crate). Distinct from the egress port so both channels coexist on the
/// one vsock device.
#[allow(dead_code)]
pub(crate) const BROKER_VSOCK_PORT: u32 = 1026;
/// In-guest UDS the worker dials for its broker and the relay binds. One generic
/// path suffices (a worker binds at most one broker socket). Shared with the
/// sandbox crate's `GUEST_BROKER_UDS`.
#[allow(dead_code)]
pub(crate) const GUEST_BROKER_UDS: &str = "/run/kastellan-broker.sock";

/// Broker channel config parsed from the kernel cmdline (VM × broker). Pure →
/// unit-testable on any platform.
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct BrokerConfig {
    pub(crate) enabled: bool,
}

/// Parse the broker token out of the kernel cmdline: `enabled` from
/// `kastellan.broker=1`. Pure.
#[allow(dead_code)]
pub(crate) fn parse_broker_config(cmdline: &str) -> BrokerConfig {
    let mut c = BrokerConfig::default();
    for t in cmdline.split_whitespace() {
        if t == "kastellan.broker=1" {
            c.enabled = true;
        }
    }
    c
}

#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct MountManifest {
    pub(crate) ro: Option<RoMount>,
    /// All RW drives, in manifest order. Slice 3 = one scratch drive; slice 5b-2
    /// adds a second persistent drive. The guest mounts every entry.
    pub(crate) rw: Vec<RwMount>,
}
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
pub(crate) struct RoMount {
    pub(crate) dev: String,
    pub(crate) targets: Vec<String>,
}
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
pub(crate) struct RwMount {
    pub(crate) dev: String,
    pub(crate) mountpoint: String,
}

/// Decode the `kastellan.mounts=<hex>` token into a [`MountManifest`]. Pure →
/// unit-testable on any platform. Fail-safe: a missing/garbled token, bad hex,
/// non-UTF-8, or a malformed line yields an empty/partial manifest rather than an
/// error (the guest still boots a working worker, just without that share).
#[allow(dead_code)]
pub(crate) fn parse_mount_manifest(cmdline: &str) -> MountManifest {
    let prefix = format!("{MOUNTS_CMDLINE_KEY}=");
    let Some(token) = cmdline.split_whitespace().find_map(|t| t.strip_prefix(&prefix)) else {
        return MountManifest::default();
    };
    let Some(bytes) = hex_decode(token) else {
        return MountManifest::default();
    };
    let Ok(block) = String::from_utf8(bytes) else {
        return MountManifest::default();
    };
    let mut m = MountManifest::default();
    for line in block.split('\n') {
        let mut fields = line.split('\t');
        match fields.next() {
            Some("ro") => {
                if let Some(dev) = fields.next() {
                    let targets: Vec<String> = fields.map(|s| s.to_string()).collect();
                    if !targets.is_empty() {
                        m.ro = Some(RoMount { dev: dev.to_string(), targets });
                    }
                }
            }
            Some("rw") => {
                if let (Some(dev), Some(mp)) = (fields.next(), fields.next()) {
                    m.rw.push(RwMount { dev: dev.to_string(), mountpoint: mp.to_string() });
                }
            }
            _ => {}
        }
    }
    m
}

/// Top-level anchor of an absolute path ("/opt/venv" → "/opt"). Returns `None`
/// for `/tmp/*` (already a writable tmpfs, no anchor needed) and for `/`. Pure.
#[allow(dead_code)]
pub(crate) fn anchor_of(path: &str) -> Option<String> {
    let first = path.trim_start_matches('/').split('/').next()?;
    if first.is_empty() || first == "tmp" {
        return None;
    }
    Some(format!("/{first}"))
}

/// Returns the (cid, port) pair the guest vsock listener should bind to.
/// Pure function — no syscalls — so it is unit-testable on any platform.
#[allow(dead_code)]
pub(crate) fn vsock_listen_cid_port() -> (u32, u32) {
    (VMADDR_CID_ANY, WORKER_VSOCK_PORT)
}

/// Pack an interface name into a 16-byte `ifr_name` buffer: NUL-padded, truncated
/// to 15 chars + a trailing NUL. Pure — unit-testable without a socket. Only
/// `bring_loopback_up` (Linux-only) calls this; cross-platform so its RED→GREEN
/// TDD cycle and unit tests run on the Mac dev box too (slice 5b-4b, task 2).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn pack_ifname(name: &str) -> [libc::c_char; 16] {
    let mut buf = [0 as libc::c_char; 16];
    for (i, b) in name.bytes().take(15).enumerate() {
        buf[i] = b as libc::c_char;
    }
    buf
}

/// How a RO-share bind target must be prepared before `MS_BIND`, decided purely
/// from the source's kind (probed at `/ro-share{target}`) so it is unit-testable
/// without root or real mounts.
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
pub(crate) enum BindPrep {
    /// Source is a directory: create the target dir, then bind (slice-3 default).
    Dir,
    /// Source is a regular file (e.g. the per-instance `ca.pem`): create the
    /// target's PARENT dir + an empty target file, then bind. A file bind needs
    /// an existing regular-file target.
    File,
    /// Source missing or neither file nor dir: skip the bind.
    Skip,
}

#[allow(dead_code)]
pub(crate) fn bind_prep(src_is_dir: bool, src_is_file: bool) -> BindPrep {
    if src_is_dir {
        BindPrep::Dir
    } else if src_is_file {
        BindPrep::File
    } else {
        BindPrep::Skip
    }
}
