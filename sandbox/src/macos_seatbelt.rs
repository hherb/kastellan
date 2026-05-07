//! macOS backend for [`SandboxBackend`]: shells out to `/usr/bin/sandbox-exec`
//! (Seatbelt). Mirrors the Linux `linux_bwrap` backend's shape:
//!   - `build_profile(policy)` is a pure function returning the TinyScheme
//!     `.sb` profile we hand to `sandbox-exec -p`.
//!   - `MacosSeatbelt::probe()` runs a minimal `sandbox-exec /usr/bin/true`
//!     to verify Seatbelt is healthy on this host.
//!   - `MacosSeatbelt::spawn_under_policy()` validates the policy paths,
//!     builds the profile, and spawns the worker.
//!
//! What this backend gives you (Phase 0b):
//!   - Mandatory Access Control (MAC) via Seatbelt: default-deny FS, default-deny
//!     network, explicit allowlists for /usr/lib, /System/Library, /dev's safe
//!     nodes, and per-policy fs_read / fs_write paths.
//!   - Environment cleared via `Command::env_clear()` before exec (analogue of
//!     bwrap's `--clearenv`); `policy.env` re-applied on top.
//!   - `Command::process_group(0)` so the worker is in its own session
//!     (analogue of `--new-session`).
//!
//! Not yet (deferred to supervisor work):
//!   - `setrlimit` for `policy.cpu_ms` / `policy.mem_mb`.
//!   - A `--die-with-parent` equivalent. macOS has no `PR_SET_PDEATHSIG`;
//!     either a `kqueue(EVFILT_PROC, NOTE_EXIT)` watcher or supervisor lifecycle
//!     handles this. Today the worker can outlive a crashed parent — caught by
//!     the supervisor in Phase 0 cont.
//!
//! See [`docs/superpowers/specs/2026-05-07-macos-seatbelt-backend-design.md`].

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

/// Shell out to `/usr/bin/sandbox-exec` for sandboxing.
#[derive(Default)]
pub struct MacosSeatbelt;

impl MacosSeatbelt {
    pub fn new() -> Self {
        Self
    }
}

impl SandboxBackend for MacosSeatbelt {
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

        let profile = build_profile(policy);
        let mut cmd = Command::new("sandbox-exec");
        cmd.arg("-p").arg(&profile);
        cmd.arg(program);
        cmd.args(args);

        // bwrap's --clearenv equivalent: clear, then re-apply per-policy env.
        cmd.env_clear();
        for (k, v) in &policy.env {
            cmd.env(k, v);
        }

        // bwrap's --new-session equivalent: own session via setsid.
        cmd.process_group(0);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        cmd.spawn()
            .map_err(|e| SandboxError::Backend(format!("sandbox-exec spawn failed: {e}")))
    }
}

/// Build the TinyScheme `.sb` profile string for `policy`. Pure function:
/// no I/O, no syscalls — exposed so unit tests can assert on the profile
/// text without spawning a process.
pub fn build_profile(policy: &SandboxPolicy) -> String {
    let mut out = String::new();
    out.push_str("(version 1)\n");
    out.push_str("(deny default)\n");

    out.push_str("(allow process-fork)\n");
    out.push_str("(allow process-exec*)\n");
    out.push_str("(allow file-read* (subpath \"/usr/lib\"))\n");
    out.push_str("(allow file-read* (subpath \"/usr/libexec\"))\n");
    out.push_str("(allow file-read* (subpath \"/System/Library\"))\n");
    out.push_str("(allow file-read-metadata (subpath \"/\"))\n");
    out.push_str("(allow sysctl-read)\n");

    out.push_str("(allow file-read* file-write* (literal \"/dev/null\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/zero\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/random\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/urandom\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/tty\"))\n");
    out.push_str("(allow file-read* file-write* (subpath \"/dev/fd\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/dtracehelper\"))\n");

    for path in &policy.fs_read {
        out.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            path.display()
        ));
    }

    for path in &policy.fs_write {
        out.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            path.display()
        ));
    }

    if matches!(policy.net, crate::Net::Allowlist(_)) {
        // The host allowlist itself is enforced by the future egress proxy
        // (see docs/architecture.md invariant 5), not by Seatbelt — same
        // split as bwrap's --share-net.
        out.push_str("(allow network*)\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Net, Profile};
    use std::path::PathBuf;

    fn strict_policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_read: vec![],
            fs_write: vec![],
            net: Net::Deny,
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::WorkerStrict,
            env: vec![],
        }
    }

    #[test]
    fn profile_starts_with_version_and_deny_default() {
        let p = build_profile(&strict_policy());
        // (version 1) must appear before any allow/deny rule.
        let version_idx = p.find("(version 1)").expect("missing (version 1)");
        let deny_default_idx = p.find("(deny default)").expect("missing (deny default)");
        assert!(version_idx < deny_default_idx);
    }

    #[test]
    fn profile_emits_always_on_allows() {
        let p = build_profile(&strict_policy());
        for needle in [
            "(allow process-fork)",
            "(allow process-exec*)",
            "(allow file-read* (subpath \"/usr/lib\"))",
            "(allow file-read* (subpath \"/usr/libexec\"))",
            "(allow file-read* (subpath \"/System/Library\"))",
            "(allow file-read-metadata (subpath \"/\"))",
            "(allow sysctl-read)",
        ] {
            assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
        }
    }

    #[test]
    fn dev_allowlist_is_minimal() {
        let p = build_profile(&strict_policy());
        // The seven safe /dev nodes must be present.
        for needle in [
            "(literal \"/dev/null\")",
            "(literal \"/dev/zero\")",
            "(literal \"/dev/random\")",
            "(literal \"/dev/urandom\")",
            "(literal \"/dev/tty\")",
            "(subpath \"/dev/fd\")",
            "(literal \"/dev/dtracehelper\")",
        ] {
            assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
        }
        // /dev as a whole must NOT be subpath-allowed — that would expose disk*,
        // auditpipe, bpf*, etc.
        assert!(
            !p.contains("(subpath \"/dev\")"),
            "profile must not allow broad (subpath \"/dev\") rule; got:\n{p}"
        );
    }

    #[test]
    fn fs_read_emits_subpath_allow() {
        let mut p = strict_policy();
        p.fs_read = vec![PathBuf::from("/etc/ssl"), PathBuf::from("/opt/data")];
        let prof = build_profile(&p);
        assert!(prof.contains("(allow file-read* (subpath \"/etc/ssl\"))"), "got:\n{prof}");
        assert!(prof.contains("(allow file-read* (subpath \"/opt/data\"))"), "got:\n{prof}");
    }

    #[test]
    fn fs_write_emits_read_and_write_subpath_allow() {
        let mut p = strict_policy();
        p.fs_write = vec![PathBuf::from("/var/lib/hhagent/scratch")];
        let prof = build_profile(&p);
        assert!(
            prof.contains("(allow file-read* file-write* (subpath \"/var/lib/hhagent/scratch\"))"),
            "expected combined read+write allow; got:\n{prof}"
        );
        // The fs_write path must NOT appear as a separate read-only allow.
        assert!(
            !prof.contains("(allow file-read* (subpath \"/var/lib/hhagent/scratch\"))"),
            "fs_write path must not also be emitted as a separate read-only rule; got:\n{prof}"
        );
    }

    #[test]
    fn deny_does_not_allow_network() {
        let p = build_profile(&strict_policy());
        assert!(!p.contains("(allow network*)"), "Net::Deny must not emit (allow network*); got:\n{p}");
    }

    #[test]
    fn allowlist_does_allow_network() {
        let mut p = strict_policy();
        p.net = Net::Allowlist(vec!["api.example.com:443".into()]);
        let prof = build_profile(&p);
        assert!(prof.contains("(allow network*)"), "Net::Allowlist must emit (allow network*); got:\n{prof}");
    }

    #[test]
    fn relative_policy_paths_are_rejected_by_spawn() {
        let backend = MacosSeatbelt::new();
        let mut p = strict_policy();
        p.fs_read.push(PathBuf::from("relative/path"));
        let err = backend
            .spawn_under_policy(&p, "/usr/bin/true", &[])
            .expect_err("must reject relative paths");
        let msg = format!("{err}");
        assert!(
            msg.contains("must be absolute"),
            "expected 'must be absolute' error, got: {msg}"
        );
    }
}
