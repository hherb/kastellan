//! Every door a REQUIRE knob value passes through installs the neutralising panic hook
//! ([#748](https://github.com/hherb/kastellan/issues/748)).
//!
//! #745 installed the hook from `RequireKnob::action_reporting_to`, documented
//! as the chokepoint every knob read funnels through. It was not. On a healthy
//! micro-VM host every precondition is MET, so the suite never reaches the
//! unmet path (`report_unmet_microvm` → `require_action_to` →
//! `action_reporting_to`); it announces `[E2E]` through `announce_demanded_to`,
//! which reads the knob via `raw()` and bypassed the install entirely. So every
//! GREEN micro-VM gate run had the default panic hook. #748's run-time check
//! found it on its first real run: all 15 micro-VM binaries refused, with 65
//! `[E2E]` lines proving they had read the knob.
//!
//! ## Why a child process per door
//!
//! The hook is installed through a `Once`, which is per PROCESS. Two doors
//! tested in one process would pass the second on the first's install. Each
//! fixture therefore runs alone in a re-exec'd child, asserts the hook is NOT
//! installed before its one knob read and IS installed after, and prints a
//! sentinel the parent requires — because `--exact` on a name that matches
//! nothing exits 0 having run nothing.

use std::process::Command;

use kastellan_tests_common::panic_hook::is_installed;

/// Set by the parent on the child it launches; the fixtures do nothing without it.
const FIXTURE_ENV: &str = "KASTELLAN_KNOB_HOOK_FIXTURE";

/// Printed on stdout by a fixture that got past both assertions.
const DOOR_PROVEN: &str = "kastellan-test: knob door installed the panic hook";

fn is_the_child() -> bool {
    std::env::var(FIXTURE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Assert `read_the_knob` takes this process from "no hook" to "hook", then
/// print the sentinel.
fn prove_the_door_installs(read_the_knob: impl FnOnce()) {
    assert!(!is_installed(), "the hook was installed before the door under test ran");
    read_the_knob();
    assert!(is_installed(), "this knob read did not install the neutralising panic hook");
    println!("{DOOR_PROVEN}");
}

/// THE REGRESSION: the met-precondition path. Knob set, nothing unmet, so the
/// only knob read is the `[E2E]` announcement.
#[test]
#[ignore = "inner fixture: needs a fresh process; run by its parent"]
fn inner_fixture_announce_demanded() {
    if !is_the_child() {
        return;
    }
    prove_the_door_installs(|| {
        // Into a buffer, so no real `[E2E]` line inflates a gate that runs this.
        let mut sink = Vec::new();
        kastellan_tests_common::microvm::announce_microvm_to("door test", &mut sink);
        assert!(!sink.is_empty(), "the knob was not read as set, so this proves nothing");
    });
}

/// The decision route, `RequireKnob::action()` — not itself a door: it calls
/// both, and is pinned here as the route most suites take.
#[test]
#[ignore = "inner fixture: needs a fresh process; run by its parent"]
fn inner_fixture_action() {
    if !is_the_child() {
        return;
    }
    prove_the_door_installs(|| {
        let _ = kastellan_tests_common::require::GUARD_TIER_KNOB.action();
    });
}

/// The micro-VM decision route. Since #755 it reads the value through `raw()`
/// and hands it to `action_reporting_to`, so it passes BOTH doors and pins
/// neither on its own — it stays as coverage of the route the `microvm`
/// profile's unmet path takes. The `action_reporting_to` door is pinned alone
/// by [`inner_fixture_action_reporting_to`].
#[test]
#[ignore = "inner fixture: needs a fresh process; run by its parent"]
fn inner_fixture_microvm_require_action() {
    if !is_the_child() {
        return;
    }
    prove_the_door_installs(|| {
        let mut sink = Vec::new();
        let _ = kastellan_tests_common::microvm::require_action_to(&mut sink);
    });
}

/// The supplied-value door, `RequireKnob::action_reporting_to`, reached WITHOUT
/// `raw()`: the value is handed in, so the environment is never read. Until
/// #755 the micro-VM route pinned this door; once that route went through
/// `raw()` first, deleting the install from `action_reporting_to` would have
/// stayed green — hence a fixture that reaches it alone.
#[test]
#[ignore = "inner fixture: needs a fresh process; run by its parent"]
fn inner_fixture_action_reporting_to() {
    if !is_the_child() {
        return;
    }
    prove_the_door_installs(|| {
        let mut sink = Vec::new();
        let _ = kastellan_tests_common::require::GUARD_TIER_KNOB
            .action_reporting_to(Some("1".into()), &mut sink);
    });
}

/// Run one fixture alone in a child, with every knob it could read set truthy.
fn assert_door_installs(fixture: &str) {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args([fixture, "--exact", "--ignored", "--test-threads", "1", "--nocapture"])
        .env(FIXTURE_ENV, "1")
        .env("KASTELLAN_MICROVM_REQUIRE_E2E", "1")
        .env("KASTELLAN_GUARD_REQUIRE_E2E", "1")
        .output()
        .expect("re-exec this test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let both = format!("--- stdout ---\n{stdout}\n--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        stdout.contains(DOOR_PROVEN),
        "`{fixture}` did not prove its door (a failed assertion, or the name filter \
         selected nothing).\n{both}"
    );
    assert!(out.status.success(), "`{fixture}` failed.\n{both}");
}

#[test]
fn the_met_precondition_announcement_installs_the_hook() {
    kastellan_tests_common::panic_hook::install_once();
    assert_door_installs("inner_fixture_announce_demanded");
}

#[test]
fn the_action_route_installs_the_hook() {
    kastellan_tests_common::panic_hook::install_once();
    assert_door_installs("inner_fixture_action");
}

#[test]
fn the_microvm_decision_route_installs_the_hook() {
    kastellan_tests_common::panic_hook::install_once();
    assert_door_installs("inner_fixture_microvm_require_action");
}

#[test]
fn the_supplied_value_door_installs_the_hook() {
    kastellan_tests_common::panic_hook::install_once();
    assert_door_installs("inner_fixture_action_reporting_to");
}
