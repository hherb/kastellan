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
pub(crate) fn parse_env_cmdline(cmdline: &str) -> Result<Vec<(String, String)>, String> {
    // Fail CLOSED on a present-but-undecodable token (security audit
    // 2026-09-02, prelude F2). The old decoder returned an EMPTY env for bad
    // hex / bad UTF-8 "so the worker still boots" — but the env carries the
    // worker's whole lockdown (`KASTELLAN_SECCOMP_PROFILE`, the Landlock
    // grants): an empty env meant a guest-root worker with no seccomp and a
    // Landlock ruleset missing its RW grants, silently. A missing token is
    // still an empty env (a worker with no env is legitimate); a corrupt one
    // is an error the init turns into a refused boot.
    let prefix = format!("{ENV_CMDLINE_KEY}=");
    let Some(token) = cmdline.split_whitespace().find_map(|t| t.strip_prefix(&prefix)) else {
        return Ok(Vec::new());
    };
    let Some(bytes) = hex_decode(token) else {
        return Err(format!("{ENV_CMDLINE_KEY}= token is not valid hex ({} chars)", token.len()));
    };
    let Ok(block) = String::from_utf8(bytes) else {
        return Err(format!("{ENV_CMDLINE_KEY}= token is not valid UTF-8"));
    };
    Ok(block
        .split('\n')
        .filter_map(|line| {
            line.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect())
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

/// Mount options for the guest `/run` tmpfs.
///
/// **An explicit mode, because the kernel default is not one anybody chose.** A
/// tmpfs mounted with no `mode=` comes up **1777** — world-writable and sticky —
/// regardless of the mounting process's umask (measured, not assumed). Inside
/// the VM that was close to harmless, since the worker is the only workload and
/// is the uid everything is chowned to anyway; but it was load-bearing in a
/// misleading way. #669 originally chowned `/run` to the worker, justified as
/// "the worker needs to traverse and write in it" — both of which the 1777
/// default had silently already granted, so the chown's only real effect was to
/// hand the worker ownership of a *sticky* directory, which is precisely what
/// lets an owner unlink entries it does not own.
///
/// At `0755` the directory's permissions are a decision: root (the pre-drop
/// init) creates the relay sockets, everyone can traverse to reach them, and
/// the per-socket `chown` in [`worker_owned_paths`] is the ONLY grant the
/// worker gets. Removing that chown must then break the worker — which is the
/// property #669 wanted and the 1777 default was quietly masking.
///
/// Lives here rather than beside the `mount(2)` call so it is readable and
/// unit-testable on a Mac; `guest::egress::mount_run_tmpfs` applies it. (#672)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const RUN_TMPFS_MOUNT_OPTS: &str = "mode=0755";

/// What a path in the chown set is FOR — the two kinds fail very differently,
/// and a single flat `Vec<String>` could not say so.
///
/// The distinction is not cosmetic: it decides whether a failed `chown` is a
/// degradation or a guaranteed dead worker, and re-deriving it in the chown
/// loop by comparing paths against the UDS constants would put a second place
/// in the tree that knows which paths are sockets. That kind of second place is
/// how the first version of this drifted. (#670)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnedPathRole {
    /// A directory the worker writes into: an RW mountpoint, a share anchor, or
    /// `/tmp`. If the chown fails the worker still runs and fails on the
    /// specific write — bad, but survivable and self-describing.
    Writable,
    /// A relay socket the worker must `connect(2)` to. `connect` on an
    /// `AF_UNIX` socket needs **write** permission on the socket *file*, so a
    /// failed chown here is not a degradation: that worker dies on its first
    /// dial, every time, reporting a permission error that names the proxy
    /// rather than the cause.
    RelaySocket,
}

impl OwnedPathRole {
    /// Whether a failed `chown` of a path in this role must halt the guest.
    ///
    /// Fail-closed for the socket, and the asymmetry is the whole point: every
    /// other step of the privilege drop already panics, and the chowns were the
    /// one exception. That exception was defensible only while a panicking
    /// PID 1 was *illegible* — the VM halted and the host saw the same
    /// contentless `Protocol(EarlyExit)` the whole defect class hides behind.
    /// Now that the launcher captures the guest console (#666), the panic text
    /// reaches the host, so refusing to serve a worker that cannot possibly
    /// work is strictly better than serving it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn chown_failure_is_fatal(self) -> bool {
        match self {
            OwnedPathRole::Writable => false,
            OwnedPathRole::RelaySocket => true,
        }
    }
}

/// One entry of the chown set: the path, and what it is for.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedPath {
    pub(crate) path: String,
    pub(crate) role: OwnedPathRole,
}

impl OwnedPath {
    /// A directory the worker writes into.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn writable(path: impl Into<String>) -> Self {
        Self { path: path.into(), role: OwnedPathRole::Writable }
    }

    /// A relay socket the worker must be able to connect to.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn relay_socket(path: impl Into<String>) -> Self {
        Self { path: path.into(), role: OwnedPathRole::RelaySocket }
    }
}

/// Every path the guest init must hand to the worker uid before it drops root.
///
/// Pure function of the kernel cmdline — no syscalls — so the *decision* is
/// unit-testable on any platform and only the `chown` loop is Linux-only.
///
/// Two kinds of path are in the set:
///
/// 1. **The RW mountpoints and their share anchors**, plus `/tmp`. These are
///    what agent-authored code writes.
/// 2. **The relay sockets themselves.** This is the one that is easy to miss
///    and the reason this function exists: connecting to an `AF_UNIX` socket
///    requires *write* permission on the socket **file**, and `bind` creates it
///    `0777 & ~umask` (0755, root-owned, under PID 1's umask), so a relay bound
///    by root before the drop is unreachable afterwards. The symptom is not a
///    containment failure but a flat `connect proxy uds: Permission denied`
///    from inside the guest, which reads like a proxy or vsock fault — it cost
///    the whole networked half of the Firecracker suite (every egress and
///    broker VM worker) between the 2026-09-02 audit and the gate that found it.
///
/// **`/run` is deliberately NOT in the set**, and re-adding it would be a
/// regression rather than belt-and-braces. The first version of this fix
/// chowned it too, justified as "the worker needs to traverse and write in it".
/// It does not: `mount_run_tmpfs` mounts `/run` passing no `mode=`, and a tmpfs
/// mounted that way comes up **1777** whatever the mounting process's umask
/// (measured, not assumed), so every uid can already traverse it and create in
/// it. The chown only transferred ownership of a *sticky directory*, which is
/// precisely what lets the owner unlink entries it does not own — a widening
/// inside a hardening change, with nothing asking for it. The two socket files
/// are the whole fix.
///
/// Each entry carries its [`OwnedPathRole`], because a failed `chown` of a
/// writable directory and a failed `chown` of a relay socket are not the same
/// event (#670): the first is a degradation, the second is a worker that will
/// fail every dial it ever makes. The role travels WITH the path so the chown
/// loop never has to re-derive it by comparing against the UDS constants.
///
/// Paths are returned in a stable order and are **not** deduplicated: two RW
/// mounts under one top-level directory yield that anchor twice, and an RW
/// mountpoint that is itself top-level appears as both mountpoint and anchor.
/// Chowning a path twice is harmless, so this is documented rather than fixed.
///
/// Only the Linux guest path calls this, and `mod cmdline` is compiled on
/// macOS too so the pure parsers stay unit-testable on the dev box — hence a
/// `dead_code` allowance. Unlike the blanket `#[allow]` the older items in this
/// module carry, it is narrowed to non-Linux so the Linux build still fails if
/// the guest ever stops calling this.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn worker_owned_paths(cmdline: &str) -> Vec<OwnedPath> {
    let m = parse_mount_manifest(cmdline);
    let mut paths: Vec<OwnedPath> = m
        .rw
        .iter()
        .map(|rw| OwnedPath::writable(rw.mountpoint.clone()))
        .collect();
    paths.push(OwnedPath::writable("/tmp"));
    for t in m.rw.iter().map(|rw| rw.mountpoint.as_str()) {
        if let Some(a) = anchor_of(t) {
            paths.push(OwnedPath::writable(a));
        }
    }
    // Each socket is conditional on ITS OWN relay, never on "any relay": a
    // broker-only worker never binds the egress UDS, so chowning it would be a
    // chown of a path that was never created — an error line on every boot of
    // the other worker class, training everyone to ignore the one diagnostic
    // this defect would announce itself through next time.
    let egress = parse_egress_config(cmdline).enabled;
    let broker = parse_broker_config(cmdline).enabled;
    if egress {
        paths.push(OwnedPath::relay_socket(GUEST_EGRESS_UDS));
    }
    if broker {
        paths.push(OwnedPath::relay_socket(GUEST_BROKER_UDS));
    }
    paths
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
