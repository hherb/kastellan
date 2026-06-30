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
        // proxy_uds is interpolated into the profile as a `(path-literal ...)`
        // rule just like fs_read/fs_write, so it must pass the same absolute +
        // injection-foreclosing checks — otherwise it would be the one policy
        // path that skips the guard the comment below promises.
        // The persistent store's guest_mount is interpolated into the profile as
        // a `(subpath "...")` rule (build_profile), so it must pass the same
        // absolute + injection-foreclosing checks. host_backing is validated too
        // because it must match guest_mount on this backend (checked after
        // canonicalization below — Seatbelt has no path remap).
        let persistent_paths: Vec<&std::path::PathBuf> = policy
            .persistent_store
            .iter()
            .flat_map(|ps| [&ps.host_backing, &ps.guest_mount])
            .collect();
        for p in policy
            .fs_read
            .iter()
            .chain(policy.fs_write.iter())
            .chain(policy.proxy_uds.iter())
            .chain(persistent_paths.iter().copied())
        {
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
        // Slice 5b-2: Seatbelt has no mount remap — build_profile grants only
        // guest_mount, so host_backing is inert. If a caller sets them to distinct
        // paths (valid on bwrap/Firecracker), writes would land at guest_mount with
        // NO relation to the intended stable host_backing — silent divergence of the
        // cross-platform abstraction. Require equality and fail closed, and create
        // the dir up front so first boot works without the caller pre-creating it.
        if let Some(ps) = &policy.persistent_store {
            if ps.host_backing != ps.guest_mount {
                return Err(SandboxError::Backend(format!(
                    "persistent_store on macOS requires host_backing == guest_mount (no path \
                     remap under Seatbelt), got host_backing={:?} guest_mount={:?}",
                    ps.host_backing, ps.guest_mount
                )));
            }
            std::fs::create_dir_all(&ps.guest_mount).map_err(|e| {
                SandboxError::Backend(format!(
                    "persistent_store guest_mount {:?}: {e}",
                    ps.guest_mount
                ))
            })?;
        }
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

/// Canonicalize a single path, resolving symlinks. For a path whose final
/// component does not exist yet (e.g. a not-yet-created socket or scratch
/// file), the parent directory is canonicalized and the filename appended —
/// this correctly resolves `/tmp/foo.sock` to `/private/tmp/foo.sock` on
/// macOS even before the socket file exists. Any other `io::Error`
/// (PermissionDenied on a parent, etc.) propagates so callers don't silently
/// emit a non-functional Seatbelt rule.
fn canonicalize_one(p: &std::path::Path) -> Result<std::path::PathBuf, SandboxError> {
    match std::fs::canonicalize(p) {
        Ok(resolved) => Ok(resolved),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The path itself doesn't exist yet; canonicalize the parent to
            // resolve any symlinks there (e.g. /tmp -> /private/tmp on macOS),
            // then reattach the filename so Seatbelt sees the real path.
            match p.parent().zip(p.file_name()) {
                Some((parent, name)) => {
                    match std::fs::canonicalize(parent) {
                        Ok(canon_parent) => Ok(canon_parent.join(name)),
                        Err(pe) if pe.kind() == std::io::ErrorKind::NotFound => Ok(p.to_path_buf()),
                        Err(pe) => Err(SandboxError::Backend(format!(
                            "could not canonicalize policy path {p:?}: {pe}"
                        ))),
                    }
                }
                // No parent (root path) or no filename — keep as-is.
                None => Ok(p.to_path_buf()),
            }
        }
        Err(e) => Err(SandboxError::Backend(format!(
            "could not canonicalize policy path {p:?}: {e}"
        ))),
    }
}

/// Return a clone of `policy` with each fs_read / fs_write / proxy_uds path
/// canonicalized (symlinks resolved). `NotFound` errors fall back to the
/// original path for fs_read / fs_write (legitimate for not-yet-created
/// scratch dirs); for proxy_uds the parent-dir canonicalization approach in
/// [`canonicalize_one`] additionally resolves e.g. `/tmp` → `/private/tmp`
/// on macOS even before the socket file exists. Any other `io::Error` —
/// most importantly `PermissionDenied` on a parent directory — propagates as
/// a `SandboxError::Backend`, because emitting a rule for an unresolved path
/// would silently produce a non-functional Seatbelt rule and mask user errors
/// as "the sandbox is just too strict."
///
/// `persistent_store.guest_mount` and `persistent_store.host_backing` are
/// also canonicalized. On macOS, `$TMPDIR` resolves through
/// `/var/folders/…` → `/private/var/folders/…`; an unresolved `guest_mount`
/// produces a `(subpath …)` rule the kernel never matches, causing the
/// worker to start but silently fail all persistent-store writes.
fn canonicalize_policy_paths(policy: &SandboxPolicy) -> Result<SandboxPolicy, SandboxError> {
    let canon_list =
        |paths: &[std::path::PathBuf]| -> Result<Vec<std::path::PathBuf>, SandboxError> {
            paths.iter().map(|p| canonicalize_one(p)).collect()
        };
    let mut out = policy.clone();
    out.fs_read = canon_list(&policy.fs_read)?;
    out.fs_write = canon_list(&policy.fs_write)?;
    if let Some(uds) = &policy.proxy_uds {
        out.proxy_uds = Some(canonicalize_one(uds)?);
    }
    if let Some(ps) = &policy.persistent_store {
        out.persistent_store = Some(crate::PersistentStore {
            host_backing: canonicalize_one(&ps.host_backing)?,
            guest_mount: canonicalize_one(&ps.guest_mount)?,
            size_mib: ps.size_mib,
        });
    }
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
    // shipping kastellan worker today (`kastellan-worker-shell-exec`,
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

    // Persistent store: a stable RW mount that survives worker respawns.
    // On macOS there is no path remap, so we grant guest_mount directly;
    // spawn_under_policy enforces host_backing == guest_mount and creates the
    // dir up front, so this rule grants exactly the host path the worker writes.
    if let Some(ps) = &policy.persistent_store {
        out.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            ps.guest_mount.display()
        ));
    }

    // Browser-only Seatbelt widening (spike findings, design spec §3.1). A
    // headless Chromium SIGSEGVs at launch without all three clusters; bisected
    // on macOS 26.4 ARM64 (dropping any one → child crash). This is gated to
    // `WorkerBrowserClient` ALONE — every other worker keeps the strict
    // deny-default (incl. the `mach-lookup` deny of issue #1). It is a real,
    // documented threat-model widening for the browser tool only.
    //
    // Phase-2 hardening (deferred): narrow `mach-lookup` to the specific
    // `(global-name …)` services Chromium needs rather than the unrestricted
    // form; record the service set first.
    if matches!(policy.profile, crate::Profile::WorkerBrowserClient) {
        out.push_str("(allow ipc-posix-shm*)\n"); // shared-memory IPC between browser processes
        out.push_str("(allow iokit-open)\n"); // GPU/graphics probing (even under SwiftShader)
        out.push_str("(allow iokit-get-properties)\n");
        out.push_str("(allow mach-lookup)\n"); // Mach bootstrap — re-grants issue #1's deny
        out.push_str("(allow mach-register)\n");
        // The spike's proven-working render profile also carried these two
        // (`scripts/spikes/browser-driver/seatbelt-run.sh`, the `chromium`
        // case): sysctl-write (Chromium tweaks a few sysctls at startup) and
        // system-socket (PF_SYSTEM kernel-control socket for network-config
        // detection). They were not part of the shm/iokit/mach bisect but were
        // present when render succeeded, so we include them to match the
        // verified set. Narrowing both is a Phase-2 hardening follow-up.
        out.push_str("(allow sysctl-write)\n");
        out.push_str("(allow system-socket)\n");
    }

    match (&policy.net, &policy.proxy_uds) {
        (crate::Net::Allowlist(_), Some(uds)) => {
            // Force-routed: deny all outbound, then re-allow ONLY the proxy UDS.
            // The host-level allowlist is enforced by the egress proxy itself;
            // Seatbelt's job here is to make the netns-less routing unbypassable
            // by closing every AF_INET/AF_INET6 socket call.
            out.push_str("(deny network-outbound)\n");
            // `{uds:?}` emits the path Rust-debug-quoted (escaping `"` and `\`).
            // The disallowed-char guard in `spawn_under_policy` already rejects
            // structural chars before we get here; debug-quoting is belt-and-
            // braces and, unlike `.display().to_string()`, is not lossy for a
            // non-UTF8 path (which would otherwise silently mis-match the rule).
            out.push_str(&format!(
                "(allow network-outbound (remote unix-socket (path-literal {uds:?})))\n",
            ));
            // The browser reaches its in-jail loopback-TCP↔UDS shim over
            // 127.0.0.1 (egress slice #2): allow loopback TCP bind/accept (shim)
            // + connect (Chromium). Scoped to the browser profile so the other
            // UDS workers (in-process CONNECT-over-UDS, no loopback) stay strict.
            // CAVEAT (#286): unlike Linux (private netns isolates loopback to the
            // worker), macOS has no netns, so `localhost:*` is the *host's*
            // loopback — a compromised browser worker could reach host-local
            // services directly, bypassing the sidecar. Latent today (Chromium is
            // proxy-routed; macOS render is blocked by #284). See docs/threat-model.md.
            if matches!(policy.profile, crate::Profile::WorkerBrowserClient) {
                out.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
                out.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
                out.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
            }
        }
        (crate::Net::Allowlist(_), None) | (crate::Net::ProxyEgress, _) => {
            // The host allowlist itself is enforced by the future egress proxy
            // (see docs/architecture.md invariant 5), not by Seatbelt — same
            // split as bwrap's --share-net.
            out.push_str("(allow network*)\n");
        }
        (crate::Net::Deny, _) => { /* no network rules */ }
    }

    out
}

#[cfg(test)]
mod tests;
