//! #725, the test-harness half of #666: a worker's last words must reach a
//! **failing test**, not only a daemon that installed a `tracing` subscriber.
//!
//! #666 routed a dead worker's own stderr into `tracing::warn!`. That is the
//! right channel for the daemon, which always installs a subscriber
//! (`core/src/main.rs`). **Test binaries install none**, so in 27 of the 28
//! `core/tests/*.rs` suites that dispatch to a real worker the report goes
//! nowhere and the failure prints only `Protocol(EarlyExit)` — the least
//! informative error this system produces.
//!
//! What that costs is measured: #719's author spent a session ruling out five
//! hypotheses with the root cause still unknown. The next session added one
//! `tracing_subscriber` line to the failing test and the very next run printed
//! the full Python traceback. Each of the two root causes then took one run.
//!
//! ## Why this suite re-executes itself
//!
//! The property is about what an operator **sees when a test fails**, and a
//! test cannot observe its own failure output. Three things have to be true at
//! once and none of them is visible from inside the test that triggers them:
//!
//! 1. the fallback fires at all when no subscriber is installed;
//! 2. libtest's output capture reaches it — the report is written from a
//!    **tokio worker thread** (`dispatch` runs the synchronous `worker.call`
//!    inside `tokio::task::block_in_place`), not from the test thread, and
//!    capture is inherited at thread-spawn time. The issue filed this as an
//!    explicitly unverified assumption; this suite is the verification;
//!    ⚠️ **`eprintln!` is load-bearing, not a style choice.** libtest captures
//!    through `std::io::set_output_capture`, which only the `print!`/`eprint!`
//!    **macros** consult. A `writeln!(std::io::stderr(), …)` writes to the
//!    real fd, bypasses the capture, and would never appear under the failing
//!    test that needs it;
//! 3. it does **not** double-report in a binary that did install one.
//!
//! So each fixture below is an `#[ignore]`d inner test that deliberately
//! fails, and the visible test re-runs this same binary with `--exact` on it
//! and reads the child's output. A separate child per fixture is not
//! fastidiousness: `tracing::dispatcher::has_been_set()` is a process-global
//! `AtomicBool` set by `with_default` as well as `set_global_default`, so one
//! subscriber anywhere in a binary pins the branch for every later test in it.
//! Running the two fixtures in one process would make the second answer for
//! the first.
//!
//! ⚠️ **Every child run asserts `1 failed`.** `cargo test`-style name filters
//! exit 0 when they match nothing, so "the child's output lacks the phrase"
//! and "the child ran no tests" are otherwise the same observation — the
//! `--exact`-matches-nothing trap that has reported a whole mutation batch as
//! surviving against zero tests.

use std::path::PathBuf;
use std::process::Command;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_sandbox::{Net, SandboxPolicy};
use kastellan_tests_common::{backend, skip_if_sandbox_unavailable, NoopAuditSink};

/// The shell the fixture worker is: a process that says something on stderr
/// and exits without ever speaking JSON-RPC. Present in both jails (bwrap
/// binds `/usr` and symlinks `/bin`; Seatbelt allows the system prefix
/// read-only), which is why the fixture is a shell one-liner and not a
/// compiled binary — the property under test has nothing to do with what the
/// worker is. Same fixture shape as `worker_early_exit_diagnostic_e2e`.
const FIXTURE_SHELL: &str = "/bin/sh";

/// A phrase distinctive enough that finding it in the child's output cannot be
/// a coincidence, and shaped like the real thing this stands in for
/// (`microvm-init: chown of the relay socket … Halting instead.`).
const LAST_WORDS: &str = "kastellan-test: refusing to serve, relay socket unreachable";

/// The panic message each inner fixture ends on. Its presence in the child's
/// output is how the parent knows it read the *deliberate* failure and not an
/// unrelated one (a missing worker binary, a poisoned runtime).
const DELIBERATE: &str = "deliberate failure: this fixture exists to be read from its parent";

/// Run the fixture worker to its early exit. Shared by both inner tests, which
/// differ only in whether they install a subscriber first.
///
/// Returns nothing: the dispatch's failure is the point, and the bytes this
/// test is about are written as a side effect of it.
fn dispatch_to_a_worker_that_dies_first() {
    let policy = SandboxPolicy {
        fs_read: vec![PathBuf::from(FIXTURE_SHELL)],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        ..SandboxPolicy::default()
    };
    let script = format!("echo '{LAST_WORDS}' >&2; exit 3");
    let args = ["-c", script.as_str()];
    let spec = WorkerSpec {
        policy: &policy,
        program: FIXTURE_SHELL,
        args: &args,
        wall_clock_ms: Some(10_000),
    };

    // Multi-threaded on purpose: `dispatch` runs the blocking call inside
    // `block_in_place`, which is only available on a multi-thread runtime and
    // is exactly the thread-boundary this suite exists to prove capture
    // crosses.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    let result = rt.block_on(async {
        let mut worker = spawn_worker(&*backend(), &spec).expect("spawn the fixture worker");
        let out = dispatch_with_sink(
            &NoopAuditSink,
            &Vault::new(),
            None,
            &mut worker,
            "early-exit-fixture",
            "anything",
            serde_json::json!({}),
        )
        .await;
        let _ = worker.close();
        out
    });

    assert!(
        result.is_err(),
        "the fixture never answers, so the dispatch must fail: {result:?}"
    );
}

/// Inner fixture: no subscriber, so the stderr fallback is the only channel.
///
/// `#[ignore]` so a normal sweep never runs it — it is meant to fail, and it
/// is run by [`a_failing_test_with_no_subscriber_shows_the_workers_last_words`]
/// through a child process.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_early_exit_without_a_subscriber() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    dispatch_to_a_worker_that_dies_first();
    panic!("{DELIBERATE}");
}

/// Inner fixture: a subscriber **is** installed, writing to the process's real
/// stderr, so the report must arrive exactly once — through `tracing` — and
/// the fallback must stay quiet.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_early_exit_with_a_subscriber() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    // Global, not scoped: the report is emitted from a tokio worker thread, so
    // a thread-local subscriber would not be in scope where it is written and
    // the fixture would prove the opposite of what it claims.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    dispatch_to_a_worker_that_dies_first();
    panic!("{DELIBERATE}");
}

/// Re-run this test binary with `--exact <inner>` and return its combined
/// stdout+stderr plus whether it failed.
///
/// Deliberately **without** `--nocapture`: the claim under test is about what
/// libtest prints for a *failing* test, and `--nocapture` would bypass the
/// capture entirely and prove nothing about it.
fn run_inner_fixture(name: &str) -> (bool, String) {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args(["--exact", name, "--ignored", "--test-threads", "1"])
        // Inherit the environment so a REQUIRE knob set for the parent is also
        // set for the child; a child that silently skipped would otherwise
        // report "0 failed" and be caught by the assertion below anyway.
        .output()
        .unwrap_or_else(|e| panic!("re-exec this test binary to run `{name}`: {e}"));

    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// Assert the child ran exactly the one fixture and that it failed.
///
/// The positive control. Without it, a typo in the fixture name gives
/// `0 passed; 0 failed; N filtered out` and **exit 0**, and every assertion
/// about the output below would be checking a string that was never produced.
fn assert_one_deliberate_failure(name: &str, succeeded: bool, output: &str) {
    assert!(
        !succeeded,
        "the child must FAIL — `{name}` panics on purpose, and a passing child means the \
         filter matched nothing (`--exact` on a misspelled name exits 0).\nChild output:\n{output}"
    );
    assert!(
        output.contains("1 failed"),
        "exactly one test must have run and failed in the child; anything else means the \
         name filter did not select `{name}`.\nChild output:\n{output}"
    );
    assert!(
        output.contains(DELIBERATE),
        "the child must have reached the fixture's own panic, not died earlier for an \
         unrelated reason.\nChild output:\n{output}"
    );
}

#[test]
fn a_failing_test_with_no_subscriber_shows_the_workers_last_words() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let name = "inner_fixture_early_exit_without_a_subscriber";
    let (succeeded, output) = run_inner_fixture(name);
    assert_one_deliberate_failure(name, succeeded, &output);

    assert!(
        output.contains(LAST_WORDS),
        "a test binary installs no `tracing` subscriber, so `tracing::warn!` discards the \
         report and the failure reads only `Protocol(EarlyExit)` — the shrug that cost #719 \
         a whole session. The dying worker's own explanation must appear in the failing \
         test's captured output (#725).\nChild output:\n{output}"
    );
    assert!(
        output.contains(FIXTURE_SHELL),
        "the report must name WHICH worker died — on the micro-VM path the process that \
         exits is the launcher, not the tool.\nChild output:\n{output}"
    );
}

#[test]
fn a_binary_that_installed_a_subscriber_does_not_get_the_report_twice() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let name = "inner_fixture_early_exit_with_a_subscriber";
    let (succeeded, output) = run_inner_fixture(name);
    assert_one_deliberate_failure(name, succeeded, &output);

    let seen = output.matches(LAST_WORDS).count();
    assert_eq!(
        seen, 1,
        "with a subscriber installed the report must arrive exactly once, through `tracing`. \
         Seeing it twice means the stderr fallback fires unconditionally, which would double \
         every early-exit report in the daemon's own log.\nChild output:\n{output}"
    );
}
