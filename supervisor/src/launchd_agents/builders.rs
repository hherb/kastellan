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

use crate::ServiceSpec;

/// Default seconds to wait for SIGTERM to take effect before launchd
/// escalates to SIGKILL on `bootout`.
///
/// 10 s matches the systemd backend's `TimeoutStopSec` so behaviour is
/// uniform across OSes; long enough for a graceful exit, short enough
/// that test teardown does not hang on a misbehaving inner process.
pub(super) const DEFAULT_EXIT_TIMEOUT_SEC: u32 = 10;

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
/// kastellan doesn't use). For our "install + start" model to actually
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

// The name gate lives at the crate root (#642) — see the note on the
// same `pub use` in the systemd backend, including why the cap is not
// re-exported alongside it. The rule set was always meant to be
// identical to Linux's so that one service name is portable to either
// OS; now it is the same code rather than the same intent.
pub use crate::service_name::validate_service_name;

#[cfg(test)]
mod tests;
