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

/// Returns the (cid, port) pair the guest vsock listener should bind to.
/// Pure function — no syscalls — so it is unit-testable on any platform.
#[allow(dead_code)]
fn vsock_listen_cid_port() -> (u32, u32) {
    (VMADDR_CID_ANY, WORKER_VSOCK_PORT)
}

// ── Linux-only: real syscall implementations ──────────────────────────────────

#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;

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
}
