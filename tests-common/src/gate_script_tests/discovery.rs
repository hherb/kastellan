//! The gate script's suite discovery and target count, run as real bash.

use std::path::PathBuf;

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

/// The two discoveries partition the `core` suites that use the shared
/// micro-VM module: none is selected by both, so neither profile runs a suite
/// its host compiles to an empty binary, and none is selected by neither.
///
/// The container half was first a grep for `skip_if_no_container`, which also
/// selected `gliner_relex_e2e`'s file-local helper of that name (#722).
#[test]
fn the_firecracker_and_container_discoveries_partition_the_microvm_suites() {
    let names = |snippet: &str| -> Vec<String> {
        run_with_script_functions(snippet)
            .split_whitespace()
            .filter(|t| *t != "--test")
            .map(str::to_string)
            .collect()
    };
    let (fc, ct) = (names("firecracker_suites"), names("container_suites"));
    assert!(!ct.is_empty() && !fc.is_empty(), "a discovery found nothing");
    for suite in &ct {
        assert!(!fc.contains(suite), "`{suite}` is selected by both discoveries");
    }
    let all = names(
        "for f in core/tests/*.rs; do if grep -q 'kastellan_tests_common::microvm' \"$f\"; \
         then printf -- '--test %s ' \"$(basename \"$f\" .rs)\"; fi; done",
    );
    assert_eq!(
        fc.len() + ct.len(),
        all.len(),
        "the discoveries do not partition the micro-VM suites: {fc:?} + {ct:?} vs {all:?}"
    );
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
