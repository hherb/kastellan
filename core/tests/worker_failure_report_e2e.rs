//! #737: a tool worker retired for a reason **other than** `EarlyExit` still
//! reports its last words.
//!
//! `worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead` calls five
//! `ClientError` variants fatal. Before this, `tool_host` reported exactly one
//! of them — `EarlyExit` — and the other four reached a teardown that wrote
//! **nothing on any channel**, not `tracing` and not the #725/#730 stderr
//! fallback. A warm worker that flooded the pipe, or one that answered with
//! bytes that are not a JSON-RPC record, was retired with its captured stderr
//! discarded and no trace anywhere, in the daemon as well as in a test binary.
//! That is strictly wider than the #725/#730 defect, which was at least visible
//! to a daemon with a subscriber.
//!
//! ## What this suite adds, and what it deliberately does not re-prove
//!
//! Its sibling `worker_early_exit_stderr_fallback_e2e` owns the *mechanics* —
//! that `eprintln!` reaches libtest's capture where `writeln!(stderr(), …)`
//! does not, that a binary with a subscriber does not double-report, and why
//! the child's two streams must never be merged. Those are properties of the
//! shared emitter and are unchanged here.
//!
//! What was never tested is the **census**: that a cause other than `EarlyExit`
//! reaches the emitter at all. So this drives one — `Decode` — end to end
//! through the real dispatch path and asserts the marked line appears, names
//! the worker, and describes *that* cause rather than claiming the worker
//! exited.
//!
//! ⚠️ **`Decode` is chosen because it is provokable without a purpose-built
//! binary and because it is the one that most needs distinct wording.** The
//! worker is `/bin/sh` printing a non-JSON line to stdout and then *staying
//! alive*, which is the classic production shape: a Python worker that prints
//! to stdout instead of stderr. The worker has not exited, so a report saying
//! "exited before responding" — the only wording the tree had before #737 —
//! would send a reader looking for a corpse that is not there.
//!
//! ⚠️ **This suite does NOT pin the drain wait, and saying otherwise would be
//! wrong — measured.** An earlier draft of this comment claimed the assertion
//! below on the worker's last words also defends
//! `worker_stderr::collect_tail_after_drain`. It does not: deleting the wait
//! leaves this suite GREEN, because the fixture writes its stderr line before
//! the garbage on stdout and the drainer has invariably picked it up by the
//! time the decode fails. The wait is pinned where the race can be *staged* —
//! `worker_stderr`'s own unit test, which holds the drainer back 30 ms — and
//! that mutant survived every e2e in the tree until it existed
//! [[unreachable-success-path-proves-nothing]].
//!
//! ⚠️ **The `sleep` is load-bearing.** Without it the shell exits as soon as it
//! has printed, and the dispatch races between `Decode` (the line was read
//! first) and `EarlyExit` (the pipe closed first). The assertion on the error
//! *variant* below is what turns that race into a failure rather than a
//! silently vacuous pass.

use std::path::PathBuf;
use std::process::Command;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, ToolHostError, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::env_flag_enabled;
use kastellan_core::worker_stderr::WORKER_FAILED_STDERR_MARKER;
use kastellan_protocol::client::ClientError;
use kastellan_sandbox::{Net, SandboxPolicy};
use kastellan_tests_common::{backend, skip_if_sandbox_unavailable, NoopAuditSink};

/// The shell the fixture worker is. Reachable in both jails and named
/// explicitly in `fs_read`, so the suite needs no build step — what the worker
/// *is* has nothing to do with the property under test.
const FIXTURE_SHELL: &str = "/bin/sh";

/// The non-JSON line the fixture writes to **stdout**, which is what makes the
/// client's decode fail. Deliberately shaped like the real cause: a worker
/// printing a human log line where a JSON-RPC record belongs.
const GARBAGE_ON_STDOUT: &str = "kastellan-test: loading model from cache...";

/// What the fixture writes to **stderr** — the explanation that must survive
/// into the report. Distinctive enough that finding it cannot be chance.
const LAST_WORDS: &str = "kastellan-test: wrote a progress line to stdout by mistake";

/// The method dispatched. Plain: the hostile-method neutralisation is pinned by
/// the sibling suite and by `worker_stderr`'s unit tests, and repeating it here
/// would only make these assertions harder to read.
const METHOD: &str = "fixture.decode";

/// The panic message the inner fixture ends on, so the parent knows it read the
/// *deliberate* failure and not an unrelated one.
const DELIBERATE: &str = "deliberate failure: this fixture exists to be read from its parent";

/// Set by the parent on the child it launches.
///
/// ⚠️ **Load-bearing, not belt-and-braces with `#[ignore]`.** This fixture
/// fails on purpose, and the tree documents `cargo test … -- --ignored` as the
/// way to run the Firecracker tier — which would otherwise surface a red test
/// that reads exactly like a regression.
const FIXTURE_ENV: &str = "KASTELLAN_WORKER_FAILURE_FIXTURE";

/// Dispatch to a worker that answers with garbage and stays alive.
fn dispatch_to_a_worker_that_answers_with_garbage() {
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
            // stderr first so the drainer has it; then the garbage the client
            // will try to decode; then stay alive so the pipe does NOT close
            // and the failure is unambiguously `Decode`.
            let script = format!(
                "echo '{LAST_WORDS}' >&2; echo '{GARBAGE_ON_STDOUT}'; sleep 30"
            );
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
                "decode-fixture",
                METHOD,
                serde_json::json!({}),
            )
            .await;
            let _ = worker.kill();
            let _ = worker.close();
            out
        })
        .await
        .expect("the spawned dispatch task must not panic")
    });

    // The VARIANT, not just `is_err()`. A wall-clock kill, a spawn refusal or an
    // `EarlyExit` from the shell exiting too soon would all satisfy `is_err()`
    // while exercising a different arm — and the parent would then report "the
    // report did not appear", blaming the producer from two process boundaries
    // away. This is also what keeps the `sleep` above honest.
    let err = result.expect_err("a garbage answer must fail the dispatch");
    assert!(
        matches!(err, ToolHostError::Protocol(ClientError::Decode(_))),
        "the fixture must provoke a DECODE failure specifically — it is the arm #737 adds and \
         the one whose wording this suite checks. Got: {err:?}"
    );
}

/// `true` when this process was launched by [`run_inner_fixture`].
///
/// Routes through the tree's one implementation of the `1|true|yes|on` dialect
/// rather than re-spelling it.
fn is_the_child() -> bool {
    env_flag_enabled(std::env::var(FIXTURE_ENV).ok())
}

/// Inner fixture: no subscriber, so the stderr fallback is the only channel.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_decode_failure_without_a_subscriber() {
    if !is_the_child() || skip_if_sandbox_unavailable() {
        return;
    }
    dispatch_to_a_worker_that_answers_with_garbage();
    panic!("{DELIBERATE}");
}

/// What the parent learns from the child run. The two streams stay **apart**;
/// the sibling suite's module doc explains why merging them voids the claim.
struct ChildRun {
    succeeded: bool,
    /// Where libtest re-prints a failing test's *captured* output.
    stdout: String,
    /// The raw fd — where anything that bypassed the capture lands.
    stderr: String,
}

impl ChildRun {
    fn both(&self) -> String {
        format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", self.stdout, self.stderr)
    }
}

/// Re-run this test binary with `--exact <name>`, returning its two streams.
///
/// Deliberately **without** `--nocapture`: the claim is about what libtest
/// prints for a *failing* test, and `--nocapture` would disable the capture
/// entirely and prove nothing about it.
fn run_inner_fixture(name: &str) -> ChildRun {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args(["--exact", name, "--ignored", "--test-threads", "1"])
        .env(FIXTURE_ENV, "1")
        // libtest reads this as `!= "0"`, so even an empty value disables the
        // capture — which would push the report onto the raw fd and turn the
        // stdout assertions below into a false red.
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
fn a_worker_retired_for_a_non_early_exit_reason_still_reports_its_last_words() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let name = "inner_fixture_decode_failure_without_a_subscriber";
    let run = run_inner_fixture(name);
    let both = run.both();

    // The positive control. Without it a typo in the fixture name gives
    // `0 passed; 0 failed; N filtered out` and **exit 0**, and every assertion
    // below would be checking a string that was never produced.
    assert!(
        !run.succeeded,
        "the child must FAIL — `{name}` panics on purpose. A passing child means the filter \
         matched nothing (`--exact` on a misspelled name exits 0), or the fixture skipped for \
         want of a sandbox in the child while the parent's own check passed.\n{both}"
    );
    // The FULL libtest phrase, not `contains("1 failed")`: that substring also
    // appears in "11 failed" and — the one that matters — in "1 passed; 1 failed".
    assert!(
        run.stdout.contains("test result: FAILED. 0 passed; 1 failed;"),
        "exactly one test must have run and failed in the child.\n{both}"
    );
    assert!(
        run.stdout.contains(DELIBERATE),
        "the child must have reached the fixture's own panic, not died earlier.\n{both}"
    );

    // ONE line carrying all of it, not three `contains` over the whole stream:
    // searched separately, a producer that emitted the marker on one line and
    // the report on another would satisfy all three.
    let marked: Vec<&str> =
        run.stdout.lines().filter(|l| l.starts_with(WORKER_FAILED_STDERR_MARKER)).collect();
    assert_eq!(
        marked.len(),
        1,
        "expected exactly ONE `{WORKER_FAILED_STDERR_MARKER}` line in the failing test's \
         CAPTURED output. NONE is #737 itself: before it, only `EarlyExit` was ever reported \
         and a decode failure retired the worker in total silence.\ngot: {marked:?}\n{both}"
    );

    assert!(
        marked[0].contains(LAST_WORDS),
        "the marked line must CARRY the worker's own explanation — that is the entire point of \
         the line.\ngot: {}\n{both}",
        marked[0]
    );
    assert!(
        marked[0].contains(FIXTURE_SHELL),
        "the marked line must name WHICH worker.\ngot: {}\n{both}",
        marked[0]
    );
    assert!(
        marked[0].contains(METHOD),
        "the marked line must name WHICH call.\ngot: {}\n{both}",
        marked[0]
    );

    // The cause-specific half. A generic report would satisfy every assertion
    // above while telling the reader the opposite of what happened.
    assert!(
        !marked[0].contains("exited"),
        "this worker did NOT exit — it answered with garbage and is still running. The \
         pre-#737 wording (\"exited before responding\") would send a reader looking for a \
         corpse that is not there.\ngot: {}\n{both}",
        marked[0]
    );
    assert!(
        marked[0].contains("not a JSON-RPC response"),
        "the line must say what was actually wrong with the answer, which is the whole reason \
         #737 gives each cause its own wording.\ngot: {}\n{both}",
        marked[0]
    );
}
