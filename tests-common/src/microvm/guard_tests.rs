//! Unit tests for [`super::guard`] — the rules the source scanner applies.
//!
//! The scanner is the load-bearing half of #679. #680's review found the *last*
//! knob's wiring could be replaced with `false` and the suite stayed green, and
//! #683's review found this guard's own first rule could be walked past by a
//! `[SKIP]` message long enough for rustfmt to wrap. Both are the same lesson:
//! a check nothing checks is a check that stops working quietly. Each rule
//! below is pinned from both sides — it fires on the shape it forbids, and it
//! does *not* fire on the remedy for that shape.

use super::guard::{
    bypassed_gates, ends_inside_string_literal, GateKind, BANNED_HELPERS, REQUIRE_AWARE,
};

// ---------------------------------------------------------------------------
// bypassed_gates — the pure source scanner
// ---------------------------------------------------------------------------

/// A bare call to a non-REQUIRE-aware helper is the defect #679 is about.
#[test]
fn bypassed_gates_flags_a_bare_helper_call() {
    let src = "fn t() {\n    if skip_if_no_supervisor() {\n        return;\n    }\n}\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "one violation expected: {found:?}");
    assert_eq!(found[0].line, 2, "must report the line to fix");
    assert_eq!(found[0].kind, GateKind::Helper("skip_if_no_supervisor".to_string()));
}

/// The `||`-chained shape the issue was filed about: three helpers on one
/// line are three findings, because the fix is three substitutions — and the
/// REQUIRE-aware one among them is not a finding.
#[test]
fn bypassed_gates_flags_every_helper_on_one_line() {
    let src = "if skip_if_no_microvm(R) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {\n";
    let found = bypassed_gates(src);
    let names: Vec<String> = found.iter().map(|g| g.kind.to_string()).collect();
    assert_eq!(names, vec!["skip_if_no_supervisor", "skip_if_sandbox_unavailable"]);
    assert!(found.iter().all(|g| g.line == 1), "each names its own line: {found:?}");
}

/// Every banned helper is actually reachable by the scanner. Without this a
/// typo in one entry of [`BANNED_HELPERS`] would silently stop checking that
/// one — a guard that guards less than it claims, which is #667's own shape.
#[test]
fn bypassed_gates_flags_every_banned_helper() {
    for helper in BANNED_HELPERS {
        let src = format!("    let _ = {helper}();\n");
        let found = bypassed_gates(&src);
        assert_eq!(found.len(), 1, "{helper} must be detected: {found:?}");
        assert_eq!(found[0].kind, GateKind::Helper((*helper).to_string()));
    }
}

/// ⚠️ **The rule is the shape, not the roster.** A helper nobody has written
/// yet — the case a denylist cannot cover, and the reason the first version of
/// this guard could be walked past — is flagged on sight.
#[test]
fn bypassed_gates_flags_a_shaped_helper_that_is_on_no_list() {
    for invented in ["skip_if_no_gpu", "weights_dir_or_skip"] {
        let src = format!("    let _ = {invented}();\n");
        let found = bypassed_gates(&src);
        assert_eq!(found.len(), 1, "{invented} is skip-shaped and unlisted: {found:?}");
        assert_eq!(found[0].kind, GateKind::Helper(invented.to_string()));
    }
}

/// ...and every REQUIRE-aware helper must pass, or the fix cannot be applied.
/// `dep_or_skip` is the trap here: it ends `_or_skip`, so the shape rule flags
/// it unless the allowlist is consulted.
#[test]
fn bypassed_gates_accepts_every_require_aware_helper() {
    for allowed in REQUIRE_AWARE {
        let src = format!("    let _ = {allowed}(x);\n");
        assert!(
            bypassed_gates(&src).is_empty(),
            "{allowed} consults the knob and must pass: {:?}",
            bypassed_gates(&src)
        );
    }
}

/// Whole tokens, not substrings, and `_reason` beats the `skip_if_` prefix.
///
/// Every line here is the *remedy* for a flagged one or a near miss of it.
/// The real remedies come first: flagging those would make the guard's own fix
/// unappliable, which is the failure mode a shape rule is most prone to.
#[test]
fn bypassed_gates_matches_whole_identifiers_only() {
    for benign in [
        // the real `*_or_reason` siblings the call sites were converted to
        "    let d = pg_bin_dir_or_reason();\n",
        "    let p = egress_proxy_bin_or_reason();\n",
        "    let o = origin_unreachable_reason(HOST);\n",
        // a `skip_if_*`-prefixed reason sibling: the suffix wins
        "    let r = skip_if_no_supervisor_reason();\n",
        // near misses on both boundaries
        "    let n = my_skip_if_no_supervisor;\n",
        "    let c = pg_bin_dir_or_skipped;\n",
    ] {
        assert!(bypassed_gates(benign).is_empty(), "not a bypass: {benign:?}");
    }
}

/// A banned name inside a *string* is data, not a call. The guard's own
/// failure message names the helpers it forbids, and a suite may quote one.
#[test]
fn bypassed_gates_ignores_a_helper_named_in_a_string_literal() {
    let src = "    panic!(\"replace skip_if_no_supervisor with the reason sibling\");\n";
    assert!(bypassed_gates(src).is_empty(), "a name in a message is not a call");
}

/// Prose is not a call either. The module docs of a suite may well name a
/// helper while explaining why it does not use it; flagging that would make
/// the guard's own remedy impossible to document.
#[test]
fn bypassed_gates_ignores_a_helper_named_in_a_comment() {
    let src = "// skip_if_no_supervisor() is routed through the knob below\n\
               /// See skip_if_sandbox_unavailable for the bare form.\n";
    assert!(bypassed_gates(src).is_empty(), "a mention in prose is not a call");
}

/// A REQUIRE-aware call site must not trip the guard, or the fix cannot be
/// applied. `supervisor_unavailable_reason` is the sibling that IS routed.
#[test]
fn bypassed_gates_accepts_the_reason_siblings() {
    let src = "if skip_unless_ready(&[&supervisor_unavailable_reason, &sandbox_unavailable_reason]) {\n";
    assert!(bypassed_gates(src).is_empty(), "the fixed shape must pass: {:?}", bypassed_gates(src));
}

/// A hand-written `[SKIP]` is the other half of the class — it is how the
/// broker-binary checks were written, and it bypasses the knob just as
/// completely as a helper call does.
#[test]
fn bypassed_gates_flags_a_hand_written_skip_literal() {
    let src = "    eprintln!(\"\\n[SKIP] search-broker binary not built\\n\");\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "one violation expected: {found:?}");
    assert_eq!(found[0].kind, GateKind::HandWrittenSkip);
}

/// ⚠️ **The regression this guard shipped with.** rustfmt splits a print macro
/// whose message is long — i.e. exactly the informative ones — so the macro
/// lands on one line and the `[SKIP]` literal on the next. The first version
/// of this rule required both on the same line and therefore could not see
/// `net_demo_firecracker_egress_e2e.rs`'s live bypass at all, while reporting
/// the file clean.
///
/// The finding is reported at the line the macro **opens** on, because that is
/// the line to edit.
#[test]
fn bypassed_gates_flags_a_skip_literal_wrapped_onto_the_next_line() {
    let src = "            None => {\n\
               \x20               eprintln!(\n\
               \x20                   \"[SKIP] egress-proxy not built; run \\\n\
               \x20                    `cargo build -p kastellan-worker-egress-proxy`\"\n\
               \x20               );\n\
               \x20               return;\n\
               \x20           }\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "a wrapped literal is still a hand-written skip: {found:?}");
    assert_eq!(found[0].kind, GateKind::HandWrittenSkip);
    assert_eq!(found[0].line, 2, "reported where the macro opens — the line to edit");
}

/// Every print macro renders, not just `eprintln!`. Reducing the list to the
/// one the tree happens to use today is a mutation no other test kills.
#[test]
fn bypassed_gates_flags_a_skip_from_any_print_macro() {
    for macro_name in ["eprintln!", "eprint!", "println!", "print!", "writeln!", "write!"] {
        let src = format!("    {macro_name}(\"[SKIP] not built\");\n");
        let found = bypassed_gates(&src);
        assert_eq!(found.len(), 1, "{macro_name} renders a skip: {found:?}");
        assert_eq!(found[0].kind, GateKind::HandWrittenSkip);
    }
}

/// ...but a line that merely *mentions* the string does not render one. An
/// assertion about `[SKIP]` output is how this module's own tests are written,
/// and flagging it would make the guard unable to coexist with them.
#[test]
fn bypassed_gates_ignores_a_skip_string_that_is_not_printed() {
    for benign in [
        "    assert!(!rendered.contains(\"[SKIP]\"));\n",
        "    let expected = \"[SKIP] not built\";\n",
    ] {
        assert!(bypassed_gates(benign).is_empty(), "not a render: {benign:?}");
    }
}

/// ...and neither does a commented-out one. Rule 1 exempts prose; rule 2 must
/// too, or the only way to document the banned shape is to stop documenting it.
#[test]
fn bypassed_gates_ignores_a_commented_out_skip_render() {
    let src = "    // eprintln!(\"[SKIP] the old shape, kept for the record\");\n";
    assert!(bypassed_gates(src).is_empty(), "a commented-out render is prose");
}

/// An *opt-in enablement flag* is legitimately not a host precondition. A test
/// whose own env gate is unset was never asked for, so demanding a micro-VM
/// run must not turn it into a failure. The marker on the very line it excuses
/// is the tightest placement, and it must work: without this the inclusive
/// upper bound of the window (`..=idx`) can be dropped with the suite green.
#[test]
fn bypassed_gates_honours_a_marker_on_the_literals_own_line() {
    let src = "    eprintln!(\"[SKIP] GATE unset\"); // REQUIRE-EXEMPT: opt-in enablement flag\n";
    assert!(bypassed_gates(src).is_empty(), "a same-line marker exempts: {:?}", bypassed_gates(src));
}

/// The same, one line up — the form the fixtures and the tree both use.
#[test]
fn bypassed_gates_honours_an_inline_exemption_marker() {
    let src = "    // REQUIRE-EXEMPT: opt-in enablement flag, not a host precondition\n\
               \x20   eprintln!(\"\\n[SKIP] {GATE} unset\\n\");\n";
    assert!(bypassed_gates(src).is_empty(), "an exempted literal must pass: {:?}", bypassed_gates(src));
}

/// The real placement: the marker sits above the `if` that guards the print,
/// so one line (the `if`) separates it from the literal. A window that
/// rejected this would reject the only exemption in the tree — measured, not
/// assumed: it did, at a window of 1.
#[test]
fn bypassed_gates_honours_a_marker_above_the_guarding_if() {
    let src = "    // REQUIRE-EXEMPT: opt-in enablement flag, not a host precondition.\n\
               \x20   if std::env::var(GATE).is_err() {\n\
               \x20       eprintln!(\"\\n[SKIP] {GATE} unset\\n\");\n";
    assert!(bypassed_gates(src).is_empty(), "the real shape must pass: {:?}", bypassed_gates(src));
}

/// ...but no further. Three lines up is a marker that has drifted away from
/// what it excuses, and this is the guard's only escape hatch — it must stay
/// visibly attached.
#[test]
fn a_marker_three_lines_up_does_not_exempt() {
    let src = "    // REQUIRE-EXEMPT: drifted\n\
               \x20   let a = 1;\n\
               \x20   let b = 2;\n\
               \x20   eprintln!(\"[SKIP] gate unset\");\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "a drifted marker must not exempt: {found:?}");
    assert_eq!(found[0].line, 4);
}

/// The window is measured from where the macro **opens**, not from the
/// literal. Wrapping a long message must not push a legitimate marker out of
/// range — otherwise fixing rule 2's line-locality would break the one
/// exemption the tree has the moment somebody reformats it.
#[test]
fn a_marker_above_a_wrapped_macro_still_exempts() {
    let src = "    // REQUIRE-EXEMPT: opt-in enablement flag, not a host precondition.\n\
               \x20   if std::env::var(GATE).is_err() {\n\
               \x20       eprintln!(\n\
               \x20           \"[SKIP] {GATE} unset — this tier needs a live homeserver\"\n\
               \x20       );\n";
    assert!(
        bypassed_gates(src).is_empty(),
        "the marker covers the macro's opening line: {:?}",
        bypassed_gates(src)
    );
}

/// The marker exempts only what it is next to. A second, unmarked literal
/// further down the same file is still a finding — otherwise one exemption
/// would silently disarm the whole file.
#[test]
fn an_exemption_marker_does_not_cover_a_later_literal() {
    let src = "    // REQUIRE-EXEMPT: opt-in enablement flag\n\
               \x20   eprintln!(\"[SKIP] gate unset\");\n\
               \x20   let x = 1;\n\
               \x20   let y = 2;\n\
               \x20   let z = 3;\n\
               \x20   eprintln!(\"[SKIP] egress-proxy not built\");\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "only the unmarked literal is a finding: {found:?}");
    assert_eq!(found[0].line, 6);
}

/// A parenthesis inside the message must not be counted as the macro's own.
/// Without string-aware paren tracking a `(` in the text leaves the invocation
/// "open" forever, and every later `[SKIP]` in the file is attributed to it.
#[test]
fn bypassed_gates_does_not_let_a_parenthesis_in_the_message_leak() {
    let src = "    eprintln!(\"not a skip (see docs)\");\n\
               \x20   let x = 1;\n\
               \x20   eprintln!(\"[SKIP] gate unset\");\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "only the real skip is a finding: {found:?}");
    assert_eq!(found[0].line, 3, "attributed to its own macro, not the earlier one");
}

/// The scanner knows when it has gone blind.
///
/// String state carries across lines, so an unclosed literal blanks the rest of
/// a file and every rule sees nothing. `call_site_tests` turns that into a
/// named failure per file; this pins the predicate it relies on, in both
/// directions — a predicate stuck at `false` would restore the silent hole.
#[test]
fn an_unterminated_string_literal_is_reported() {
    assert!(!ends_inside_string_literal("let a = \"closed\";\n"), "a closed literal is fine");
    assert!(
        !ends_inside_string_literal("let a = \"one \\\n     two\";\n"),
        "a literal continued onto the next line and then closed is fine"
    );
    assert!(ends_inside_string_literal("let a = \"never closed;\n"), "an open literal is reported");
    assert!(
        ends_inside_string_literal("let a = \"an escaped \\\" does not close it;\n"),
        "an escaped quote does not close the literal"
    );
}
