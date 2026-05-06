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
//!   - Landlock LSM as a second FS-allowlist layer
//!   - seccomp-bpf syscall filter
//!   - cgroup CPU/memory caps (will use `systemd-run --user` from the supervisor)
//!   - Per-host network allowlist (handled by the egress proxy, not bwrap)

use std::path::PathBuf;
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
        let output = Command::new("bwrap")
            .args([
                "--unshare-user",
                "--ro-bind",
                "/usr",
                "/usr",
                "--symlink",
                "usr/bin",
                "/bin",
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

        if output.status.success() {
            return Ok(());
        }
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
        Err(SandboxError::Backend(format!(
            "bwrap probe failed: {}{hint}",
            stderr.trim()
        )))
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

        let argv = build_argv(policy, program, args);
        let mut cmd = Command::new("bwrap");
        cmd.args(&argv[1..]);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.spawn()
            .map_err(|e| SandboxError::Backend(format!("bwrap spawn failed: {e}")))
    }
}

/// Build the bwrap argv (including the leading `bwrap`) for `program` `args`
/// under `policy`. Pure function, no I/O — exposed so unit tests can assert
/// on the argv shape without spawning a process.
pub fn build_argv(policy: &SandboxPolicy, program: &str, args: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(64);
    argv.push("bwrap".into());

    argv.push("--unshare-all".into());
    if matches!(policy.net, Net::Allowlist(_)) {
        // Allowlist is enforced by the egress proxy on the host side; bwrap just
        // needs to keep the host network namespace so the worker can reach it.
        argv.push("--share-net".into());
    }

    argv.push("--die-with-parent".into());
    argv.push("--new-session".into());
    argv.push("--as-pid-1".into());
    argv.push("--clearenv".into());

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

    argv.push("--".into());
    argv.push(program.into());
    for a in args {
        argv.push((*a).into());
    }
    argv
}

fn push_bind(argv: &mut Vec<String>, flag: &str, path: &PathBuf) {
    let s = path.display().to_string();
    argv.push(flag.into());
    argv.push(s.clone());
    argv.push(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Profile;

    fn strict_policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_read: vec![],
            fs_write: vec![],
            net: Net::Deny,
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::WorkerStrict,
        }
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
        p.fs_write = vec![PathBuf::from("/var/lib/hhagent/scratch")];
        let argv = build_argv(&p, "/bin/true", &[]);
        let joined = argv.join(" ");
        assert!(joined.contains("--bind-try /var/lib/hhagent/scratch /var/lib/hhagent/scratch"));
        assert!(!joined.contains("--ro-bind-try /var/lib/hhagent/scratch"));
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
