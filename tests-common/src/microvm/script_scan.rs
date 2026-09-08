//! The one shell-reading rule the build-script scanners share (issue #682).
//!
//! # Why this is a module and not a helper inside one test
//!
//! Six `#[test]`s across two files read the same eight `build-*-rootfs.sh`
//! bodies and assert something about what they *run*. Before this module
//! there were **three** different notions of "ignore the prose" among them —
//! a trailing-comment stripper in `images.rs`, a `!starts_with('#')` line
//! filter in `kernel_pin_tests.rs`, and two scanners that matched raw text
//! and so could be satisfied (or tripped) by a comment. That is one rule in
//! three copies, which is the drift channel this tree keeps getting bitten
//! by: `CLAUDE.md` records it for the bwrap argv pair, [`super::images`]
//! records it for eight unchecked `curl`s (#471), and #682 itself is one
//! copy of a cargo invocation per build script.
//!
//! # Why the detection is factored out of the assertions
//!
//! [`crate::microvm::call_site_tests`] states the house rule: an
//! `assert!(violations.is_empty())` over a loop is green whether the loop
//! found nothing or never ran, so the scan must be a pure function over
//! `(name, source)` that a **positive control** can feed a planted
//! violation. [`cargo_invocations`] and [`invokes`] exist for exactly that;
//! their controls live beside the tests that use them.

use std::path::Path;

/// Where a `#` may open a comment: the start of a *word*.
///
/// bash begins a word at the line start, after blanks, and after the
/// metacharacters that terminate one. `;#`, `&#` and `(#` are therefore real
/// comments even with no space — the shape that let prose satisfy a
/// *presence* check before this rule was widened.
fn opens_word(prev: u8) -> bool {
    prev.is_ascii_whitespace() || matches!(prev, b';' | b'&' | b'|' | b'(')
}

/// The executable part of one shell line: everything before its comment.
///
/// Every scanner over a `build-*-rootfs.sh` goes through this, or a comment
/// that merely *names* a path or a command reads as one being run. That is
/// not hypothetical: the #682 note added to all eight build scripts contains
/// the words `target/release/` and `cargo build -p`, and tripped two
/// scanners within minutes of being written — one by claiming the script
/// interpolates a variable it does not have.
///
/// Quote state is tracked because both error directions are real, and the
/// first version of this rule (whitespace-before-`#` only) got the dangerous
/// one wrong:
///
/// * **under-stripping** — a `#` not recognised as a comment leaves *more*
///   text to scan. A fail-closed guard then complains about prose; noisy,
///   never quiet. `;#` used to land here, and it is a real comment, so a
///   script could satisfy `contains("bash …/build-release.sh")` from a
///   comment alone.
/// * **over-stripping** — a `#` that is *not* a comment (inside `'…'`,
///   `"…"`, or after a backslash) truncates the line and leaves *less*. That
///   is the direction that makes a `!contains(…)` guard go silent, and
///   `echo "step # 2" && cargo build --release -p x` was a live instance of
///   it.
///
/// The residual limit is stated rather than papered over: state resets each
/// line, so a **heredoc body** or a quoted string continued across lines is
/// read as code. That is the under-stripping direction, so it can only make
/// a guard complain — and no build script in the registry has either shape.
pub(super) fn code_of(line: &str) -> &str {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let bytes = line.as_bytes();
    let mut quote = Quote::None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Quote::None => match c {
                // `#` is ASCII, so slicing at `i` is always on a char
                // boundary however many multi-byte chars precede it.
                b'#' if i == 0 || opens_word(bytes[i - 1]) => return &line[..i],
                b'\'' => quote = Quote::Single,
                b'"' => quote = Quote::Double,
                b'\\' => i += 1,
                _ => {}
            },
            // Nothing escapes inside single quotes, not even a backslash.
            Quote::Single => {
                if c == b'\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => match c {
                b'\\' => i += 1,
                b'"' => quote = Quote::None,
                _ => {}
            },
        }
        i += 1;
    }
    line
}

/// Every line of a script with its prose removed, in order.
pub(super) fn code_lines(source: &str) -> Vec<&str> {
    source.lines().map(code_of).collect()
}

/// The whole script with its prose removed, for scanners that match across
/// the body rather than line by line.
pub(super) fn code_body(source: &str) -> String {
    code_lines(source).join("\n")
}

/// One token as the shell would see it, minus the quoting.
///
/// Enough for the argv shapes a build script actually uses; it is not a word
/// splitter. Deliberately keeps `$`-expansions intact so `"$CARGO"` is still
/// recognisable as a cargo reference.
fn bare(token: &str) -> &str {
    token.trim_matches(|c| c == '"' || c == '\'')
}

/// Does this token name the cargo binary?
///
/// Matches the bare name, any path ending in it (`~/.cargo/bin/cargo`), and
/// the `$CARGO` indirection. It deliberately does **not** match
/// `"$HOME/.cargo/env"`, whose basename is `env` — that is a `source` line,
/// not an invocation.
fn is_cargo(token: &str) -> bool {
    let t = bare(token);
    matches!(t, "$CARGO" | "${CARGO}") || t.rsplit('/').next() == Some("cargo")
}

/// Words that may sit in front of the real command without changing it.
const WRAPPERS: &[&str] = &["!", "time", "exec", "env", "nice", "sudo", "then", "do", "else"];

/// Is cargo the COMMAND of any of this line's segments?
///
/// Command position, not "the token appears": `echo "==> cargo build …"` and
/// `command -v cargo` both mention cargo without running it, and
/// `build-release.sh` — which
/// [`super::images::tests::the_canonical_producer_is_the_one_the_deploy_path_runs`]
/// scans — contains both.
///
/// Segments split on the operators that begin a new command, so
/// `foo && cargo build` counts. Leading `VAR=value` assignments and the
/// wrappers above are stepped over, so `env FOO=1 cargo build` counts too.
fn runs_cargo(line: &str) -> bool {
    line.split("&&")
        .flat_map(|s| s.split("||"))
        .flat_map(|s| s.split(';'))
        .flat_map(|s| s.split('|'))
        .any(|segment| {
            let mut words = segment.split_whitespace().skip_while(|w| {
                WRAPPERS.contains(w) || (w.contains('=') && !w.starts_with('-'))
            });
            words.next().is_some_and(is_cargo)
        })
}

/// Every line of `source` that invokes cargo, prose excluded.
///
/// A **pure function over the text** so a planted-violation test can prove
/// the detector detects — see the module docs.
///
/// Tokenised rather than `contains("cargo build")`, because that literal
/// missed every one of `cargo  build` (two spaces), `cargo\tbuild`,
/// `"$CARGO" build` and `cargo rustc`. Any subcommand counts, including none:
/// fail closed, since the point is that a build script must not drive cargo
/// at all, not that it must avoid one spelling.
pub(super) fn cargo_invocations(source: &str) -> Vec<&str> {
    code_lines(source).into_iter().filter(|line| runs_cargo(line)).collect()
}

/// Does `source` run `script` (repo-relative), however the path is spelled?
///
/// Matches the path itself rather than one fixed invocation, so
/// `bash scripts/build-release.sh` and `bash "$REPO_ROOT/scripts/…"` both
/// satisfy it. Pinning the literal would have frozen the cwd-relative form
/// and blocked the very fix `build-browser-driver-rootfs.sh` needs.
pub(super) fn invokes(source: &str, script: &str) -> bool {
    code_lines(source).iter().any(|line| line.contains(script))
}

/// The script plus every in-repo file it `source`s, as `(path, text)`.
///
/// Following the `source` is what makes [`cargo_invocations`] mean *"this
/// build does not run cargo"* rather than *"this file does not spell
/// cargo"*. Without it, moving the invocation into `lib/guest-kernel.sh` —
/// which all eight scripts already source — would evade the guard entirely,
/// reopening #682 one indirection out.
///
/// Only the `$(dirname "${BASH_SOURCE[0]}")/…` form is resolved, because it
/// is the only one the tree uses; an unresolvable `source` **panics** rather
/// than being skipped, so a new spelling forces a deliberate update instead
/// of silently shrinking the scan.
pub(super) fn sources_of(root: &Path, script: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    collect(root, script, &mut out);
    out
}

/// Depth-first, because the chain is real: every build script sources
/// `lib/guest-kernel.sh`, which in turn sources `lib/verify.sh`. Stopping at
/// one level would leave a hiding place two hops from the script.
fn collect(root: &Path, script: &str, out: &mut Vec<(String, String)>) {
    if out.iter().any(|(p, _)| p == script) {
        return; // a shared lib is reached from eight scripts, and could cycle
    }
    let text = std::fs::read_to_string(root.join(script))
        .unwrap_or_else(|e| panic!("read {script}: {e}"));
    let dir = Path::new(script).parent().unwrap_or(Path::new("")).to_path_buf();
    out.push((script.to_string(), text.clone()));

    for line in code_lines(&text) {
        let trimmed = line.trim();
        // The argument is the REST of the line, not the next whitespace
        // token: the tree's only spelling is
        // `source "$(dirname "${BASH_SOURCE[0]}")/lib/…"`, which contains a
        // space inside the command substitution and so tokenises wrong.
        let Some(arg) = trimmed
            .strip_prefix("source ")
            .or_else(|| trimmed.strip_prefix(". "))
        else {
            continue;
        };
        let rel = bare(arg.trim()).replace("$(dirname \"${BASH_SOURCE[0]}\")/", "");
        if rel.contains('$') {
            // An unresolved expansion would silently scan nothing, so a new
            // spelling must force a deliberate update instead.
            panic!("{script} sources a path this scan cannot resolve: {arg}");
        }
        let cleaned = normalise(&dir.join(&rel));
        collect(root, &cleaned.display().to_string(), out);
    }
}

/// Resolve `a/b/../c` textually.
///
/// `Path::canonicalize` would work too, but it returns an absolute path and
/// every message in these tests is repo-relative — an absolute one would put
/// the reviewer's home directory in a CI failure.
fn normalise(path: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comment rule is load-bearing for six scanners, so it is tested
    /// directly rather than only through them — and in **both** error
    /// directions, because the first version's doc claimed one of them was
    /// impossible while a one-line example disproved it.
    #[test]
    fn code_of_strips_prose_and_leaves_shell_syntax_alone() {
        // Code with no comment survives whole.
        assert_eq!(
            code_of("install -D target/release/foo \"$WORK/bin/foo\""),
            "install -D target/release/foo \"$WORK/bin/foo\""
        );
        assert_eq!(code_of("no comment here"), "no comment here");
        assert_eq!(code_of("# target/release/phantom is only mentioned"), "");
        assert_eq!(code_of("mkdir -p \"$WORK/run\"   # slice 4a"), "mkdir -p \"$WORK/run\"   ");

        // A `#` that does not begin a word is shell syntax, not a comment.
        assert_eq!(
            code_of("script_of() { echo \"${1#*:}\"; }"),
            "script_of() { echo \"${1#*:}\"; }"
        );
        assert_eq!(code_of("echo \"a#b\""), "echo \"a#b\"");
    }

    /// OVER-stripping is the direction that makes a `!contains(…)` guard go
    /// quiet, so the quoted cases are pinned by name.
    ///
    /// `echo "step # 2" && cargo build …` is the shape that hid a real
    /// invocation from `no_rootfs_build_script_runs_its_own_cargo_build`
    /// while the rule's own doc said that could not happen.
    #[test]
    fn a_hash_inside_quotes_is_not_a_comment() {
        let line = "echo \"step # 2\" && cargo build --release -p kastellan-microvm-init";
        assert_eq!(code_of(line), line, "a quoted # must not truncate the line");
        assert_eq!(cargo_invocations(line).len(), 1, "and the invocation must still be seen");

        let single = "awk '{ print # }' ; install -D target/release/extra \"$W/extra\"";
        assert_eq!(code_of(single), single, "single quotes hide a # just as well");

        assert_eq!(code_of("echo \\# not a comment"), "echo \\# not a comment");
    }

    /// UNDER-stripping lets prose satisfy a *presence* check, which is how a
    /// fail-closed guard goes green while nothing is enforced.
    #[test]
    fn a_hash_after_a_metacharacter_is_a_comment() {
        assert_eq!(code_of(": ;# bash scripts/build-release.sh"), ": ;");
        assert!(
            !invokes(": ;# bash scripts/build-release.sh", "scripts/build-release.sh"),
            "a commented-out producer must not satisfy the presence check"
        );
        assert_eq!(code_of("(#comment"), "(");
    }

    /// The literal `contains("cargo build")` this replaced missed every one
    /// of these, and each is a plausible reflow rather than an attack.
    #[test]
    fn cargo_invocations_survive_respelling() {
        for line in [
            "cargo build --release -p x",
            "cargo  build --release -p x",
            "cargo\tbuild --release -p x",
            "\"$CARGO\" build --release -p x",
            "$CARGO build --release -p x",
            "cargo rustc --release -p x",
            "~/.cargo/bin/cargo build --release",
            "    cargo build",
        ] {
            assert_eq!(cargo_invocations(line).len(), 1, "must be seen as cargo: {line}");
        }
    }

    /// Naming cargo is not running it, or the guard would flag its own
    /// producer: `scripts/build-release.sh` both `echo`s its invocations and
    /// preflights with `command -v cargo`, and
    /// `the_canonical_producer_is_the_one_the_deploy_path_runs` scans it.
    #[test]
    fn naming_cargo_is_not_running_it() {
        for line in [
            "source \"$HOME/.cargo/env\"",
            ". \"$HOME/.cargo/env\"",
            "# cargo build --release -p kastellan-microvm-init",
            "bash scripts/build-release.sh",
            "echo \"==> cargo build --release --workspace\"",
            "if ! command -v cargo >/dev/null 2>&1; then",
            "    echo \"cargo is not on PATH\" >&2",
        ] {
            assert!(cargo_invocations(line).is_empty(), "not an invocation: {line}");
        }
    }

    /// ...but cargo behind an operator or a wrapper still is one.
    #[test]
    fn cargo_after_an_operator_or_a_wrapper_still_counts() {
        for line in [
            "require_guest_kernel \"$OUT\" && cargo build --release -p x",
            "cd \"$R\"; cargo build --release",
            "env RUSTFLAGS=-g cargo build --release",
            "sudo cargo build --release",
        ] {
            assert_eq!(cargo_invocations(line).len(), 1, "must be seen as cargo: {line}");
        }
    }

    /// `invokes` matches the PATH, not one spelling of the invocation —
    /// otherwise it would forbid the `$REPO_ROOT`-qualified form that
    /// `build-browser-driver-rootfs.sh` needs to stay runnable from any cwd.
    #[test]
    fn invokes_accepts_any_spelling_of_the_same_script() {
        for line in [
            "bash scripts/build-release.sh",
            "bash \"$REPO_ROOT/scripts/build-release.sh\"",
            "    bash \"$REPO_ROOT/scripts/build-release.sh\" || exit 1",
        ] {
            assert!(invokes(line, "scripts/build-release.sh"), "must count: {line}");
        }
        assert!(!invokes("bash scripts/other.sh", "scripts/build-release.sh"));
    }

    /// The `source`-following is what makes the cargo ban mean anything, so
    /// it is pinned against the real tree rather than a fixture.
    #[test]
    fn sources_of_follows_the_shared_guest_kernel_lib() {
        let root = crate::microvm::repo_root();
        let scanned = sources_of(&root, "scripts/workers/microvm/build-rootfs.sh");
        let names: Vec<&str> = scanned.iter().map(|(p, _)| p.as_str()).collect();
        assert!(
            names.contains(&"scripts/workers/microvm/lib/guest-kernel.sh"),
            "the sourced pin must be scanned too, or moving a cargo call into it \
             would evade the guard: {names:?}"
        );

        assert!(
            names.contains(&"scripts/workers/microvm/lib/verify.sh"),
            "the chain is two hops — guest-kernel.sh sources verify.sh — and stopping \
             at one level would leave a hiding place: {names:?}"
        );

        // kv-demo reaches the same lib through `../microvm/`, which only
        // works if the relative path is normalised.
        let across = sources_of(&root, "scripts/workers/kv-demo/build-kv-demo-rootfs.sh");
        let names: Vec<&str> = across.iter().map(|(p, _)| p.as_str()).collect();
        assert!(
            names.contains(&"scripts/workers/microvm/lib/guest-kernel.sh"),
            "a `../` hop must resolve, not read as a missing file: {names:?}"
        );
    }
}
