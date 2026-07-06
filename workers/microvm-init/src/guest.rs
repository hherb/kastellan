//! Linux-only guest PID1 mechanism: the real syscalls that mount the guest's
//! pseudo-filesystems and host shares, bring loopback up, accept the host's
//! JSON-RPC bridge over vsock, and finally `exec` the worker. Reached only via
//! `#[cfg(target_os = "linux")] mod guest;` in the crate root, so the whole
//! module (including the [`egress`] submodule) is Linux-only without per-item
//! `#[cfg]` gates.
//!
//! Provenance: lifted verbatim from the former single `main.rs` during the
//! Item 9b prod-split (2026-07-06). The per-fn `#[cfg(target_os = "linux")]`
//! gates were dropped (redundant under the gated `mod guest;`); the entry
//! functions were widened to `pub(crate)` (their caller is `crate::main`); the
//! pure inputs come from [`crate::cmdline`]. The slice-4a egress relay lives in
//! [`egress`].

mod egress;
pub(crate) use egress::{egress_selftest, setup_egress_relay};

use crate::cmdline::{
    anchor_of, bind_prep, parse_env_cmdline, parse_worker_args_cmdline, parse_worker_cmdline,
    vsock_listen_cid_port, BindPrep, MountManifest, VMADDR_CID_ANY,
};
use std::os::unix::io::RawFd;

/// Bring the guest loopback interface (`lo`) UP. A minimal Firecracker guest boots
/// with `lo` DOWN; the matrix worker's in-guest `ProxyBridge` binds and dials
/// `127.0.0.1:<port>`, which fails on a down loopback. Called UNCONDITIONALLY from
/// `main` — it is harmless for workers that never touch loopback (removing a
/// per-worker conditional). Fail-loud to the kernel console but never aborts PID1:
/// read the current flags (SIOCGIFFLAGS), OR in IFF_UP, write back (SIOCSIFFLAGS).
pub(crate) fn bring_loopback_up() {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            eprintln!(
                "microvm-init: loopback socket() failed (errno {})",
                *libc::__errno_location()
            );
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        ifr.ifr_name = crate::cmdline::pack_ifname("lo");
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut ifr) != 0 {
            eprintln!(
                "microvm-init: SIOCGIFFLAGS(lo) failed (errno {})",
                *libc::__errno_location()
            );
            libc::close(fd);
            return;
        }
        // ifr_ifru is a union; ifru_flags is the active member after SIOCGIFFLAGS.
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS, &mut ifr) != 0 {
            eprintln!(
                "microvm-init: SIOCSIFFLAGS(lo) IFF_UP failed (errno {})",
                *libc::__errno_location()
            );
        } else {
            eprintln!("LOOPBACK_UP");
        }
        libc::close(fd);
    }
}

/// Apply the host-dir-share mounts (slice 3). RO drive → /ro-share, then each
/// fs_read root bind-mounted to its absolute path (tmpfs-anchored so mkdir works
/// on the read-only root); RW drive → its mountpoint. Best-effort per mount: a
/// failure is logged to stderr (the kernel console) but does not abort PID1 —
/// the worker simply won't see that path, surfaced as a normal file error.
pub(crate) fn apply_host_mounts(m: &MountManifest) {
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
    for rw in &m.rw {
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
                // Probe the source kind on the mounted RO image (symlink_metadata
                // does not follow links — the staged tree is symlink-free).
                let (is_dir, is_file) = std::fs::symlink_metadata(&from)
                    .map(|m| (m.is_dir(), m.is_file()))
                    .unwrap_or((false, false));
                match bind_prep(is_dir, is_file) {
                    BindPrep::Dir => {
                        // Directory share (slice-3 fs_read root): create the target
                        // dir, then bind. MS_BIND alone is read-only here because the
                        // /ro-share superblock above is MS_RDONLY + the image is
                        // ephemeral with no host write-back.
                        if std::fs::create_dir_all(t).is_ok() {
                            mount(&from, t, None, libc::MS_BIND);
                        }
                    }
                    BindPrep::File => {
                        // Single-file share (the per-instance egress CA): a file bind
                        // needs an existing regular-file target. Make the parent
                        // writable (it may live in the /tmp scratch tmpfs) + touch
                        // the target, then bind. Best-effort: never abort PID1.
                        if let Some(parent) = std::path::Path::new(t).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if std::fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(false)
                            .open(t)
                            .is_ok()
                        {
                            mount(&from, t, None, libc::MS_BIND);
                        }
                    }
                    BindPrep::Skip => {
                        eprintln!("microvm-init: RO source {from} missing; skipping bind of {t}");
                    }
                }
            }
        }
    }

    // RW drives (scratch + persistent): mount each blank/persistent ext4 read-write
    // at its mountpoint. Slice 3 = one scratch drive; slice 5b-2 may add a second
    // persistent drive; every entry is mounted.
    for rw in &m.rw {
        let _ = std::fs::create_dir_all(&rw.mountpoint);
        mount(&rw.dev, &rw.mountpoint, Some("ext4"), 0);
    }
}

pub(crate) fn mount_pseudo_fs() {
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

pub(crate) fn accept_host_bridge() -> RawFd {
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

pub(crate) fn exec_worker() {
    use std::ffi::CString;
    // SAFETY: single-threaded PID1; no other threads to race with.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    // Forwarded worker path (slice 4b) with the slice-1 python-exec bake as the
    // fail-safe fallback, so slices 1–3 (which forward their own python path now,
    // or nothing) still boot a working worker.
    let prog_path = parse_worker_cmdline(&cmdline)
        .unwrap_or_else(|| "/usr/local/bin/kastellan-worker-python-exec".to_string());
    let prog = match CString::new(prog_path) {
        Ok(c) => c,
        Err(_) => CString::new("/usr/local/bin/kastellan-worker-python-exec").unwrap(),
    };
    // Forwarded worker argv (#374). Empty for every worker with
    // `lockdown_shim: None` (today: all of them) — exec runs `prog` bare,
    // byte-identical to slice 4b. A shimmed worker carries [target_binary, …],
    // which the lockdown-exec shim reads from argv[1]. All-or-nothing decode
    // (see parse_worker_args_cmdline): any interior NUL drops the WHOLE arg list
    // and runs `prog` bare rather than feeding the shim a positionally-shifted
    // argv — never aborts PID1.
    let arg_cstrings: Vec<CString> = parse_worker_args_cmdline(&cmdline)
        .into_iter()
        .map(CString::new)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|_| {
            eprintln!("microvm-init: worker arg contained an interior NUL; running with no args");
            Vec::new()
        });
    #[allow(deprecated)]
    unsafe {
        // Baked python interpreter default (harmless for non-python workers,
        // which ignore it); host-forwarded policy.env overrides it.
        std::env::set_var("KASTELLAN_PYTHON_EXEC_PYTHON", "/usr/bin/python3");
        for (k, v) in parse_env_cmdline(&cmdline) {
            std::env::set_var(k, v);
        }
    }
    // execv argv = [program, args…, NULL]. argv[0] is the program itself by
    // convention; for a shimmed worker that's the shim path, args[0] the target.
    let mut argv: Vec<*const libc::c_char> = Vec::with_capacity(arg_cstrings.len() + 2);
    argv.push(prog.as_ptr());
    for c in &arg_cstrings {
        argv.push(c.as_ptr());
    }
    argv.push(std::ptr::null());
    unsafe {
        libc::execv(prog.as_ptr(), argv.as_ptr());
    }
    panic!("execv of worker failed");
}
