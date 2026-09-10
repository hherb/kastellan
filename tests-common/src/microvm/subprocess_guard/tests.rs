//! Tests for the unbounded-shell-out guard (#690).
//!
//! Two halves, and the second is the one that matters: the pure rule is tested
//! against shapes this directory does not contain, and then the **real tree**
//! is scanned. A rule proven only against its own fixtures is a rule proven
//! against its author's assumptions.

use super::*;

/// The plain violation: a builder chain ending in `.output()`.
#[test]
fn an_unbounded_output_is_a_violation() {
    let src = "fn probe() {\n    Command::new(\"x\")\n        .output()\n        .unwrap();\n}\n";
    let found = unbounded_shell_outs(src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].line, 3);
}

/// `.status()` is the same shape and the same hazard.
#[test]
fn an_unbounded_status_is_a_violation() {
    let src = "let s = Command::new(\"x\").status().unwrap();\n";
    assert_eq!(unbounded_shell_outs(src).len(), 1);
}

/// ⚠️ **`.spawn()` is deliberately not a violation.** A spawned child is one
/// the caller goes on to manage — which is what the bounded runner itself
/// does, and what every worker launch does. Flagging it would make the rule
/// unusable and force blanket exemptions, which is how a guard becomes a
/// formality.
#[test]
fn a_spawn_is_not_a_violation() {
    let src = "let child = Command::new(\"x\").spawn().unwrap();\n";
    assert!(unbounded_shell_outs(src).is_empty());
}

/// Prose is not code. This tree writes a great deal of documentation, and one
/// module doc shows an unbounded call as the thing it is warning about.
#[test]
fn a_documented_example_is_not_a_violation() {
    let src = "//! let listed = Command::new(\"c\").args([\"image\"]).output();\n\
               /// Historically this called `.output()` with no budget.\n\
               // .status()\n";
    assert!(unbounded_shell_outs(src).is_empty());
}

/// An exemption on the same line is honoured.
#[test]
fn a_marker_on_the_line_exempts_it() {
    let src = "let s = cmd.status().unwrap(); // BOUNDED-EXEMPT: spawn path\n";
    assert!(unbounded_shell_outs(src).is_empty());
}

/// An exemption above the chain is honoured — the natural place to write one,
/// since the reason is about the `Command`, not about the `.output()` line.
#[test]
fn a_marker_above_the_chain_exempts_it() {
    let src = "// BOUNDED-EXEMPT: mkfs on a multi-gigabyte image\n\
               Command::new(\"mkfs.ext4\")\n    .args(a)\n    .status()\n";
    assert!(unbounded_shell_outs(src).is_empty());
}

/// ⚠️ **The window has an end**, or a single marker at the top of a file would
/// exempt everything below it and the guard would be decorative.
#[test]
fn a_marker_far_above_does_not_exempt() {
    let mut src = String::from("// BOUNDED-EXEMPT: about something else entirely\n");
    for _ in 0..20 {
        src.push_str("// filler\n");
    }
    src.push_str("cmd.output();\n");
    assert_eq!(unbounded_shell_outs(&src).len(), 1, "a distant marker must not reach");
}

/// A marker *below* the call is not an exemption: it would describe the next
/// call, not this one.
#[test]
fn a_marker_below_does_not_exempt() {
    let src = "cmd.output();\n// BOUNDED-EXEMPT: this is about the NEXT one\n";
    assert_eq!(unbounded_shell_outs(src).len(), 1);
}

/// Line numbers are 1-based and point at the offending line, because the whole
/// value of the report is that an operator can open the file at it.
#[test]
fn the_report_names_the_line_and_carries_its_text() {
    let src = "a\nb\nlet x = cmd.output();\n";
    let found = unbounded_shell_outs(src);
    assert_eq!(found[0].line, 3);
    assert_eq!(found[0].text, "let x = cmd.output();");
}

// ---------------------------------------------------------------------------
// The real tree
// ---------------------------------------------------------------------------

/// ⚠️ **The rule cannot be applied to its own definition, and that exclusion
/// is pinned so a third file cannot join it.**
///
/// This module *names* the shape it forbids — in a const, and in fixtures
/// built out of it — so the scanner reports itself. That is the documented
/// fail-closed direction working as designed, not a hole: the exclusion is by
/// exact file, the count is asserted below, and every other file in the tree
/// stays subject to the rule.
const RULE_OWN_FILES: &[&str] = &["subprocess_guard.rs", "subprocess_guard/tests.rs"];

/// Pure: is this path the rule's own definition or its fixtures?
fn defines_the_rule(path: &str) -> bool {
    RULE_OWN_FILES.iter().any(|own| path.ends_with(own))
}

/// The directories a micro-VM preflight can run code from.
///
/// Discovered recursively rather than hand-listed, so a new file is covered
/// the day it is written — #683's rule, after a hand-listed suite set proved
/// to be missing one.
fn scanned_roots() -> Vec<std::path::PathBuf> {
    let root = crate::microvm::repo_root();
    vec![root.join("sandbox/src"), root.join("tests-common/src/microvm")]
}

/// ⚠️ **Every shell-out in the micro-VM path answers to a budget, or says why
/// not.**
///
/// This is the assertion #690 exists to make permanent. Before it, four probe
/// call sites — `container --version`, `container system status`, `container
/// image inspect` and the `systemd-run` cgroup probe — waited forever, and two
/// of those four talk to a **daemon**, where a wedge is a known class rather
/// than a hypothetical.
#[test]
fn no_preflight_shells_out_without_a_budget() {
    let mut violations: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut excluded = 0usize;
    for dir in scanned_roots() {
        for (path, src) in rust_sources_under(&dir) {
            if defines_the_rule(&path) {
                excluded += 1;
                continue;
            }
            scanned += 1;
            for hit in unbounded_shell_outs(&src) {
                violations.push(format!("{path}:{}: {}", hit.line, hit.text));
            }
        }
    }
    // The exclusion is exactly the rule's own two files. A third would mean
    // somebody widened it, which is how a narrow exception becomes a hole.
    assert_eq!(
        excluded,
        RULE_OWN_FILES.len(),
        "the rule excluded {excluded} files, not {}: the exclusion has drifted",
        RULE_OWN_FILES.len()
    );
    // ⚠️ The positive control. `assert!(violations.is_empty())` over a loop is
    // green whether the loop found nothing or never ran at all, and a guard
    // that silently scanned zero files is the failure this whole module is
    // about. #683's review added exactly this after the same hole.
    assert!(scanned > 20, "the scan visited only {scanned} files — it is not scanning the tree");
    assert!(
        violations.is_empty(),
        "these shell out with no budget, so a wedged helper stalls the sweep with no output \
         at all (#690). Route them through `kastellan_sandbox::bounded_command::probe_output`, \
         or mark the line `// {EXEMPT_MARKER}: <why a budget would be wrong here>`:\n  {}",
        violations.join("\n  ")
    );
}

/// ⚠️ **A live negative control.** The test above is green over the real tree;
/// green over a loop proves nothing unless the loop can go red. This plants
/// the defect in a source the scanner has never seen and watches it be found.
#[test]
fn the_scan_finds_a_planted_violation() {
    let planted = "fn preflight() {\n    Command::new(\"container\")\n        \
                   .args([\"system\", \"status\"])\n        .output()\n}\n";
    let found = unbounded_shell_outs(planted);
    assert_eq!(found.len(), 1, "the rule cannot see the defect it exists for: {found:?}");
    assert!(found[0].text.contains(".output()"));
}

/// Every exemption in the tree must carry a **reason**, not just the marker.
///
/// A bare marker is an exemption nobody can review later, which is how a
/// justified exception becomes an unjustified one without anybody deciding.
#[test]
fn every_exemption_in_the_tree_gives_a_reason() {
    let mut bare: Vec<String> = Vec::new();
    let mut markers = 0usize;
    for dir in scanned_roots() {
        for (path, src) in rust_sources_under(&dir) {
            if defines_the_rule(&path) {
                continue;
            }
            for (idx, line) in src.lines().enumerate() {
                let Some(after) = line.split_once(EXEMPT_MARKER) else { continue };
                markers += 1;
                // A reason is `: something`, and "something" has to be words.
                let reason = after.1.trim_start_matches(':').trim();
                if reason.len() < 10 {
                    bare.push(format!("{path}:{}", idx + 1));
                }
            }
        }
    }
    assert!(markers > 0, "no exemption found at all — this test is asserting over nothing");
    assert!(bare.is_empty(), "these exemptions state no reason: {bare:?}");
}
