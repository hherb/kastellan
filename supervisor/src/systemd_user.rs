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
//! Module structure mirrors `sandbox/src/linux_cgroup.rs`:
//!   1. A pure [`build_unit_file`] returning the unit-file contents as a
//!      `String`. No I/O, fully unit-testable.
//!   2. A pure [`validate_service_name`] guarding against path traversal
//!      and systemd-syntax breakage.
//!   3. [`SystemdUser`] — the driver that combines the builder with file
//!      I/O and `systemctl --user` invocations.
//!   4. [`probe`] — fail-closed check that `systemctl --user` is usable.
//!
//! ### Unit-file shape
//!
//! ```ini
//! [Unit]
//! Description=hhagent service: <name>
//! After=<dep>.service                   # one line per spec.after entry (omitted when empty)
//! PartOf=<target>.target                # only when spec.part_of is set
//!
//! [Service]
//! Type=simple
//! ExecStart=/abs/program "arg one" arg2
//! Environment="KEY=value with spaces"
//! WorkingDirectory=/abs/dir
//! StandardOutput=append:/abs/log/out
//! StandardError=append:/abs/log/err
//! Restart=on-failure                    # only when keep_alive=true
//! RestartSec=5
//! TimeoutStopSec=10
//!
//! [Install]
//! WantedBy=<target>.target              # spec.part_of.target when set, else default.target
//! ```
//!
//! Each section's directives are emitted in a deterministic order so the
//! generated file is diffable and unit-testable.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{ServiceSpec, ServiceStatus, Supervisor, SupervisorError};

/// Default seconds before SIGKILL after SIGTERM on stop.
///
/// 10 s matches systemd's own default and is short enough that test
/// teardown doesn't hang if the inner process ignores SIGTERM.
const DEFAULT_TIMEOUT_STOP_SEC: u32 = 10;

/// Default seconds between restart attempts when `keep_alive=true`.
///
/// Resists tight crash loops without being so long that recovery from
/// transient errors is annoyingly slow.
const DEFAULT_RESTART_SEC: u32 = 5;

/// Maximum length of a service name. Generous compared to systemd's
/// own 255-byte filename ceiling — leaves headroom for the
/// `.service` suffix and any future namespacing prefix.
const MAX_NAME_LEN: usize = 200;

/// Build the textual contents of a `<name>.service` unit file.
///
/// Pure function: no I/O, no environment access, deterministic output.
/// Returns the full file as a `String` ready to be written to disk.
///
/// The caller is responsible for validating the spec's name with
/// [`validate_service_name`] before calling this — the builder assumes
/// its input is already well-formed.
///
/// # Quoting
///
/// `program` and each entry in `args` are emitted into `ExecStart=`,
/// space-separated. Tokens that contain whitespace, quotes, or
/// backslashes are wrapped in `"..."` with `"` and `\` escaped per
/// systemd's quoting rules. Same for environment values.
pub fn build_unit_file(spec: &ServiceSpec) -> String {
    let mut out = String::with_capacity(512);

    // [Unit] section.
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=hhagent service: {}\n", spec.name));
    // Ordering: one After= per dependency. systemd only *orders* against
    // units present in the same start transaction — harmless if absent.
    for dep in &spec.after {
        out.push_str(&format!("After={dep}.service\n"));
    }
    // PartOf binds this unit's stop/restart to the target's: `systemctl
    // stop <target>.target` propagates to PartOf members.
    if let Some(target) = &spec.part_of {
        out.push_str(&format!("PartOf={target}.target\n"));
    }
    out.push('\n');

    // [Service] section.
    out.push_str("[Service]\n");
    out.push_str("Type=simple\n");

    // ExecStart: program then args, space-separated, each quoted only
    // when the token actually needs it.
    let mut exec_start = String::from("ExecStart=");
    exec_start.push_str(&quote_if_needed(&spec.program.to_string_lossy()));
    for a in &spec.args {
        exec_start.push(' ');
        exec_start.push_str(&quote_if_needed(a));
    }
    exec_start.push('\n');
    out.push_str(&exec_start);

    // Environment: one per line, deterministic order = the order the
    // caller provided. systemd accepts both `Environment=KEY=val` and
    // `Environment="KEY=val with spaces"`; we always use the second
    // form when the value contains anything fragile, the first when not.
    for (k, v) in &spec.env {
        let kv = format!("{k}={v}");
        out.push_str("Environment=");
        out.push_str(&quote_if_needed(&kv));
        out.push('\n');
    }

    if let Some(dir) = &spec.working_dir {
        out.push_str(&format!("WorkingDirectory={}\n", dir.display()));
    }

    if let Some(log) = &spec.stdout_log {
        out.push_str(&format!("StandardOutput=append:{}\n", log.display()));
    }
    if let Some(log) = &spec.stderr_log {
        out.push_str(&format!("StandardError=append:{}\n", log.display()));
    }

    if spec.keep_alive {
        out.push_str("Restart=on-failure\n");
        out.push_str(&format!("RestartSec={}\n", DEFAULT_RESTART_SEC));
    }

    out.push_str(&format!("TimeoutStopSec={}\n", DEFAULT_TIMEOUT_STOP_SEC));
    out.push('\n');

    // [Install] section so `systemctl --user enable` works if the caller
    // ever wants it. We don't enable by default — that's a separate
    // policy decision.
    out.push_str("[Install]\n");
    // A target member is wanted by its target; a standalone service is
    // wanted by default.target so `enable` starts it at login.
    match &spec.part_of {
        Some(target) => out.push_str(&format!("WantedBy={target}.target\n")),
        None => out.push_str("WantedBy=default.target\n"),
    }

    out
}

/// Build the systemd `.target` unit body for a [`TargetSpec`].
///
/// The target `Wants=` all its members, so `systemctl --user start
/// <name>.target` pulls them in; per-member `After=` lines (emitted by
/// [`build_unit_file`] from each member's `ServiceSpec.after`) order the
/// start. We use `Wants=` (soft) rather than `Requires=` so a single
/// member failing does not tear the whole target down — the agent is
/// still useful if, say, an optional future member is absent.
///
/// Pure: no I/O. Same `TargetSpec` → same body.
pub fn build_target_unit(target: &crate::TargetSpec) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=hhagent service bundle: {}\n", target.name));
    if !target.members.is_empty() {
        let wants: Vec<String> = target
            .members
            .iter()
            .map(|m| format!("{m}.service"))
            .collect();
        out.push_str(&format!("Wants={}\n", wants.join(" ")));
    }
    out.push('\n');
    out.push_str("[Install]\n");
    out.push_str("WantedBy=default.target\n");
    out
}

/// Quote a token for systemd unit-file syntax when it contains
/// whitespace, quotes, backslashes, or is empty.
///
/// Returns the original string when no quoting is needed (so the
/// emitted unit file stays human-readable in the common case).
fn quote_if_needed(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars()
            .any(|c| matches!(c, ' ' | '\t' | '"' | '\\' | '\n' | '\r'));
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Validate a service name against `[A-Za-z0-9._-]{1,200}` minus `.`,
/// `..`, and any name starting with `.` (hidden files) or `-` (would
/// be parsed as a flag by some tools).
///
/// Rejects path-traversal characters (`/`, `\0`) and any byte the
/// systemd unit-name grammar would refuse. Returning `Ok` is the
/// gate that lets [`SystemdUser::install`] write a file to disk.
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
        validate_service_name(&spec.name)?;
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
        write_atomic(&path, body.as_bytes())?;

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
}

impl SystemdUser {
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
            after: vec![],
            part_of: None,
        }
    }

    // ---------- pure-builder tests (no I/O, no systemctl) ----------

    #[test]
    fn build_unit_file_emits_three_sections_in_order() {
        let s = build_unit_file(&minimal_spec("hhagent"));
        let unit = s.find("[Unit]").expect("[Unit]");
        let svc = s.find("[Service]").expect("[Service]");
        let install = s.find("[Install]").expect("[Install]");
        assert!(unit < svc, "[Unit] must precede [Service]");
        assert!(svc < install, "[Service] must precede [Install]");
    }

    #[test]
    fn build_unit_file_description_includes_name() {
        let s = build_unit_file(&minimal_spec("hhagent-core"));
        assert!(
            s.contains("Description=hhagent service: hhagent-core"),
            "missing Description, got:\n{s}"
        );
    }

    #[test]
    fn build_unit_file_exec_start_uses_program_path() {
        let mut spec = minimal_spec("svc");
        spec.program = PathBuf::from("/usr/local/bin/foo");
        spec.args = vec!["--flag".into(), "value".into()];
        let s = build_unit_file(&spec);
        assert!(s.contains("ExecStart=/usr/local/bin/foo --flag value"), "{s}");
    }

    #[test]
    fn build_unit_file_quotes_args_with_spaces() {
        let mut spec = minimal_spec("svc");
        spec.args = vec!["arg with space".into(), "plain".into()];
        let s = build_unit_file(&spec);
        assert!(s.contains("ExecStart=/usr/bin/true \"arg with space\" plain"), "{s}");
    }

    #[test]
    fn build_unit_file_escapes_quotes_and_backslashes_in_args() {
        let mut spec = minimal_spec("svc");
        // Argument containing both " and \ — both must be backslash-escaped
        // and the whole token wrapped in quotes.
        spec.args = vec!["a\"b\\c".into()];
        let s = build_unit_file(&spec);
        assert!(s.contains("ExecStart=/usr/bin/true \"a\\\"b\\\\c\""), "{s}");
    }

    #[test]
    fn build_unit_file_emits_one_environment_line_per_var_in_order() {
        let mut spec = minimal_spec("svc");
        spec.env = vec![
            ("FIRST".into(), "1".into()),
            ("SECOND".into(), "two".into()),
        ];
        let s = build_unit_file(&spec);
        let first = s.find("Environment=FIRST=1").expect("FIRST not found");
        let second = s.find("Environment=SECOND=two").expect("SECOND not found");
        assert!(first < second, "env order must be preserved");
    }

    #[test]
    fn build_unit_file_quotes_environment_values_with_spaces() {
        let mut spec = minimal_spec("svc");
        spec.env = vec![("MSG".into(), "hello world".into())];
        let s = build_unit_file(&spec);
        assert!(
            s.contains("Environment=\"MSG=hello world\""),
            "env value with space must be quoted, got:\n{s}"
        );
    }

    #[test]
    fn build_unit_file_emits_working_directory_when_set() {
        let mut spec = minimal_spec("svc");
        spec.working_dir = Some(PathBuf::from("/var/lib/hhagent"));
        let s = build_unit_file(&spec);
        assert!(s.contains("WorkingDirectory=/var/lib/hhagent"), "{s}");
    }

    #[test]
    fn build_unit_file_omits_working_directory_when_none() {
        let s = build_unit_file(&minimal_spec("svc"));
        assert!(!s.contains("WorkingDirectory="), "{s}");
    }

    #[test]
    fn build_unit_file_emits_log_redirects_when_set() {
        let mut spec = minimal_spec("svc");
        spec.stdout_log = Some(PathBuf::from("/var/log/svc.out"));
        spec.stderr_log = Some(PathBuf::from("/var/log/svc.err"));
        let s = build_unit_file(&spec);
        assert!(s.contains("StandardOutput=append:/var/log/svc.out"), "{s}");
        assert!(s.contains("StandardError=append:/var/log/svc.err"), "{s}");
    }

    #[test]
    fn build_unit_file_keep_alive_emits_restart_directives() {
        let mut spec = minimal_spec("svc");
        spec.keep_alive = true;
        let s = build_unit_file(&spec);
        assert!(s.contains("Restart=on-failure"), "{s}");
        assert!(s.contains(&format!("RestartSec={}", DEFAULT_RESTART_SEC)), "{s}");
    }

    #[test]
    fn build_unit_file_no_keep_alive_omits_restart() {
        let s = build_unit_file(&minimal_spec("svc"));
        assert!(!s.contains("Restart="), "no Restart when keep_alive=false, got:\n{s}");
    }

    #[test]
    fn build_unit_file_always_emits_timeout_stop_sec() {
        let s = build_unit_file(&minimal_spec("svc"));
        assert!(
            s.contains(&format!("TimeoutStopSec={}", DEFAULT_TIMEOUT_STOP_SEC)),
            "{s}"
        );
    }

    #[test]
    fn build_unit_file_install_section_wants_default_target() {
        let s = build_unit_file(&minimal_spec("svc"));
        assert!(s.contains("[Install]\nWantedBy=default.target"), "{s}");
    }

    // ---------- name validator tests ----------

    #[test]
    fn validate_service_name_accepts_typical_names() {
        for n in &["hhagent", "hhagent-core", "hhagent.core", "a_b", "abc123"] {
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

    // ---------- driver tests using a custom units dir ----------
    //
    // These exercise the file-writing half of `install`/`uninstall`
    // without touching the live `systemctl --user` manager. They run
    // on any host with a writable /tmp.

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Tempdir helper mirroring `core::workspace::tests::TestRoot`:
    /// unique per process+test+call, removed on drop.
    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hhagent-supervisor-test-{}-{}-{}",
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
    fn install_writes_unit_file_with_expected_content() {
        let dir = TestRoot::new("install-content");
        let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
        let spec = minimal_spec("hhagent-test");
        sup.install(&spec).expect("install");

        let path = sup.unit_path("hhagent-test");
        assert!(path.exists(), "unit file not written: {}", path.display());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("[Unit]"), "{body}");
        assert!(body.contains("ExecStart=/usr/bin/true"), "{body}");
    }

    #[test]
    fn install_rejects_relative_program_path() {
        let dir = TestRoot::new("rel-program");
        let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
        let mut spec = minimal_spec("svc");
        spec.program = PathBuf::from("relative/foo");
        let err = sup.install(&spec).expect_err("relative program");
        assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    }

    #[test]
    fn install_rejects_invalid_name() {
        let dir = TestRoot::new("bad-name");
        let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
        let mut spec = minimal_spec("svc");
        spec.name = "../traversal".into();
        let err = sup.install(&spec).expect_err("traversal name");
        assert!(matches!(err, SupervisorError::InvalidName(_)), "{err}");
    }

    #[test]
    fn install_creates_units_dir_if_missing() {
        let dir = TestRoot::new("nested-dir");
        let nested = dir.path().join("a").join("b").join("c");
        let sup = SystemdUser::with_units_dir(nested.clone());
        sup.install(&minimal_spec("svc")).expect("install");
        assert!(nested.is_dir(), "nested units dir should be created");
        assert!(nested.join("svc.service").is_file());
    }

    #[test]
    fn uninstall_removes_unit_file() {
        let dir = TestRoot::new("uninstall");
        let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
        sup.install(&minimal_spec("svc")).expect("install");
        let path = sup.unit_path("svc");
        assert!(path.exists());
        sup.uninstall("svc").expect("uninstall");
        assert!(!path.exists(), "unit file still present after uninstall");
    }

    #[test]
    fn uninstall_is_idempotent_when_nothing_installed() {
        let dir = TestRoot::new("idempotent");
        let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
        sup.uninstall("nonexistent")
            .expect("uninstall must be idempotent");
    }

    #[test]
    fn status_returns_not_installed_when_unit_absent() {
        let dir = TestRoot::new("status-absent");
        let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
        let s = sup.status("never-installed").expect("status");
        assert_eq!(s, ServiceStatus::NotInstalled);
    }

    #[test]
    fn unit_file_emits_after_and_part_of_when_set() {
        let mut spec = minimal_spec("hhagent-core");
        spec.after = vec!["hhagent-postgres".into()];
        spec.part_of = Some("hhagent".into());
        let body = build_unit_file(&spec);
        assert!(body.contains("After=hhagent-postgres.service\n"), "{body}");
        assert!(body.contains("PartOf=hhagent.target\n"), "{body}");
        assert!(body.contains("WantedBy=hhagent.target\n"), "{body}");
        assert!(!body.contains("WantedBy=default.target\n"), "target member must not target default.target: {body}");
    }

    #[test]
    fn unit_file_unchanged_when_ordering_unset() {
        // The behaviour-preserving pin: a spec with no ordering emits
        // neither After= nor PartOf=, and keeps WantedBy=default.target.
        let body = build_unit_file(&minimal_spec("svc"));
        assert!(!body.contains("After="), "{body}");
        assert!(!body.contains("PartOf="), "{body}");
        assert!(body.contains("WantedBy=default.target\n"), "{body}");
    }

    #[test]
    fn target_unit_wants_all_members() {
        let t = crate::TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        let body = build_target_unit(&t);
        assert!(body.starts_with("[Unit]\n"), "{body}");
        assert!(
            body.contains("Wants=hhagent-postgres.service hhagent-core.service\n"),
            "{body}"
        );
        assert!(body.contains("[Install]\nWantedBy=default.target\n"), "{body}");
    }
}
