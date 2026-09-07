//! The source-level guard behind #679: a pure scanner that reads a micro-VM
//! suite's own text and reports every precondition that bypasses
//! [`REQUIRE_ENV`](super::REQUIRE_ENV).
//!
//! Separate from [`super::require`] — the vocabulary — and `#[cfg(test)]`,
//! because it is machinery for this crate's tests rather than something a
//! suite ever calls. `call_site_tests` runs it over the real `core/tests`
//! sources; `guard_tests` pins its rules.
//!
//! # Why a textual rule at all
//!
//! The property is invisible to the type system and to every run. A call site
//! that asks a non-REQUIRE-aware precondition beside `skip_if_no_microvm`
//! reports a false green only on a host where the micro-VM preconditions are
//! MET and the neighbouring one is not — which is, by definition, not the host
//! anybody gates on. Reading the text is the only instrument that sees it.
//!
//! # Reach
//!
//! Two textual rules ([`bypassed_gates`]) covering the two shapes a bypass has
//! taken in this tree: a call to a skip-rendering helper, and a `[SKIP]`
//! written by hand into a print macro. Rule 1 is fail-closed on a *shape*, so
//! it covers helpers nobody has written yet; rule 2 is not — it reads a literal
//! `[SKIP]`, so a message assembled elsewhere and interpolated
//! (`let m = "[SKIP] …"; eprintln!("{m}")`) is out of reach. That form has
//! never appeared here, and closing it would cost the ability to write an
//! assertion *about* `[SKIP]` output, which this crate's own tests need.
//!
//! Discovery keys on `skip_if_no_microvm`, so the scope is the Firecracker
//! tier; the macOS Apple-`container` suites are outside it and have no knob at
//! all (#684).

/// Helpers that **are** REQUIRE-aware, and so may be called from a micro-VM
/// suite.
///
/// ⚠️ **An allowlist, not a denylist, is the rule.** The first version of this
/// guard asked "is this one of five known-bad names?", which is a check that
/// stops working the moment a sixth is written — the "guards less than it
/// claims" shape #667 itself failed at. It now asks the complementary
/// question: *does this identifier look like a skip-rendering helper, and is it
/// not one of the ones that consult the knob?* A helper added tomorrow is
/// caught with no edit here.
///
/// [`BANNED_HELPERS`] survives as the concrete roster of shaped helpers that
/// exist today, cross-checked against the real `tests-common` sources so the
/// two lists cannot silently fall out of step.
pub(crate) const REQUIRE_AWARE: &[&str] = &[
    "skip_if_no_microvm",
    "skip_if_image_stale",
    "skip_if_image_stale_to",
    "dep_or_skip",
    "dep_or_skip_to",
];

/// Skip helpers that are **not** REQUIRE-aware, and so must not be called
/// from a micro-VM suite.
///
/// None of them can be made require-aware in place: the knob is a micro-VM
/// concept and these helpers have no business knowing about it. The resolution
/// is #653's — the helper keeps its `*_or_reason` sibling, and the *call site*
/// decides the verdict.
///
/// They are shared with the rest of the tree to very different degrees —
/// `pg_bin_dir_or_skip` and `skip_if_no_supervisor` with ~60 test files each,
/// `skip_if_sandbox_unavailable` with 28, `egress_proxy_bin_or_skip` with 2 —
/// so breadth of use is *not* why none of them grew a knob. The reason is the
/// layering above.
///
/// This roster is not the detection rule (see [`REQUIRE_AWARE`]); it is
/// asserted *complete* against the sources by
/// `the_banned_roster_matches_the_helpers_that_actually_exist`, so a new shaped
/// helper cannot appear without a decision being recorded in one list or the
/// other.
pub(crate) const BANNED_HELPERS: &[&str] = &[
    "skip_if_no_supervisor",
    "skip_if_sandbox_unavailable",
    "pg_bin_dir_or_skip",
    "egress_proxy_bin_or_skip",
    "resolve_weights_dir_or_skip",
    "skip_line",
];

/// The marker that exempts a hand-written `[SKIP]` from [`bypassed_gates`].
///
/// For an **opt-in enablement flag** — a test whose own env gate is unset was
/// never asked for, so demanding a micro-VM run must not turn it into a
/// failure. That is categorically different from an unmet *host* precondition,
/// which is what the knob is about. The exemption is inline and carries its
/// reason so the distinction is made where it applies, not in a list some
/// other file owns.
pub(crate) const EXEMPT_MARKER: &str = "REQUIRE-EXEMPT";

/// How many lines above a hand-written `[SKIP]` an [`EXEMPT_MARKER`] may sit.
///
/// **Two**, and the number is measured rather than chosen: the idiomatic
/// placement is above the `if` that guards the print, not inside the block —
///
/// ```ignore
/// // REQUIRE-EXEMPT: opt-in enablement flag, not a host precondition.
/// if std::env::var(GATE).is_err() {
///     eprintln!("\n[SKIP] {GATE} unset …");
/// ```
///
/// — which puts exactly one line (the `if`) between the marker and what it
/// excuses. A window of 1 rejected the only real exemption in the tree.
///
/// The window is measured from the line the print macro **opens** on, not from
/// the line carrying the literal, so wrapping a long message across lines
/// cannot push a marker out of range.
///
/// It stays deliberately small. A marker that has drifted away from its
/// literal is a marker nobody re-reads, and this one is the single escape
/// hatch in the guard: it must stay visibly attached to what it excuses.
const EXEMPT_WINDOW: usize = 2;

/// What kind of bypass [`bypassed_gates`] found.
///
/// An enum rather than the free-text string this started as: the two cases
/// have different remedies (substitute the helper vs. route the reason through
/// [`super::require::dep_or_skip`]), and a test comparing against a hand-spelled
/// `"hand-written [SKIP]"` is a test that passes when the guard reports the
/// wrong kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateKind {
    /// A named helper that renders a `[SKIP]` without consulting the knob.
    Helper(String),
    /// A `[SKIP]` written by hand into a print macro.
    HandWrittenSkip,
}

impl std::fmt::Display for GateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateKind::Helper(name) => write!(f, "{name}"),
            GateKind::HandWrittenSkip => write!(f, "hand-written [SKIP]"),
        }
    }
}

/// A precondition that reports a green run when [`super::REQUIRE_ENV`]
/// demanded a real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BypassedGate {
    /// 1-based line within the scanned source. For a print macro split across
    /// lines this is the line the macro **opens** on, which is the line to
    /// edit.
    pub line: usize,
    /// What was found.
    pub kind: GateKind,
}

/// Pure: every precondition in `src` that bypasses [`super::REQUIRE_ENV`].
///
/// Two rules, both textual, because the property is not visible to the type
/// system and not visible to any run — the false green only appears on a host
/// where the micro-VM preconditions are met and a neighbouring one is not,
/// which is by definition not the host anybody gates on.
///
/// 1. **No call to a skip-rendering helper outside [`REQUIRE_AWARE`].**
///    "Skip-rendering" is a *shape* ([`is_skip_shaped`]), so a helper nobody
///    has written yet is covered. Comments and string literals are exempt: a
///    suite's docs may well name a helper while explaining why it does not use
///    it, and flagging that would make this guard's own remedy
///    undocumentable.
/// 2. **No hand-written `[SKIP]` inside a print macro**, unless an
///    [`EXEMPT_MARKER`] sits within [`EXEMPT_WINDOW`] lines above the line the
///    macro opens on (or on that line). The invocation is tracked **across**
///    lines, because rustfmt splits exactly the long, informative messages —
///    that blind spot is how `net_demo_firecracker_egress_e2e.rs` kept a live
///    bypass right through the first version of this guard.
pub(crate) fn bypassed_gates(src: &str) -> Vec<BypassedGate> {
    let lines: Vec<&str> = src.lines().collect();
    let (code_lines, _unterminated) = strip_strings_and_comments(&lines);
    let mut found = Vec::new();

    // Where the currently-open print macro started, and how deep inside its
    // parentheses we are.
    let mut open_print: Option<usize> = None;
    let mut depth: i32 = 0;

    for (idx, raw) in lines.iter().enumerate() {
        let code: &str = &code_lines[idx];

        // ── rule 1: shaped helper calls, over code only ──
        for name in identifiers(code) {
            if is_skip_shaped(&name) && !REQUIRE_AWARE.contains(&name.as_str()) {
                found.push(BypassedGate { line: idx + 1, kind: GateKind::Helper(name) });
            }
        }

        // ── rule 2: a [SKIP] literal rendered by a print macro ──
        let mut from = 0usize;
        if open_print.is_none() {
            if let Some(at) = print_macro_open(code) {
                open_print = Some(idx);
                depth = 0;
                from = at;
            }
        }
        if open_print.is_some() {
            depth += paren_delta(&code[from..]);

            if raw.contains("[SKIP]") {
                let start = open_print.expect("just checked");
                if !exempt_near(&lines, start) {
                    found.push(BypassedGate { line: start + 1, kind: GateKind::HandWrittenSkip });
                }
                // One finding per invocation: the fix is one substitution.
                open_print = None;
                depth = 0;
                continue;
            }
            if depth <= 0 {
                open_print = None;
                depth = 0;
            }
        }
    }
    found
}

/// Pure: does `name` look like a helper that renders a `[SKIP]` verdict?
///
/// The shape, not a roster — that is the whole point of the inversion. See
/// [`REQUIRE_AWARE`].
///
/// A `_reason` suffix is the tree's mark for the half that renders *no*
/// verdict (#653), and it wins over the prefix: `skip_if_no_x_reason` is the
/// remedy for `skip_if_no_x`, so flagging it would make the guard's own fix
/// unappliable. Not hypothetical in shape — `pg_bin_dir_or_reason` and
/// `egress_proxy_bin_or_reason` are exactly this pair, and only their `_or_`
/// spelling keeps them clear of the suffix rule by luck.
pub(crate) fn is_skip_shaped(name: &str) -> bool {
    if name.ends_with("_reason") {
        return false;
    }
    name.starts_with("skip_if_") || name.ends_with("_or_skip") || name == "skip_line"
}

/// Pure: blank out string-literal contents and comment tails, line by line,
/// so the two rules read code rather than prose.
///
/// Rule 1 must not match a helper named inside an assertion message; rule 2
/// must not count a parenthesis inside the very message it is reading. String
/// state carries **across** lines, because a Rust literal continued with a
/// trailing `\` spans them — which is exactly the shape rustfmt produces for a
/// long `[SKIP]` message.
///
/// Block comments (`/* … */`) and `"`-bearing char literals are not handled.
/// Neither occurs in this tree's test sources; the second would be a *silent*
/// hole rather than a loud one, which is why
/// [`ends_inside_string_literal`] exists and `call_site_tests` asserts it.
fn strip_strings_and_comments(lines: &[&str]) -> (Vec<String>, bool) {
    let mut out = Vec::with_capacity(lines.len());
    let mut in_string = false;

    for line in lines {
        let mut kept = String::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if in_string {
                // An escape consumes the next char — including the `"` that
                // would otherwise look like the end of the literal, and the
                // line-continuation at end of line.
                if ch == '\\' {
                    chars.next();
                    continue;
                }
                if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '/' if chars.peek() == Some(&'/') => break,
                _ => kept.push(ch),
            }
        }
        out.push(kept);
    }
    (out, in_string)
}

/// Pure: does `src` leave [`strip_strings_and_comments`] mid-literal?
///
/// String state carries across lines, so a construct this scanner does not
/// model — a `'"'` char literal, a `r#"…"#` raw string — blanks everything
/// after it, and rule 1 then silently sees an empty file. That is the exact
/// failure this guard exists to refuse, so `call_site_tests` asserts it for
/// every real source it scans and names the file that broke.
///
/// Neither construct occurs in the tree's test sources today; this is here so
/// that if one arrives, it arrives loudly.
pub(crate) fn ends_inside_string_literal(src: &str) -> bool {
    let lines: Vec<&str> = src.lines().collect();
    strip_strings_and_comments(&lines).1
}

/// Pure: the identifiers on a stripped code line.
///
/// Exact tokens, so no substring-boundary logic is needed at all:
/// `skip_if_no_supervisor` and `skip_if_no_supervisor_reason` are simply
/// different tokens. The first version bounded a substring search on
/// identifier characters by hand; tokenising deletes that code rather than
/// testing it.
fn identifiers(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in code.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Pure: byte offset just past a print macro's name, if this code line opens
/// one.
///
/// The `[SKIP]` rule targets a line that *renders* a skip, not one that
/// mentions the string — an assertion such as
/// `assert!(!rendered.contains("[SKIP]"))` must not be a finding. Overlapping
/// names (`eprintln!` contains `println!`) end at the same offset, so taking
/// the minimum is unambiguous.
fn print_macro_open(code: &str) -> Option<usize> {
    ["eprintln!", "eprint!", "println!", "print!", "writeln!", "write!"]
        .iter()
        .filter_map(|m| code.find(m).map(|at| at + m.len()))
        .min()
}

/// Pure: net change in parenthesis depth across `code`.
fn paren_delta(code: &str) -> i32 {
    code.chars().fold(0, |d, c| match c {
        '(' => d + 1,
        ')' => d - 1,
        _ => d,
    })
}

/// Pure: does an [`EXEMPT_MARKER`] sit on line `idx` or within
/// [`EXEMPT_WINDOW`] lines above it?
fn exempt_near(lines: &[&str], idx: usize) -> bool {
    let first = idx.saturating_sub(EXEMPT_WINDOW);
    lines[first..=idx].iter().any(|l| l.contains(EXEMPT_MARKER))
}
