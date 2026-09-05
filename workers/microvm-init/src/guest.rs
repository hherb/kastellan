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
pub(crate) use egress::{egress_selftest, mount_run_tmpfs, setup_relay};

use crate::cmdline::{
    anchor_of, bind_prep, parse_env_cmdline, parse_worker_args_cmdline,
    parse_worker_cmdline, vsock_listen_cid_port, worker_owned_paths, BindPrep, MountManifest,
    VMADDR_CID_ANY,
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
        // nosuid + nodev (audit 2026-09-02): agent-authored code writes here.
        mount(&rw.dev, &rw.mountpoint, Some("ext4"), libc::MS_NOSUID | libc::MS_NODEV);
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
        // A corrupt env token is a refused boot, never an un-locked worker
        // (audit 2026-09-02, F2). PID 1 panicking halts the VM; the launcher
        // reports the death and the spawn fails closed on the host.
        let env = match parse_env_cmdline(&cmdline) {
            Ok(env) => env,
            Err(e) => panic!("microvm-init: refusing to exec the worker: {e}"),
        };
        for (k, v) in env {
            std::env::set_var(k, v);
        }
    }
    // execv argv = [program, args…, NULL]. argv[0] is the program itself by
    // convention; for a shimmed worker that's the shim path, args[0] the target.
    drop_privileges_for_worker(&cmdline);
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

/// Leave root before the worker runs (security audit 2026-09-02, workers 2 /
/// prelude F3). `exec_worker` used to `execv` straight from PID 1, so
/// agent-authored Python ran as guest root with every capability: DAC was a
/// no-op inside the VM and **seccomp was the only in-guest gate** — the pinned
/// guest kernel is built without `CONFIG_SECURITY_LANDLOCK`, so the worker-side
/// FS layer has never existed on this path (see
/// `kastellan_sandbox::linux_firecracker::plan::GUEST_LANDLOCK_PROFILE_ENV`).
/// That makes this drop the *second* in-guest layer rather than a third, which
/// is why its failure modes below are fatal.
///
/// `PR_SET_NO_NEW_PRIVS` is set **unconditionally and first**, before the env is
/// even consulted, so the compatibility paths below are still no-new-privs.
///
/// The host then passes the daemon's euid as `KASTELLAN_MICROVM_WORKER_UID`.
/// When present: every path `cmdline::worker_owned_paths` names (the RW
/// mountpoints — rw scratch and persistent store — their share anchors, `/tmp`,
/// and each enabled relay's UDS) is chowned to it, supplementary groups are
/// cleared, then gid and uid are switched. Every step is fatal (PID 1 panics →
/// the VM halts → the spawn fails closed). The chowns are split by role (#670):
/// a relay socket the worker cannot own is fatal, a writable directory it
/// cannot own only warns — see the loop for why.
/// When the variable is absent — a rootfs newer than its host — the worker stays
/// root exactly as before, and says so on stderr, so the two halves can be
/// upgraded in either order without a silent change.
///
/// The numeric uid is deliberately never echoed to stderr or into a panic
/// message: the host chose it (it is the daemon's own euid) and already knows
/// it, and identifiers in log lines are what code scanning flags as cleartext
/// logging. The messages name the env var instead.
fn drop_privileges_for_worker(cmdline: &str) {
    // SAFETY: prctl with immediate arguments; process-wide flag only.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            panic!("microvm-init: PR_SET_NO_NEW_PRIVS failed: {}", std::io::Error::last_os_error());
        }
    }
    let Some(raw) = std::env::var_os(WORKER_UID_ENV) else {
        eprintln!(
            "microvm-init: {WORKER_UID_ENV} not set by the host; worker runs as guest root \
             (upgrade the daemon to drop privileges)"
        );
        return;
    };
    let uid: u32 = raw
        .to_str()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("microvm-init: {WORKER_UID_ENV} is not a uid"));
    if uid == 0 {
        eprintln!("microvm-init: {WORKER_UID_ENV}=0 — worker stays root");
        return;
    }
    // Everything the worker may write — or, for the relay sockets, must be able
    // to CONNECT to — is owned by the uid it will run as. The set is decided by
    // the pure `worker_owned_paths`, which is where the reasoning and the tests
    // live; this loop only applies it.
    //
    // How a failure is treated depends on the path's ROLE, and the two are not
    // equally survivable (#670): a mountpoint the worker cannot own is a
    // degradation — the worker runs and fails on the specific write — but a
    // RELAY SOCKET it cannot own is a total failure, because `connect(2)` needs
    // write permission on the socket file, so that worker dies on its first
    // dial, every time. Only the second halts the VM.
    //
    // This asymmetry was deliberately NOT applied while a panicking PID 1 was
    // illegible: the VM halted and the host saw the same contentless
    // `Protocol(EarlyExit)` the whole defect class hides behind, because the
    // launcher discarded the guest console. Since #666 the launcher captures
    // the console and echoes a tail of it on a boot failure, so the panic text
    // below actually reaches the operator — which is what makes fail-closed the
    // better trade here.
    //
    // The role travels with the path from `worker_owned_paths`; this loop never
    // re-derives it. A second place that knew which paths are sockets is how
    // the first version of this drifted.
    for owned in worker_owned_paths(cmdline) {
        let path = owned.path;
        let c = std::ffi::CString::new(path.clone()).expect("mountpoint has no NUL");
        // SAFETY: chown on a path we own as root; the cstring outlives the call.
        let rc = unsafe { libc::chown(c.as_ptr(), uid, uid) };
        if rc == 0 {
            continue;
        }
        let err = std::io::Error::last_os_error();
        if owned.role.chown_failure_is_fatal() {
            panic!(
                "microvm-init: chown of the relay socket {path} to the worker uid failed: \
                 {err} — the worker could not connect to it, so every dial would fail with a \
                 permission error naming the proxy rather than this. Halting instead."
            );
        }
        eprintln!(
            "microvm-init: chown {path} to the worker uid failed: {err} \
             (the worker may be unable to write there)"
        );
    }
    // SAFETY: plain setgroups/setgid/setuid; each return code is checked.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 {
            panic!("microvm-init: setgroups failed: {}", std::io::Error::last_os_error());
        }
        if libc::setgid(uid) != 0 {
            panic!("microvm-init: setgid to the worker uid failed: {}", std::io::Error::last_os_error());
        }
        if libc::setuid(uid) != 0 {
            panic!("microvm-init: setuid to the worker uid failed: {}", std::io::Error::last_os_error());
        }
        // A successful setuid from root must be irreversible; prove it.
        if libc::setuid(0) == 0 || libc::getuid() != uid || libc::geteuid() != uid {
            panic!("microvm-init: privilege drop to the worker uid did not stick");
        }
    }
    eprintln!("microvm-init: worker privileges dropped to the uid/gid named by {WORKER_UID_ENV}");
}

/// Mirror of `kastellan_sandbox::linux_firecracker::plan::GUEST_WORKER_UID_ENV`
/// (the two crates share no code by design; keep them identical).
const WORKER_UID_ENV: &str = "KASTELLAN_MICROVM_WORKER_UID";
