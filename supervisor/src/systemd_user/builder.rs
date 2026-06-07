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
pub fn build_target_unit(target: &TargetSpec) -> String {
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
mod tests {
    use super::*;
    use std::path::PathBuf;

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
            restart_backoff: None,
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
        let t = TargetSpec {
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
}
