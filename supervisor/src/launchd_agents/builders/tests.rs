//! Unit tests for the launchd LaunchAgents plist builders, lifted verbatim
//! from `builders.rs` to keep that file under the 500-LOC cap. `use super::*;`
//! resolves to the parent `builders` module (the builders + its imports).

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
        environment_file: None,
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
    let s = build_plist(&minimal_spec("kastellan-core"));
    assert!(
        s.contains("<key>Label</key>\n    <string>kastellan-core</string>"),
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
    spec.working_dir = Some(PathBuf::from("/var/lib/kastellan"));
    let s = build_plist(&spec);
    assert!(
        s.contains("<key>WorkingDirectory</key>\n    <string>/var/lib/kastellan</string>"),
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
        after: vec![],
        part_of: None,
        restart_backoff: None,
        environment_file: None,
    };
    let s = build_plist(&spec);
    assert!(s.contains("<string>a&amp;b&lt;c</string>"), "{s}");
}

// ---------- validator tests ----------

#[test]
fn validate_service_name_accepts_typical_names() {
    for n in &[
        "kastellan",
        "kastellan-core",
        "kastellan.core",
        "org.kastellan.core",
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

#[test]
fn build_plist_identical_with_and_without_backoff() {
    let mut spec = minimal_spec("svc");
    spec.keep_alive = true;
    let without = build_plist(&spec);
    spec.restart_backoff = Some(RestartBackoff { max_delay_sec: 300, steps: 8 });
    let with = build_plist(&spec);
    assert_eq!(
        without, with,
        "launchd plist must not change when restart_backoff is set"
    );
}

#[test]
fn build_plist_ignores_after_and_part_of() {
    // launchd has no ordering / target concept: setting these fields
    // must not change the emitted plist. This pins the documented
    // "ignored on launchd" contract.
    let base = minimal_spec("kastellan-core");
    let mut with_ordering = minimal_spec("kastellan-core");
    with_ordering.after = vec!["kastellan-postgres".into()];
    with_ordering.part_of = Some("kastellan".into());
    assert_eq!(build_plist(&base), build_plist(&with_ordering));
}

// ---------- xml_escape tests ----------

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

// ---------- environment_file parsing/merge (launchd's EnvironmentFile= fold) ----------

#[test]
fn parse_env_file_skips_comments_blanks_and_keeps_embedded_equals() {
    let parsed = parse_env_file("# header\n\nFOO=bar\n  BAZ =qux=zap\nnokey\n");
    assert_eq!(
        parsed,
        vec![
            ("FOO".to_string(), "bar".to_string()),
            // key trimmed; value taken verbatim after the first '=' (so an
            // embedded '=', e.g. a URL query, is preserved). Lines without '='
            // ("nokey") and '#' comments are skipped.
            ("BAZ".to_string(), "qux=zap".to_string()),
        ]
    );
}

#[test]
fn merge_env_file_values_override_inline_env_keeping_position() {
    let mut env = vec![("A".into(), "1".into()), ("B".into(), "2".into())];
    merge_env(&mut env, vec![("B".into(), "override".into()), ("C".into(), "3".into())]);
    assert_eq!(
        env,
        vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "override".to_string()), // overridden in place
            ("C".to_string(), "3".to_string()),        // new key appended
        ]
    );
}
