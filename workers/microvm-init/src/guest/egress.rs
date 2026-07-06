//! Slice-4a in-guest egress relay: the guest→host reverse tunnel that lets a
//! sandboxed worker reach the host egress proxy without a direct route. The
//! worker dials the in-guest UDS ([`GUEST_EGRESS_UDS`]); this relay pipes every
//! accepted connection to the host over `AF_VSOCK(VMADDR_CID_HOST,
//! EGRESS_VSOCK_PORT)`, which firecracker forwards to the launcher's
//! reverse-relay listener.
//!
//! Provenance: lifted verbatim from the former single `main.rs` during the
//! Item 9b prod-split (2026-07-06). The per-fn `#[cfg(target_os = "linux")]`
//! gates were dropped — this whole module is reached only through
//! `#[cfg(target_os = "linux")] mod guest;` in the crate root, so they were
//! redundant. Only [`setup_egress_relay`] and [`egress_selftest`] are widened to
//! `pub(crate)` (their sole caller is `crate::main`); the socket helpers stay
//! module-private.

use crate::cmdline::{EGRESS_VSOCK_PORT, GUEST_EGRESS_UDS, VMADDR_CID_HOST};
use std::os::unix::io::RawFd;

/// Slice 4a: stand up the in-guest egress relay. Mount a writable `/run` tmpfs,
/// bind the in-guest UDS the worker dials, and fork a child that pipes every
/// accepted UDS connection to the host over `AF_VSOCK(VMADDR_CID_HOST,
/// EGRESS_VSOCK_PORT)` (firecracker forwards that to the launcher's reverse-relay
/// listener at `<base>_<port>`, which dials the real host egress proxy). Bind
/// happens in the parent BEFORE `exec`, so the worker can never dial before the
/// listener exists. Best-effort: a failure logs and returns (the worker then
/// fails its first dial, surfaced as a normal error — PID1 is never aborted).
pub(crate) fn setup_egress_relay() {
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
    if pid < 0 {
        eprintln!("microvm-init: fork for egress relay failed; worker egress disabled");
        unsafe { libc::close(listener) };
        return;
    }
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
fn egress_relay_loop(listener: RawFd) {
    loop {
        let conn = unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn < 0 {
            let err = unsafe { *libc::__errno_location() };
            if err == libc::EINTR {
                continue;
            }
            eprintln!("microvm-init: egress relay accept failed (errno {err}); relay exiting");
            break;
        }
        // Service each accepted connection on its own thread so concurrent worker
        // egress connections don't serialize behind one another (mirrors the
        // host-side reverse-relay in `microvm-run::egress_relay`). A worker that
        // opens two simultaneous proxy connections would otherwise hang the second
        // in the listen backlog until the first closed.
        std::thread::spawn(move || relay_one_connection(conn));
    }
}

/// Pump one accepted in-guest UDS connection to the host over vsock and back.
/// Takes ownership of `conn` (closes it on return).
fn relay_one_connection(conn: RawFd) {
    match connect_host_vsock(VMADDR_CID_HOST, EGRESS_VSOCK_PORT) {
        Some(vfd) => {
            // conn/vfd are RawFd (Copy); both directions run concurrently on
            // the same full-duplex sockets.
            let up = std::thread::spawn(move || pump_raw(conn, vfd));
            pump_raw(vfd, conn);
            // Force-shut both fds before joining so the sibling pump's blocking
            // `read` can never hang `join` — covers the case where the inline
            // pump exits via a write error (which alone leaves the peer read
            // unblocked). Idempotent after the EOF-path half-close.
            unsafe {
                libc::shutdown(conn, libc::SHUT_RDWR);
                libc::shutdown(vfd, libc::SHUT_RDWR);
            }
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

/// One-direction raw-fd byte copy until EOF/err; half-closes the writer on EOF.
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
                // Write side is broken: half-close the writer so the peer pump
                // sees EOF (mirrors the read-EOF path above) before bailing.
                unsafe { libc::shutdown(to_fd, libc::SHUT_WR) };
                return;
            }
            off += w;
        }
    }
}

/// Slice 4a self-test: connect our own in-guest UDS, write `PING`, expect `PONG`.
/// Proves the full guest→host reverse path on real KVM. Logs `EGRESS_CHANNEL_OK`
/// to the kernel console on success. Best-effort; never aborts PID1.
pub(crate) fn egress_selftest() {
    let Some(fd) = connect_unix(GUEST_EGRESS_UDS) else {
        eprintln!("microvm-init: egress selftest connect failed");
        return;
    };
    let ping = b"PING\n";
    unsafe { libc::write(fd, ping.as_ptr() as *const libc::c_void, ping.len()) };
    let mut buf = [0u8; 16];
    // Retry across EINTR so a stray signal during boot can't produce a false
    // "no PONG" on a healthy channel — this log is the operator's certification.
    let n = loop {
        let r = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if r < 0 && unsafe { *libc::__errno_location() } == libc::EINTR {
            continue;
        }
        break r;
    };
    unsafe { libc::close(fd) };
    if n >= 4 && &buf[..4] == b"PONG" {
        eprintln!("EGRESS_CHANNEL_OK");
    } else {
        eprintln!("microvm-init: egress selftest got no PONG (n={n})");
    }
}
