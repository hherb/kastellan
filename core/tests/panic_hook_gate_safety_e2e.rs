//! #742: a panicking test cannot forge a column-0 gate evidence line.
//!
//! `tests_common::panic_hook`'s unit tests pin the **renderer**. They cannot
//! pin the thing that actually matters — that the hook is *installed* in a real
//! test binary — because a test cannot observe its own panic output. Deleting
//! the `panic_hook::install_once()` call from `RequireKnob::action` survives
//! every one of them.
//!
//! So this suite re-executes itself, exactly as the two `*_stderr_fallback_e2e`
//! suites do, and reads a deliberately panicking child's streams from outside.
//!
//! ## What the child does, and why in that order
//!
//! 1. consults a REQUIRE knob — the chokepoint that installs the hook, and the
//!    call every gated tier already makes;
//! 2. panics with a payload carrying an ESC and a `\n[WARN] …`.
//!
//! Step 1 is the property. If the hook is installed anywhere *else* — an
//! explicit call in this file, say — this suite would pass while proving
//! nothing about the ~30 other suites that never make such a call. It
//! deliberately does not install the hook itself.
//!
//! ⚠️ **The child must FAIL, and the parent asserts it.** `--exact` on a
//! misspelled name exits 0 having run nothing, which would make "no forged
//! line in the output" true of a child that never panicked — the
//! matches-nothing trap that has reported a whole mutation batch as surviving
//! against zero tests.

use std::process::Command;

use kastellan_tests_common::panic_hook::PANIC_MARKER;
use kastellan_tests_common::sandbox::skip_if_sandbox_unavailable;

/// The payload from #742's own reproduction.
///
/// ⚠️ **The `\n` is the weapon and the `[WARN]` is the target.**
/// `scripts/run-e2e-gate.sh` greps `^\[WARN\]` and asserts zero matches over a
/// profile run, and every profile passes `--nocapture`. Without the hook, the
/// default panic hook prints this verbatim and the text after the newline
/// lands at column 0.
const HOSTILE_PAYLOAD: &str = "deliberate panic\u{1b}[31m\n[WARN] FORGED-PANIC-GATE-LINE";

/// The forgery attempt inside [`HOSTILE_PAYLOAD`].
///
/// ⚠️ **It must survive as text and must never begin a line.**
/// `neutralise_controls` maps the class to `' '`, so the correct outcome is
/// this phrase sitting mid-line behind a space — not the phrase disappearing.
/// Asserting its absence would pass just as well if the payload never reached
/// the output at all.
const FORGED_LINE_START: &str = "[WARN] FORGED-PANIC-GATE-LINE";

/// The part of the forgery that no neutralisation touches, used as the
/// positive control for "the payload really did reach the stream".
const FORGED_TEXT: &str = "FORGED-PANIC-GATE-LINE";

/// Set by the parent on the child it launches. The fixture does nothing
/// without it.
///
/// ⚠️ **Load-bearing, not belt-and-braces with `#[ignore]`.** This fixture
/// fails on purpose, and this tree documents `cargo test … -- --ignored` as the
/// way to run the Firecracker tier — which would otherwise surface a red test
/// reading exactly like a regression.
const FIXTURE_ENV: &str = "KASTELLAN_PANIC_HOOK_FIXTURE";

/// `true` when this process was launched by [`run_inner_fixture`].
///
/// Honours the tree's knob dialect (`1|true|yes|on`) rather than inventing a
/// second one, so an operator who exported `…=0` believing that disabled the
/// fixture gets what they expected.
fn is_the_child() -> bool {
    std::env::var(FIXTURE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Inner fixture: consult a REQUIRE knob, then panic with a hostile payload.
#[test]
#[ignore = "inner fixture: panics on purpose; run by its parent test in a child process"]
fn inner_fixture_panics_with_a_forged_gate_line() {
    if !is_the_child() {
        return;
    }
    // THE PROPERTY. This is an ordinary gated-suite preflight and nothing else
    // — no `panic_hook::install_once()` anywhere in this file. The hook must
    // arrive through `RequireKnob::action`, which this calls, or the ~30 suites
    // that likewise never mention the hook are unprotected and this suite would
    // be lying about them.
    //
    // Its verdict is discarded on purpose: whether the sandbox is available
    // decides nothing here, and returning early on an unavailable one would
    // make the child exit 0 and the parent's positive control fire.
    let _ = skip_if_sandbox_unavailable();

    panic!("{HOSTILE_PAYLOAD}");
}

/// What the parent learns from one child run.
///
/// ⚠️ **The two streams stay apart.** libtest re-prints a *captured* panic into
/// the failure block on the child's **stdout**; anything that bypassed the
/// capture goes to its **stderr**. Concatenating them before asserting would
/// make every `contains` true either way — the mistake the sibling suites
/// document at length.
struct ChildRun {
    succeeded: bool,
    stdout: String,
    stderr: String,
}

fn run_inner_fixture(name: &str) -> ChildRun {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args(["--exact", name, "--ignored", "--test-threads", "1"])
        .env(FIXTURE_ENV, "1")
        // ⚠️ libtest reads this as `!= "0"`, so even an empty value disables
        // the capture and would push the panic onto the raw fd, turning the
        // stdout assertions into a false red.
        .env_remove("RUST_TEST_NOCAPTURE")
        .output()
        .unwrap_or_else(|e| panic!("re-exec this test binary to run `{name}`: {e}"));
    ChildRun {
        succeeded: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn a_panicking_test_cannot_forge_a_column_zero_gate_line() {
    let name = "inner_fixture_panics_with_a_forged_gate_line";
    let run = run_inner_fixture(name);
    let both =
        format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", run.stdout, run.stderr);

    // POSITIVE CONTROL. Without it a typo in the fixture name gives
    // `0 passed; 0 failed; N filtered out` and exit 0, and every assertion
    // below would be checking output that was never produced.
    assert!(
        !run.succeeded,
        "the child must FAIL — `{name}` panics on purpose. A passing child means the filter \
         matched nothing (`--exact` on a misspelled name exits 0) or the fixture no-op'd.\n{both}"
    );
    assert!(
        run.stdout.contains("test result: FAILED. 0 passed; 1 failed;"),
        "exactly one test must have run and failed in the child.\n{both}"
    );

    // POSITIVE CONTROL for the payload itself: it has to have REACHED a stream,
    // or "no forged line" is satisfied by the panic message going missing.
    assert!(
        run.stdout.contains(FORGED_TEXT) || run.stderr.contains(FORGED_TEXT),
        "the hostile payload never reached either stream, so the checks below would pass \
         vacuously — neutralisation moves the text off column 0, it does not censor it.\n{both}"
    );

    // THE PROPERTY, in the form the gate reads it.
    for (label, stream) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
        let forged: Vec<&str> =
            stream.lines().filter(|l| l.starts_with(FORGED_LINE_START)).collect();
        assert!(
            forged.is_empty(),
            "a panic payload forged a COLUMN-0 line on the child's {label}: {forged:?}. \
             `scripts/run-e2e-gate.sh` greps `^\\[WARN\\]` and asserts zero matches over a \
             profile run with `--nocapture`, so this turns an unrelated profile red. The hook \
             must be installed — check that `RequireKnob::action` still calls \
             `panic_hook::install_once` (#742).\n{both}"
        );
    }

    // The hook's own marker must be what carried the message, which is what
    // distinguishes "the hook ran" from "the payload happened not to forge a
    // line this time".
    assert!(
        run.stdout.contains(PANIC_MARKER) || run.stderr.contains(PANIC_MARKER),
        "the neutralising hook's `{PANIC_MARKER}` marker is absent, so the default hook \
         rendered this panic and the absence of a forged line above is luck rather than \
         containment.\n{both}"
    );

    // The everyday win, and the one a human notices: no ANSI sequence executing
    // in the terminal of whoever reads the failure.
    assert!(
        !run.stdout.contains('\u{1b}') && !run.stderr.contains('\u{1b}'),
        "an ESC from a panic payload reached the reader's terminal unneutralised.\n{both}"
    );
}
