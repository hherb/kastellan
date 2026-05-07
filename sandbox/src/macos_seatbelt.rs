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
//!   - `Command::process_group(0)` so the worker is in its own process group
//!     (analogue of bwrap's `--new-session`; uses setpgid, not setsid).
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

    /// Run a minimal `sandbox-exec /usr/bin/true` to verify Seatbelt is
    /// healthy on this host. Catches: missing `/usr/bin/sandbox-exec`,
    /// SIP-related Seatbelt scope clipping, profile-syntax regressions in
    /// a future macOS release. Mirrors [`LinuxBwrap::probe`]'s posture so
    /// integration tests can `[SKIP]` rather than false-green when the
    /// platform sandbox is unavailable.
    ///
    /// The probe profile is itself a minimal working allowlist (not a
    /// no-op): without `process-fork`, `process-exec*`, dyld + System
    /// reads, metadata, and `sysctl-read`, even `/usr/bin/true` fails to
    /// launch and the probe spuriously reports "broken Seatbelt" on a
    /// healthy host.
    ///
    /// Note: the probe profile intentionally uses `(subpath "/usr")` and
    /// `(subpath "/System")` — broader than `build_profile`'s narrower
    /// `/usr/lib` + `/usr/libexec` + `/System/Library` rules. See the
    /// comment inside the implementation for the full rationale.
    pub fn probe() -> Result<(), SandboxError> {
        // INTENTIONAL DIVERGENCE from build_profile: this probe profile
        // uses (subpath "/usr") and (subpath "/System") whereas build_profile
        // uses (subpath "/usr/lib") + (subpath "/usr/libexec") +
        // (subpath "/System/Library"). The probe is the *binary* canary
        // ("can sandbox-exec spawn anything?") and should not false-fail on
        // a healthy host because of legitimate /usr/share or /System/Volumes
        // reads. build_profile is intentionally narrower because real
        // workers have a tighter contract. If a future macOS release tightens
        // /usr/bin/true's read set in a way that build_profile doesn't cover,
        // the relevant integration smoke tests (echo_runs_inside_sandbox)
        // will catch the regression — not the probe.
        //
        // The probe profile is a minimal allowlist — not a no-op — so dyld +
        // libsystem can resolve and exec succeeds on a healthy host. Key rules:
        //   (literal "/")        — the root inode itself must be readable for
        //                         the kernel to walk the path to /usr/bin/true;
        //                         without it, exec fails even when every other
        //                         subpath is allowed.
        //   (subpath "/usr")     — binary + dyld shared cache
        //   (subpath "/System")  — System frameworks and dyld closures
        //   mach-lookup          — launchd bootstrap lookups required by dyld
        let profile = "(version 1)\n\
                       (deny default)\n\
                       (allow process-fork)\n\
                       (allow process-exec*)\n\
                       (allow file-read* (literal \"/\"))\n\
                       (allow file-read* (subpath \"/usr\"))\n\
                       (allow file-read* (subpath \"/System\"))\n\
                       (allow file-read-metadata (subpath \"/\"))\n\
                       (allow mach-lookup)\n\
                       (allow sysctl-read)\n";
        let output = Command::new("sandbox-exec")
            .args(["-p", profile, "/usr/bin/true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SandboxError::Backend(format!("could not spawn sandbox-exec: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(SandboxError::Backend(format!(
            "sandbox-exec probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
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

        // macOS Seatbelt resolves symlinks when matching FS rules. /etc, /tmp,
        // and /var are platform symlinks (-> /private/etc, etc.), so a caller
        // passing /etc/hosts would have their (subpath "/etc/hosts") rule
        // ignored by the kernel, which sees /private/etc/hosts. Canonicalize
        // before building the profile. canonicalize() requires the path to
        // exist on disk; for paths that don't (e.g. a fresh scratch dir),
        // fall back to the literal path — the rule still works because
        // those paths typically aren't symlinks themselves.
        let policy = canonicalize_policy_paths(policy);
        let profile = build_profile(&policy);
        let mut cmd = Command::new("sandbox-exec");
        cmd.arg("-p").arg(&profile);
        cmd.arg(program);
        cmd.args(args);

        // bwrap's --clearenv equivalent: clear, then re-apply per-policy env.
        cmd.env_clear();
        for (k, v) in &policy.env {
            cmd.env(k, v);
        }

        // bwrap's --new-session analogue: own process group via setpgid(0, 0)
        // so signals to the parent's process group don't reach the worker.
        // Strictly weaker than `setsid` (new session) but sufficient for our
        // signal-isolation needs; can tighten via a posix_spawn shim later.
        cmd.process_group(0);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        cmd.spawn()
            .map_err(|e| SandboxError::Backend(format!("sandbox-exec spawn failed: {e}")))
    }
}

/// Return a clone of `policy` with each fs_read / fs_write path canonicalized
/// (symlinks resolved). Paths that don't exist yet keep their original form;
/// they typically aren't symlinks anyway, and if they are the caller's
/// problem, the resulting rule will silently fail to grant access — surfaced
/// when the worker hits an EACCES.
fn canonicalize_policy_paths(policy: &SandboxPolicy) -> SandboxPolicy {
    let canon = |paths: &[std::path::PathBuf]| -> Vec<std::path::PathBuf> {
        paths
            .iter()
            .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
            .collect()
    };
    let mut out = policy.clone();
    out.fs_read = canon(&policy.fs_read);
    out.fs_write = canon(&policy.fs_write);
    out
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
    // Root-inode read is required for the kernel path-walk to ANY /usr/...
    // or /bin/... binary, even when the per-subpath read rules below are
    // present. Without this rule, /bin/echo and /usr/bin/true abort with
    // SIGABRT before dyld even runs (empirically confirmed on macOS 26.4
    // ARM64). This is broader than bwrap's --ro-bind /usr — it's a
    // documented consequence of Seatbelt being a MAC layer with no
    // mount-remap counterpart, and the threat-model already flags this
    // asymmetry.
    out.push_str("(allow file-read* (literal \"/\"))\n");
    out.push_str("(allow file-read* (subpath \"/usr/lib\"))\n");
    out.push_str("(allow file-read* (subpath \"/usr/libexec\"))\n");
    out.push_str("(allow file-read* (subpath \"/System/Library\"))\n");
    out.push_str("(allow file-read-metadata (subpath \"/\"))\n");
    out.push_str("(allow sysctl-read)\n");
    // launchd bootstrap lookups required by dyld and libdispatch on newer
    // macOS. Empirically needed for Python, libdispatch users, and any
    // binary calling getpwuid/getgrgid. The unrestricted form (no
    // `global-name` qualifier) is intentional: enumerating every Mach
    // service dyld might query is brittle across macOS versions, and the
    // Mach bootstrap namespace is not itself a privilege-escalation
    // surface beyond what the profile's other rules permit.
    out.push_str("(allow mach-lookup)\n");

    // /dev allowlist: only the safe pseudo-device nodes workers legitimately
    // need. /dev as a whole is NOT allowed (that would expose disk*, bpf*,
    // auditpipe, etc.).
    //
    // tty is intentionally NOT exposed: Linux's bwrap --new-session detaches
    // the controlling terminal so /dev/tty is unusable (ENXIO) by Linux
    // workers anyway, and our process_group(0) uses setpgid(0,0) which does
    // NOT create a new session — granting write access here would create a
    // covert channel to the user's terminal that Linux workers don't have.
    // JSON-RPC workers communicate via stdin/stdout (piped) and have no
    // legitimate use for /dev/tty.
    out.push_str("(allow file-read* file-write* (literal \"/dev/null\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/zero\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/random\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/urandom\"))\n");
    out.push_str("(allow file-read* file-write* (subpath \"/dev/fd\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/dtracehelper\"))\n");

    // Per-policy paths are interpolated as TinyScheme string literals.
    // We do NOT escape `"` or `\` here — `SandboxPolicy` is constructed by
    // trusted core code (`tool_host`), and absolute-path validation in
    // `spawn_under_policy` rules out the most obvious malformed paths.
    // If a future caller starts to pass *untrusted* path inputs through this
    // crate, add an escape-and-validate helper and route both loops through
    // it.
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
            "(allow file-read* (literal \"/\"))",
            "(allow file-read* (subpath \"/usr/lib\"))",
            "(allow file-read* (subpath \"/usr/libexec\"))",
            "(allow file-read* (subpath \"/System/Library\"))",
            "(allow file-read-metadata (subpath \"/\"))",
            "(allow sysctl-read)",
            "(allow mach-lookup)",
        ] {
            assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
        }
    }

    #[test]
    fn dev_allowlist_is_minimal() {
        let p = build_profile(&strict_policy());
        // The six safe /dev nodes must be present (/dev/tty is intentionally absent).
        for needle in [
            "(literal \"/dev/null\")",
            "(literal \"/dev/zero\")",
            "(literal \"/dev/random\")",
            "(literal \"/dev/urandom\")",
            "(subpath \"/dev/fd\")",
            "(literal \"/dev/dtracehelper\")",
        ] {
            assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
        }
        // /dev/tty must NOT be present — it would create a covert channel to
        // the user's terminal that Linux workers (bwrap --new-session) can't use.
        assert!(
            !p.contains("(literal \"/dev/tty\")"),
            "profile must not expose /dev/tty; got:\n{p}"
        );
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

    // This test runs a real sandbox-exec invocation. It only meaningfully runs
    // on macOS hosts; the parent module is cfg(target_os = "macos") so this
    // file isn't compiled elsewhere.
    #[test]
    fn probe_succeeds_on_this_host() {
        MacosSeatbelt::probe().expect("sandbox-exec probe must succeed on a healthy macOS host");
    }

    #[test]
    fn canonicalize_policy_paths_resolves_etc_symlink() {
        // /etc is a symlink to /private/etc on macOS — verify the helper
        // resolves it. /etc/hosts is guaranteed to exist on any macOS host.
        let mut p = strict_policy();
        p.fs_read = vec![PathBuf::from("/etc/hosts")];
        let canon = canonicalize_policy_paths(&p);
        let resolved = &canon.fs_read[0];
        assert_eq!(
            resolved,
            &PathBuf::from("/private/etc/hosts"),
            "canonicalize did not resolve /etc -> /private/etc symlink; got {resolved:?}"
        );
    }

    #[test]
    fn canonicalize_policy_paths_falls_back_for_nonexistent() {
        // /var/lib/hhagent/scratch_xyz_does_not_exist — should keep its literal form.
        let mut p = strict_policy();
        let nonexistent = PathBuf::from("/var/lib/hhagent/scratch_xyz_does_not_exist");
        p.fs_write = vec![nonexistent.clone()];
        let canon = canonicalize_policy_paths(&p);
        assert_eq!(canon.fs_write[0], nonexistent);
    }
}
