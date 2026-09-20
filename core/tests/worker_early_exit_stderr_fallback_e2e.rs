//! #725, the test-harness half of #666: a worker's last words must reach a
//! **failing test**, not only a daemon that installed a `tracing` subscriber.
//!
//! #666 routed a dead worker's own stderr into `tracing::warn!`. That is the
//! right channel for the daemon, which always installs a subscriber
//! (`core/src/main.rs`). **Test binaries install none**, so in 29 of the 30
//! `core/tests/*.rs` suites that dispatch to a real worker the report goes
//! nowhere and the failure prints only `Protocol(EarlyExit)` — the least
//! informative error this system produces.
//!
//! What that costs is measured: #719's author spent a session on it with the
//! root cause still unknown. The next session added one `tracing_subscriber`
//! line to the failing test and the very next run printed the full Python
//! traceback. Each of the two root causes then took one run.
//!
//! ## Why this suite re-executes itself
//!
//! The property is about what an operator **sees when a test fails**, and a
//! test cannot observe its own failure output. Three things have to be true at
//! once and none of them is visible from inside the test that triggers them:
//!
//! 1. the fallback fires at all when no subscriber is installed;
//! 2. **libtest's capture reaches it**, so the report lands in the failing
//!    test's `---- <name> stdout ----` block rather than on the raw fd;
//! 3. it does **not** double-report in a binary that did install one.
//!
//! ⚠️ **Point 2 is why the two child streams are read separately and never
//! merged.** It is the whole reason the producer must use `eprintln!` rather
//! than `writeln!(std::io::stderr(), …)`: libtest captures through
//! `std::io::set_output_capture`, which the `print!`/`eprint!` macros consult
//! and the `Stdout`/`Stderr` handles do not. A captured `eprintln!` is
//! re-printed by libtest into the failure block on the child's **stdout**; the
//! `writeln!` variant goes straight to the child's **stderr**. Concatenating
//! the two before asserting — which this suite did in its first draft — makes
//! `contains(LAST_WORDS)` true either way and silently stops testing the one
//! claim the suite exists to make.
//!
//! ## Why the fixture dispatches from a spawned task
//!
//! The capture is installed per-thread, inherited at thread-spawn time (libtest
//! registers a `std::thread::add_spawn_hook`), so "does it cross a thread
//! boundary" is a real question — and `dispatch` runs the blocking
//! `worker.call` inside `tokio::task::block_in_place`.
//!
//! ⚠️ **`block_in_place` does NOT hand off to another thread** — it runs the
//! closure on the current one and migrates the *other* tasks away. Measured:
//! under a bare `rt.block_on`, `block_in_place` reports the same `ThreadId` as
//! the test thread, so a fixture written that way crosses no boundary and
//! proves nothing about inheritance. Under `tokio::spawn` it reports a
//! different one. The fixture therefore dispatches from inside a spawned task,
//! which is both the stronger test and the shape the daemon actually runs.
//!
//! ⚠️ **One child process per fixture.** `tracing::dispatcher::has_been_set()`
//! is a process-global `AtomicBool` that `with_default` sets as well as
//! `set_global_default`, and which is never cleared — so one subscriber
//! anywhere in a binary pins the branch for every later test in it. Running
//! both fixtures in one process would make the second answer for the first.
//!
//! ⚠️ **Every child run asserts `1 failed`.** A libtest name filter exits 0
//! when it matches nothing, so "the child's output lacks the phrase" and "the
//! child ran no tests" would otherwise be the same observation — the
//! `--exact`-matches-nothing trap that has reported a whole mutation batch as
//! surviving against zero tests.

use std::path::PathBuf;
use std::process::Command;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_core::worker_stderr::EARLY_EXIT_STDERR_MARKER;
use kastellan_sandbox::{Net, SandboxPolicy};
use kastellan_tests_common::{backend, skip_if_sandbox_unavailable, NoopAuditSink};

/// The shell the fixture worker is: a process that says something on stderr
/// and exits without ever speaking JSON-RPC. `/bin/sh` is reachable in both
/// jails, and the fixture names it explicitly in `fs_read`, so it needs no
/// build step — the property under test has nothing to do with what the worker
/// is. Same fixture shape as `worker_early_exit_diagnostic_e2e`.
const FIXTURE_SHELL: &str = "/bin/sh";

/// A phrase distinctive enough that finding it in the child's output cannot be
/// a coincidence, and shaped like the real thing this stands in for
/// (`microvm-init: chown of the relay socket … Halting instead.`).
const LAST_WORDS: &str = "kastellan-test: refusing to serve, relay socket unreachable";

/// The panic message each inner fixture ends on. Its presence in the child's
/// output is how the parent knows it read the *deliberate* failure and not an
/// unrelated one (a missing worker binary, a poisoned runtime).
const DELIBERATE: &str = "deliberate failure: this fixture exists to be read from its parent";

/// Set by the parent on the child it launches. The inner fixtures do nothing
/// unless they see it.
///
/// ⚠️ **Load-bearing, and not merely belt-and-braces with `#[ignore]`.** These
/// fixtures fail on purpose, and this tree documents `cargo test … -- --ignored`
/// as the way to run the Firecracker tier — which would otherwise surface two
/// red tests that read exactly like a regression. `#[ignore]` keeps them out of
/// a normal sweep; this keeps them out of an `--ignored` one. Because the
/// parent asserts the child *failed*, a fixture that silently did nothing makes
/// the parent fail loudly rather than pass quietly.
const FIXTURE_ENV: &str = "KASTELLAN_EARLY_EXIT_FIXTURE";

/// Run the fixture worker to its early exit, from inside a spawned task.
///
/// Everything the worker needs is built *inside* the spawned future so it
/// borrows nothing from the caller's frame and satisfies `tokio::spawn`'s
/// `'static` bound.
fn dispatch_to_a_worker_that_dies_first() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    let result = rt.block_on(async {
        tokio::spawn(async {
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
        })
        .await
        .expect("the spawned dispatch task must not panic")
    });

    assert!(
        result.is_err(),
        "the fixture never answers, so the dispatch must fail: {result:?}"
    );
}

/// `true` when this process was launched by [`run_inner_fixture`].
fn is_the_child() -> bool {
    std::env::var_os(FIXTURE_ENV).is_some()
}

/// Inner fixture: no subscriber, so the stderr fallback is the only channel.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_early_exit_without_a_subscriber() {
    if !is_the_child() || skip_if_sandbox_unavailable() {
        return;
    }
    dispatch_to_a_worker_that_dies_first();
    panic!("{DELIBERATE}");
}

/// Inner fixture: a subscriber **is** installed, so the report must arrive
/// exactly once — through `tracing` — and the fallback must stay quiet.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_early_exit_with_a_subscriber() {
    if !is_the_child() || skip_if_sandbox_unavailable() {
        return;
    }
    // Global, not scoped: the report is written from a spawned task on another
    // thread, so a thread-local subscriber would not be in scope where it is
    // written and the fixture would prove the opposite of what it claims.
    //
    // ⚠️ `WARN`, deliberately. `drain_reader` logs the very same worker bytes
    // at `debug` (`core/src/worker_stderr.rs`), so raising this to `DEBUG`
    // would make the parent's exactly-once assertion fail for a reason with
    // nothing to do with the fallback.
    //
    // `with_writer(std::io::stderr)` is the raw handle, which libtest's
    // capture does NOT intercept — so this lands on the child's real stderr,
    // which is exactly how the parent tells the two channels apart.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    dispatch_to_a_worker_that_dies_first();
    panic!("{DELIBERATE}");
}

/// What the parent learns from one child run. The two streams stay **apart**;
/// see the module doc for why merging them silently voids the suite.
struct ChildRun {
    succeeded: bool,
    /// Where libtest re-prints a failing test's *captured* output.
    stdout: String,
    /// The raw fd — where anything that bypassed the capture lands.
    stderr: String,
}

/// Re-run this test binary with `--exact <name>`, returning its two streams.
///
/// Deliberately **without** `--nocapture`: the claim under test is about what
/// libtest prints for a *failing* test, and `--nocapture` would disable the
/// capture entirely and prove nothing about it.
fn run_inner_fixture(name: &str) -> ChildRun {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args(["--exact", name, "--ignored", "--test-threads", "1"])
        .env(FIXTURE_ENV, "1")
        // ⚠️ libtest reads this as `!= "0"`, so even an empty value disables
        // the capture — which would push the report onto the raw fd and turn
        // the stdout assertions below into a false red.
        .env_remove("RUST_TEST_NOCAPTURE")
        .output()
        .unwrap_or_else(|e| panic!("re-exec this test binary to run `{name}`: {e}"));

    ChildRun {
        succeeded: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Assert the child ran exactly the one fixture and that it failed.
///
/// The positive control. Without it, a typo in the fixture name gives
/// `0 passed; 0 failed; N filtered out` and **exit 0**, and every assertion
/// about the output below would be checking a string that was never produced.
/// It also catches a fixture that no-op'd because [`FIXTURE_ENV`] failed to
/// reach it, and one that died before its own panic.
fn assert_one_deliberate_failure(name: &str, run: &ChildRun) {
    let both = format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", run.stdout, run.stderr);
    assert!(
        !run.succeeded,
        "the child must FAIL — `{name}` panics on purpose. A passing child means the filter \
         matched nothing (`--exact` on a misspelled name exits 0) or the fixture no-op'd.\n{both}"
    );
    assert!(
        run.stdout.contains("1 failed"),
        "exactly one test must have run and failed in the child; anything else means the \
         name filter did not select `{name}`.\n{both}"
    );
    assert!(
        run.stdout.contains(DELIBERATE),
        "the child must have reached the fixture's own panic, not died earlier for an \
         unrelated reason.\n{both}"
    );
}

#[test]
fn a_failing_test_with_no_subscriber_shows_the_workers_last_words() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let name = "inner_fixture_early_exit_without_a_subscriber";
    let run = run_inner_fixture(name);
    assert_one_deliberate_failure(name, &run);
    let both = format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", run.stdout, run.stderr);

    assert!(
        run.stdout.contains(LAST_WORDS),
        "a test binary installs no `tracing` subscriber, so `tracing::warn!` discards the \
         report and the failure reads only `Protocol(EarlyExit)` — the shrug that cost #719 a \
         session. The dying worker's own explanation must appear in the failing test's \
         CAPTURED output (#725).\n{both}"
    );
    assert!(
        run.stdout.contains(EARLY_EXIT_STDERR_MARKER),
        "the report must carry its marker, which is what makes the fallback greppable and \
         tells a reader which channel produced it.\n{both}"
    );
    assert!(
        !run.stderr.contains(LAST_WORDS),
        "the report must go through `eprintln!`, which libtest's capture intercepts — NOT to \
         the raw stderr fd. Finding it on the child's real stderr means the producer used \
         `writeln!(std::io::stderr(), …)` or equivalent, and under a normal (capturing) run \
         it would never appear beneath the failing test that needs it.\n{both}"
    );
}

#[test]
fn a_binary_that_installed_a_subscriber_does_not_get_the_report_twice() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let name = "inner_fixture_early_exit_with_a_subscriber";
    let run = run_inner_fixture(name);
    assert_one_deliberate_failure(name, &run);
    let both = format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", run.stdout, run.stderr);

    assert_eq!(
        run.stderr.matches(LAST_WORDS).count(),
        1,
        "the fixture's subscriber writes to the raw stderr handle, so with a subscriber \
         installed the report must arrive there exactly once.\n{both}"
    );
    assert!(
        !run.stdout.contains(LAST_WORDS),
        "the fallback must stay QUIET when a subscriber is installed. Seeing the report in \
         the captured stream too means the `has_been_set()` guard is not holding, which \
         would double every early-exit report in the daemon's own log.\n{both}"
    );
}
