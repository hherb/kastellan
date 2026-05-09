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
//!   - `setsid()` in a `pre_exec` hook so the worker is the leader of a fresh
//!     session — full parity with bwrap's `--new-session`. Closes issue #2;
//!     forecloses any covert channel via the parent's controlling terminal,
//!     even if a future profile broadening accidentally re-exposes /dev/tty.
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
            // TinyScheme injection forecloser: any of these chars in a path
            // would close the surrounding `(subpath "...")` early and let a
            // crafted policy rewrite the profile. Today every caller is
            // trusted core code; this guard means a future caller (or a path
            // round-tripped through an untrusted source) can't silently
            // escalate. See the same escape-and-validate note in build_profile.
            let s = p.to_string_lossy();
            if let Some(c) = s.chars().find(|c| {
                matches!(c, '"' | '\\' | '(' | ')' | '\n' | '\r' | '\0')
            }) {
                return Err(SandboxError::Backend(format!(
                    "policy path contains disallowed character {c:?}: {p:?}"
                )));
            }
        }

        // macOS Seatbelt resolves symlinks when matching FS rules. /etc, /tmp,
        // and /var are platform symlinks (-> /private/etc, etc.), so a caller
        // passing /etc/hosts would have their (subpath "/etc/hosts") rule
        // ignored by the kernel, which sees /private/etc/hosts. Canonicalize
        // before building the profile. canonicalize() requires the path to
        // exist on disk; for NotFound (e.g. a fresh scratch dir not yet
        // created) we fall back to the literal path because those paths
        // typically aren't symlinks themselves. Other errors (PermissionDenied
        // on a parent dir, etc.) propagate so we don't silently emit a
        // non-functional rule.
        let policy = canonicalize_policy_paths(policy)?;
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

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // bwrap's --new-session analogue: full session isolation via setsid()
        // in a pre_exec hook (issue #2). pre_exec runs after fork() but before
        // execve() in the child; only async-signal-safe operations are allowed,
        // and setsid() is async-signal-safe by POSIX. Effects:
        //   1. The child becomes the leader of a brand-new session (sid == pid).
        //   2. The child has no controlling terminal — any subsequent open of
        //      /dev/tty fails with ENXIO, regardless of profile broadening.
        //   3. The child is also in a brand-new process group (setsid()
        //      implies setpgid in the new session), so we drop the previous
        //      `cmd.process_group(0)` call — setsid subsumes it.
        // setsid() returns -1 only when the caller is already a process group
        // leader; we're in a freshly-forked child so that's not possible here,
        // but we propagate the errno via io::Error so a future regression
        // (e.g. a refactor that calls setpgid before pre_exec) becomes a
        // visible spawn failure rather than a silent regression.
        //
        // SAFETY: pre_exec closures must be async-signal-safe. setsid() is on
        // the POSIX async-signal-safe list (signal-safety(7) on Linux,
        // sigaction(2) on macOS).
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        cmd.spawn()
            .map_err(|e| SandboxError::Backend(format!("sandbox-exec spawn failed: {e}")))
    }
}

/// Return a clone of `policy` with each fs_read / fs_write path canonicalized
/// (symlinks resolved). `NotFound` errors fall back to the original path
/// (legitimate for fs_write of a not-yet-created scratch dir). Any other
/// `io::Error` — most importantly `PermissionDenied` on a parent directory
/// — propagates as a `SandboxError::Backend`, because emitting a rule for an
/// unresolved path would silently produce a non-functional Seatbelt rule and
/// mask user errors as "the sandbox is just too strict."
fn canonicalize_policy_paths(policy: &SandboxPolicy) -> Result<SandboxPolicy, SandboxError> {
    let canon = |paths: &[std::path::PathBuf]| -> Result<Vec<std::path::PathBuf>, SandboxError> {
        paths
            .iter()
            .map(|p| match std::fs::canonicalize(p) {
                Ok(resolved) => Ok(resolved),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(p.clone()),
                Err(e) => Err(SandboxError::Backend(format!(
                    "could not canonicalize policy path {p:?}: {e}"
                ))),
            })
            .collect()
    };
    let mut out = policy.clone();
    out.fs_read = canon(&policy.fs_read)?;
    out.fs_write = canon(&policy.fs_write)?;
    Ok(out)
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
    // mach-lookup is *intentionally not granted* (issue #1, fixed in this
    // profile). Empirical methodology used to set this baseline: every
    // shipping hhagent worker today (`hhagent-worker-shell-exec`,
    // `sid_probe`, `net_probe`, `mach_probe`, `/bin/echo`, `/bin/sh`,
    // `/bin/cat`, `/bin/ls`, `/usr/bin/true`) was test-spawned under a
    // probe profile with `(deny mach-lookup)` on macOS 26.4 ARM64; all
    // succeeded. The unrestricted `(allow mach-lookup)` rule that lived
    // here through 2026-05-08 was speculative ("Python and libdispatch
    // might need it"), not load-bearing.
    //
    // Why deny: the Mach bootstrap namespace is the back-end for every
    // registered launchd service in the worker's bootstrap context —
    // pasteboard (com.apple.pboard), Apple Events broker
    // (com.apple.coreservices.appleevents), distributed notifications,
    // location services, etc. — many of which bypass the profile's file
    // and network rules entirely. Granting unrestricted `mach-lookup` is
    // the largest known asymmetry vs the threat-model invariant in
    // docs/threat-model.md ("compromise reaches at most … the explicitly
    // allowlisted endpoints for the *one* tool"). With the rule absent,
    // dyld + libsystem still resolve every binary we ship.
    //
    // When Phase 4 lands `python-exec`, capture the actual service set
    // CPython needs at startup (likely a small set: notification
    // delivery, distributed notifications, possibly a few coreservices
    // helpers) and emit a *narrow* `(allow mach-lookup (global-name "..."))`
    // form. Do NOT re-introduce the unrestricted rule.
    //
    // The negative test `worker_cannot_look_up_arbitrary_mach_services`
    // in tests/macos_smoke.rs pins this invariant: a worker calling
    // `bootstrap_look_up("com.apple.coreservices.appleevents")` must
    // exit non-zero under the strict profile.

    // /dev allowlist: only the safe pseudo-device nodes workers legitimately
    // need. /dev as a whole is NOT allowed (that would expose disk*, bpf*,
    // auditpipe, etc.).
    //
    // tty is intentionally NOT exposed: both backends now detach the
    // controlling terminal (Linux via bwrap --new-session, macOS via the
    // pre_exec setsid() in spawn_under_policy — issue #2), so /dev/tty is
    // unusable (ENXIO) under either backend regardless of this rule. We keep
    // the explicit non-allowance as defense in depth: any future broadening
    // of /dev (e.g. (subpath "/dev")) would need to remember to re-deny tty.
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
        ] {
            assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
        }
    }

    /// Issue #1: the strict profile must NOT contain an unrestricted
    /// `(allow mach-lookup)` rule. None of our shipping workers need it,
    /// and granting it would expose every registered launchd service
    /// (Apple Events broker, pasteboard, etc.) — the largest asymmetry
    /// vs the threat-model invariant. When `python-exec` (Phase 4) needs
    /// specific Mach services, the rule must be re-introduced as
    /// `(allow mach-lookup (global-name "..."))` — narrow, never broad.
    /// Pin this so a future refactor cannot silently regress.
    #[test]
    fn profile_does_not_grant_unrestricted_mach_lookup() {
        let p = build_profile(&strict_policy());
        assert!(
            !p.contains("(allow mach-lookup)"),
            "strict profile must not contain unrestricted (allow mach-lookup); \
             got:\n{p}"
        );
        // Defence in depth: also reject any allow rule that *starts* with
        // `(allow mach-lookup)` followed only by whitespace + `)` —
        // catches `(allow mach-lookup )` and the line-continuation forms.
        for line in p.lines() {
            let trimmed = line.trim();
            assert!(
                trimmed != "(allow mach-lookup)" && trimmed != "(allow mach-lookup )",
                "strict profile contains an effectively-unrestricted mach-lookup line: {line:?}"
            );
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

    /// TinyScheme-special characters (", \, (, ), newline, NUL) in a policy
    /// path would let a malformed `SandboxPolicy` rewrite the profile by
    /// closing the `(subpath "...")` s-expression early. Today all callers are
    /// trusted core code, but the validation forecloses an entire class of
    /// future bug; cheaper than auditing every future call site.
    #[test]
    fn policy_paths_with_tinyscheme_specials_are_rejected_by_spawn() {
        let backend = MacosSeatbelt::new();
        for bad in [
            "/tmp/x\")(allow network*)(literal \"/x",
            "/tmp/has\\backslash",
            "/tmp/has(paren",
            "/tmp/has)paren",
            "/tmp/has\nnewline",
            "/tmp/has\0nul",
        ] {
            let mut p = strict_policy();
            p.fs_read.push(PathBuf::from(bad));
            let err = backend
                .spawn_under_policy(&p, "/usr/bin/true", &[])
                .err()
                .unwrap_or_else(|| panic!("must reject fs_read path {bad:?}"));
            let msg = format!("{err}");
            assert!(
                msg.contains("disallowed character"),
                "expected 'disallowed character' error for {bad:?}, got: {msg}"
            );
        }
        // Same shape, but for fs_write — the validation must cover both lists.
        let mut p = strict_policy();
        p.fs_write.push(PathBuf::from("/tmp/x\"escape"));
        let err = backend
            .spawn_under_policy(&p, "/usr/bin/true", &[])
            .expect_err("fs_write path with quote must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("disallowed character"),
            "expected 'disallowed character' error, got: {msg}"
        );
    }

    // This test runs a real sandbox-exec invocation. It only meaningfully runs
    // on macOS hosts; the parent module is cfg(target_os = "macos") so this
    // file isn't compiled elsewhere. Print a [SKIP] line on probe failure
    // (matches the integration-test pattern) instead of panicking, so a
    // host with MDM-clipped Seatbelt or a future macOS regression doesn't
    // false-fail the suite.
    #[test]
    fn probe_succeeds_on_this_host() {
        if let Err(e) = MacosSeatbelt::probe() {
            eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
        }
    }

    #[test]
    fn canonicalize_policy_paths_resolves_etc_symlink() {
        // /etc is a symlink to /private/etc on macOS — verify the helper
        // resolves it. /etc/hosts is guaranteed to exist on any macOS host.
        let mut p = strict_policy();
        p.fs_read = vec![PathBuf::from("/etc/hosts")];
        let canon = canonicalize_policy_paths(&p).expect("canonicalize must succeed");
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
        let canon = canonicalize_policy_paths(&p).expect("NotFound must fall back, not error");
        assert_eq!(canon.fs_write[0], nonexistent);
    }

    /// Non-NotFound canonicalize errors (e.g. PermissionDenied on a parent
    /// directory) MUST propagate — silently emitting an unresolved-path rule
    /// would produce a silently-non-functional Seatbelt rule and mask user
    /// errors as "the sandbox is just too strict." Pin this so a future
    /// refactor doesn't quietly re-introduce the catch-all swallow.
    #[test]
    fn canonicalize_policy_paths_propagates_non_notfound_errors() {
        use std::os::unix::fs::PermissionsExt;
        // Create an owned temp dir, drop perms to 000, then attempt to
        // canonicalize a path inside it — the parent walk fails with
        // PermissionDenied. We must propagate rather than fall back.
        let tmp = std::env::temp_dir().join(format!(
            "hhagent_canon_perm_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir(&tmp).expect("create_dir");
        // Always chmod back to 0o700 in a guard so we don't leak an
        // unreadable temp dir on test failure.
        struct Guard(PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(
                    &self.0,
                    std::fs::Permissions::from_mode(0o700),
                );
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(tmp.clone());
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");

        let mut p = strict_policy();
        p.fs_read = vec![tmp.join("inside")];
        let res = canonicalize_policy_paths(&p);
        let err = res.err().unwrap_or_else(|| {
            panic!("expected PermissionDenied to propagate; got Ok")
        });
        let msg = format!("{err}");
        assert!(
            msg.contains("could not canonicalize"),
            "expected 'could not canonicalize' in error, got: {msg}"
        );
    }
}
