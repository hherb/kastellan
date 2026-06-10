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
            if !p.is_absolute() {
                return Err(SandboxError::Backend(format!(
                    "policy paths must be absolute, got {p:?}"
                )));
            }
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
    fn separator_then_program_then_args() {
        let argv = build_argv(&strict_policy(), "/bin/echo", &["hello", "world"]);
        let i = argv.iter().position(|s| s == "--").expect("missing --");
        assert_eq!(argv[i + 1], "/bin/echo");
        assert_eq!(argv[i + 2], "hello");
        assert_eq!(argv[i + 3], "world");
    }
}
