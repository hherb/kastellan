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
    let after = after.strip_prefix(['"', '\'']).unwrap_or(after);
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    // The profile may be quoted too — `"mail-live"` is the same argument.
    let word = after.split_whitespace().next()?.trim_matches(['"', '\'']);
    // A profile name is `[A-Za-z0-9_-]`, never a flag: this refuses `--list`,
    // a `$variable` and the usage text's `<profile>`.
    let is_name = !word.is_empty()
        && !word.starts_with('-')
        && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    is_name.then_some(word)
}

/// Every gate invocation in a script, as (1-based line, profile). A line
/// ending in `\` is joined to the next first, so an invocation split across
/// lines is read as the one command the shell runs; it is reported at the
/// line it starts on.
fn invocations(src: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut command = String::new();
    let mut start = 0;
    for (n, line) in src.lines().enumerate() {
        if command.is_empty() {
            start = n + 1;
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                command.push_str(head);
                command.push(' ');
            }
            None => {
                command.push_str(line);
                if let Some(profile) = profile_named_on(&command) {
                    found.push((start, profile.to_string()));
                }
                command.clear();
            }
        }
    }
    found
}

/// The invocations that name a profile not in `defined`.
///
/// Pure, so the refusal has its own test: over the real scripts it only ever
/// sees names that exist, and a check that has never been shown a bad one is
/// not known to fire.
fn undefined<'a>(found: &'a [(usize, String)], defined: &[String]) -> Vec<&'a (usize, String)> {
    found.iter().filter(|(_, profile)| !defined.contains(profile)).collect()
}

/// Every file under `dir` with extension `ext`, recursively, sorted so a
/// failure names the same file on every run.
fn files_with_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            found.extend(files_with_extension(&path, ext));
        } else if path.extension().is_some_and(|x| x == ext) {
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
    assert_eq!(profile_named_on("bash '/x/scripts/run-e2e-gate.sh' guard-tier"), Some("guard-tier"));
    assert_eq!(profile_named_on(r#"bash scripts/run-e2e-gate.sh "guard-tier""#), Some("guard-tier"));
    assert_eq!(profile_named_on("bash scripts/run-e2e-gate.sh 'guard-tier'"), Some("guard-tier"));
    assert_eq!(profile_named_on("# then runs the `mail-live` profile of `scripts/run-e2e-gate.sh`, which"), None);
    assert_eq!(profile_named_on("bash scripts/run-e2e-gate.sh --list"), None);
    assert_eq!(profile_named_on(r#"bash scripts/run-e2e-gate.sh "$profile""#), None);
    assert_eq!(profile_named_on(r#"bash scripts/run-e2e-gate.sh """#), None);
    assert_eq!(profile_named_on("echo hello"), None);
    // The gate script's own lines, which this scan also reads.
    assert_eq!(profile_named_on(r#"  echo "  Run: bash scripts/run-e2e-gate.sh <profile>" >&2"#), None);
    assert_eq!(profile_named_on(r#"echo "run-e2e-gate.sh: profile '$name' names no E2E floor" >&2"#), None);
}

#[test]
fn an_invocation_split_across_lines_is_read_at_its_first_line() {
    let src = "set -eu\nexec bash scripts/run-e2e-gate.sh \\\n  mail-live \"$@\"\necho done\n";
    assert_eq!(invocations(src), [(2, "mail-live".to_string())]);
}

#[test]
fn a_profile_the_table_does_not_define_is_refused_by_name() {
    let defined = ["mail-live".to_string(), "guard-tier".to_string()];
    let found = invocations("bash scripts/run-e2e-gate.sh guard-tier\nbash scripts/run-e2e-gate.sh mail-lve\n");
    assert_eq!(undefined(&found, &defined), [&(2, "mail-lve".to_string())]);
    assert!(undefined(&found[..1], &defined).is_empty(), "a defined profile is not refused");
}

/// Every script or workflow that runs the gate by a literal profile name names
/// one the table defines.
#[test]
fn every_script_that_runs_the_gate_names_a_real_profile() {
    let scripts_dir = super::script_path().parent().expect("scripts/").to_path_buf();
    let workflows_dir = scripts_dir.parent().expect("the repo root").join(".github/workflows");
    let defined: Vec<String> = profiles().into_iter().map(|p| p.name).collect();
    let mut sources = files_with_extension(&scripts_dir, "sh");
    sources.extend(files_with_extension(&workflows_dir, "yml"));
    let mut callers = Vec::new();
    for source in sources {
        let src = std::fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", source.display()));
        let found = invocations(&src);
        if let Some((line, profile)) = undefined(&found, &defined).first() {
            panic!(
                "{}:{line} runs the gate with profile `{profile}`, which scripts/run-e2e-gate.sh \
                 does not define (profiles: {defined:?})",
                source.display()
            );
        }
        if !found.is_empty() {
            callers.push(source);
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
