//! Pure, I/O-free helpers for the macOS `launchd` backend.
//!
//! This sibling module holds the two deterministic, fully unit-testable
//! pieces of [`crate::launchd_agents`] — sections 1 and 2 of that
//! module's structure:
//!
//!   1. [`build_plist`] — turns a [`ServiceSpec`] into the XML body of a
//!      LaunchAgent plist (no I/O, no environment access).
//!   2. [`validate_service_name`] — guards a user-supplied service name
//!      against path-traversal and characters launchd's grammar refuses.
//!
//! Both are re-exported from the parent so the public paths
//! `launchd_agents::build_plist` and `launchd_agents::validate_service_name`
//! are unchanged. `xml_escape` stays private to this module (it is an
//! implementation detail of `build_plist`).
//!
//! Lifted verbatim out of `launchd_agents.rs` when that file outgrew the
//! 500-LOC cap; the behaviour is identical and the parent's driver still
//! calls these via the re-export.

use crate::{ServiceSpec, SupervisorError};

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
/// `Ok` is the gate that lets [`crate::launchd_agents::LaunchAgents`]
/// write a file to disk in its `install` step.
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
}
