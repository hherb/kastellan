//! #725, the test-harness half of #666: a worker's last words must reach a
//! **failing test**, not only a daemon that installed a `tracing` subscriber.
//!
//! #666 routed a dead worker's own stderr into `tracing::warn!`. That is the
//! right channel for the daemon, which always installs a subscriber
//! (`core/src/main.rs`). **Test binaries install none**, so in 29 of the 31
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
//! test cannot observe its own failure output. **Four** things have to be true
//! at once — one per fixture, plus the third state #734 added — and none of
//! them is visible from inside the test that triggers them:
//!
//! 1. the fallback fires at all when no subscriber is installed;
//! 2. **libtest's capture reaches it**, so the report lands in the failing
//!    test's `---- <name> stdout ----` block rather than on the raw fd;
//! 3. it does **not** double-report in a binary that did install one;
//! 4. it **still** fires when a subscriber exists but `EnvFilter` drops the
//!    event — the #734 state, which neither 1 nor 3 can reach.
//!
//! A fifth rides along, because this is the only place it can be observed end
//! to end: the report is **control-neutralised on every channel**. The fixture
//! dispatches [`HOSTILE_METHOD`] — a model-authored `method` is the one part of
//! the report interpolated raw — so an ESC or a forged column-0 `[WARN]` line
//! shows up in a child stream the parent can read. Without it, dropping the
//! `neutralise_controls` call in `emit_worker_failure_report` passed every test in
//! this file.
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
//! ⚠️ **One child process per fixture.** The fixtures install their
//! subscribers with `set_global_default`, which succeeds **once per process**
//! and cannot be undone — so a second fixture in the same binary could not
//! install its own, and would silently answer for the first.
//!
//! ⚠️ **This rule used to be justified by `has_been_set()`**, a never-cleared
//! process-global `AtomicBool` that pinned the fallback branch for a whole
//! binary. #734 replaced that guard with a per-event `event_enabled!` check,
//! so that hazard is **gone** — a scoped `with_default` no longer leaks into
//! later tests. The rule survives it for the reason above; the old reason is
//! recorded here so nobody re-derives it, finds it false, and drops the rule.
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
use kastellan_core::worker_stderr::WORKER_FAILED_STDERR_MARKER;
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

/// The method the fixture dispatches, carrying the two control characters a
/// **model-authored** method can really contain.
///
/// ⚠️ **This is a hostile input, not decoration.** `method` reaches
/// `format_worker_failure_report` interpolated **raw** — unlike tail lines, which
/// are neutralised as they enter the ring (`push_trimmed`) — and it is
/// attacker-influenced: `qualified_method` returns `None` for anything outside
/// the tool's advertised set, and `scheduler::tool_dispatch` then puts the
/// planner's own string on the wire verbatim
/// (`let method = qualified.as_deref().unwrap_or(&step.method);`). So the
/// planner writes these bytes and `emit_worker_failure_report` is the only thing
/// standing between them and a gate log.
///
/// Two payloads, because they fail differently:
/// - `\u{1b}[31m` — an ANSI sequence executing in the terminal of whoever reads
///   the failing test.
/// - a `\n` followed by [`FORGED_LINE_START`] — a forged **column-0** line.
///   `scripts/run-e2e-gate.sh` greps `^\[WARN\]` and asserts zero matches, so
///   this is the shape that turns someone else's green gate red.
const HOSTILE_METHOD: &str = "anything\u{1b}[31m\n[WARN] kastellan-test: FORGED-GATE-LINE";

/// The forgery attempt inside [`HOSTILE_METHOD`].
///
/// ⚠️ **It must survive as text and must never begin a line.**
/// `neutralise_controls` maps the class to `' '`, so the correct outcome is
/// this phrase sitting mid-line behind a space — not the phrase disappearing.
/// Asserting its absence would pass just as well if `method` never reached the
/// report at all, which is why the assertions below check *position* and pair
/// it with [`FORGED_TEXT`] as the positive control.
const FORGED_LINE_START: &str = "[WARN] kastellan-test: FORGED-GATE-LINE";

/// The part of [`FORGED_LINE_START`] that no neutralisation touches. Its
/// presence proves the hostile method actually reached the rendered report, so
/// "no forged line" cannot be satisfied by the method going missing.
const FORGED_TEXT: &str = "FORGED-GATE-LINE";

/// The `RUST_LOG` an operator writes when debugging the scheduler — which
/// names a real target and therefore enables **nothing** for the worker-report
/// emitter.
///
/// ⚠️ `EnvFilter`'s default directive applies only when the env string is
/// **empty**, so a non-empty target-scoped string like this leaves every other
/// target disabled. That is the whole mechanism of #734.
const FILTERED_AWAY_DIRECTIVE: &str = "kastellan_core::scheduler=debug";

/// The module the early-exit `tracing::warn!` is written in, and therefore the
/// target its event carries.
///
/// ⚠️ **Spelled out rather than derived, on purpose.** The fix for #734 turns
/// on the emitter's target being the one the delivery check asks about, so a
/// test that computed this from the same source as the code could not notice
/// them drifting apart. If the emitter moves module, this const must move with
/// it — and the fixture's positive control is what says so out loud.
const EMITTER_TARGET: &str = "kastellan_core::worker_stderr::report::tool_worker";

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
                HOSTILE_METHOD,
                serde_json::json!({}),
            )
            .await;
            let _ = worker.close();
            out
        })
        .await
        .expect("the spawned dispatch task must not panic")
    });

    // The VARIANT, not just `is_err()`. Only `Protocol(EarlyExit)` reaches
    // `warn_early_exit` (`core/src/tool_host.rs`), so a wall-clock kill, a
    // spawn refusal or a params failure would all satisfy `is_err()` while
    // exercising none of the code under test — and the parent would then
    // report "the report did not appear", blaming the producer from several
    // process boundaries away.
    let err = result.expect_err("the fixture never answers, so the dispatch must fail");
    assert!(
        matches!(
            err,
            kastellan_core::tool_host::ToolHostError::Protocol(
                kastellan_protocol::client::ClientError::EarlyExit
            )
        ),
        "the fixture must fail with Protocol(EarlyExit) — the ONLY variant that reaches \
         `warn_early_exit`. Anything else means this fixture stopped exercising the early-exit \
         path and the parent's assertions are about a report nothing produced. Got: {err:?}"
    );
}

/// `true` when this process was launched by [`run_inner_fixture`].
///
/// ⚠️ Presence is **not** enough: these fixtures fail on purpose, so an
/// operator who exported `KASTELLAN_EARLY_EXIT_FIXTURE=0` believing that
/// disabled them would get two red tests from the documented
/// `cargo test … -- --ignored` recipe, reading exactly like a regression. The
/// tree's knob dialect is `1|true|yes|on`; this honours it rather than
/// inventing a second one.
fn is_the_child() -> bool {
    std::env::var(FIXTURE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
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
    // `expect`, not `let _`: an `Err` here means a subscriber was ALREADY
    // installed, which is precisely the hazard this file documents
    // (`set_global_default` succeeds once per process and cannot be undone).
    // Swallowing it would leave the parent asserting "the report must arrive
    // on stderr exactly once" against a fixture whose own setup failed — and
    // if that pre-existing subscriber happened to write WARN to stderr, the
    // count would be 1 and the test would PASS having proven nothing.
    tracing::subscriber::set_global_default(subscriber)
        .expect("install the fixture's global subscriber; a prior install would void this test");

    dispatch_to_a_worker_that_dies_first();
    panic!("{DELIBERATE}");
}

/// Inner fixture: a subscriber is installed that **filters this event out**.
///
/// The [#734](https://github.com/hherb/kastellan/issues/734) case, and the one
/// the other two fixtures cannot reach between them. "No subscriber" takes the
/// fallback and "a subscriber that records WARN" takes `tracing`; this is the
/// third state, where a subscriber exists and the report still goes **nowhere**.
///
/// ⚠️ **This is not a contrived filter.** `core/src/main.rs` builds the
/// daemon's subscriber from `EnvFilter::try_from_default_env()`, and
/// `supervisor/src/specs.rs` documents `RUST_LOG` as operator-settable through
/// the `kastellan.env.local` overlay. An operator debugging the scheduler
/// writes exactly the directive below — and before #734 that silenced the most
/// valuable diagnostic in the system while they were trying to debug it, with
/// nothing saying so.
///
/// The directive names a **real** target that is not this one, rather than
/// `off`: `off` would also be silenced by a fix that merely checked the max
/// level, and the point is that the filter is *target-scoped*.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_early_exit_with_a_filtering_subscriber() {
    if !is_the_child() || skip_if_sandbox_unavailable() {
        return;
    }
    // Global, and to the raw stderr handle, for the same reasons as the
    // with-subscriber fixture: the report is written from a spawned task, and
    // the raw handle is how the parent tells the two channels apart.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(FILTERED_AWAY_DIRECTIVE))
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("install the fixture's global subscriber; a prior install would void this test");

    // POSITIVE CONTROL, inside the child, before anything else runs: prove the
    // filter really does drop a WARN from the emitter's target. Without it a
    // typo in the directive (or a `tracing-subscriber` change that made
    // `with_env_filter` permissive) would leave the parent asserting the
    // presence of a fallback line that fired for a completely different
    // reason, and the suite would pass while testing nothing.
    assert!(
        !tracing::event_enabled!(target: EMITTER_TARGET, tracing::Level::WARN),
        "the fixture's own filter must DROP a WARN from `{EMITTER_TARGET}`, or this fixture is \
         not the #734 state at all. Directive: {FILTERED_AWAY_DIRECTIVE}"
    );

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
    // The FULL libtest phrase, not `contains("1 failed")`: that substring is
    // also in "11 failed", "21 failed" and — the one that matters — in
    // "1 passed; 1 failed". Two tests running in one child would defeat the
    // one-child-per-fixture rule the module doc explains, because
    // `set_global_default` succeeds only once per process and the first
    // fixture would have consumed it.
    assert!(
        run.stdout.contains("test result: FAILED. 0 passed; 1 failed;"),
        "exactly one test must have run and failed in the child; anything else means the \
         name filter did not select `{name}`, or more than one test ran and they can no \
         longer answer for themselves.\n{both}"
    );
    assert!(
        run.stdout.contains(DELIBERATE),
        "the child must have reached the fixture's own panic, not died earlier for an \
         unrelated reason.\n{both}"
    );
}

/// Assert the channel named by `channel` carries the hostile method's text but
/// neither of its control characters' effects.
///
/// Both channels are checked, by their respective parents, because
/// `emit_worker_failure_report` neutralises **once** and then feeds both — so a
/// mutant that drops the neutralisation has to be caught wherever the report
/// actually lands. The no-subscriber run proves it for the `eprintln!`
/// fallback; the with-subscriber run proves it for `tracing`, whose `fmt`
/// layer writes the message through `Debug for Arguments` (i.e. Display, no
/// escaping), so an un-neutralised `\n` really does break the line there.
fn assert_the_hostile_method_was_defanged(channel: &str, stream: &str, both: &str) {
    assert!(
        stream.contains(FORGED_TEXT),
        "POSITIVE CONTROL: the hostile method never reached {channel} at all, so the \
         checks below would pass vacuously. Either `method` stopped being interpolated into the \
         report, or the dispatch failed before `warn_early_exit`.\n{both}"
    );
    assert!(
        !stream.contains('\u{1b}'),
        "an ESC from a MODEL-AUTHORED `method` reached {channel} unneutralised — it is an ANSI \
         sequence executing in the terminal of whoever reads this failure. \
         `emit_worker_failure_report` must `neutralise_controls` the WHOLE report, not just the \
         tail (tail lines are already stripped by `push_trimmed`; `program`/`method` are \
         interpolated raw).\n{both}"
    );
    let forged: Vec<&str> = stream
        .lines()
        .filter(|l| l.starts_with(FORGED_LINE_START))
        .collect();
    assert!(
        forged.is_empty(),
        "a `\\n` in a model-authored `method` forged a COLUMN-0 line on {channel}: {forged:?}. \
         `scripts/run-e2e-gate.sh` greps `^\\[WARN\\]` and asserts zero matches, so this turns \
         an unrelated profile red — the planner would be able to fail someone else's gate.\n\
         {both}"
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

    // ONE line carrying BOTH, not two independent `contains` over the whole
    // stream: the property is "the marked fallback line carries the worker's
    // explanation". Searched separately, a producer that emitted the marker on
    // its own line and the report on another would satisfy both — and so would
    // one that let the report break across lines, which is exactly what an
    // un-neutralised `\n` does.
    let marked: Vec<&str> = run
        .stdout
        .lines()
        .filter(|l| l.starts_with(WORKER_FAILED_STDERR_MARKER))
        .collect();
    assert_eq!(
        marked.len(),
        1,
        "expected exactly ONE `{WORKER_FAILED_STDERR_MARKER}` line in the failing test's CAPTURED \
         output. None means `tracing::warn!` swallowed the report as it did before #725 — the \
         shrug that cost #719 a session, leaving only `Protocol(EarlyExit)`. More than one \
         means the report broke across lines.\ngot: {marked:?}\n{both}"
    );
    assert!(
        marked[0].contains(LAST_WORDS),
        "the marked fallback line must CARRY the dying worker's own explanation — that report \
         is the entire point of the line (#725).\ngot: {}\n{both}",
        marked[0]
    );
    assert_the_hostile_method_was_defanged("the captured fallback", &run.stdout, &both);
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
         the captured stream too means the delivery check is not holding, which would \
         double every early-exit report in the daemon's own log.\n{both}"
    );
    // The `tracing` half of the same property. The daemon takes this channel
    // and nothing else, so neutralisation has to hold here too — and this is
    // the only fixture in which the report reaches a subscriber at all.
    assert_the_hostile_method_was_defanged("the `tracing` channel", &run.stderr, &both);
}

#[test]
fn a_subscriber_that_filters_the_event_out_still_gets_the_fallback() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let name = "inner_fixture_early_exit_with_a_filtering_subscriber";
    let run = run_inner_fixture(name);
    assert_one_deliberate_failure(name, &run);
    let both = format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", run.stdout, run.stderr);

    // The `tracing` channel must genuinely be silent. This is the half that
    // makes the assertion below meaningful: if the subscriber HAD recorded the
    // event, a fallback line would be a double-report bug rather than the fix.
    assert!(
        !run.stderr.contains(LAST_WORDS),
        "the fixture's filter must drop the `tracing` event — finding the report on the \
         child's raw stderr means the subscriber recorded it after all, and this suite is no \
         longer testing the #734 state.\n{both}"
    );

    let marked: Vec<&str> = run
        .stdout
        .lines()
        .filter(|l| l.starts_with(WORKER_FAILED_STDERR_MARKER))
        .collect();
    assert_eq!(
        marked.len(),
        1,
        "expected exactly ONE `{WORKER_FAILED_STDERR_MARKER}` line. NONE is #734: a subscriber \
         exists, so the old `has_been_set()` guard suppressed the fallback, while `EnvFilter` \
         dropped the `tracing` event — the report reached nobody at all and nothing said so. \
         An operator hits this by setting a target-scoped `RUST_LOG` in the \
         `kastellan.env.local` overlay while debugging.\ngot: {marked:?}\n{both}"
    );
    assert!(
        marked[0].contains(LAST_WORDS),
        "the fallback line must CARRY the dying worker's own explanation.\ngot: {}\n{both}",
        marked[0]
    );
    // The neutralisation has to hold on this path too — it is a third route to
    // the same line, and a fix that rendered it anywhere but through
    // `format_stderr_fallback` would bypass the guarantee.
    assert_the_hostile_method_was_defanged("the filtered-away fallback", &run.stdout, &both);
}
