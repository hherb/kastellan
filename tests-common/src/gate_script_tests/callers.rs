//! Scripts that hand off to `run-e2e-gate.sh` by profile name (#768).
//!
//! `scripts/mail/live-shape-gate.sh` finds the localmail credentials and then
//! `exec`s the gate with the `mail-live` profile. Renaming that profile in the
//! gate's table used to break the wrapper only at run time — on the one host
//! with a localmail, the next time someone remembered to run it. Every caller
//! found here must name a profile the table defines.

use std::path::{Path, PathBuf};

use super::profiles;

/// The profile a line hands to the gate script, if it invokes it: the first
/// word after `run-e2e-gate.sh` on a line that is not a comment. `None` for
/// a line that does not run it (or runs it with a flag, such as `--list`).
///
/// Pure, so the parser has its own test below: a scan whose parser matched
/// nothing would report every caller as fine.
fn profile_named_on(line: &str) -> Option<&str> {
    let code = line.trim_start();
    if code.starts_with('#') {
        return None;
    }
    let (_, after) = code.split_once("run-e2e-gate.sh")?;
    // The path may be quoted: `"$repo_root/scripts/run-e2e-gate.sh" mail-live`.
    // Whitespace must follow it: the gate's own messages read
    // `run-e2e-gate.sh: profile '…'`, which is not an invocation.
    let after = after.strip_prefix('"').unwrap_or(after);
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    let word = after.split_whitespace().next()?;
    // A profile name is `[A-Za-z0-9_-]`, never a flag: this refuses `--list`,
    // a `"$variable"` and the usage text's `<profile>`.
    let is_name = !word.starts_with('-')
        && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    is_name.then_some(word)
}

/// Every `*.sh` under `dir`, recursively, sorted so a failure names the same
/// file on every run.
fn shell_scripts(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            found.extend(shell_scripts(&path));
        } else if path.extension().is_some_and(|x| x == "sh") {
            found.push(path);
        }
    }
    found.sort();
    found
}

#[test]
fn the_profile_parser_reads_the_live_wrapper_line_and_nothing_else() {
    assert_eq!(profile_named_on(r#"exec bash "$repo_root/scripts/run-e2e-gate.sh" mail-live "$@""#), Some("mail-live"));
    assert_eq!(profile_named_on("bash scripts/run-e2e-gate.sh guard-tier"), Some("guard-tier"));
    assert_eq!(profile_named_on("# then runs the `mail-live` profile of `scripts/run-e2e-gate.sh`, which"), None);
    assert_eq!(profile_named_on("bash scripts/run-e2e-gate.sh --list"), None);
    assert_eq!(profile_named_on(r#"bash scripts/run-e2e-gate.sh "$profile""#), None);
    assert_eq!(profile_named_on("echo hello"), None);
    // The gate script's own lines, which this scan also reads.
    assert_eq!(profile_named_on(r#"  echo "  Run: bash scripts/run-e2e-gate.sh <profile>" >&2"#), None);
    assert_eq!(profile_named_on(r#"echo "run-e2e-gate.sh: profile '$name' names no E2E floor" >&2"#), None);
}

/// Every script that runs the gate by a literal profile name names one the
/// table defines.
#[test]
fn every_script_that_runs_the_gate_names_a_real_profile() {
    let scripts_dir = super::script_path().parent().expect("scripts/").to_path_buf();
    let names: Vec<String> = profiles().into_iter().map(|p| p.name).collect();
    let mut callers = Vec::new();
    for script in shell_scripts(&scripts_dir) {
        let src = std::fs::read_to_string(&script)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", script.display()));
        for (n, line) in src.lines().enumerate() {
            if let Some(profile) = profile_named_on(line) {
                assert!(
                    names.iter().any(|p| p == profile),
                    "{}:{} runs the gate with profile `{profile}`, which scripts/run-e2e-gate.sh \
                     does not define (profiles: {names:?})",
                    script.display(),
                    n + 1
                );
                callers.push(script.clone());
            }
        }
    }
    // Positive control: the wrapper that motivated this test must be seen, or
    // the scan found nothing and would pass whatever the wrapper said.
    assert!(
        callers.iter().any(|p| p.ends_with("mail/live-shape-gate.sh")),
        "the scan did not find scripts/mail/live-shape-gate.sh running the gate — the \
         wrapper moved or changed shape, and this check is now checking nothing: {callers:?}"
    );
}
