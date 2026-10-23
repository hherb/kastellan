//! Linux backend: `systemd --user` user-level services.
//!
//! Generates a `<name>.service` unit file from a [`crate::ServiceSpec`],
//! writes it to `~/.config/systemd/user/`, and drives `systemctl --user`
//! for the lifecycle (`daemon-reload`, `enable`, `start`, `stop`,
//! `disable`, plus `is-active` for status queries).
//!
//! ### Surviving a reboot (#508)
//!
//! Writing a unit file only makes the unit *known*. At boot the per-user
//! manager starts `default.target` and pulls in whatever is linked below
//! it, so [`Supervisor::install`] and [`Supervisor::install_target`] also
//! run `systemctl --user enable`, creating that link from the unit's
//! `[Install] WantedBy=` directive. Without it a rebooted host comes up
//! with nothing running, and `loginctl enable-linger` does not help —
//! lingering starts the user *manager*, not units that nothing wants.
//!
//! This is the Linux half of a cross-platform contract: the launchd
//! backend gets the same guarantee declaratively, from the unconditional
//! `RunAtLoad=true` in every generated plist. Both are established at
//! *install* time, so a service is armed for the next boot as soon as it
//! is installed, whether or not it is started now.
//!
//! Why user-level only:
//!   - `systemctl --user` does not need root and runs against the
//!     per-user systemd manager that's already up in any normal desktop
//!     or `loginctl enable-linger`-ed headless session.
//!   - Containment is consistent with the rest of the codebase
//!     (`systemd-run --user --scope` cgroup wrapper, `bwrap` user
//!     namespaces) — no privilege escalation, no system-wide effect.
//!
//! Module structure mirrors `launchd_agents` (and `sandbox/src/linux_cgroup.rs`):
//!   1. The pure builders [`build_unit_file`] / [`build_target_unit`] and
//!      the [`validate_service_name`] guard live in the sibling
//!      [`builder`] module (no I/O, fully unit-testable). They are
//!      re-exported here so `systemd_user::build_unit_file` etc. keep
//!      their public paths.
//!   2. [`SystemdUser`] — the driver that combines the builders with file
//!      I/O and `systemctl --user` invocations.
//!   3. [`probe`] — fail-closed check that `systemctl --user` is usable.
//!
//! Driver tests (file-writing half of install/uninstall/install_target)
//! live in the sibling [`tests`] module; the pure-builder tests live
//! alongside their code in [`builder`].

use std::fs;
use std::path::{Path, PathBuf};
use crate::atomic_write::write_atomic;
use crate::remove_after::remove_after_stop;
use crate::{ServiceSpec, ServiceStatus, Supervisor, SupervisorError, TargetSpec};

mod builder;
// Re-exported so `systemd_user::build_unit_file`, `::build_target_unit`,
// and `::validate_service_name` keep their public paths after the split.
pub use builder::{build_target_unit, build_unit_file, validate_service_name};

mod systemctl;
pub use systemctl::probe;
use systemctl::{run_systemctl_user, run_user, status_from_is_active, stop_and_disable};

/// `systemctl --user` driver.
///
/// `units_dir` is the directory the unit file is written to. Defaults
/// to `~/.config/systemd/user/`, which is the only location the
/// running user manager actually reads. Tests can point at a temp dir
/// to exercise just the file-writing half without touching the live
/// manager.
pub struct SystemdUser {
    units_dir: PathBuf,
}

impl SystemdUser {
    /// Construct a driver pointing at the default user units dir.
    ///
    /// Resolves `~/.config/systemd/user/` from `$HOME` (does not yet
    /// honour `$XDG_CONFIG_HOME` — that's a follow-up if anyone needs
    /// it). The directory is *not* created here; [`install`] creates
    /// it on demand so the driver itself has no I/O side effects.
    ///
    /// [`install`]: SystemdUser::install
    pub fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            units_dir: home.join(".config").join("systemd").join("user"),
        }
    }

    /// Construct a driver writing units into a custom directory.
    ///
    /// Used by tests that want to exercise just the file-writing half
    /// without polluting the user's real systemd config dir or
    /// daemon-reloading the live manager.
    ///
    /// **Note:** units written here are invisible to `systemctl
    /// --user` because the running user manager only reads its
    /// configured search path. So `start`/`stop`/`status` against a
    /// custom-dir driver will fail unless the path happens to be one
    /// the manager already scans.
    pub fn with_units_dir(units_dir: PathBuf) -> Self {
        Self { units_dir }
    }

    /// Return the directory this driver writes units into.
    pub fn units_dir(&self) -> &Path {
        &self.units_dir
    }

    /// Path the driver would write `<name>.service` to.
    pub fn unit_path(&self, name: &str) -> PathBuf {
        self.units_dir.join(format!("{name}.service"))
    }

    /// Path the driver would write `<name>.target` to.
    pub fn target_path(&self, name: &str) -> PathBuf {
        self.units_dir.join(format!("{name}.target"))
    }

    /// Run `systemctl --user daemon-reload`, returning a structured
    /// error on non-zero exit.
    fn daemon_reload(&self) -> Result<(), SupervisorError> {
        run_systemctl_user(&["daemon-reload"]).map(|_| ())
    }

    /// Run `systemctl --user enable <unit>`, linking it into the target
    /// named by its `[Install] WantedBy=` directive.
    ///
    /// `unit` must carry its suffix (`foo.service`, `kastellan.target`).
    ///
    /// **This is what makes the unit come back after a reboot** (#508).
    /// Writing the unit file only makes it *known* to the user manager;
    /// at boot the manager starts `default.target`, and pulls in solely
    /// what is linked below it. Without this call nothing is linked, so
    /// a rebooted host runs nothing — and `loginctl enable-linger` does
    /// not help, because lingering starts the user *manager*, not units
    /// that nothing wants.
    ///
    /// Errors are propagated rather than swallowed: a silent failure
    /// here reproduces exactly the bug this call exists to fix, and the
    /// operator would not learn about it until the next reboot.
    fn enable(&self, unit: &str) -> Result<(), SupervisorError> {
        run_systemctl_user(&["enable", unit]).map(|_| ())
    }
}

impl Default for SystemdUser {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor for SystemdUser {
    fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError> {
        self.write_unit_file(spec)?;
        // Only talk to the live manager when we're writing into the real
        // user units dir — pointless otherwise (it doesn't scan custom
        // dirs anyway), and it lets unit tests run without a live --user
        // manager.
        if self.is_default_units_dir() {
            // Reload first so the manager can see the file we just wrote;
            // `enable` reads its `[Install]` section.
            self.daemon_reload()?;
            // Then link it under `[Install] WantedBy=` so it starts itself
            // after a reboot — the launchd backend gets this for free from
            // its unconditional `RunAtLoad=true`, and Linux must ask (#508).
            self.enable(&format!("{}.service", spec.name))?;
        }
        Ok(())
    }

    fn start(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        run_systemctl_user(&["start", &format!("{name}.service")]).map(|_| ())
    }

    fn stop(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        run_systemctl_user(&["stop", &format!("{name}.service")]).map(|_| ())
    }

    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        validate_service_name(name)?;
        // `uninstall` drives systemctl only for the canonical units dir: for
        // a custom dir (tests) a name collision must not stop someone else's
        // real unit — the launchd backend's rule too.
        let stopped = if self.is_default_units_dir() {
            stop_and_disable(&format!("{name}.service"))
        } else {
            Ok(())
        };
        // A unit that may still be running (or still linked for the next
        // boot) keeps its file, and the error says so (#896).
        remove_after_stop(stopped, &self.unit_path(name))?;
        if self.is_default_units_dir() {
            self.daemon_reload()?;
        }
        Ok(())
    }

    fn status_with_word(
        &self,
        name: &str,
    ) -> Result<(ServiceStatus, Option<String>), SupervisorError> {
        validate_service_name(name)?;
        // No file on disk → not installed (regardless of what
        // systemctl thinks; the live manager may have a unit cached).
        // `try_exists`: an unreadable units dir is an error, not "absent".
        let path = self.unit_path(name);
        if !path
            .try_exists()
            .map_err(|e| SupervisorError::Io(format!("stat {}: {e}", path.display())))?
        {
            return Ok((ServiceStatus::NotInstalled, None));
        }
        // Bounded, and read by the pure `status_from_is_active`: stdout's
        // word, not the exit code — and no word at all is an error (#896).
        let unit = format!("{name}.service");
        status_from_is_active(&unit, &run_user(&["is-active", &unit])?)
    }

    /// Install the target the systemd-native way: write each member unit,
    /// then a `kastellan.target` unit that `Wants=` them.
    ///
    /// Overrides the generic-bundle default. Member unit files are written
    /// in order (inheriting the same name/absolute-path validation as
    /// [`Supervisor::install`]) with a single `daemon-reload` at the end;
    /// fail-fast with no rollback — on a mid-loop error, already-written
    /// member units remain and the `.target` unit is not written.
    fn install_target(
        &self,
        target: &TargetSpec,
        members: &[ServiceSpec],
    ) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // Member names are formatted into the Wants= directive of the target
        // unit; validate them before writing anything so a crafted name cannot
        // inject directives.
        for member in &target.members {
            validate_service_name(member)?;
        }
        // Member units first. `write_unit_file` applies the same
        // name/absolute-path validation as `install` but skips the per-unit
        // daemon-reload; we reload once at the end so a multi-member target
        // costs a single reload, not one per member.
        for spec in members {
            self.write_unit_file(spec)?;
        }
        // Then the .target unit that Wants= them.
        // Ensure the units dir exists even when `members` is empty (the
        // member loop, which also creates it via `write_unit_file`, ran zero
        // times).
        fs::create_dir_all(&self.units_dir).map_err(|e| {
            SupervisorError::Io(format!("create {}: {e}", self.units_dir.display()))
        })?;
        let path = self.target_path(&target.name);
        write_atomic(&path, build_target_unit(target).as_bytes())?;
        if self.is_default_units_dir() {
            self.daemon_reload()?;
            // Enable the *target* only. It is the boot entry point — the
            // unit carrying `WantedBy=default.target` — and its own
            // `Wants=<member>.service` line pulls every member in whenever
            // it starts. Enabling the members as well would add a second
            // link expressing the same edge, so we don't (#508).
            self.enable(&format!("{}.target", target.name))?;
        }
        Ok(())
    }

    /// Start the native `kastellan.target`; systemd resolves member start
    /// order from each member unit's `After=`.
    fn start_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // systemd resolves member ordering from each member's After=.
        run_systemctl_user(&["start", &format!("{}.target", target.name)]).map(|_| ())
    }

    /// Stop the native `kastellan.target`; the stop propagates to members
    /// via their `PartOf=`.
    fn stop_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // PartOf= on members propagates the stop to them.
        run_systemctl_user(&["stop", &format!("{}.target", target.name)]).map(|_| ())
    }

    /// Tear down the native target: stop and disable it, uninstall members
    /// in reverse, then remove the `.target` unit file.
    ///
    /// Fail-fast, like the trait default (#896): a failed stop or disable,
    /// or a member that could not be uninstalled, returns at once and keeps
    /// the `.target` file, so a retry finds everything it needs — every
    /// step is idempotent ("not loaded" and "no unit file" are success).
    fn uninstall_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // Every name before any side effect, as `install_target` does: a bad
        // member found mid-teardown would wedge every retry on it.
        for member in &target.members {
            validate_service_name(member)?;
        }
        // Stop the target (propagates via PartOf= to members whose spec sets
        // `part_of`, as both of `kastellan_target_spec`'s do) and undo
        // `install_target`'s enable while the unit file is still on disk —
        // `disable` resolves the `[Install]` section to know which links to
        // drop, so removing the file first would strand the
        // `default.target.wants/` symlink.
        let stopped = if self.is_default_units_dir() {
            stop_and_disable(&format!("{}.target", target.name))
        } else {
            Ok(())
        };
        // Members next, so a member that fails leaves the `.target` file.
        let torn_down =
            stopped.and_then(|()| target.members.iter().rev().try_for_each(|m| self.uninstall(m)));
        remove_after_stop(torn_down, &self.target_path(&target.name))?;
        if self.is_default_units_dir() {
            self.daemon_reload()?;
        }
        Ok(())
    }
}

impl SystemdUser {
    /// Validate a spec and write its `<name>.service` unit file, **without**
    /// running `daemon-reload`. Callers that write several units in one
    /// batch (e.g. [`Supervisor::install_target`]) reload once at the end
    /// instead of once per unit.
    fn write_unit_file(&self, spec: &ServiceSpec) -> Result<(), SupervisorError> {
        validate_service_name(&spec.name)?;
        // Ordering fields are formatted into unit-file directives, so they
        // must pass the same name validation as the unit name — otherwise a
        // crafted value (e.g. containing a newline) could inject directives.
        for dep in &spec.after {
            validate_service_name(dep)?;
        }
        if let Some(target) = &spec.part_of {
            validate_service_name(target)?;
        }
        // Working dir / log paths must be absolute or systemd refuses
        // them at unit-load time. Catch this at the host boundary so
        // we get a structured error instead of a parse failure on
        // daemon-reload.
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
        // program must be absolute too — systemd refuses relative
        // ExecStart paths.
        if !spec.program.is_absolute() {
            return Err(SupervisorError::Io(format!(
                "program must be absolute, got {}",
                spec.program.display()
            )));
        }
        // Env-file paths too. This one is fail-OPEN without the check: systemd
        // drops a relative `EnvironmentFile=` with a journal warning and loads
        // the unit anyway, so the daemon starts with NO environment at all —
        // #458's failure in its total form, and invisible unless the operator
        // reads the journal.
        for ef in &spec.environment_files {
            crate::env_file::validate_env_file_path(&ef.path)?;
        }

        // Defense-in-depth (audit finding #10): path fields are written into
        // unit-file directives verbatim via `Display`, so a value containing a
        // newline (e.g. "/tmp\nExecStartPre=/evil") would inject a `[Service]`
        // directive. `args`/`env` already go through `quote_if_needed` (which
        // escapes newlines), but these path fields did not. Specs are
        // code-constructed today, but `ServiceSpec` is `Deserialize`, so reject
        // any control character before the write.
        // `environment_files` is deliberately NOT in this chain: it is screened
        // by `validate_env_file_path` above, which is stricter (it also refuses
        // non-UTF-8, which `to_string_lossy` would mangle into a different path
        // that systemd silently skips). Listing it here too would let that call
        // be deleted with every test still green, which is exactly the mutation
        // this backend cannot afford now that #530 made the driver the only
        // thing standing between a control character and a rendered directive.
        let path_fields = std::iter::once(("program", &spec.program))
            .chain(spec.working_dir.as_ref().map(|p| ("working_dir", p)))
            .chain(spec.stdout_log.as_ref().map(|p| ("stdout_log", p)))
            .chain(spec.stderr_log.as_ref().map(|p| ("stderr_log", p)));
        for (field, p) in path_fields {
            if p.to_string_lossy().contains(|c: char| c.is_control()) {
                return Err(SupervisorError::Io(format!(
                    "{field} must not contain control characters, got {p:?}"
                )));
            }
        }

        fs::create_dir_all(&self.units_dir)
            .map_err(|e| SupervisorError::Io(format!("create {}: {e}", self.units_dir.display())))?;

        let path = self.unit_path(&spec.name);
        let body = build_unit_file(spec);
        write_atomic(&path, body.as_bytes())
    }

    /// True iff the driver writes into the canonical
    /// `~/.config/systemd/user/` location. Only then are `daemon-reload`,
    /// `install`'s `enable` and the uninstalls' `stop` + `disable` sent to
    /// the live manager: it does not scan a custom dir (tests), and a name
    /// collision there must not stop or unlink someone else's real unit.
    fn is_default_units_dir(&self) -> bool {
        let home = match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h),
            None => return false,
        };
        self.units_dir == home.join(".config").join("systemd").join("user")
    }
}

#[cfg(test)]
mod tests;
