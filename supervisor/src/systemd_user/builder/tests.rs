//! Unit tests for the systemd `--user` unit-file builders, lifted verbatim
//! from `builder.rs` to keep that file under the 500-LOC cap. `use super::*;`
//! resolves to the parent `builder` module (the builders + its `use crate::…`).

use super::*;
use crate::RestartBackoff;
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
    let s = build_unit_file(&minimal_spec("kastellan"));
    let unit = s.find("[Unit]").expect("[Unit]");
    let svc = s.find("[Service]").expect("[Service]");
    let install = s.find("[Install]").expect("[Install]");
    assert!(unit < svc, "[Unit] must precede [Service]");
    assert!(svc < install, "[Service] must precede [Install]");
}

#[test]
fn build_unit_file_description_includes_name() {
    let s = build_unit_file(&minimal_spec("kastellan-core"));
    assert!(
        s.contains("Description=kastellan service: kastellan-core"),
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
    spec.working_dir = Some(PathBuf::from("/var/lib/kastellan"));
    let s = build_unit_file(&spec);
    assert!(s.contains("WorkingDirectory=/var/lib/kastellan"), "{s}");
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
fn build_unit_file_keep_alive_with_backoff_emits_steps_and_max_delay() {
    let mut spec = minimal_spec("svc");
    spec.keep_alive = true;
    spec.restart_backoff = Some(RestartBackoff { max_delay_sec: 300, steps: 8 });
    let s = build_unit_file(&spec);
    assert!(s.contains("RestartSteps=8"), "{s}");
    assert!(s.contains("RestartMaxDelaySec=300"), "{s}");
    // RestartSec must precede the ramp directives.
    let sec = s.find("RestartSec=").expect("RestartSec present");
    let steps = s.find("RestartSteps=").expect("RestartSteps present");
    let maxd = s.find("RestartMaxDelaySec=").expect("RestartMaxDelaySec present");
    assert!(sec < steps && steps < maxd, "directive order wrong:\n{s}");
}

#[test]
fn build_unit_file_keep_alive_without_backoff_omits_steps_and_max_delay() {
    let mut spec = minimal_spec("svc");
    spec.keep_alive = true;
    spec.restart_backoff = None;
    let s = build_unit_file(&spec);
    assert!(!s.contains("RestartSteps="), "{s}");
    assert!(!s.contains("RestartMaxDelaySec="), "{s}");
}

#[test]
fn build_unit_file_backoff_inert_without_keep_alive() {
    let mut spec = minimal_spec("svc");
    spec.keep_alive = false;
    spec.restart_backoff = Some(RestartBackoff { max_delay_sec: 300, steps: 8 });
    let s = build_unit_file(&spec);
    assert!(!s.contains("Restart="), "no restart directives without keep_alive:\n{s}");
    assert!(!s.contains("RestartSteps="), "{s}");
    assert!(!s.contains("RestartMaxDelaySec="), "{s}");
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
    let mut spec = minimal_spec("kastellan-core");
    spec.after = vec!["kastellan-postgres".into()];
    spec.part_of = Some("kastellan".into());
    let body = build_unit_file(&spec);
    assert!(body.contains("After=kastellan-postgres.service\n"), "{body}");
    assert!(body.contains("PartOf=kastellan.target\n"), "{body}");
    assert!(body.contains("WantedBy=kastellan.target\n"), "{body}");
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
        name: "kastellan".into(),
        members: vec!["kastellan-postgres".into(), "kastellan-core".into()],
    };
    let body = build_target_unit(&t);
    assert!(body.starts_with("[Unit]\n"), "{body}");
    assert!(
        body.contains("Wants=kastellan-postgres.service kastellan-core.service\n"),
        "{body}"
    );
    assert!(body.contains("[Install]\nWantedBy=default.target\n"), "{body}");
}

// ---------- name validator tests ----------

#[test]
fn validate_service_name_accepts_typical_names() {
    for n in &["kastellan", "kastellan-core", "kastellan.core", "a_b", "abc123"] {
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
