//! Linux backend for [`SandboxBackend`]: shells out to `bwrap` (bubblewrap).
//!
//! What this backend gives you (Phase 0):
//!   - User / IPC / PID / UTS / cgroup namespace isolation (`--unshare-all`)
//!   - Network namespace isolation when [`Net::Deny`] (no net at all)
//!   - Filesystem isolation: only `/usr` and the caller-listed paths are visible;
//!     `/proc`, `/dev`, `/tmp` are minimal/tmpfs
//!   - `--die-with-parent` so leaks can't outlive the supervisor
//!   - `--new-session` so the worker can't read/write the parent's TTY
//!   - `--as-pid-1` so signals stay contained
//!   - `--clearenv` so host env vars don't leak in
//!
//! Not yet (deferred to Phase 0 hardening, tracked in `docs/threat-model.md`):
//!   - Per-host network allowlist (handled by the egress proxy, not bwrap)
//!
//! Wired in from sibling modules:
//!   - Landlock LSM as a second FS-allowlist layer — `workers/prelude::landlock_lock`
//!   - seccomp-bpf syscall filter — `workers/prelude::seccomp_lock`
//!   - cgroup v2 CPU/memory/tasks caps — [`crate::linux_cgroup`], wrapped
//!     **outside** `bwrap` here so the cgroup is in place before the
//!     unshare-all namespace is created.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::{Net, SandboxBackend, SandboxError, SandboxPolicy};

/// Shell out to `bwrap` for sandboxing. Assumes `bwrap` is on `$PATH`.
#[derive(Default)]
pub struct LinuxBwrap;

impl LinuxBwrap {
    pub fn new() -> Self {
        Self
    }

    /// Run a trivial bwrap invocation and report whether the kernel will
    /// actually let us create an unprivileged user namespace. This catches
    /// the common Ubuntu 24.04 AppArmor restriction
    /// (`kernel.apparmor_restrict_unprivileged_userns=1`) before we try to
    /// spawn real workers.
    pub fn probe() -> Result<(), SandboxError> {
        // The probe runs `/usr/bin/true` inside a minimal jail, so it must
        // also stand up enough of the FS for the dynamic linker to resolve
        // (`ld-linux-*.so` lives under `/lib*` on most distros, and `/lib`
        // is itself a symlink to `usr/lib` on merged-/usr systems). Without
        // these symlinks, `execvp` returns ENOENT *not* because the binary
        // is missing but because its loader is — which is what we hit on
        // Ubuntu 24.04+ before this fix and which masked broken probes as
        // "kernel restricts userns" false positives.
        let output = Command::new("bwrap")
            .args([
                "--unshare-user",
                "--ro-bind",
                "/usr",
                "/usr",
                "--symlink",
                "usr/bin",
                "/bin",
                "--symlink",
                "usr/sbin",
                "/sbin",
                "--symlink",
                "usr/lib",
                "/lib",
                "--symlink",
                "usr/lib64",
                "/lib64",
                "--ro-bind-try",
                "/etc/ld.so.cache",
                "/etc/ld.so.cache",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "/usr/bin/true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SandboxError::Backend(format!("could not spawn bwrap: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let hint = if stderr.contains("setting up uid map")
                || stderr.contains("Operation not permitted")
                || stderr.contains("RTM_NEWADDR")
            {
                "\n\nThis kernel is restricting unprivileged user namespaces (Ubuntu 24.04+ default).\n\
                 Install the AppArmor profile: scripts/linux/install-bwrap-apparmor-profile.sh \
                 (one-time, sudo required)."
            } else {
                ""
            };
            return Err(SandboxError::Backend(format!(
                "bwrap probe failed: {}{hint}",
                stderr.trim()
            )));
        }

        // Defense-in-depth requires the cgroup ceiling layer too: a
        // failed probe here means we can't enforce MemoryMax / CPUQuota
        // / TasksMax, so the sandbox contract is degraded. Fail closed
        // — `LinuxBwrap::probe` is `Ok` only when *all* layers are
        // available.
        crate::linux_cgroup::cgroup_probe()?;
        Ok(())
    }
}

impl SandboxBackend for LinuxBwrap {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<Child, SandboxError> {
        for p in policy.fs_read.iter().chain(policy.fs_write.iter()) {
            crate::validate_linux_bind_path(p, "policy")?;
        }
        // The proxy UDS is bound into the jail too (force-routing); hold it to
        // the same absolute + no-`..` rule (issue #387).
        if let Some(uds) = &policy.proxy_uds {
            crate::validate_linux_bind_path(uds, "proxy_uds")?;
        }
        // The embed-broker UDS is bound into the jail the same way; same absolute
        // + no-`..` rule (issue #387).
        if let Some(uds) = &policy.embed_broker_uds {
            crate::validate_linux_bind_path(uds, "embed_broker_uds")?;
        }
        // Slice 5b-2: the persistent store is a separate field, so it bypasses the
        // loop above. Its paths must be absolute (a relative dest mis-binds against
        // the jail cwd), and we create host_backing up front so the `--bind` below
        // is fail-closed: a missing/unwritable store dir errors here instead of the
        // `--bind-try` silently dropping the bind and writes vanishing on respawn.
        if let Some(ps) = &policy.persistent_store {
            for p in [&ps.host_backing, &ps.guest_mount] {
                crate::validate_linux_bind_path(p, "persistent_store")?;
            }
            // host_backing is a DIRECTORY on this backend — it is an ext4 image
            // FILE only on Firecracker (see `PersistentStore` doc). If a regular
            // file already exists at the path (e.g. a policy built for the
            // Firecracker backend was routed here), `create_dir_all` would fail
            // with an opaque "File exists"; reject up front with the cross-backend
            // hint instead.
            if ps.host_backing.is_file() {
                return Err(SandboxError::Backend(format!(
                    "persistent_store host_backing {:?} is a file, but the bwrap backend expects a \
                     directory (a file is the Firecracker ext4-image form)",
                    ps.host_backing
                )));
            }
            std::fs::create_dir_all(&ps.host_backing).map_err(|e| {
                SandboxError::Backend(format!(
                    "persistent_store host_backing {:?}: {e}",
                    ps.host_backing
                ))
            })?;
        }

        let bwrap_argv = build_argv(policy, program, args);
        // systemd-run is the **outer** process; it sets up the cgroup
        // before bwrap creates the unshare-all namespace. Final shape:
        //   systemd-run --user --scope ... -- bwrap --unshare-all ... -- <program> <args>
        let cgroup_argv = crate::linux_cgroup::build_systemd_run_argv(policy);

        let mut cmd = Command::new(&cgroup_argv[0]);
        cmd.args(&cgroup_argv[1..]);
        cmd.args(&bwrap_argv);

        // stdin is piped so workers speaking JSON-RPC over stdio can be driven
        // by the core. Workers that don't read stdin (one-shot commands like
        // /usr/bin/echo in tests) simply ignore the open pipe.
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.spawn()
            .map_err(|e| SandboxError::Backend(format!("systemd-run+bwrap spawn failed: {e}")))
    }
}

/// Build the bwrap argv (including the leading `bwrap`) for `program` `args`
/// under `policy`. Pure function, no I/O — exposed so unit tests can assert
/// on the argv shape without spawning a process.
pub fn build_argv(policy: &SandboxPolicy, program: &str, args: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(64);
    argv.push("bwrap".into());

    argv.push("--unshare-all".into());
    match (&policy.net, &policy.proxy_uds) {
        // Force-routed worker: private netns (no route out); only the bound
        // proxy UDS reaches the host. AF_UNIX is mount-ns-scoped, not net-ns.
        (Net::Allowlist(_), Some(_uds)) => { /* no --share-net: keep --unshare-all's private netns */ }
        // The proxy itself, or legacy Allowlist without a proxy: real netns.
        (Net::ProxyEgress, _) | (Net::Allowlist(_), None) => argv.push("--share-net".into()),
        (Net::Deny, _) => { /* no net */ }
    }

    argv.push("--die-with-parent".into());
    argv.push("--new-session".into());
    argv.push("--as-pid-1".into());
    argv.push("--clearenv".into());

    for (k, v) in &policy.env {
        argv.push("--setenv".into());
        argv.push(k.clone());
        argv.push(v.clone());
    }

    argv.extend(["--proc".into(), "/proc".into()]);
    argv.extend(["--dev".into(), "/dev".into()]);
    argv.extend(["--tmpfs".into(), "/tmp".into()]);

    argv.extend(["--ro-bind".into(), "/usr".into(), "/usr".into()]);
    argv.extend(["--symlink".into(), "usr/bin".into(), "/bin".into()]);
    argv.extend(["--symlink".into(), "usr/sbin".into(), "/sbin".into()]);
    argv.extend(["--symlink".into(), "usr/lib".into(), "/lib".into()]);
    argv.extend(["--symlink".into(), "usr/lib64".into(), "/lib64".into()]);
    argv.extend([
        "--ro-bind-try".into(),
        "/etc/ld.so.cache".into(),
        "/etc/ld.so.cache".into(),
    ]);

    for path in &policy.fs_read {
        push_bind(&mut argv, "--ro-bind-try", path);
    }
    for path in &policy.fs_write {
        push_bind(&mut argv, "--bind-try", path);
    }
    if let Some(uds) = &policy.proxy_uds {
        // Bind the proxy UDS rw at an identical host↔jail path.
        // AF_UNIX connect requires write permission on the inode; `--bind`
        // (not `--ro-bind`) gives the worker that permission while keeping
        // the path identical so no path rewrite is needed inside the jail.
        push_bind(&mut argv, "--bind", uds);
    }
    if let Some(uds) = &policy.embed_broker_uds {
        // Bind the embed-broker UDS rw at an identical host↔jail path — same
        // rationale as proxy_uds above (AF_UNIX connect needs write on the
        // inode). Independent of the netns match: the worker may or may not be
        // force-routed, but reaching the broker socket only needs the bind.
        push_bind(&mut argv, "--bind", uds);
    }

    // Slice 5b-2: a persistent store is a RW bind from a stable host dir to the
    // jail's guest_mount (distinct paths, so not push_bind which uses one path).
    // `--bind` (not `--bind-try`): spawn_under_policy created host_backing, so a
    // failed bind is a real error and must fail closed — a silent skip would drop
    // every write and defeat the cross-respawn persistence guarantee.
    if let Some(ps) = &policy.persistent_store {
        argv.push("--bind".into());
        argv.push(ps.host_backing.display().to_string());
        argv.push(ps.guest_mount.display().to_string());
    }

    argv.push("--".into());
    argv.push(program.into());
    for a in args {
        argv.push((*a).into());
    }
    argv
}

fn push_bind(argv: &mut Vec<String>, flag: &str, path: &Path) {
    let s = path.display().to_string();
    argv.push(flag.into());
    argv.push(s.clone());
    argv.push(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn strict_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    #[test]
    fn argv_starts_with_bwrap_and_unshare_all() {
        let argv = build_argv(&strict_policy(), "/bin/echo", &["hi"]);
        assert_eq!(argv[0], "bwrap");
        assert!(argv.contains(&"--unshare-all".into()));
        assert!(argv.contains(&"--die-with-parent".into()));
        assert!(argv.contains(&"--new-session".into()));
        assert!(argv.contains(&"--clearenv".into()));
    }

    #[test]
    fn deny_does_not_share_net() {
        let argv = build_argv(&strict_policy(), "/bin/true", &[]);
        assert!(!argv.contains(&"--share-net".into()));
    }

    #[test]
    fn allowlist_does_share_net() {
        let mut p = strict_policy();
        p.net = Net::Allowlist(vec!["api.example.com:443".into()]);
        let argv = build_argv(&p, "/bin/true", &[]);
        assert!(argv.contains(&"--share-net".into()));
    }

    #[test]
    fn proxy_egress_shares_net_like_allowlist() {
        let p = SandboxPolicy {
            net: Net::ProxyEgress,
            ..SandboxPolicy::default()
        };
        let argv = build_argv(&p, "/bin/true", &[]);
        assert!(argv.contains(&"--share-net".into()));
    }

    #[test]
    fn fs_read_uses_ro_bind_try() {
        let mut p = strict_policy();
        p.fs_read = vec![PathBuf::from("/etc/ssl")];
        let argv = build_argv(&p, "/bin/true", &[]);
        let joined = argv.join(" ");
        assert!(joined.contains("--ro-bind-try /etc/ssl /etc/ssl"));
    }

    #[test]
    fn fs_write_uses_bind_try() {
        let mut p = strict_policy();
        p.fs_write = vec![PathBuf::from("/var/lib/kastellan/scratch")];
        let argv = build_argv(&p, "/bin/true", &[]);
        let joined = argv.join(" ");
        assert!(joined.contains("--bind-try /var/lib/kastellan/scratch /var/lib/kastellan/scratch"));
        assert!(!joined.contains("--ro-bind-try /var/lib/kastellan/scratch"));
    }

    #[test]
    fn allowlist_with_proxy_uds_uses_private_netns_and_binds_socket() {
        let p = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            proxy_uds: Some(PathBuf::from("/scratch/egress.sock")),
            ..SandboxPolicy::default()
        };
        let argv = build_argv(&p, "/bin/worker", &[]);
        // No host-net sharing — private netns only.
        assert!(!argv.contains(&"--share-net".to_string()),
            "Net::Allowlist with proxy_uds must NOT --share-net; got: {argv:?}");
        // The proxy UDS is bind-mounted in (rw) at an identical path. Scan for the
        // `--bind <src> <dst>` triple matching the UDS anywhere in argv — do NOT
        // assume the *first* `--bind` is the proxy socket: fs_write binds can
        // precede it, so a `position(|a| a == "--bind")` on the first match would
        // assert against the wrong entry once a worker has fs_write paths.
        let has_uds_bind = argv.windows(3).any(|w| {
            w[0] == "--bind" && w[1] == "/scratch/egress.sock" && w[2] == "/scratch/egress.sock"
        });
        assert!(has_uds_bind,
            "proxy UDS must be --bind'd at an identical host↔jail path; got: {argv:?}");
    }

    #[test]
    fn embed_broker_uds_is_bound_without_touching_netns() {
        // The embed-broker UDS is an *additional* bound socket, orthogonal to the
        // egress netns decision: a worker in the legacy `--share-net` Allowlist mode
        // (no proxy_uds) still keeps `--share-net`, and the broker socket is bound in
        // rw at an identical host↔jail path. AF_UNIX is mount-ns-scoped, so the bind
        // works regardless of the net policy.
        let p = SandboxPolicy {
            net: Net::Allowlist(vec!["searx.example.org:443".into()]),
            embed_broker_uds: Some(PathBuf::from("/scratch/embed.sock")),
            ..SandboxPolicy::default()
        };
        let argv = build_argv(&p, "/bin/worker", &[]);
        // Broker UDS present ⇒ still legacy share-net (embed_broker_uds must NOT
        // flip the netns like proxy_uds does).
        assert!(argv.contains(&"--share-net".to_string()),
            "embed_broker_uds must not change the netns decision; got: {argv:?}");
        let has_uds_bind = argv.windows(3).any(|w| {
            w[0] == "--bind" && w[1] == "/scratch/embed.sock" && w[2] == "/scratch/embed.sock"
        });
        assert!(has_uds_bind,
            "embed broker UDS must be --bind'd at an identical host↔jail path; got: {argv:?}");
    }

    #[test]
    fn embed_broker_uds_binds_under_force_routed_private_netns() {
        // A force-routed worker (Net::Allowlist + proxy_uds ⇒ private netns) that
        // also reaches a broker must bind BOTH sockets and keep the private netns.
        let p = SandboxPolicy {
            net: Net::Allowlist(vec!["searx.example.org:443".into()]),
            proxy_uds: Some(PathBuf::from("/scratch/egress.sock")),
            embed_broker_uds: Some(PathBuf::from("/scratch/embed.sock")),
            ..SandboxPolicy::default()
        };
        let argv = build_argv(&p, "/bin/worker", &[]);
        assert!(!argv.contains(&"--share-net".to_string()),
            "force-routed worker keeps private netns even with a broker socket; got: {argv:?}");
        let bound = |sock: &str| {
            argv.windows(3).any(|w| w[0] == "--bind" && w[1] == sock && w[2] == sock)
        };
        assert!(bound("/scratch/egress.sock"), "proxy UDS must still be bound; got: {argv:?}");
        assert!(bound("/scratch/embed.sock"), "broker UDS must be bound; got: {argv:?}");
    }

    #[test]
    fn allowlist_without_proxy_uds_keeps_legacy_share_net() {
        let p = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            // proxy_uds = None (default)
            ..SandboxPolicy::default()
        };
        let argv = build_argv(&p, "/bin/worker", &[]);
        assert!(argv.contains(&"--share-net".to_string()),
            "legacy Allowlist (no proxy_uds) keeps --share-net; got: {argv:?}");
    }

    #[test]
    fn proxy_egress_still_shares_net() {
        let p = SandboxPolicy {
            net: Net::ProxyEgress,
            ..SandboxPolicy::default()
        };
        let argv = build_argv(&p, "/bin/proxy", &[]);
        assert!(argv.contains(&"--share-net".to_string()));
    }

    #[test]
    fn persistent_store_bind_maps_host_backing_to_guest_mount() {
        let mut policy = strict_policy();
        policy.persistent_store = Some(crate::PersistentStore {
            host_backing: std::path::PathBuf::from("/srv/kv-state"),
            guest_mount: std::path::PathBuf::from("/data"),
            size_mib: 0,
        });
        let argv = build_argv(&policy, "/bin/true", &[]);
        // a fail-closed `--bind` with DISTINCT host/jail paths (not the same-path
        // push_bind, and not `--bind-try` which would silently drop a missing store)
        let i = argv.iter().position(|a| a == "/srv/kv-state").unwrap();
        assert_eq!(argv[i - 1], "--bind");
        assert_eq!(argv[i + 1], "/data");
    }

    #[test]
    fn persistent_store_rejects_file_host_backing() {
        // host_backing is a DIRECTORY on bwrap; a regular file (the Firecracker
        // ext4-image form, e.g. a policy routed to the wrong backend) must be
        // rejected with a clear cross-backend hint before `create_dir_all` fails
        // opaquely with "File exists".
        let f = std::env::temp_dir().join(format!("kv-host-file-{}.ext4", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        let mut policy = strict_policy();
        policy.persistent_store = Some(crate::PersistentStore {
            host_backing: f.clone(),
            guest_mount: PathBuf::from("/data"),
            size_mib: 0,
        });
        let err = LinuxBwrap
            .spawn_under_policy(&policy, "/bin/true", &[])
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("is a file"),
            "file host_backing must be rejected with a cross-backend hint: {err:?}"
        );
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn embed_broker_uds_relative_path_is_rejected() {
        // The bind-path validation must fire for embed_broker_uds too (a relative
        // dest mis-binds against the jail cwd, issue #387). Pins that
        // spawn_under_policy actually calls the validator for this field with the
        // right `kind` label — a refactor that drops the call would regress here
        // before ever reaching bwrap.
        let mut policy = strict_policy();
        policy.embed_broker_uds = Some(PathBuf::from("relative/embed.sock"));
        let err = LinuxBwrap
            .spawn_under_policy(&policy, "/bin/true", &[])
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("embed_broker_uds") && msg.contains("absolute"),
            "relative embed_broker_uds must be rejected with the field label: {msg}"
        );
    }

    #[test]
    fn embed_broker_uds_parent_dir_component_is_rejected() {
        // The no-`..` rule (issue #387) applies to embed_broker_uds too: a
        // traversal component must be rejected up front.
        let mut policy = strict_policy();
        policy.embed_broker_uds = Some(PathBuf::from("/scratch/../etc/embed.sock"));
        let err = LinuxBwrap
            .spawn_under_policy(&policy, "/bin/true", &[])
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("embed_broker_uds") && msg.contains(".."),
            "embed_broker_uds with a '..' component must be rejected: {msg}"
        );
    }

    #[test]
    fn separator_then_program_then_args() {
        let argv = build_argv(&strict_policy(), "/bin/echo", &["hello", "world"]);
        let i = argv.iter().position(|s| s == "--").expect("missing --");
        assert_eq!(argv[i + 1], "/bin/echo");
        assert_eq!(argv[i + 2], "hello");
        assert_eq!(argv[i + 3], "world");
    }
}
