//! Linux backend: `systemd --user` user-level services.
//!
//! Generates a `<name>.service` unit file from a [`crate::ServiceSpec`],
//! writes it to `~/.config/systemd/user/`, and drives `systemctl --user`
//! for the lifecycle (`daemon-reload`, `start`, `stop`, `disable`, plus
//! `is-active` for status queries).
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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{ServiceSpec, ServiceStatus, Supervisor, SupervisorError, TargetSpec};

mod builder;
// Re-exported so `systemd_user::build_unit_file`, `::build_target_unit`,
// and `::validate_service_name` keep their public paths after the split.
pub use builder::{build_target_unit, build_unit_file, validate_service_name};

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
}

impl Default for SystemdUser {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor for SystemdUser {
    fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError> {
        self.write_unit_file(spec)?;
        // Only run daemon-reload when we're writing into the real
        // user units dir — pointless otherwise (the live manager
        // doesn't scan custom dirs anyway), and it lets unit tests
        // run without a live --user manager.
        if self.is_default_units_dir() {
            self.daemon_reload()?;
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
        let unit = format!("{name}.service");
        // Stop is best-effort; the unit may already be inactive.
        let _ = run_systemctl_user(&["stop", &unit]);
        // Disable is best-effort; the unit may not be enabled.
        let _ = run_systemctl_user(&["disable", &unit]);

        let path = self.unit_path(name);
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| SupervisorError::Io(format!("remove {}: {e}", path.display())))?;
        }
        if self.is_default_units_dir() {
            self.daemon_reload()?;
        }
        Ok(())
    }

    fn status(&self, name: &str) -> Result<ServiceStatus, SupervisorError> {
        validate_service_name(name)?;
        // No file on disk → not installed (regardless of what
        // systemctl thinks; the live manager may have a unit cached).
        if !self.unit_path(name).exists() {
            return Ok(ServiceStatus::NotInstalled);
        }
        // `systemctl is-active` exits 0 for active, 3 for inactive,
        // and prints the canonical state on stdout in either case.
        // We trust stdout, not the exit code.
        let unit = format!("{name}.service");
        let out = Command::new("systemctl")
            .args(["--user", "is-active", &unit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SupervisorError::Io(format!("spawn systemctl: {e}")))?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(match state.as_str() {
            "active" => ServiceStatus::Active,
            "inactive" => ServiceStatus::Inactive,
            "failed" => ServiceStatus::Failed,
            // unknown / activating / deactivating / reloading: the
            // unit *exists* (we checked the file) so it's not
            // NotInstalled. Map to Inactive so callers don't have to
            // poll a transient state forever.
            _ => ServiceStatus::Inactive,
        })
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

    /// Tear down the native target: best-effort stop, uninstall members in
    /// reverse, then remove the `.target` unit file.
    fn uninstall_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // Stop the target (propagates to members via PartOf=), then
        // remove every member unit and the target unit file.
        let _ = run_systemctl_user(&["stop", &format!("{}.target", target.name)]);
        for name in target.members.iter().rev() {
            // Best-effort: keep tearing down remaining members even if one
            // member's uninstall errors (e.g. its unit file is already gone).
            let _ = self.uninstall(name);
        }
        let path = self.target_path(&target.name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| {
                SupervisorError::Io(format!("remove {}: {e}", path.display()))
            })?;
        }
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

        fs::create_dir_all(&self.units_dir)
            .map_err(|e| SupervisorError::Io(format!("create {}: {e}", self.units_dir.display())))?;

        let path = self.unit_path(&spec.name);
        let body = build_unit_file(spec);
        write_atomic(&path, body.as_bytes())
    }

    /// True iff the driver writes into the canonical
    /// `~/.config/systemd/user/` location. Used to decide whether
    /// `daemon-reload` makes sense — for custom dirs (tests) it
    /// doesn't, since the live manager doesn't scan them.
    fn is_default_units_dir(&self) -> bool {
        let home = match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h),
            None => return false,
        };
        self.units_dir == home.join(".config").join("systemd").join("user")
    }
}

/// Probe whether `systemctl --user` can talk to a live user manager.
///
/// Mirrors `sandbox::linux_cgroup::cgroup_probe`: succeed silently or
/// return a structured error with a hint pointing at the most common
/// recovery (`loginctl enable-linger $USER` for headless sessions).
///
/// Used by callers that want fail-closed behaviour at startup — if
/// the supervisor cannot reach the user manager, every lifecycle
/// call would fail anyway, so failing once up front is friendlier.
pub fn probe() -> Result<(), SupervisorError> {
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SupervisorError::Probe(format!("spawn systemctl: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let hint = if stderr.contains("Failed to connect") || stderr.contains("No such file") {
        "\n\nThe per-user systemd manager does not appear to be running. \
         On a normal desktop session it starts automatically; on headless \
         hosts run `loginctl enable-linger $USER` and re-login."
    } else {
        ""
    };
    Err(SupervisorError::Probe(format!(
        "systemctl --user show-environment failed: {}{hint}",
        stderr.trim()
    )))
}

/// Atomically write `bytes` to `path` via write-to-tmp + rename.
///
/// systemd's daemon-reload reads each unit file in one shot, but a
/// concurrent reader (e.g. another `systemctl --user` invocation)
/// could otherwise see a half-written file. Atomic rename keeps the
/// observable state binary: either the old contents are visible, or
/// the new ones — never a torn read.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SupervisorError> {
    let tmp = path.with_extension("service.tmp");
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

/// Run `systemctl --user <args>` with stdio captured. Maps non-zero
/// exits to [`SupervisorError::Backend`] with the trimmed stderr.
fn run_systemctl_user(args: &[&str]) -> Result<String, SupervisorError> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SupervisorError::Io(format!("spawn systemctl: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(SupervisorError::Backend(format!(
            "systemctl --user {}: {stderr}",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests;
