//! The gate script's suite discovery and target count, run as real bash.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::run::{describe, run_gate_with, GateEdits};
use super::{gate_script, script_path};

/// Run a snippet of bash with the gate script's `count_test_targets` and
/// suite-discovery functions defined, from the repository root.
///
/// Extracted by name rather than by sourcing the script, which would run its
/// preflight and profile validation.
fn run_with_script_functions(snippet: &str) -> String {
    let src = gate_script();
    let mut defs = String::new();
    for name in ["count_test_targets", "firecracker_suites", "container_suites"] {
        let start = src
            .find(&format!("\n{name}() {{\n"))
            .unwrap_or_else(|| panic!("the gate script defines no `{name}()`"));
        let body = &src[start + 1..];
        let end = body.find("\n}\n").expect("function body closes at column 0") + 3;
        defs.push_str(&body[..end]);
    }
    let root = script_path().parent().and_then(|p| p.parent()).map(PathBuf::from).unwrap();
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!("{defs}\n{snippet}"))
        .current_dir(root)
        .output()
        .expect("run bash");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The target count counts TARGETS, not lines.
///
/// It used to be `grep -c -- '--test'`, which counts LINES — and
/// `firecracker_suites` emits every `--test x` on one line. So it read 1 on
/// every host, below the floor of 12, and the `microvm` profile refused every
/// run from #720 until #748 found it. It was never reached on the Mac (the `os`
/// check refuses first), and no test ran it.
#[test]
fn the_target_count_counts_targets_not_lines() {
    assert_eq!(run_with_script_functions("count_test_targets '--test a --test b --test c '"), "3");
    assert_eq!(run_with_script_functions("count_test_targets ''"), "0");
    assert_eq!(run_with_script_functions("count_test_targets '-p kastellan-core'"), "0");
}

/// The REAL discovery, counted by the REAL count, clears the script's own
/// floor on this tree.
///
/// The positive control #720 lacked: run end to end, the pair above would have
/// shown `1 < 12` the day it shipped. Host-agnostic — discovery greps sources,
/// so the Mac, which the profile itself refuses, checks it too.
#[test]
fn the_real_firecracker_discovery_clears_its_own_floor() {
    assert_discovery_clears_its_floor("firecracker_suites", "MIN_FIRECRACKER_SUITES");
}

/// The same positive control for the container tier's discovery (#722).
#[test]
fn the_real_container_discovery_clears_its_own_floor() {
    assert_discovery_clears_its_floor("container_suites", "MIN_CONTAINER_SUITES");
}

/// The two discoveries partition the `core` suites that gate on the shared
/// micro-VM preflight: none is selected by both, so neither profile runs a
/// suite its host compiles to an empty binary, and none is selected by neither.
///
/// "Every such suite" comes from `microvm_suites`, NOT from the script's own
/// grep: that census parses imports (`gates_on_shared_preflight`), so a suite
/// the grep misses — a brace-grouped `use kastellan_tests_common::{microvm::…}`
/// — fails here instead of dropping out of both profiles. The container half
/// was first a grep for `skip_if_no_container`, which also selected
/// `gliner_relex_e2e`'s file-local helper of that name (#722).
#[test]
fn the_firecracker_and_container_discoveries_partition_the_microvm_suites() {
    let names = |snippet: &str| -> BTreeSet<String> {
        run_with_script_functions(snippet)
            .split_whitespace()
            .filter(|t| *t != "--test")
            .map(str::to_string)
            .collect()
    };
    let (fc, ct) = (names("firecracker_suites"), names("container_suites"));
    assert!(!ct.is_empty() && !fc.is_empty(), "a discovery found nothing");
    let both: Vec<_> = fc.intersection(&ct).collect();
    assert!(both.is_empty(), "selected by both discoveries: {both:?}");

    let census = crate::microvm::call_site_tests::microvm_suites();
    let stem = |p: &std::path::Path| p.file_stem().unwrap().to_string_lossy().into_owned();
    let all: BTreeSet<String> = census.iter().map(|(p, _)| stem(p)).collect();
    let selected: BTreeSet<String> = fc.union(&ct).cloned().collect();
    assert_eq!(selected, all, "the discoveries do not cover exactly the micro-VM suites");

    // The cfg half, cross-checked loosely: a macOS-only suite whose cfg is
    // spelled otherwise (`all(target_os = "macos", …)`) must not fall to the
    // Linux profile, where it would compile empty.
    let mac_only: BTreeSet<String> = census
        .iter()
        .filter(|(_, src)| src.contains("target_os = \"macos\"") && !src.contains("target_os = \"linux\""))
        .map(|(p, _)| stem(p))
        .collect();
    assert_eq!(ct, mac_only, "container_suites is not the macOS-only micro-VM suites");
}

/// The script REFUSES a discovery below its floor (#722 review).
///
/// The floor tests above prove the real discovery clears the floor, not that
/// the script acts on a miss: deleting the `if` in `expand_suites`, flipping
/// its `-lt`, or misspelling a token all passed them. Run from a copy whose
/// repo root has no `core/tests`, behind a fake `uname` so either host reaches
/// the check for either profile.
#[test]
fn a_discovery_below_its_floor_is_refused() {
    for (profile, os, refusal) in [
        ("container", "Darwin", "container_suites discovered 0 suites, floor is 3."),
        ("microvm", "Linux", "firecracker_suites discovered 0 suites, floor is 12."),
    ] {
        let uname = format!("echo {os}");
        let edits = GateEdits { tools: &[("uname", &uname)], relocated: true, ..Default::default() };
        let out = run_gate_with(profile, &format!("floor-{profile}"), "", 0, &edits);
        assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
        assert!(String::from_utf8_lossy(&out.stderr).contains(refusal), "{}", describe(&out));
    }
}

fn assert_discovery_clears_its_floor(discover: &str, floor_var: &str) {
    let floor: usize = gate_script()
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{floor_var}=")))
        .unwrap_or_else(|| panic!("the script sets {floor_var}"))
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{floor_var} is a number"));
    let n: usize = run_with_script_functions(&format!("count_test_targets \"$({discover})\""))
        .parse()
        .expect("count_test_targets prints a number");
    assert!(n >= floor, "`{discover}` found {n} suites, the script's floor is {floor}");
}
