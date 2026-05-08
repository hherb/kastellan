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

/// Default seconds to wait for SIGTERM to take effect before launchd
/// escalates to SIGKILL on `bootout`.
///
/// 10 s matches the systemd backend's `TimeoutStopSec` so behaviour is
/// uniform across OSes; long enough for a graceful exit, short enough
/// that test teardown does not hang on a misbehaving inner process.
const DEFAULT_EXIT_TIMEOUT_SEC: u32 = 10;

/// Maximum length of a service name. Generous compared to the file-system
/// 255-byte basename ceiling — leaves headroom for the `.plist` suffix
/// and any future namespacing prefix.
const MAX_NAME_LEN: usize = 200;

/// Build the XML body of a `<name>.plist` LaunchAgent file.
///
/// Pure function: no I/O, no environment access, deterministic output.
/// Returns the full file as a `String` ready to be written to disk.
///
/// The caller is responsible for validating the spec's name with
/// [`validate_service_name`] before calling this — the builder assumes
/// its input is already well-formed. All free-form string fields
/// (`name`, args, env keys/values, paths) are XML-escaped on the way
/// out (see [`xml_escape`]).
///
/// # Element order
///
/// `Label`, `ProgramArguments`, `EnvironmentVariables` (when non-empty),
/// `WorkingDirectory` (when set), `StandardOutPath` (when set),
/// `StandardErrorPath` (when set), `RunAtLoad` (always `true`),
/// `KeepAlive` (`true` iff `spec.keep_alive`), `ExitTimeOut`. The order
/// is fixed so a textual diff of two plists is meaningful.
///
/// # `RunAtLoad=true` is unconditional
///
/// `bootstrap` only runs the program when `RunAtLoad=true` (otherwise
/// the agent sits dormant waiting for a demand-driven trigger that
/// hhagent doesn't use). For our "install + start" model to actually
/// run anything, this must always be `true`.
pub fn build_plist(spec: &ServiceSpec) -> String {
    let mut out = String::with_capacity(1024);

    // XML preamble + DOCTYPE — both required by `plutil` to consider
    // the file a well-formed XML plist.
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTD/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n");
    out.push_str("<dict>\n");

    // Label — must equal the file basename minus `.plist`. Other
    // launchctl invocations identify the agent by this string.
    out.push_str("    <key>Label</key>\n");
    out.push_str(&format!("    <string>{}</string>\n", xml_escape(&spec.name)));

    // ProgramArguments — array, [program, arg1, arg2, ...]. launchd
    // requires program to be the first element when both `Program`
    // and `ProgramArguments` are absent/present in different combos;
    // using `ProgramArguments` exclusively is the simplest and most
    // portable form.
    out.push_str("    <key>ProgramArguments</key>\n");
    out.push_str("    <array>\n");
    out.push_str(&format!(
        "        <string>{}</string>\n",
        xml_escape(&spec.program.to_string_lossy())
    ));
    for a in &spec.args {
        out.push_str(&format!(
            "        <string>{}</string>\n",
            xml_escape(a)
        ));
    }
    out.push_str("    </array>\n");

    // EnvironmentVariables — only emitted when non-empty. launchd
    // starts each agent from a minimal environment regardless, so
    // omitting this key when there are no overrides is the closest
    // match to systemd's `--clean-env` behavior.
    if !spec.env.is_empty() {
        out.push_str("    <key>EnvironmentVariables</key>\n");
        out.push_str("    <dict>\n");
        for (k, v) in &spec.env {
            out.push_str(&format!(
                "        <key>{}</key>\n",
                xml_escape(k)
            ));
            out.push_str(&format!(
                "        <string>{}</string>\n",
                xml_escape(v)
            ));
        }
        out.push_str("    </dict>\n");
    }

    if let Some(dir) = &spec.working_dir {
        out.push_str("    <key>WorkingDirectory</key>\n");
        out.push_str(&format!(
            "    <string>{}</string>\n",
            xml_escape(&dir.to_string_lossy())
        ));
    }

    if let Some(log) = &spec.stdout_log {
        out.push_str("    <key>StandardOutPath</key>\n");
        out.push_str(&format!(
            "    <string>{}</string>\n",
            xml_escape(&log.to_string_lossy())
        ));
    }
    if let Some(log) = &spec.stderr_log {
        out.push_str("    <key>StandardErrorPath</key>\n");
        out.push_str(&format!(
            "    <string>{}</string>\n",
            xml_escape(&log.to_string_lossy())
        ));
    }

    // RunAtLoad — see module docs. Always true so `bootstrap` runs
    // the program immediately.
    out.push_str("    <key>RunAtLoad</key>\n");
    out.push_str("    <true/>\n");

    // KeepAlive — true iff the caller asked for restart-on-exit.
    // launchd's `KeepAlive=true` restarts the agent unconditionally on
    // any exit; finer-grained variants exist (`SuccessfulExit`,
    // `Crashed`, …) but the bool form mirrors systemd's
    // `Restart=on-failure` closely enough for the Phase 0 supervisor.
    out.push_str("    <key>KeepAlive</key>\n");
    out.push_str(if spec.keep_alive { "    <true/>\n" } else { "    <false/>\n" });

    // ExitTimeOut — seconds between SIGTERM and SIGKILL on bootout.
    out.push_str("    <key>ExitTimeOut</key>\n");
    out.push_str(&format!("    <integer>{}</integer>\n", DEFAULT_EXIT_TIMEOUT_SEC));

    out.push_str("</dict>\n");
    out.push_str("</plist>\n");
    out
}

/// XML-escape the five characters that have meaning inside element
/// content / attribute values: `<`, `>`, `&`, `"`, `'`.
///
/// All other characters pass through unchanged. This is enough for
/// `<string>...</string>` and `<key>...</key>` content; it is *not*
/// enough for arbitrary XML attribute values (we don't write any).
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Validate a service name against `[A-Za-z0-9._-]{1,200}` minus `.`,
/// `..`, and any name starting with `.` (hidden files) or `-` (would
/// be parsed as a flag by `launchctl`).
///
/// Rejects path-traversal characters (`/`, `\0`) and any byte that
/// would either confuse a shell-style arg parse or break the
/// `Label`-equals-basename invariant launchd cares about. Returning
/// `Ok` is the gate that lets [`LaunchAgents::install`] write a file
/// to disk.
///
/// The rule set is intentionally identical to the Linux backend's so
/// a single user-facing service name is portable to either OS without
/// a "rename for macOS" step.
pub fn validate_service_name(name: &str) -> Result<(), SupervisorError> {
    if name.is_empty() {
        return Err(SupervisorError::InvalidName(
            "service name must not be empty".into(),
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(SupervisorError::InvalidName(format!(
            "service name longer than {MAX_NAME_LEN} chars"
        )));
    }
    if name == "." || name == ".." {
        return Err(SupervisorError::InvalidName(
            ". and .. are not valid service names".into(),
        ));
    }
    if name.starts_with('.') {
        return Err(SupervisorError::InvalidName(
            "service name must not start with '.'".into(),
        ));
    }
    if name.starts_with('-') {
        return Err(SupervisorError::InvalidName(
            "service name must not start with '-'".into(),
        ));
    }
    for ch in name.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-') {
            return Err(SupervisorError::InvalidName(format!(
                "service name contains illegal character: {ch:?}"
            )));
        }
    }
    Ok(())
}

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
mod tests {
    use super::*;

    /// Minimal spec used as a starting point in builder tests.
    fn minimal_spec(name: &str) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            program: PathBuf::from("/usr/bin/true"),
            args: vec![],
            env: vec![],
            working_dir: None,
            keep_alive: false,
            stdout_log: None,
            stderr_log: None,
        }
    }

    // ---------- pure-builder tests (no I/O, no launchctl) ----------

    #[test]
    fn build_plist_starts_with_xml_preamble_and_doctype() {
        let s = build_plist(&minimal_spec("svc"));
        assert!(s.starts_with("<?xml version=\"1.0\""), "{s}");
        assert!(
            s.contains("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\""),
            "{s}"
        );
        assert!(s.contains("<plist version=\"1.0\">"), "{s}");
    }

    #[test]
    fn build_plist_emits_label_matching_name() {
        let s = build_plist(&minimal_spec("hhagent-core"));
        assert!(
            s.contains("<key>Label</key>\n    <string>hhagent-core</string>"),
            "{s}"
        );
    }

    #[test]
    fn build_plist_program_arguments_starts_with_program_then_args() {
        let mut spec = minimal_spec("svc");
        spec.program = PathBuf::from("/usr/local/bin/foo");
        spec.args = vec!["--flag".into(), "value".into()];
        let s = build_plist(&spec);
        // Program must appear before "--flag"; "--flag" before "value".
        let prog = s.find("<string>/usr/local/bin/foo</string>").expect("program");
        let flag = s.find("<string>--flag</string>").expect("flag");
        let value = s.find("<string>value</string>").expect("value");
        assert!(prog < flag && flag < value, "argv order broken:\n{s}");
    }

    #[test]
    fn build_plist_xml_escapes_args_with_special_chars() {
        let mut spec = minimal_spec("svc");
        spec.args = vec!["a<b&c\"d'e".into()];
        let s = build_plist(&spec);
        // The escaped form must appear; the raw form must not.
        assert!(s.contains("<string>a&lt;b&amp;c&quot;d&apos;e</string>"), "{s}");
        assert!(!s.contains("a<b&c"), "raw <, & must not leak: {s}");
    }

    #[test]
    fn build_plist_emits_environment_variables_in_order_when_set() {
        let mut spec = minimal_spec("svc");
        spec.env = vec![
            ("FIRST".into(), "1".into()),
            ("SECOND".into(), "two".into()),
        ];
        let s = build_plist(&spec);
        assert!(s.contains("<key>EnvironmentVariables</key>"), "{s}");
        let first = s.find("<key>FIRST</key>").expect("FIRST not found");
        let second = s.find("<key>SECOND</key>").expect("SECOND not found");
        assert!(first < second, "env order must be preserved");
    }

    #[test]
    fn build_plist_omits_environment_variables_when_empty() {
        let s = build_plist(&minimal_spec("svc"));
        assert!(
            !s.contains("EnvironmentVariables"),
            "should not emit empty env block, got:\n{s}"
        );
    }

    #[test]
    fn build_plist_emits_working_directory_when_set() {
        let mut spec = minimal_spec("svc");
        spec.working_dir = Some(PathBuf::from("/var/lib/hhagent"));
        let s = build_plist(&spec);
        assert!(
            s.contains("<key>WorkingDirectory</key>\n    <string>/var/lib/hhagent</string>"),
            "{s}"
        );
    }

    #[test]
    fn build_plist_omits_working_directory_when_none() {
        let s = build_plist(&minimal_spec("svc"));
        assert!(!s.contains("WorkingDirectory"), "{s}");
    }

    #[test]
    fn build_plist_emits_log_redirects_when_set() {
        let mut spec = minimal_spec("svc");
        spec.stdout_log = Some(PathBuf::from("/var/log/svc.out"));
        spec.stderr_log = Some(PathBuf::from("/var/log/svc.err"));
        let s = build_plist(&spec);
        assert!(
            s.contains("<key>StandardOutPath</key>\n    <string>/var/log/svc.out</string>"),
            "{s}"
        );
        assert!(
            s.contains("<key>StandardErrorPath</key>\n    <string>/var/log/svc.err</string>"),
            "{s}"
        );
    }

    #[test]
    fn build_plist_run_at_load_is_always_true() {
        // RunAtLoad=true is required for `bootstrap` to actually run
        // the program. Pin the invariant.
        let s = build_plist(&minimal_spec("svc"));
        assert!(
            s.contains("<key>RunAtLoad</key>\n    <true/>"),
            "RunAtLoad must be true unconditionally, got:\n{s}"
        );
    }

    #[test]
    fn build_plist_keep_alive_true_when_spec_keep_alive_true() {
        let mut spec = minimal_spec("svc");
        spec.keep_alive = true;
        let s = build_plist(&spec);
        assert!(s.contains("<key>KeepAlive</key>\n    <true/>"), "{s}");
    }

    #[test]
    fn build_plist_keep_alive_false_when_spec_keep_alive_false() {
        let s = build_plist(&minimal_spec("svc"));
        assert!(s.contains("<key>KeepAlive</key>\n    <false/>"), "{s}");
    }

    #[test]
    fn build_plist_always_emits_exit_timeout() {
        let s = build_plist(&minimal_spec("svc"));
        assert!(
            s.contains(&format!(
                "<key>ExitTimeOut</key>\n    <integer>{}</integer>",
                DEFAULT_EXIT_TIMEOUT_SEC
            )),
            "{s}"
        );
    }

    #[test]
    fn build_plist_label_is_xml_escaped() {
        // Defense-in-depth: even though `validate_service_name`
        // forbids `<`, `&`, etc., the builder must not assume that.
        // If a future caller bypasses validation, output must still
        // be well-formed XML.
        let spec = ServiceSpec {
            name: "a&b<c".into(),
            program: PathBuf::from("/usr/bin/true"),
            args: vec![],
            env: vec![],
            working_dir: None,
            keep_alive: false,
            stdout_log: None,
            stderr_log: None,
        };
        let s = build_plist(&spec);
        assert!(s.contains("<string>a&amp;b&lt;c</string>"), "{s}");
    }

    // ---------- validator tests ----------

    #[test]
    fn validate_service_name_accepts_typical_names() {
        for n in &[
            "hhagent",
            "hhagent-core",
            "hhagent.core",
            "org.hhagent.core",
            "a_b",
            "abc123",
        ] {
            validate_service_name(n).expect(n);
        }
    }

    #[test]
    fn validate_service_name_rejects_empty() {
        let err = validate_service_name("").expect_err("empty must reject");
        assert!(matches!(err, SupervisorError::InvalidName(_)));
    }

    #[test]
    fn validate_service_name_rejects_path_traversal() {
        for n in &["../evil", "a/b", "foo\\bar", ".."] {
            let err = validate_service_name(n).expect_err(n);
            assert!(matches!(err, SupervisorError::InvalidName(_)), "{n}: {err}");
        }
    }

    #[test]
    fn validate_service_name_rejects_dot_prefix_and_dash_prefix() {
        for n in &[".hidden", "-flagish"] {
            let err = validate_service_name(n).expect_err(n);
            assert!(matches!(err, SupervisorError::InvalidName(_)), "{n}: {err}");
        }
    }

    #[test]
    fn validate_service_name_rejects_overlong() {
        let n = "a".repeat(MAX_NAME_LEN + 1);
        let err = validate_service_name(&n).expect_err("overlong");
        assert!(matches!(err, SupervisorError::InvalidName(_)));
    }

    #[test]
    fn validate_service_name_rejects_whitespace_and_specials() {
        for n in &["has space", "has\ttab", "has;semi", "has*star", "has\0nul"] {
            let err = validate_service_name(n).expect_err(n);
            assert!(matches!(err, SupervisorError::InvalidName(_)), "{n}: {err}");
        }
    }

    // ---------- helper tests ----------

    #[test]
    fn xml_escape_handles_all_five_predefined_entities() {
        assert_eq!(xml_escape("a&b<c>d\"e'f"), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    }

    #[test]
    fn xml_escape_passes_through_unicode_unchanged() {
        // Only the five ASCII entities are escaped; everything else
        // is fine in UTF-8 XML element content.
        assert_eq!(xml_escape("héllo 世界"), "héllo 世界");
    }

    #[test]
    fn parse_print_state_finds_state_line_with_indentation() {
        let stdout = "gui/501/foo = {\n\ttype = LaunchAgent\n\tstate = running\n\tlast exit code = 0\n}";
        assert_eq!(parse_print_state(stdout), Some("running".into()));
    }

    #[test]
    fn parse_print_state_returns_none_when_absent() {
        let stdout = "Could not find service \"foo\" in domain for login: 501";
        assert_eq!(parse_print_state(stdout), None);
    }

    #[test]
    fn parse_print_state_handles_multi_word_state() {
        let stdout = "    state = not running\n";
        assert_eq!(parse_print_state(stdout), Some("not running".into()));
    }

    #[test]
    fn is_no_such_service_error_recognises_known_phrases() {
        assert!(is_no_such_service_error(
            "Could not find service \"foo\" in domain"
        ));
        assert!(is_no_such_service_error("No such process"));
        assert!(!is_no_such_service_error("permission denied"));
    }

    #[test]
    fn user_domain_target_starts_with_gui() {
        // We can't pin the UID (varies per host) but the prefix is
        // invariant.
        let t = user_domain_target().expect("uid resolves");
        assert!(t.starts_with("gui/"), "got: {t}");
        // Must be `gui/<digits>` and nothing else.
        let suffix = t.strip_prefix("gui/").unwrap();
        assert!(
            suffix.chars().all(|c| c.is_ascii_digit()),
            "uid suffix must be all digits, got: {t}"
        );
    }

    // ---------- driver tests using a custom agents dir ----------
    //
    // These exercise the file-writing half of `install`/`uninstall`
    // without touching the real `launchctl` GUI domain. They run on
    // any host with a writable /tmp.

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Tempdir helper mirroring `systemd_user::tests::TestRoot`:
    /// unique per process+test+call, removed on drop.
    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hhagent-launchd-test-{}-{}-{}",
                std::process::id(),
                label,
                n
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create test root");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn install_writes_plist_with_expected_content() {
        let dir = TestRoot::new("install-content");
        let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
        let spec = minimal_spec("hhagent-test");
        sup.install(&spec).expect("install");

        let path = sup.plist_path("hhagent-test");
        assert!(path.exists(), "plist not written: {}", path.display());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("<?xml version=\"1.0\""), "{body}");
        assert!(
            body.contains("<key>Label</key>\n    <string>hhagent-test</string>"),
            "{body}"
        );
        assert!(
            body.contains("<string>/usr/bin/true</string>"),
            "{body}"
        );
    }

    #[test]
    fn install_rejects_relative_program_path() {
        let dir = TestRoot::new("rel-program");
        let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
        let mut spec = minimal_spec("svc");
        spec.program = PathBuf::from("relative/foo");
        let err = sup.install(&spec).expect_err("relative program");
        assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    }

    #[test]
    fn install_rejects_invalid_name() {
        let dir = TestRoot::new("bad-name");
        let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
        let mut spec = minimal_spec("svc");
        spec.name = "../traversal".into();
        let err = sup.install(&spec).expect_err("traversal name");
        assert!(matches!(err, SupervisorError::InvalidName(_)), "{err}");
    }

    #[test]
    fn install_rejects_relative_working_dir() {
        let dir = TestRoot::new("rel-wd");
        let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
        let mut spec = minimal_spec("svc");
        spec.working_dir = Some(PathBuf::from("relative/wd"));
        let err = sup.install(&spec).expect_err("relative wd");
        assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    }

    #[test]
    fn install_creates_agents_dir_if_missing() {
        let dir = TestRoot::new("nested-dir");
        let nested = dir.path().join("a").join("b").join("c");
        let sup = LaunchAgents::with_agents_dir(nested.clone());
        sup.install(&minimal_spec("svc")).expect("install");
        assert!(nested.is_dir(), "nested agents dir should be created");
        assert!(nested.join("svc.plist").is_file());
    }

    #[test]
    fn uninstall_removes_plist_file() {
        let dir = TestRoot::new("uninstall");
        let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
        sup.install(&minimal_spec("svc")).expect("install");
        let path = sup.plist_path("svc");
        assert!(path.exists());
        sup.uninstall("svc").expect("uninstall");
        assert!(!path.exists(), "plist still present after uninstall");
    }

    #[test]
    fn uninstall_is_idempotent_when_nothing_installed() {
        let dir = TestRoot::new("idempotent");
        let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
        sup.uninstall("nonexistent")
            .expect("uninstall must be idempotent");
    }

    #[test]
    fn status_returns_not_installed_when_plist_absent() {
        let dir = TestRoot::new("status-absent");
        let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
        let s = sup.status("never-installed").expect("status");
        assert_eq!(s, ServiceStatus::NotInstalled);
    }
}
