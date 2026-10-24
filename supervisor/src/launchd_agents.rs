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
//!     <string>kastellan-core</string>
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
//! | `install`  | write unit + `daemon-reload` + `enable` | write plist (no launchctl)                      |
//! | `start`    | `start <name>.service`             | `bootstrap gui/<uid> <plist-path>` (load + run)      |
//! | `stop`     | `stop <name>.service`              | `bootout gui/<uid>/<label>` (unload + stop)          |
//! | `uninstall`| best-effort stop/disable + remove  | `bootout`, then remove plist (kept if bootout fails) |
//! | `status`   | `is-active <name>.service`         | parse `print gui/<uid>/<label>` `state = ...` line   |
//!
//! On macOS, `bootstrap` *is* the load step — there is no separate
//! daemon-reload — and `RunAtLoad=true` (set unconditionally) means
//! the program runs as soon as the agent is loaded. `bootout` undoes
//! both, so a `stop` followed by a `start` re-runs the program from
//! the persisted plist on disk. This is consistent with the Linux
//! semantic of "stop preserves the unit file, start re-activates it".
//!
//! `RunAtLoad=true` is also what satisfies the [`crate::Supervisor`]
//! "install implies auto-start at boot" contract here: launchd loads
//! every plist in `~/Library/LaunchAgents/` at login, and runs those
//! that ask for it. The systemd backend needs an explicit
//! `systemctl --user enable` for the same guarantee — it had none until
//! [#508](https://github.com/hherb/kastellan/issues/508), so macOS
//! survived a reboot and Linux did not.
//!
//! ### No native ordering — readiness-based bundles
//!
//! launchd has no inter-agent ordering and no target/aggregation
//! concept. The [`crate::Supervisor`] default `*_target` methods install
//! and start members in declared order, but launchd may still race their
//! startup. kastellan tolerates this because core fail-closed-restarts
//! until Postgres is reachable (`KeepAlive=true`), so the bundle
//! converges regardless of launchd start order. `ServiceSpec.after` and
//! `ServiceSpec.part_of` are therefore **ignored** by `build_plist`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::atomic_write::write_atomic;
use crate::remove_after::remove_after_stop;
use crate::{ServiceSpec, ServiceStatus, Supervisor, SupervisorError};

/// Wall-clock cap on every `launchctl` call made through `run_launchctl`
/// (`bootstrap`, `bootout`, `print`).
///
/// launchctl normally returns in well under a second. The cap exists
/// solely so a *churn-degraded* `gui/<uid>` domain — which can leave
/// `bootstrap`/`bootout` blocked indefinitely under heavy
/// install/uninstall load (issue #130) — surfaces as a fast, structured
/// `Backend` error instead of an unbounded 0-CPU hang that wedges the
/// caller (and, in the test harness, every thread waiting on the
/// process-global serial lock the hung bring-up still holds). 20 s is
/// far above any healthy call yet bounded enough to fail well within a
/// test's bring-up budget.
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(20);

/// Pure, I/O-free helpers (the plist builder + the name validator).
/// Re-exported below so `launchd_agents::build_plist` and
/// `launchd_agents::validate_service_name` keep their public paths.
mod builders;
pub use builders::{build_plist, validate_service_name};

mod unload_wait;

/// The `launchctl` invocations and the pure readers of their answers.
mod launchctl;
use launchctl::{
    bootout_outcome, evidence_from_print, is_loaded_in_domain, run_launchctl, status_from_print,
    user_domain_target,
};

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
        // Env-file paths too: this backend resolves them against the INSTALLER's
        // cwd, and the systemd side would silently drop a relative directive.
        for ef in &spec.environment_files {
            crate::env_file::validate_env_file_path(&ef.path)?;
        }

        // launchd has no operator-controllable exponential restart backoff
        // (its only knob, ThrottleInterval, is a constant floor, not a ramp).
        // Honour the field by surfacing it, then fall back to KeepAlive's
        // default — same "degrade with a visible warning" posture as the
        // `after`/`part_of` fields, which launchd also cannot express.
        if spec.restart_backoff.is_some() {
            tracing::warn!(
                service = %spec.name,
                "restart_backoff requested but launchd has no equivalent; \
                 falling back to KeepAlive default"
            );
        }

        fs::create_dir_all(&self.agents_dir)
            .map_err(|e| SupervisorError::Io(format!("create {}: {e}", self.agents_dir.display())))?;

        // launchd has no `EnvironmentFile=` directive, so honour
        // `spec.environment_files` by reading each one at install time and
        // folding its KEY=value pairs into the plist's `EnvironmentVariables`
        // — in declared order, later winning on key collision (over both
        // each other and `spec.env`), matching systemd's file-order-wins
        // semantics at start time. This gives macOS the same guarantee the
        // Linux backend gets from `EnvironmentFile=` rather than silently
        // dropping the operator's tuned config. An absent OPTIONAL file is
        // skipped (the normal state of `kastellan.env.local`); an absent
        // REQUIRED one is still an error.
        let path = self.plist_path(&spec.name);
        let mut merged = spec.clone();
        crate::env_file::fold_env_files(&mut merged.env, &spec.environment_files)?;
        merged.environment_files = Vec::new(); // already folded into env
        let body = build_plist(&merged);
        write_atomic(&path, body.as_bytes())?;
        // Unlike the Linux backend, there is no separate "reload"
        // step — `bootstrap` is the load step and is invoked from
        // `start`. So `install` ends here.
        Ok(())
    }

    fn start(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        let path = self.plist_path(name);
        // `try_exists`: an unreadable agents dir is an error, not "absent".
        if !path
            .try_exists()
            .map_err(|e| SupervisorError::Io(format!("stat {}: {e}", path.display())))?
        {
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
        if is_loaded_in_domain(&target)? {
            return Ok(());
        }
        let path_str = path.to_string_lossy().into_owned();
        run_launchctl(&["bootstrap", &domain, &path_str])?;
        Ok(())
    }

    fn stop(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        let target = format!("{}/{name}", user_domain_target()?);
        // bootout against an agent that isn't loaded fails with exit 3,
        // "Boot-out failed: 3: No such process" (ESRCH), or with exit 112
        // "Could not find domain" when there is no GUI login. Both are
        // success (`is_not_loaded_answer`).
        bootout_outcome(run_launchctl(&["bootout", &target]))
    }

    /// Stop the members in reverse order, each one **gone from the
    /// domain** before the next is asked to stop.
    ///
    /// `kastellan-cli install` stops the target and then starts it, and
    /// `start` skips `bootstrap` for an agent launchd has not finished
    /// removing (see `unload_wait`) — so without this wait a slow-exiting
    /// Postgres was left unloaded. Waiting member by member also lets core
    /// drop its connections before Postgres is told to stop.
    ///
    /// Fail-fast like the trait default: if a member does not leave the
    /// domain in time, the members before it are not stopped, and the
    /// caller must not go on to `start_target` as if they were.
    ///
    /// The wait lives here, not in `stop`: a caller of `stop` may still
    /// hold connections to the agent (PG tests stop a `PgCluster` through
    /// its `ServiceGuard` while their own pools may be connected), and
    /// Postgres's SIGTERM shutdown waits for every client — the caller
    /// would block it until launchd's SIGKILL, which leaks the cluster's
    /// SysV shared-memory segment (for good, once the test deletes the
    /// data dir that would have let a later postmaster reclaim it).
    fn stop_target(&self, target: &crate::TargetSpec) -> Result<(), SupervisorError> {
        let domain = user_domain_target()?;
        unload_wait::stop_each_in_reverse(
            &target.members,
            |name| self.stop(name),
            |name| unload_wait::wait_until_unloaded(&format!("{domain}/{name}")),
        )
    }

    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        // Bootout only when we're operating on the canonical agents dir.
        // For custom dirs (tests) we skip the launchctl call entirely so
        // a name collision with a real installed agent cannot bootout
        // someone else's service. `stop` already reads "not loaded" (incl.
        // no GUI domain) as success; only a bootout that may have left the
        // agent loaded fails. A `$HOME` owned by another uid (sudo) is
        // refused before any of this (`ensure_home_owned_by`).
        let bootout = if self.is_default_agents_dir() { self.stop(name) } else { Ok(()) };
        remove_after_stop(bootout, &self.plist_path(name))
    }

    fn status_with_word(
        &self,
        name: &str,
    ) -> Result<(ServiceStatus, Option<String>), SupervisorError> {
        validate_service_name(name)?;
        // No file on disk → not installed (regardless of what
        // launchctl thinks; the live manager may have a stale entry
        // cached for a name we just uninstalled but on a different
        // driver instance).
        // `try_exists`: an unreadable agents dir is an error, not "absent".
        let path = self.plist_path(name);
        if !path
            .try_exists()
            .map_err(|e| SupervisorError::Io(format!("stat {}: {e}", path.display())))?
        {
            return Ok((ServiceStatus::NotInstalled, None));
        }
        let target = format!("{}/{name}", user_domain_target()?);
        // Bounded by `run_launchctl`, so a churn-degraded domain cannot
        // hang a status query either.
        status_from_print(&target, run_launchctl(&["print", &target]))
    }

    fn crash_evidence(&self, name: &str) -> Result<Option<String>, SupervisorError> {
        validate_service_name(name)?;
        let target = format!("{}/{name}", user_domain_target()?);
        evidence_from_print(&target, run_launchctl(&["print", &target]))
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

#[cfg(test)]
mod tests;

