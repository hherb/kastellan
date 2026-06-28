//! PID1 inside the Firecracker guest. Mounts the minimal pseudo-filesystems,
//! accepts the host's JSON-RPC bridge over AF_VSOCK, wires it onto the worker's
//! fd 0/1, and execs the worker. The worker (`serve_stdio`) is UNCHANGED — this
//! init performs the vsock<->stdio adaptation so the worker still "speaks stdio".
//!
//! The worker binary path + env arrive via the kernel cmdline / a baked config
//! (see WORKER_CMD). Slice 1 bakes the python-exec worker invocation.
//!
//! This crate is guest-only (Linux). On macOS the binary stubs out with an
//! error message so `cargo build --workspace` stays green on the dev box.

/// WORKER_VSOCK_PORT is the vsock port the guest listens on. The value is shared
/// with `kastellan-sandbox::linux_firecracker::WORKER_VSOCK_PORT` (kept in sync
/// manually; the guest crate must not depend on the sandbox crate).
// Used on Linux (in accept_host_bridge via vsock_listen_cid_port) and in tests
// on all platforms. The Linux-gated path is not visible to the macOS compiler.
#[allow(dead_code)]
const WORKER_VSOCK_PORT: u32 = 1024;

/// VMADDR_CID_ANY mirrors `libc::VMADDR_CID_ANY` on Linux (0xffffffff). Defined
/// here as a plain u32 literal so the pure helper and its test compile on macOS
/// without the Linux-only libc items.
#[allow(dead_code)]
const VMADDR_CID_ANY: u32 = 0xffff_ffff;

/// Kernel-cmdline token carrying the host-forwarded worker env (#360). Must stay
/// in sync with `kastellan-sandbox::linux_firecracker::plan::ENV_CMDLINE_KEY`
/// (this crate must not depend on the sandbox crate — same constraint as
/// [`WORKER_VSOCK_PORT`]).
#[allow(dead_code)]
const ENV_CMDLINE_KEY: &str = "kastellan.env";

/// Decode lowercase/uppercase hex to bytes. Pure; `None` on odd length or any
/// non-hex digit (fail-safe — a garbled token yields no env rather than partial
/// junk). Mirrors `kastellan-sandbox`'s `hex_encode`.
#[allow(dead_code)]
fn hex_decode(s: &str) -> Option<Vec<u8>> {
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
fn parse_env_cmdline(cmdline: &str) -> Vec<(String, String)> {
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

/// Cmdline token carrying the hex-encoded mount manifest (slice 3). Must stay in
/// sync with `kastellan-sandbox::linux_firecracker::plan::MOUNTS_CMDLINE_KEY`.
#[allow(dead_code)]
const MOUNTS_CMDLINE_KEY: &str = "kastellan.mounts";

/// Egress vsock port (slice 4a). Shared with
/// `kastellan-sandbox::linux_firecracker::plan::EGRESS_VSOCK_PORT` (kept in sync
/// manually; this crate must not depend on the sandbox crate).
#[allow(dead_code)]
const EGRESS_VSOCK_PORT: u32 = 1025;
/// In-guest UDS the worker dials and the relay binds. Shared with the sandbox
/// crate's `GUEST_EGRESS_UDS`.
#[allow(dead_code)]
const GUEST_EGRESS_UDS: &str = "/run/kastellan-egress.sock";
/// The host's vsock CID from inside the guest (mirrors `libc::VMADDR_CID_HOST`).
/// Plain literal so the parser/tests compile on macOS without the libc item.
#[allow(dead_code)]
const VMADDR_CID_HOST: u32 = 2;

/// Egress channel config parsed from the kernel cmdline (slice 4a). Pure.
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
struct EgressConfig {
    enabled: bool,
    selftest: bool,
}

/// Parse the egress tokens out of the kernel cmdline. `enabled` from
/// `kastellan.egress=1`, `selftest` from `kastellan.egress.selftest=1`. Pure →
/// unit-testable on any platform.
#[allow(dead_code)]
fn parse_egress_config(cmdline: &str) -> EgressConfig {
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

#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
struct MountManifest {
    ro: Option<RoMount>,
    rw: Option<RwMount>,
}
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
struct RoMount {
    dev: String,
    targets: Vec<String>,
}
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
struct RwMount {
    dev: String,
    mountpoint: String,
}

/// Decode the `kastellan.mounts=<hex>` token into a [`MountManifest`]. Pure →
/// unit-testable on any platform. Fail-safe: a missing/garbled token, bad hex,
/// non-UTF-8, or a malformed line yields an empty/partial manifest rather than an
/// error (the guest still boots a working worker, just without that share).
#[allow(dead_code)]
fn parse_mount_manifest(cmdline: &str) -> MountManifest {
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
                    m.rw = Some(RwMount { dev: dev.to_string(), mountpoint: mp.to_string() });
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
fn anchor_of(path: &str) -> Option<String> {
    let first = path.trim_start_matches('/').split('/').next()?;
    if first.is_empty() || first == "tmp" {
        return None;
    }
    Some(format!("/{first}"))
}

/// Returns the (cid, port) pair the guest vsock listener should bind to.
/// Pure function — no syscalls — so it is unit-testable on any platform.
#[allow(dead_code)]
fn vsock_listen_cid_port() -> (u32, u32) {
    (VMADDR_CID_ANY, WORKER_VSOCK_PORT)
}

// ── Linux-only: real syscall implementations ──────────────────────────────────

#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;

/// Apply the host-dir-share mounts (slice 3). RO drive → /ro-share, then each
/// fs_read root bind-mounted to its absolute path (tmpfs-anchored so mkdir works
/// on the read-only root); RW drive → its mountpoint. Best-effort per mount: a
/// failure is logged to stderr (the kernel console) but does not abort PID1 —
/// the worker simply won't see that path, surfaced as a normal file error.
#[cfg(target_os = "linux")]
fn apply_host_mounts(m: &MountManifest) {
    use std::collections::BTreeSet;

    fn mount(src: &str, target: &str, fstype: Option<&str>, flags: libc::c_ulong) -> bool {
        // Build the C strings without unwrap: an interior NUL must be skipped, not
        // a panic. PID1 panicking would kill the whole guest — this path is
        // contractually best-effort (a bad mount just leaves the worker without
        // that path), so a NUL-bearing src/target/fstype is logged and skipped.
        let (csrc, ctarget) = match (std::ffi::CString::new(src), std::ffi::CString::new(target)) {
            (Ok(s), Ok(t)) => (s, t),
            _ => {
                eprintln!("microvm-init: mount {target} skipped (path contains an interior NUL)");
                return false;
            }
        };
        let fst = match fstype.map(std::ffi::CString::new).transpose() {
            Ok(f) => f,
            Err(_) => {
                eprintln!("microvm-init: mount {target} skipped (fstype contains an interior NUL)");
                return false;
            }
        };
        let fst_ptr = fst.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let rc = unsafe {
            libc::mount(csrc.as_ptr(), ctarget.as_ptr(), fst_ptr, flags, std::ptr::null())
        };
        if rc != 0 {
            eprintln!("microvm-init: mount {target} failed (errno {})", unsafe {
                *libc::__errno_location()
            });
        }
        rc == 0
    }

    // Collect every target whose parent must be made writable.
    let mut targets: Vec<&str> = Vec::new();
    if let Some(ro) = &m.ro {
        for t in &ro.targets {
            targets.push(t);
        }
    }
    if let Some(rw) = &m.rw {
        targets.push(&rw.mountpoint);
    }
    // tmpfs each unique anchor once (makes the read-only root writable there).
    let anchors: BTreeSet<String> = targets.iter().filter_map(|t| anchor_of(t)).collect();
    for a in &anchors {
        let _ = std::fs::create_dir_all(a); // anchor dir is pre-created in rootfs; harmless if exists
        mount("tmpfs", a, Some("tmpfs"), 0);
    }

    // RO share: mount the ext4 read-only at /ro-share, then bind-mount each root.
    if let Some(ro) = &m.ro {
        let _ = std::fs::create_dir_all("/ro-share");
        if mount(&ro.dev, "/ro-share", Some("ext4"), libc::MS_RDONLY) {
            for t in &ro.targets {
                let from = format!("/ro-share{t}");
                if std::fs::create_dir_all(t).is_ok() {
                    // A bind does NOT inherit a per-mount RO flag, but it is a
                    // second view of the /ro-share ext4 mounted MS_RDONLY at the
                    // SUPERBLOCK level above, so writes through this path are
                    // refused by the read-only superblock. The image is also
                    // ephemeral with no host write-back. Hence MS_BIND alone is
                    // a genuinely read-only exposure here.
                    mount(&from, t, None, libc::MS_BIND);
                }
            }
        }
    }

    // RW scratch: mount the blank ext4 read-write at its mountpoint.
    if let Some(rw) = &m.rw {
        let _ = std::fs::create_dir_all(&rw.mountpoint);
        mount(&rw.dev, &rw.mountpoint, Some("ext4"), 0);
    }
}

/// Slice 4a: stand up the in-guest egress relay. Mount a writable `/run` tmpfs,
/// bind the in-guest UDS the worker dials, and fork a child that pipes every
/// accepted UDS connection to the host over `AF_VSOCK(VMADDR_CID_HOST,
/// EGRESS_VSOCK_PORT)` (firecracker forwards that to the launcher's reverse-relay
/// listener at `<base>_<port>`, which dials the real host egress proxy). Bind
/// happens in the parent BEFORE `exec`, so the worker can never dial before the
/// listener exists. Best-effort: a failure logs and returns (the worker then
/// fails its first dial, surfaced as a normal error — PID1 is never aborted).
#[cfg(target_os = "linux")]
fn setup_egress_relay() {
    // `/run` must be a writable tmpfs (the rootfs is a read-only superblock).
    let _ = std::fs::create_dir_all("/run");
    if let (Ok(src), Ok(tgt), Ok(fst)) = (
        std::ffi::CString::new("tmpfs"),
        std::ffi::CString::new("/run"),
        std::ffi::CString::new("tmpfs"),
    ) {
        unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), fst.as_ptr(), 0, std::ptr::null()) };
    }
    let listener = match bind_unix_listener(GUEST_EGRESS_UDS) {
        Some(fd) => fd,
        None => {
            eprintln!("microvm-init: egress UDS bind failed; worker egress disabled");
            return;
        }
    };
    // SAFETY: single-threaded PID1 here; fork is safe (no other threads to race).
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        egress_relay_loop(listener); // never returns
        unsafe { libc::_exit(0) };
    }
    // Parent: drop its copy of the listener fd so the exec'd worker can't inherit
    // a stray listening fd (#361 hygiene); the child owns the accept loop.
    unsafe { libc::close(listener) };
}

/// Bind an AF_UNIX SOCK_STREAM listener at `path`. Returns the listening fd or
/// `None` on any failure. Unlinks a stale socket first.
#[cfg(target_os = "linux")]
fn bind_unix_listener(path: &str) -> Option<RawFd> {
    let _ = std::fs::remove_file(path);
    let cpath = std::ffi::CString::new(path).ok()?;
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        let bytes = cpath.as_bytes_with_nul();
        if bytes.len() > addr.sun_path.len() {
            libc::close(fd);
            return None;
        }
        for (dst, &b) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = b as libc::c_char;
        }
        let alen = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, alen) != 0
            || libc::listen(fd, 8) != 0
        {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Connect an AF_VSOCK SOCK_STREAM to `(cid, port)`. Returns the connected fd.
#[cfg(target_os = "linux")]
fn connect_host_vsock(cid: u32, port: u32) -> Option<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as _;
        addr.svm_cid = cid;
        addr.svm_port = port;
        let alen = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, alen) != 0 {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Connect an AF_UNIX SOCK_STREAM to `path` (the self-test client side).
#[cfg(target_os = "linux")]
fn connect_unix(path: &str) -> Option<RawFd> {
    let cpath = std::ffi::CString::new(path).ok()?;
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        let bytes = cpath.as_bytes_with_nul();
        if bytes.len() > addr.sun_path.len() {
            libc::close(fd);
            return None;
        }
        for (dst, &b) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = b as libc::c_char;
        }
        let alen = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, alen) != 0 {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Accept loop for the in-guest relay child: each UDS connection gets its own
/// vsock connection to the host and a bidirectional byte pump.
#[cfg(target_os = "linux")]
fn egress_relay_loop(listener: RawFd) {
    loop {
        let conn = unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn < 0 {
            continue;
        }
        match connect_host_vsock(VMADDR_CID_HOST, EGRESS_VSOCK_PORT) {
            Some(vfd) => {
                // conn/vfd are RawFd (Copy); both directions run concurrently on
                // the same full-duplex sockets.
                let up = std::thread::spawn(move || pump_raw(conn, vfd));
                pump_raw(vfd, conn);
                let _ = up.join();
                unsafe {
                    libc::close(conn);
                    libc::close(vfd);
                }
            }
            None => unsafe {
                libc::close(conn);
            },
        }
    }
}

/// One-direction raw-fd byte copy until EOF/err; half-closes the writer on EOF.
#[cfg(target_os = "linux")]
fn pump_raw(from_fd: RawFd, to_fd: RawFd) {
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(from_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            unsafe { libc::shutdown(to_fd, libc::SHUT_WR) };
            break;
        }
        let mut off = 0isize;
        while off < n {
            let w = unsafe {
                libc::write(
                    to_fd,
                    buf.as_ptr().offset(off) as *const libc::c_void,
                    (n - off) as usize,
                )
            };
            if w <= 0 {
                return;
            }
            off += w;
        }
    }
}

/// Slice 4a self-test: connect our own in-guest UDS, write `PING`, expect `PONG`.
/// Proves the full guest→host reverse path on real KVM. Logs `EGRESS_CHANNEL_OK`
/// to the kernel console on success. Best-effort; never aborts PID1.
#[cfg(target_os = "linux")]
fn egress_selftest() {
    let Some(fd) = connect_unix(GUEST_EGRESS_UDS) else {
        eprintln!("microvm-init: egress selftest connect failed");
        return;
    };
    let ping = b"PING\n";
    unsafe { libc::write(fd, ping.as_ptr() as *const libc::c_void, ping.len()) };
    let mut buf = [0u8; 16];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    unsafe { libc::close(fd) };
    if n >= 4 && &buf[..4] == b"PONG" {
        eprintln!("EGRESS_CHANNEL_OK");
    } else {
        eprintln!("microvm-init: egress selftest got no PONG (n={n})");
    }
}

#[cfg(target_os = "linux")]
fn mount_pseudo_fs() {
    let mounts: &[(&str, &str, &str)] = &[
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("tmpfs", "/tmp", "tmpfs"),
    ];
    for (src, target, fstype) in mounts {
        let src = std::ffi::CString::new(*src).unwrap();
        let target = std::ffi::CString::new(*target).unwrap();
        let fstype = std::ffi::CString::new(*fstype).unwrap();
        // Ignore EBUSY (already mounted by the kernel or a prior call).
        unsafe {
            libc::mount(
                src.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                std::ptr::null(),
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn accept_host_bridge() -> RawFd {
    let (_, port) = vsock_listen_cid_port();
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "AF_VSOCK socket failed");
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as _;
        // Use the local VMADDR_CID_ANY const (= libc::VMADDR_CID_ANY) so the
        // value is defined once and the const is used consistently.
        addr.svm_cid = VMADDR_CID_ANY;
        addr.svm_port = port;
        let alen = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        assert_eq!(
            libc::bind(fd, &addr as *const _ as *const libc::sockaddr, alen),
            0,
            "vsock bind"
        );
        assert_eq!(libc::listen(fd, 1), 0, "vsock listen");
        let conn = libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(conn >= 0, "vsock accept");
        // Serve exactly one connection: close the listen socket so the exec'd
        // worker does not inherit a stray listening fd (#361).
        libc::close(fd);
        conn
    }
}

#[cfg(target_os = "linux")]
fn exec_worker() {
    use std::ffi::CString;
    // Baked worker invocation for python-exec (slice-1 consumer). A later
    // generalization reads the program path from the cmdline too.
    let prog = CString::new("/usr/local/bin/kastellan-worker-python-exec").unwrap();
    // SAFETY: single-threaded PID1; no other threads to race with.
    #[allow(deprecated)]
    unsafe {
        // Baked fallback FIRST (the rootfs reality), so a missing/garbled
        // forwarded token still boots a working worker (#360 fail-safe).
        std::env::set_var("KASTELLAN_PYTHON_EXEC_PYTHON", "/usr/bin/python3");
        // Host-forwarded policy.env OVERRIDES the bake (operator knobs like
        // KASTELLAN_PYTHON_PARAMS_FILE_MAX, and the now-correct interpreter
        // path). Read from the kernel cmdline the launcher set.
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        for (k, v) in parse_env_cmdline(&cmdline) {
            std::env::set_var(k, v);
        }
    }
    let argv = [prog.as_ptr(), std::ptr::null()];
    unsafe {
        libc::execv(prog.as_ptr(), argv.as_ptr());
    }
    panic!("execv of worker failed");
}

// ── Entry points ──────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn main() {
    mount_pseudo_fs();
    let cmdline_for_mounts = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    apply_host_mounts(&parse_mount_manifest(&cmdline_for_mounts));
    let egress = parse_egress_config(&cmdline_for_mounts);
    if egress.enabled {
        setup_egress_relay();
        if egress.selftest {
            egress_selftest();
        }
    }
    let conn_fd = accept_host_bridge();
    // Redirect the worker's stdio onto the vsock connection. A silent dup2
    // failure here would boot the guest with a dead JSON-RPC bridge and no
    // diagnostic, so the returns are checked (#361). dup2 returns the new fd
    // number on success.
    unsafe {
        assert_eq!(libc::dup2(conn_fd, 0), 0, "dup2 vsock -> stdin");
        assert_eq!(libc::dup2(conn_fd, 1), 1, "dup2 vsock -> stdout");
        // conn_fd is now duplicated onto fd 0 and 1; close the original so the
        // worker does not inherit a third copy (#361).
        if conn_fd > 1 {
            libc::close(conn_fd);
        }
    }
    // exec the worker (baked path + args); env from the baked config.
    exec_worker();
}

/// Stub for non-Linux platforms so `cargo build --workspace` on macOS stays green.
/// This binary is guest-only and will never run on macOS.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("kastellan-microvm-init runs only inside a Linux guest");
    std::process::exit(1);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vsock_listen_addr_uses_any_cid_and_worker_port() {
        // Guest listens on VMADDR_CID_ANY:1024. Assert the helper builds the
        // right (cid, port) pair.
        assert_eq!(vsock_listen_cid_port(), (0xffffffff, 1024));
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
        assert!(m.rw.is_none());
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
        let rw = m.rw.unwrap();
        assert_eq!(rw.dev, "/dev/vdc");
        assert_eq!(rw.mountpoint, "/tmp/s");
    }

    #[test]
    fn parse_mount_manifest_missing_or_garbled_is_empty() {
        let m = parse_mount_manifest("console=ttyS0 panic=1");
        assert!(m.ro.is_none() && m.rw.is_none());
        let bad = parse_mount_manifest("kastellan.mounts=zz");
        assert!(bad.ro.is_none() && bad.rw.is_none());
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
}
