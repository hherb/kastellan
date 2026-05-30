//! macOS backend: `launchd` user-level LaunchAgents.
//!
//! Generates an XML LaunchAgent plist from a [`crate::ServiceSpec`],
//! writes it to `~/Library/LaunchAgents/`, and drives `launchctl` for
//! the lifecycle (`bootstrap` / `bootout` for load+run / unload+stop,
//! `print` for status queries) in the per-user GUI domain
//! (`gui/<uid>`).
//!
//! Why user-level only:
//!   - LaunchAgents in `gui/<uid>` run as the user, in the user's
//!     GUI session, with no need for `sudo`. System-level `LaunchDaemons`
//!     would need root and would expand the attack surface.
//!   - This keeps containment consistent with the rest of the
//!     codebase (sandbox-exec, per-user supervisor on Linux too).
//!
//! ### Module structure (mirrors `systemd_user.rs`)
//!
//!   1. A pure [`build_plist`] returning the plist contents as a
//!      `String`. No I/O, fully unit-testable.
//!   2. A pure [`validate_service_name`] guarding against path
//!      traversal and labels with characters launchd's grammar refuses.
//!   3. [`LaunchAgents`] — the driver that combines the builder with
//!      file I/O and `launchctl` invocations.
//!   4. [`probe`] — fail-closed check that the per-user GUI domain is
//!      reachable.
//!
//! ### Plist shape
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
//!     "http://www.apple.com/DTD/PropertyList-1.0.dtd">
//! <plist version="1.0">
//! <dict>
//!     <key>Label</key>
//!     <string>hhagent-core</string>
//!     <key>ProgramArguments</key>
//!     <array>
//!         <string>/abs/program</string>
//!         <string>arg one</string>
//!     </array>
//!     <key>EnvironmentVariables</key>     <!-- only when non-empty -->
//!     <dict>
//!         <key>KEY</key>
//!         <string>value</string>
//!     </dict>
//!     <key>WorkingDirectory</key>          <!-- only when set -->
//!     <string>/abs/dir</string>
//!     <key>StandardOutPath</key>           <!-- only when set -->
//!     <string>/abs/log/out</string>
//!     <key>StandardErrorPath</key>         <!-- only when set -->
//!     <string>/abs/log/err</string>
//!     <key>RunAtLoad</key>
//!     <true/>
//!     <key>KeepAlive</key>
//!     <false/>                             <!-- or <true/> -->
//!     <key>ExitTimeOut</key>
//!     <integer>10</integer>
//! </dict>
//! </plist>
//! ```
//!
//! Element order is fixed so the generated file is diffable and the
//! builder is unit-testable.
//!
//! ### Lifecycle mapping (vs. Linux systemd)
//!
//! | Public API | Linux (`systemctl --user`)         | macOS (`launchctl`)                                  |
//! | ---------- | ---------------------------------- | ---------------------------------------------------- |
//! | `install`  | write unit + `daemon-reload`       | write plist (no launchctl)                           |
//! | `start`    | `start <name>.service`             | `bootstrap gui/<uid> <plist-path>` (load + run)      |
//! | `stop`     | `stop <name>.service`              | `bootout gui/<uid>/<label>` (unload + stop)          |
//! | `uninstall`| best-effort stop/disable + remove  | best-effort `bootout` + remove plist                 |
//! | `status`   | `is-active <name>.service`         | parse `print gui/<uid>/<label>` `state = ...` line   |
//!
//! On macOS, `bootstrap` *is* the load step — there is no separate
//! daemon-reload — and `RunAtLoad=true` (set unconditionally) means
//! the program runs as soon as the agent is loaded. `bootout` undoes
//! both, so a `stop` followed by a `start` re-runs the program from
//! the persisted plist on disk. This is consistent with the Linux
//! semantic of "stop preserves the unit file, start re-activates it".

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{ServiceSpec, ServiceStatus, Supervisor, SupervisorError};

/// Pure, I/O-free helpers (the plist builder + the name validator).
/// Re-exported below so `launchd_agents::build_plist` and
/// `launchd_agents::validate_service_name` keep their public paths.
mod builders;
pub use builders::{build_plist, validate_service_name};

/// `launchctl` driver for user LaunchAgents.
///
/// `agents_dir` is the directory the plist is written to. Defaults to
/// `~/Library/LaunchAgents/`, which is the canonical location launchd
/// scans for user agents on session login. Tests can point at a temp
/// dir to exercise just the file-writing half of `install`/`uninstall`
/// without invoking `launchctl` against the real GUI domain.
pub struct LaunchAgents {
    agents_dir: PathBuf,
}

impl LaunchAgents {
    /// Construct a driver pointing at the default agents dir.
    ///
    /// Resolves `~/Library/LaunchAgents/` from `$HOME`. The directory
    /// is *not* created here; [`install`] creates it on demand so the
    /// driver itself has no I/O side effects on construction.
    ///
    /// [`install`]: LaunchAgents::install
    pub fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            agents_dir: home.join("Library").join("LaunchAgents"),
        }
    }

    /// Construct a driver writing plists into a custom directory.
    ///
    /// Used by tests that want to exercise just the file-writing half
    /// of `install`/`uninstall` without polluting the user's real
    /// `~/Library/LaunchAgents/` or talking to the live GUI launchd
    /// domain.
    ///
    /// **Note:** plists written here are invisible to `launchctl
    /// bootstrap` unless the path is the canonical one — the GUI
    /// launchd manager only auto-loads from `~/Library/LaunchAgents/`
    /// on login. Custom-dir drivers therefore won't see successful
    /// `start`/`stop` against the live system.
    pub fn with_agents_dir(agents_dir: PathBuf) -> Self {
        Self { agents_dir }
    }

    /// Return the directory this driver writes plists into.
    pub fn agents_dir(&self) -> &Path {
        &self.agents_dir
    }

    /// Path the driver would write `<name>.plist` to.
    pub fn plist_path(&self, name: &str) -> PathBuf {
        self.agents_dir.join(format!("{name}.plist"))
    }

    /// True iff the driver writes into the canonical
    /// `~/Library/LaunchAgents/` location. Used to decide whether
    /// `launchctl bootout` (best-effort cleanup) should be attempted —
    /// for custom dirs (tests) it's not only pointless but actively
    /// risky, since a name collision with a real installed agent
    /// would `bootout` someone else's service.
    fn is_default_agents_dir(&self) -> bool {
        let home = match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h),
            None => return false,
        };
        self.agents_dir == home.join("Library").join("LaunchAgents")
    }
}

impl Default for LaunchAgents {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor for LaunchAgents {
    fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError> {
        validate_service_name(&spec.name)?;
        // Working dir / log paths must be absolute or launchd refuses
        // them at bootstrap time. Catch this at the host boundary so
        // we get a structured error instead of a parse failure later.
        if let Some(d) = &spec.working_dir {
            if !d.is_absolute() {
                return Err(SupervisorError::Io(format!(
                    "working_dir must be absolute, got {}",
                    d.display()
                )));
            }
        }
        if let Some(d) = &spec.stdout_log {
            if !d.is_absolute() {
                return Err(SupervisorError::Io(format!(
                    "stdout_log must be absolute, got {}",
                    d.display()
                )));
            }
        }
        if let Some(d) = &spec.stderr_log {
            if !d.is_absolute() {
                return Err(SupervisorError::Io(format!(
                    "stderr_log must be absolute, got {}",
                    d.display()
                )));
            }
        }
        // program must be absolute too — launchd refuses relative
        // ProgramArguments[0] paths.
        if !spec.program.is_absolute() {
            return Err(SupervisorError::Io(format!(
                "program must be absolute, got {}",
                spec.program.display()
            )));
        }

        fs::create_dir_all(&self.agents_dir)
            .map_err(|e| SupervisorError::Io(format!("create {}: {e}", self.agents_dir.display())))?;

        let path = self.plist_path(&spec.name);
        let body = build_plist(spec);
        write_atomic(&path, body.as_bytes())?;
        // Unlike the Linux backend, there is no separate "reload"
        // step — `bootstrap` is the load step and is invoked from
        // `start`. So `install` ends here.
        Ok(())
    }

    fn start(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        let path = self.plist_path(name);
        if !path.exists() {
            return Err(SupervisorError::Io(format!(
                "plist not found: {} (call install first)",
                path.display()
            )));
        }
        let domain = user_domain_target()?;
        // Idempotency: launchctl bootstrap on an already-loaded agent
        // exits with various non-zero codes whose error strings
        // differ across macOS versions ("already loaded", "Bootstrap
        // failed: 5: Input/output error", "Bootstrap failed: 17:
        // File exists", etc.). Parsing those would be brittle.
        // Instead, query loaded-state first via `launchctl print`:
        // if the agent is in the domain at all (regardless of
        // running/exited), bootstrap is a no-op for our public API.
        let target = format!("{domain}/{name}");
        if is_loaded_in_domain(&target) {
            return Ok(());
        }
        let path_str = path.to_string_lossy().into_owned();
        run_launchctl(&["bootstrap", &domain, &path_str])?;
        Ok(())
    }

    fn stop(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        let target = format!("{}/{name}", user_domain_target()?);
        // bootout against an already-unloaded agent errors with
        // "no such process" (exit 113 / ESRCH). Treat as success.
        match run_launchctl(&["bootout", &target]) {
            Ok(_) => Ok(()),
            Err(SupervisorError::Backend(msg)) if is_no_such_service_error(&msg) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        // Best-effort bootout — only attempt it when we're operating
        // on the canonical agents dir. For custom dirs (tests) we
        // skip the launchctl call entirely so a name collision with
        // a real installed agent cannot bootout someone else's
        // service.
        if self.is_default_agents_dir() {
            let target = format!("{}/{name}", user_domain_target()?);
            // Errors are swallowed: the agent may already be
            // unloaded, or never have been bootstrapped at all.
            let _ = run_launchctl(&["bootout", &target]);
        }
        let path = self.plist_path(name);
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| SupervisorError::Io(format!("remove {}: {e}", path.display())))?;
        }
        Ok(())
    }

    fn status(&self, name: &str) -> Result<ServiceStatus, SupervisorError> {
        validate_service_name(name)?;
        // No file on disk → not installed (regardless of what
        // launchctl thinks; the live manager may have a stale entry
        // cached for a name we just uninstalled but on a different
        // driver instance).
        if !self.plist_path(name).exists() {
            return Ok(ServiceStatus::NotInstalled);
        }
        let target = format!("{}/{name}", user_domain_target()?);
        let out = Command::new("launchctl")
            .args(["print", &target])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SupervisorError::Io(format!("spawn launchctl: {e}")))?;
        if !out.status.success() {
            // `print` exits non-zero when the service is not loaded
            // into the GUI domain. The plist file exists (we
            // checked), so it's installed but inactive — not
            // running, not loaded, no PID.
            return Ok(ServiceStatus::Inactive);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let state = parse_print_state(&stdout).unwrap_or_default();
        Ok(match state.as_str() {
            "running" => ServiceStatus::Active,
            // Loaded-but-not-running variants: `not running`,
            // `waiting`, `exited`, `spawn scheduled` (transitional).
            // Treat all as Inactive to match the Linux backend's
            // liberal mapping (anything that isn't clearly "active"
            // is "inactive").
            _ => ServiceStatus::Inactive,
        })
    }
}

/// Probe whether the user GUI launchd domain is reachable.
///
/// Mirrors `systemd_user::probe`: succeed silently or return a
/// structured error pointing the caller at the most common recovery
/// (re-login if the GUI session is missing, or check that the user is
/// logged in at all).
///
/// The probe runs `launchctl print-disabled gui/<uid>` because it
/// reaches into the GUI domain without requiring a specific service
/// to exist — any user with an active GUI session can run it.
pub fn probe() -> Result<(), SupervisorError> {
    let domain = user_domain_target()?;
    let out = Command::new("launchctl")
        .args(["print-disabled", &domain])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SupervisorError::Probe(format!("spawn launchctl: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let hint = "\n\nThe GUI launchd domain is not reachable. \
                On macOS this needs an active console login session — \
                if running over SSH or as a daemon user, the supervisor \
                cannot drive `launchctl bootstrap gui/<uid>`. Re-login \
                from the console (or use a system administrator's \
                LaunchDaemon, which this codebase does not support).";
    Err(SupervisorError::Probe(format!(
        "launchctl print-disabled {domain} failed: {stderr}{hint}"
    )))
}

/// Atomically write `bytes` to `path` via write-to-tmp + fsync + rename.
///
/// launchd reads the plist file once at bootstrap time, but a
/// concurrent reader (another `launchctl bootstrap` against a stale
/// path, or `plutil -p` from a debugging maintainer) could otherwise
/// see a half-written file. Atomic rename keeps the observable state
/// binary: either the old contents are visible, or the new ones —
/// never a torn read.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SupervisorError> {
    let tmp = path.with_extension("plist.tmp");
    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| SupervisorError::Io(format!("create {}: {e}", tmp.display())))?;
        f.write_all(bytes)
            .map_err(|e| SupervisorError::Io(format!("write {}: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| SupervisorError::Io(format!("fsync {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        SupervisorError::Io(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

/// Build the `gui/<uid>` domain target string used by `launchctl`.
///
/// The UID comes from `getuid(2)`; the value is whatever the calling
/// process happens to run as. We do not honour `$SUDO_UID` or similar
/// because the supervisor crate is *user-level only* (running it as
/// root would put the agent into `gui/0`, which is not a regular user
/// session).
fn user_domain_target() -> Result<String, SupervisorError> {
    Ok(format!("gui/{}", current_uid()))
}

/// Current real UID via `libc::getuid()`.
///
/// `getuid` is async-signal-safe and infallible, so no error path is
/// possible here. Wrapped in a function so the type stays `u32` at
/// the call sites without forcing every caller to write a libc cast.
#[cfg(target_os = "macos")]
fn current_uid() -> u32 {
    // SAFETY: `getuid` is documented as always-succeeding and
    // async-signal-safe; it has no preconditions.
    unsafe { libc::getuid() as u32 }
}

/// Run `launchctl <args>` with stdio captured. Maps non-zero exits to
/// [`SupervisorError::Backend`] with the trimmed stderr.
fn run_launchctl(args: &[&str]) -> Result<String, SupervisorError> {
    let out = Command::new("launchctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SupervisorError::Io(format!("spawn launchctl: {e}")))?;
    if !out.status.success() {
        // `launchctl` writes most diagnostics to stdout (yes, stdout)
        // and only some to stderr. Fold both so the error message
        // surfaces whichever the tool happened to use.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{} {}", stderr.trim(), stdout.trim()).trim().to_string();
        let exit = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(SupervisorError::Backend(format!(
            "launchctl {} (exit {exit}): {combined}",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse the `state = <word>` line out of `launchctl print` output.
///
/// `launchctl print` emits a verbose nested dict; the `state` line
/// looks like `state = running`, indented by some whitespace. This
/// helper finds the first such line and returns the word — `running`,
/// `not running`, `waiting`, etc. Returns `None` if no `state` line
/// is present.
fn parse_print_state(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("state = ") {
            return Some(val.trim().to_string());
        }
    }
    None
}

/// Is the service loaded into the given launchd domain?
///
/// `launchctl print <target>` exits 0 when the service is bootstrapped
/// (regardless of whether it's currently running, exited, waiting,
/// or transitional) and non-zero when there's no such service. We
/// use the exit code, not stdout parsing, so the check is robust to
/// macOS version differences in the verbose `print` output format.
///
/// Used to make `start` idempotent without parsing version-specific
/// `bootstrap` error strings.
fn is_loaded_in_domain(target: &str) -> bool {
    Command::new("launchctl")
        .args(["print", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Heuristic: does `msg` look like the "no such service" error
/// `launchctl bootout` emits when the agent isn't in the domain?
/// Used to make `stop` idempotent.
fn is_no_such_service_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("could not find service")
        || m.contains("no such process")
        || m.contains("could not find specified service")
}

#[cfg(test)]
mod tests;

