//! Pure unit-file builders + name validator for the `systemd --user` backend.
//!
//! No I/O, no environment access, no `systemctl` invocation — every
//! function here is a deterministic `&input → String`/`Result`, so it is
//! unit-testable in isolation from the live user manager. Lifted out of
//! `systemd_user.rs` when that file outgrew the 500-LOC cap; the driver
//! ([`super::SystemdUser`], `probe`, the `systemctl` helpers) stays in the
//! parent and re-exports [`build_unit_file`], [`build_target_unit`], and
//! [`validate_service_name`] via `pub use builder::{…}` so their public
//! paths (`systemd_user::build_unit_file`, …) are unchanged.
//!
//! ### Unit-file shape
//!
//! ```ini
//! [Unit]
//! Description=kastellan service: <name>
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
//! RestartSteps=8                        # only when restart_backoff is set
//! RestartMaxDelaySec=300                # only when restart_backoff is set
//! TimeoutStopSec=10
//!
//! [Install]
//! WantedBy=<target>.target              # spec.part_of.target when set, else default.target
//! ```
//!
//! Each section's directives are emitted in a deterministic order so the
//! generated file is diffable and unit-testable.

use crate::{ServiceSpec, SupervisorError, TargetSpec};

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
    out.push_str(&format!("Description=kastellan service: {}\n", spec.name));
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
        // Optional exponential ramp. RestartSteps/RestartMaxDelaySec need
        // systemd 252+; older systemd logs an "unknown directive" warning at
        // load but still starts the unit, so emitting them is a safe degrade.
        if let Some(b) = &spec.restart_backoff {
            out.push_str(&format!("RestartSteps={}\n", b.steps));
            out.push_str(&format!("RestartMaxDelaySec={}\n", b.max_delay_sec));
        }
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
pub fn build_target_unit(target: &TargetSpec) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=kastellan service bundle: {}\n", target.name));
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
/// gate that lets [`super::SystemdUser::install`] write a file to disk.
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

#[cfg(test)]
mod tests;
