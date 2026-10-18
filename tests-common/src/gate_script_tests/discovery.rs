//! The gate script's suite discovery and target count, run as real bash.

use std::path::PathBuf;

use super::{gate_script, script_path};

/// Run a snippet of bash with the gate script's `count_test_targets` and
/// `firecracker_suites` functions defined, from the repository root.
///
/// Extracted by name rather than by sourcing the script, which would run its
/// preflight and profile validation.
fn run_with_script_functions(snippet: &str) -> String {
    let src = gate_script();
    let mut defs = String::new();
    for name in ["count_test_targets", "firecracker_suites"] {
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
    let floor: usize = gate_script()
        .lines()
        .find_map(|l| l.strip_prefix("MIN_FIRECRACKER_SUITES="))
        .expect("the script sets MIN_FIRECRACKER_SUITES")
        .trim()
        .parse()
        .expect("MIN_FIRECRACKER_SUITES is a number");
    let n: usize = run_with_script_functions("count_test_targets \"$(firecracker_suites)\"")
        .parse()
        .expect("count_test_targets prints a number");
    assert!(n >= floor, "discovery found {n} micro-VM suites, the script's floor is {floor}");
}
