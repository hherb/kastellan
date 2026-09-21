//! #730, the layer #725 missed: a **persistent** worker's death report must
//! reach a failing test, not only a daemon that installed a `tracing` subscriber.
//!
//! #666 routed a dead worker's own stderr into `tracing::warn!`, and #725 gave
//! the **tool**-worker early-exit path a second channel for the binaries that
//! install no subscriber. The persistent path kept its hand-rolled
//! `tracing::warn!` and inherited none of it. That path is how the **Matrix**
//! and **email** channel workers run, so in a test binary those died in
//! silence: the tail was captured, the report was rendered by
//! `format_death_report`, and then it was discarded because nothing was
//! listening.
//!
//! ## Why this suite re-executes itself
//!
//! The property is about what an operator **sees when a test fails**, and a
//! test cannot observe its own failure output. Three things have to be true at
//! once and none is visible from inside the test that triggers them:
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
//! the two before asserting — which the #725 suite did in its first draft —
//! makes `contains(LAST_WORDS)` true either way and silently stops testing the
//! one claim the suite exists to make.
//!
//! ## Why the fixture is hermetic
//!
//! Unlike its #725 sibling this spawns **no sandboxed worker**, so it has no
//! `[SKIP]` path and runs on every host. `PersistentTransport` is a public
//! trait and `PersistentWorker` drives it through the exact production code
//! path (`worker_lifecycle::persistent`'s driver thread), so a fake transport
//! exercises the emit site under test without making the run conditional on a
//! jail. A skip here would be the worse trade: this suite's whole subject is a
//! report that goes missing, and "[SKIP]" is what that looks like.
//!
//! ⚠️ **One child process per fixture.** `tracing::dispatcher::has_been_set()`
//! is a process-global `AtomicBool` that `with_default` sets as well as
//! `set_global_default`, and which is never cleared — so one subscriber
//! anywhere in a binary pins the branch for every later test in it. Running
//! both fixtures in one process would make the second answer for the first.
//!
//! ⚠️ **Every child run asserts the full `0 passed; 1 failed;` phrase.** A
//! libtest name filter exits 0 when it matches nothing, so "the child's output
//! lacks the phrase" and "the child ran no tests" would otherwise be the same
//! observation — the `--exact`-matches-nothing trap that has reported a whole
//! mutation batch as surviving against zero tests.

use std::process::Command;
use std::time::Duration;

use kastellan_core::worker_lifecycle::force_route::env_flag_enabled;
use kastellan_core::worker_lifecycle::{
    PersistentFactory, PersistentHandle, PersistentTransport, PersistentWorker, RestartBackoff,
};
use kastellan_core::worker_stderr::{EARLY_EXIT_STDERR_MARKER, WORKER_DEATH_STDERR_MARKER};

/// The supervisor label the fixture's worker runs under — what an operator
/// reads first to know *which* worker died. Shaped like the two real ones
/// (`matrix`, `email`) and distinctive enough that finding it cannot be chance.
///
/// ⚠️ **Hostile on purpose, and the ONLY way to reach the label's own
/// neutralisation** (#735 review). `emit_persistent_death_report` neutralises
/// `label` separately *and then* neutralises the whole folded line, so the
/// second pass masks the first for the **message** — dropping the label's own
/// `neutralise_controls` changes nothing a message assertion can see. What it
/// still protects is the `%label` **tracing field**, which carries the raw
/// value. With a clean literal here that mutant survived the entire suite; with
/// this, the with-subscriber fixture's `tracing`-channel check kills it.
const LABEL: &str = "kastellan-test-730\u{1b}[31m\n[WARN] FORGED-LABEL-LINE";

/// The part of [`LABEL`] that survives neutralisation — what a rendered line
/// must actually contain. Asserting on `LABEL` itself would fail by
/// construction, since its control characters become spaces.
const LABEL_TEXT: &str = "kastellan-test-730";

/// The forgery attempt inside [`LABEL`], for the same positive-control reason
/// [`FORGED_TEXT`] exists: "no forged line" must not be satisfiable by the
/// label never reaching the output at all.
const FORGED_LABEL_TEXT: &str = "FORGED-LABEL-LINE";

/// The dying worker's own explanation, as `ClientTransport::death_report` would
/// have rendered it from a real worker's retained stderr tail.
const LAST_WORDS: &str = "worker exited (exit status: 1); recent stderr: \
                          kastellan-test: matrix sync failed, refusing to continue";

/// What the fixture's transport answers when asked why it died: [`LAST_WORDS`]
/// plus the two control characters that fail differently.
///
/// ⚠️ **Defence-in-depth at a public trait boundary, not a live hole.** Nothing
/// reaching `emit_persistent_death_report` in production today is
/// attacker-controlled — `ClientTransport::death_report` renders an
/// `ExitStatus` plus a tail `push_trimmed` already stripped, and every `label`
/// in the tree is a literal. But `PersistentTransport::death_report` is a
/// **public trait method**, any implementor is a producer of this string, and
/// `egress::persistent_net` already delegates through it. This fixture is such
/// an implementor, which is exactly why it is the right place to pin the
/// property end to end.
///
/// - `\u{1b}[31m` — an ANSI sequence executing in the terminal of whoever reads
///   the failing test.
/// - a `\n` followed by [`FORGED_LINE_START`] — a forged **column-0** line.
///   `scripts/run-e2e-gate.sh` greps `^\[WARN\]` and asserts zero matches, so
///   this is the shape that turns someone else's green gate red.
const DEATH_REPORT: &str = "worker exited (exit status: 1); recent stderr: \
                            kastellan-test: matrix sync failed, refusing to continue\
                            \u{1b}[31m\n[WARN] kastellan-test: FORGED-GATE-LINE";

/// The forgery attempt inside [`DEATH_REPORT`].
///
/// ⚠️ **It must survive as text and must never begin a line.**
/// `neutralise_controls` maps the class to `' '`, so the correct outcome is
/// this phrase sitting mid-line behind a space — not the phrase disappearing.
/// Asserting its absence would pass just as well if the report never reached
/// the line at all, which is why the assertions below check *position* and pair
/// it with [`FORGED_TEXT`] as the positive control.
const FORGED_LINE_START: &str = "[WARN] kastellan-test: FORGED-GATE-LINE";

/// The part of [`FORGED_LINE_START`] that no neutralisation touches. Its
/// presence proves the hostile report actually reached the rendered line, so
/// "no forged line" cannot be satisfied by the report going missing.
const FORGED_TEXT: &str = "FORGED-GATE-LINE";

/// The panic message each inner fixture ends on. Its presence in the child's
/// output is how the parent knows it read the *deliberate* failure and not an
/// unrelated one.
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
const FIXTURE_ENV: &str = "KASTELLAN_PERSISTENT_DEATH_FIXTURE";

/// The first transport the fixture's factory hands out: it fails every call and
/// explains itself when the driver asks why.
struct DyingTransport;

/// [`DyingTransport`]'s own error text. Asserted on, so a fixture that stopped
/// reaching the transport under test (a call answered by the driver's
/// "persistent worker is restarting" arm instead) fails *here* rather than
/// surfacing as "the report never appeared" from two process boundaries away.
const SIMULATED_DEATH: &str = "simulated worker death";

/// [`SilentlyDyingTransport`]'s error text.
///
/// ⚠️ **Neither of these two may CONTAIN the other**, because
/// [`kill_one_transport`] matches with `contains`. The first draft of this used
/// `"simulated worker death with no report"`, which has [`SIMULATED_DEATH`] as a
/// prefix — so a call answered by the *silent* transport would satisfy a wait
/// for the *reporting* one, and the fixture could skip a death while looking
/// like it drove both. Same shape as `contains("1 failed")` also matching
/// `"1 passed; 1 failed"`. The assertion below pins it rather than trusting the
/// next editor to notice.
const SILENT_DEATH: &str = "fixture transport dying WITHOUT a report";

impl PersistentTransport for DyingTransport {
    fn call(&mut self, _m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("{SIMULATED_DEATH}")
    }

    fn death_report(&mut self) -> Option<String> {
        Some(DEATH_REPORT.to_string())
    }
}

/// The transport the supervisor respawns after [`DyingTransport`]: it dies the
/// same way but answers `death_report()` with **`None`**.
///
/// ⚠️ **This is the arm that had no coverage at all** (#735 review). The emit
/// site is `if let Some(r) = transport.death_report()`, and with only a
/// `Some`-returning transport in the fixture the `None` branch was never
/// executed — so replacing the `if let` with
/// `death_report().unwrap_or_default()` passed the whole suite while making
/// production emit `[worker-death] persistent worker X died: ` with an empty
/// report. Driving a second death through this transport and then asserting
/// **exactly one** marked line kills that mutant.
struct SilentlyDyingTransport;

impl PersistentTransport for SilentlyDyingTransport {
    fn call(&mut self, _m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("{SILENT_DEATH}")
    }
    // `death_report` deliberately NOT overridden: the `PersistentTransport`
    // default returns `None`, which is exactly the production shape this arm
    // stands in for (any implementor that does not override it, and
    // `ClientTransport` whenever stderr was never piped).
}

/// The replacement the supervisor respawns last. Never called — the fixture
/// shuts down once both deaths are done — and it exists only so the final
/// respawn **succeeds**.
///
/// ⚠️ **Not because a failing factory would be "noise on the counted stream"**,
/// which is what this said until the #735 review: a bare `tracing::warn!` with
/// no subscriber emits nothing at all, and the parent counts marked lines and
/// `LAST_WORDS`, neither of which a `respawn failed` warning contains. The real
/// reason is plainer — the fixture asserts on a *settled* state, and an
/// always-failing factory leaves the driver spinning its back-off loop while
/// the assertions run.
struct SurvivingTransport;

impl PersistentTransport for SurvivingTransport {
    fn call(&mut self, _m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("the fixture never calls the respawned transport")
    }
}

/// Back off in milliseconds, not the default second, so the driver reaches its
/// respawn promptly. The fixture's timing does not affect the property; this
/// only keeps the child quick.
fn fast_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_millis(1),
        factor_num: 1,
        factor_den: 1,
        cap: Duration::from_millis(1),
    }
}

/// Drive a persistent worker through exactly one death, then shut down.
///
/// ⚠️ **`shutdown()` JOINS the driver thread**, and the driver emits the death
/// report *after* replying to the in-flight caller (`persistent.rs` replies
/// first so a panicking `death_report` cannot strand the caller). So returning
/// from `h.call(…)` does **not** mean the report has been emitted — only the
/// join does. Asserting on the output without this would be a race that passes
/// on a quiet machine.
fn drive_a_persistent_worker_to_its_death() {
    let mut spawns = 0usize;
    let factory: PersistentFactory = Box::new(move || {
        spawns += 1;
        Ok(match spawns {
            1 => Box::new(DyingTransport) as Box<dyn PersistentTransport>,
            2 => Box::new(SilentlyDyingTransport) as Box<dyn PersistentTransport>,
            _ => Box::new(SurvivingTransport) as Box<dyn PersistentTransport>,
        })
    });
    let h: PersistentHandle = PersistentWorker::spawn_with_backoff(LABEL, factory, fast_backoff())
        .expect("spawn the fixture's persistent worker");

    // Death 1: a transport that DOES answer `death_report()`. One marked line.
    kill_one_transport(&h, SIMULATED_DEATH);
    // Death 2: a transport whose `death_report()` is `None`. No marked line —
    // and the parent's `marked.len() == 1` is what turns that into an assertion.
    kill_one_transport(&h, SILENT_DEATH);

    h.shutdown();
}

/// Drive exactly one call into the currently-live transport and assert it died
/// the way `expected` says.
///
/// ⚠️ **Retries, because "the call failed" is not "the transport failed."** A
/// call landing while the driver is between transports is answered by its
/// `"persistent worker is restarting"` arm without ever reaching
/// `death_report()`. A bare `is_err()` would accept that and the fixture would
/// silently stop exercising the emit site. Looping until the *transport's own*
/// error comes back makes the intended path the only way to pass.
fn kill_one_transport(h: &PersistentHandle, expected: &str) {
    assert!(
        !SILENT_DEATH.contains(SIMULATED_DEATH) && !SIMULATED_DEATH.contains(SILENT_DEATH),
        "the two transports' error texts must not contain one another — this helper matches with \
         `contains`, so a substring pair lets a wait for one death be satisfied by the other and \
         the fixture silently skips a death: {SIMULATED_DEATH:?} vs {SILENT_DEATH:?}"
    );
    for _ in 0..200 {
        let err = match h.call("ping", serde_json::json!({})) {
            Ok(v) => panic!("the fixture's transports must never answer a call; got {v:?}"),
            Err(e) => format!("{e:#}"),
        };
        if err.contains(expected) {
            return;
        }
        assert!(
            err.contains("restarting"),
            "unexpected error from the fixture's worker: {err:?} (wanted {expected:?}, or the \
             driver's transient `restarting` answer)"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("the fixture never reached a transport answering {expected:?} — the emit site under \
            test was not exercised and every assertion downstream would be vacuous");
}

/// `true` when this process was launched by [`run_inner_fixture`].
///
/// ⚠️ Presence is **not** enough: these fixtures fail on purpose, so an operator
/// who exported `KASTELLAN_PERSISTENT_DEATH_FIXTURE=0` believing that disabled
/// them would get two red tests from the documented `cargo test … -- --ignored`
/// recipe, reading exactly like a regression.
///
/// ⚠️ **Calls the tree's one implementation of the `1|true|yes|on` dialect
/// rather than re-spelling it.** This said it "honours the dialect rather than
/// inventing a second one" while inventing a second one — the drift shape
/// `worker_stderr::report`'s own module doc invokes to justify its shared
/// renderer. `env_flag_enabled` is the copy `tests_common::require`,
/// `gliner_e2e` and the micro-VM harness already route through.
fn is_the_child() -> bool {
    env_flag_enabled(std::env::var(FIXTURE_ENV).ok())
}

/// Inner fixture: no subscriber, so the stderr fallback is the only channel.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_persistent_death_without_a_subscriber() {
    if !is_the_child() {
        return;
    }
    drive_a_persistent_worker_to_its_death();
    panic!("{DELIBERATE}");
}

/// Inner fixture: a subscriber **is** installed, so the report must arrive
/// exactly once — through `tracing` — and the fallback must stay quiet.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_persistent_death_with_a_subscriber() {
    if !is_the_child() {
        return;
    }
    // Global, not scoped: the report is written from the supervisor's own
    // driver thread, so a thread-local subscriber would not be in scope where
    // it is written and the fixture would prove the opposite of what it claims.
    //
    // `with_writer(std::io::stderr)` is the raw handle, which libtest's capture
    // does NOT intercept — so this lands on the child's real stderr, which is
    // exactly how the parent tells the two channels apart.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    // `expect`, not `let _`: an `Err` here means a subscriber was ALREADY
    // installed, which is precisely the hazard this file documents
    // (`has_been_set()` is never cleared and `with_default` sets it too).
    // Swallowing it would leave the parent asserting "the report must arrive on
    // stderr exactly once" against a fixture whose own setup failed — and if
    // that pre-existing subscriber happened to write WARN to stderr, the count
    // would be 1 and the test would PASS having proven nothing.
    tracing::subscriber::set_global_default(subscriber)
        .expect("install the fixture's global subscriber; a prior install would void this test");

    drive_a_persistent_worker_to_its_death();
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

impl ChildRun {
    /// Both streams, labelled, for an assertion message. Only ever used in
    /// failure output — never asserted against, which is the point.
    fn both(&self) -> String {
        format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", self.stdout, self.stderr)
    }
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
        // ⚠️ libtest reads this as `!= "0"`, so even an empty value disables the
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

/// Assert the child ran exactly the one fixture and that it failed.
///
/// The positive control. Without it, a typo in the fixture name gives
/// `0 passed; 0 failed; N filtered out` and **exit 0**, and every assertion
/// about the output below would be checking a string that was never produced.
/// It also catches a fixture that no-op'd because [`FIXTURE_ENV`] failed to
/// reach it, and one that died before its own panic.
fn assert_one_deliberate_failure(name: &str, run: &ChildRun) {
    let both = run.both();
    assert!(
        !run.succeeded,
        "the child must FAIL — `{name}` panics on purpose. A passing child means the filter \
         matched nothing (`--exact` on a misspelled name exits 0) or the fixture no-op'd.\n{both}"
    );
    // The FULL libtest phrase, not `contains("1 failed")`: that substring is
    // also in "11 failed", "21 failed" and — the one that matters — in
    // "1 passed; 1 failed". Two tests running in one child would defeat the
    // one-child-per-fixture rule the module doc explains, because
    // `has_been_set()` is a never-cleared process global and the first fixture
    // would pin the branch for the second.
    assert!(
        run.stdout.contains("test result: FAILED. 0 passed; 1 failed;"),
        "exactly one test must have run and failed in the child; anything else means the name \
         filter did not select `{name}`, or more than one test ran and they can no longer \
         answer for themselves.\n{both}"
    );
    assert!(
        run.stdout.contains(DELIBERATE),
        "the child must have reached the fixture's own panic, not died earlier for an \
         unrelated reason.\n{both}"
    );
}

/// Assert the channel named by `channel` carries the hostile report's text but
/// neither of its control characters' effects.
///
/// Both channels are checked, by their respective parents, because a mutant
/// that drops a neutralisation has to be caught wherever the line actually
/// lands.
///
/// ⚠️ **Three passes, not "once", and the difference is what the mutation
/// measurement turns on.** `emit_persistent_death_report` neutralises the
/// `label`, then the whole folded line — and the fallback branch neutralises a
/// *third* time inside the shared `format_stderr_fallback`. So the `tracing`
/// half gets one pass and the stderr half gets two, which is exactly why a
/// mutant in the shared renderer survives **both** e2es and dies only in
/// `worker_stderr::report`'s unit tests. Saying "once" here contradicted that
/// measurement, recorded in those tests' own comments.
///
/// The no-subscriber run proves the property for the `eprintln!`
/// fallback; the with-subscriber run proves it for `tracing`, whose `fmt` layer
/// writes the message through `Debug for Arguments` (i.e. Display, no
/// escaping), so an un-neutralised `\n` really does break the line there.
fn assert_the_hostile_report_was_defanged(channel: &str, stream: &str, both: &str) {
    assert!(
        stream.contains(FORGED_TEXT),
        "POSITIVE CONTROL: the hostile report never reached {channel} at all, so the checks \
         below would pass vacuously. Either the report stopped being interpolated into the \
         line, or the fixture's worker never died.\n{both}"
    );
    assert!(
        !stream.contains('\u{1b}'),
        "an ESC from a `PersistentTransport` implementor's death report reached {channel} \
         unneutralised — it is an ANSI sequence executing in the terminal of whoever reads \
         this failure.\n{both}"
    );
    let forged: Vec<&str> = stream.lines().filter(|l| l.starts_with(FORGED_LINE_START)).collect();
    assert!(
        forged.is_empty(),
        "a `\\n` in a death report forged a COLUMN-0 line on {channel}: {forged:?}. \
         `scripts/run-e2e-gate.sh` greps `^\\[WARN\\]` and asserts zero matches, so this turns \
         an unrelated profile red.\n{both}"
    );
}

#[test]
fn a_failing_test_with_no_subscriber_shows_the_dead_workers_report() {
    let name = "inner_fixture_persistent_death_without_a_subscriber";
    let run = run_inner_fixture(name);
    assert_one_deliberate_failure(name, &run);
    let both = run.both();

    // ONE line carrying ALL of it, not three independent `contains` over the
    // whole stream: the property is "the marked fallback line carries which
    // worker died and why". Searched separately, a producer that emitted the
    // marker on its own line and the report on another would satisfy all three
    // — and so would one that let the report break across lines, which is
    // exactly what an un-neutralised `\n` does.
    let marked: Vec<&str> =
        run.stdout.lines().filter(|l| l.starts_with(WORKER_DEATH_STDERR_MARKER)).collect();
    assert_eq!(
        marked.len(),
        1,
        "expected exactly ONE `{WORKER_DEATH_STDERR_MARKER}` line in the failing test's \
         CAPTURED output. None means `tracing::warn!` swallowed the report as it did before \
         #730 — the shrug that leaves Matrix and email workers dying in silence. More than \
         one means the line broke.\ngot: {marked:?}\n{both}"
    );
    assert!(
        marked[0].contains(LAST_WORDS),
        "the marked fallback line must CARRY the dead worker's own explanation — that report \
         is the entire point of the line (#730).\ngot: {}\n{both}",
        marked[0]
    );
    assert!(
        marked[0].contains(LABEL_TEXT),
        "the marked fallback line must name WHICH worker died. The fallback channel carries no \
         `tracing` fields, so a label kept only in `%label` leaves an operator reading \
         `persistent worker died` with `matrix` and `email` both live.\ngot: {}\n{both}",
        marked[0]
    );
    assert_the_hostile_report_was_defanged("the captured fallback", &run.stdout, &both);
    assert!(
        run.stdout.contains(FORGED_LABEL_TEXT),
        "POSITIVE CONTROL for the hostile LABEL: it never reached the fallback at all, so the \
         defanging checks above say nothing about the label's own neutralisation.\n{both}"
    );

    // The two events must stay distinguishable. Reusing the early-exit marker
    // would make a gate log unable to say whether a tool worker failed one call
    // or a long-lived worker stopped and is being respawned.
    assert!(
        !run.stdout.contains(EARLY_EXIT_STDERR_MARKER),
        "a persistent-worker death must NOT be reported under the early-exit marker — they \
         point at different places to look.\n{both}"
    );
    assert!(
        !run.stderr.contains(LAST_WORDS),
        "the report must go through `eprintln!`, which libtest's capture intercepts — NOT to \
         the raw stderr fd. Finding it on the child's real stderr means the producer used \
         `writeln!(std::io::stderr(), …)` or equivalent, and under a normal (capturing) run it \
         would never appear beneath the failing test that needs it.\n{both}"
    );
}

#[test]
fn a_binary_that_installed_a_subscriber_does_not_get_the_report_twice() {
    let name = "inner_fixture_persistent_death_with_a_subscriber";
    let run = run_inner_fixture(name);
    assert_one_deliberate_failure(name, &run);
    let both = run.both();

    assert_eq!(
        run.stderr.matches(LAST_WORDS).count(),
        1,
        "the fixture's subscriber writes to the raw stderr handle, so with a subscriber \
         installed the report must arrive there exactly once.\n{both}"
    );
    assert!(
        !run.stdout.contains(LAST_WORDS),
        "the fallback must stay QUIET when a subscriber is installed. Seeing the report in the \
         captured stream too means the `has_been_set()` guard is not holding, which would \
         double every death report in the daemon's own log.\n{both}"
    );
    // The `tracing` half of the same property. The daemon takes this channel
    // and nothing else, so neutralisation has to hold here too — and this is
    // the only fixture in which the report reaches a subscriber at all.
    //
    // ⚠️ This is also the ONLY place the `label`'s own neutralisation is
    // reachable: the message text is neutralised a second time as a whole,
    // which masks it, but the `%label` FIELD below carries the raw value. See
    // `LABEL`'s doc.
    assert_the_hostile_report_was_defanged("the `tracing` channel", &run.stderr, &both);

    // The `%label` structured field must still be emitted. The daemon's
    // subscriber is `fmt().json()`, so this field is what makes a death
    // queryable by worker — a property the emitter's doc argues for and that
    // nothing asserted until the #735 review: the label is ALSO folded into the
    // message text, so dropping `%label` changed nothing any other check saw.
    assert!(
        run.stderr.contains("label="),
        "the `tracing` record must carry the `%label` FIELD, not only the label folded into the \
         message text — the daemon's `fmt().json()` subscriber is what makes a worker death \
         queryable by worker.\n{both}"
    );
    assert!(
        run.stderr.contains(FORGED_LABEL_TEXT),
        "POSITIVE CONTROL for the hostile LABEL on the `tracing` channel: it never arrived, so \
         the defanging checks above say nothing about the label's own neutralisation.\n{both}"
    );
}
