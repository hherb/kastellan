//! One REQUIRE-knob contract, shared by every gated end-to-end tier.
//!
//! # The class of bug this exists to retire
//!
//! A gated tier skips when its fixture is absent — a Postgres install, a
//! 1.3 GB model, a micro-VM image. That is correct on an unstaged host and
//! wrong on a staged one, because **a skip-as-pass and a real pass report the
//! same test count**. The tree has paid for this repeatedly:
//!
//! * the gliner-relex `.venv` on the DGX was a copy of the Mac's, so
//!   `bin/python` named a path that cannot exist on Linux — four tests read as
//!   passing while containing nothing, for months ([#651]);
//! * a macOS container image sat 69 days stale behind eight green e2es
//!   ([#684] + [#687] — #684 is the coverage gap, #687 the freshness gate, and
//!   the 69 days were measured while closing both, so neither number appears in
//!   #684 alone);
//! * `guard_tier_e2e`'s `bootstrap()` returns `None` on any missing
//!   precondition, so the guard tier's only end-to-end coverage of the stored
//!   audit row can report a silent PASS ([#622]);
//! * every Postgres-gated suite reports green on a host with no cluster, and a
//!   sweep count of +61 looks identical either way ([#714]).
//!
//! Each was fixed on its own tier, in its own way, and the class kept
//! regenerating. This module is the one contract instead:
//!
//! > **Every gate needs a REQUIRE knob *and* a positive control that fails
//! > when zero tests ran.**
//!
//! [`RequireKnob`] is the first half. The second half is
//! [`RequireKnob::announce`] plus `scripts/run-e2e-gate.sh`, which asserts a
//! minimum `[E2E]` count over the run's output — because the knob alone is
//! blind to the two failures that leave no trace inside a test body: the test
//! being **filtered out** by a name selector, and the test being **renamed out
//! of collection**. `cargo test` exits 0 for both ([#664]).
//!
//! # Why a type rather than a free function per tier
//!
//! The machinery was written once for gliner-relex ([#653]) as free functions
//! closing over one `const REQUIRE_ENV`. A second tier could not reuse it
//! without either taking gliner's panic text or copying the whole cascade —
//! and a copied guard shares the original's blind spots while diverging from
//! its fixes, which is the failure mode `sha256_hex`-twice ([#696]) and the
//! five hand-written truncation walks ([#591]) are both instances of.
//!
//! Naming the knob as **data** makes a new tier one `const`:
//!
//! ```no_run
//! use kastellan_tests_common::require::RequireKnob;
//!
//! const MY_TIER: RequireKnob = RequireKnob::new(
//!     "KASTELLAN_MY_REQUIRE_E2E",
//!     "my-tier",
//! );
//!
//! fn fixture() -> Option<u32> {
//!     let action = MY_TIER.action();
//!     if !std::path::Path::new("/some/fixture").exists() {
//!         // Skips, or panics naming the knob, according to the operator's flag.
//!         return MY_TIER.report_unmet(action, "the fixture is not staged");
//!     }
//!     MY_TIER.announce(action, "fixture at /some/fixture");
//!     Some(42)
//! }
//! ```
//!
//! [#591]: https://github.com/hherb/kastellan/issues/591
//! [#622]: https://github.com/hherb/kastellan/issues/622
//! [#651]: https://github.com/hherb/kastellan/pull/651
//! [#653]: https://github.com/hherb/kastellan/issues/653
//! [#664]: https://github.com/hherb/kastellan/issues/664
//! [#684]: https://github.com/hherb/kastellan/issues/684
//! [#687]: https://github.com/hherb/kastellan/issues/687
//! [#696]: https://github.com/hherb/kastellan/issues/696
//! [#714]: https://github.com/hherb/kastellan/issues/714

use kastellan_core::worker_lifecycle::force_route::env_flag_enabled;

use crate::skip::{e2e_line, one_line, skip_line, warn_line};

/// What an unmet precondition means for *this* run.
///
/// The default is [`UnmetAction::Skip`], which is what keeps a plain
/// `cargo test` green on a host that has never staged the fixture.
/// A truthy REQUIRE knob flips it, and that flip is the whole point:
/// **a skip nobody can turn into a failure cannot detect a dead fixture.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmetAction {
    /// Print `[SKIP] <reason>` and let the calling test return green.
    Skip,
    /// Panic naming the unmet precondition — the operator asked for a real run.
    Fail,
}

/// The spellings that mean "deliberately off" rather than "typo".
const FALSEY_SPELLINGS: [&str; 4] = ["0", "false", "no", "off"];

/// Pure: does this REQUIRE value demand a real run?
///
/// Routed through the one project flag dialect (`1|true|yes|on`, trimmed,
/// case-insensitive) rather than a strict `Some("1")`, because the strict form
/// is exactly the skew [#654] was filed about — the fixtures spoke a different
/// dialect than production, so an operator who wrote `=true` got a silent skip.
///
/// [#654]: https://github.com/hherb/kastellan/issues/654
pub fn unmet_action(require_flag: Option<String>) -> UnmetAction {
    if env_flag_enabled(require_flag) {
        UnmetAction::Fail
    } else {
        UnmetAction::Skip
    }
}

/// Warn when `value` is neither truthy nor a recognised opt-out.
///
/// The knob exists to abolish silent skips, so it must not silently no-op on
/// itself: `…_REQUIRE_E2E=y` (or `=2`, or `=enabled`) is operator error, and
/// treating it as "unset" hands back exactly the green run the operator was
/// trying to rule out. It cannot be a hard failure — `0`/`off` must keep
/// working as an opt-out — so it is a warning, on the same stream as the
/// `[SKIP]` lines it is about.
///
/// Pure in its output sink so a unit test can read the bytes back without
/// emitting to a real run's stderr.
pub fn warn_if_out_of_dialect(var: &str, value: Option<&str>, out: &mut dyn std::io::Write) {
    let Some(observed) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return;
    };
    if FALSEY_SPELLINGS.contains(&observed.to_ascii_lowercase().as_str()) {
        return;
    }
    let _ = write!(
        out,
        "{}",
        warn_line(&format!(
            "{var}={observed:?} is not in the flag dialect (1|true|yes|on) \
             — treating it as unset, so skips will NOT become failures"
        ))
    );
}

/// One tier's REQUIRE knob: the env var that turns its skips into failures,
/// and the phrase naming what a demanded run of it *is*.
///
/// Both fields are load-bearing in the panic message. The **var** so an
/// operator reads the failure as their own demand rather than as a regression;
/// the **tier** so a workspace sweep that trips several knobs says which one.
/// A single shared panic string would say neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequireKnob {
    env: &'static str,
    tier: &'static str,
}

impl RequireKnob {
    /// Name a tier's knob. `env` is the environment variable; `tier` is the
    /// phrase that reads naturally in "a real **`<tier>`** end-to-end run".
    pub const fn new(env: &'static str, tier: &'static str) -> Self {
        Self { env, tier }
    }

    /// The environment variable this knob reads.
    ///
    /// Public so a suite's own error text and a gate script can name the same
    /// string the panic does, rather than a second literal that drifts.
    pub const fn env(&self) -> &'static str {
        self.env
    }

    /// The phrase naming what a demanded run of this tier is.
    pub const fn tier(&self) -> &'static str {
        self.tier
    }

    /// Read the knob from the process environment.
    ///
    /// The only impure step in the decision, kept separate from
    /// [`unmet_action`] so the rule *and* the panic path can be unit-tested
    /// without mutating process-wide environment under `env_lock`.
    pub fn action(&self) -> UnmetAction {
        // The chokepoint for #742's panic hook. Every gated tier reaches this
        // to decide whether to skip, so installing here covers every suite that
        // can appear in a gate profile — by construction, rather than by ~30
        // `tests/*.rs` files each remembering an `install()` call. Idempotent;
        // see `panic_hook::install_once` for why it lives here and what it
        // deliberately does NOT cover.
        crate::panic_hook::install_once();
        self.action_reporting_to(self.raw(), &mut std::io::stderr())
    }

    /// This knob's raw value, with a non-UTF-8 setting preserved rather than
    /// discarded.
    ///
    /// `std::env::var(..).ok()` maps [`std::env::VarError::NotUnicode`] to
    /// `None`, which is indistinguishable from unset — so a knob set to a
    /// non-UTF-8 value would skip **and** emit no `[WARN]`, because
    /// [`warn_if_out_of_dialect`] returns early on `None`. A set-but-unhonoured
    /// knob that leaves no trace at all is the one outcome this type exists to
    /// abolish, so the lossy rendering is carried through to the dialect check,
    /// where it is out of dialect and therefore warns.
    fn raw(&self) -> Option<String> {
        match std::env::var(self.env) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(raw)) => Some(raw.to_string_lossy().into_owned()),
        }
    }

    /// [`RequireKnob::action`] with the value supplied and the dialect warning
    /// written to `out`.
    ///
    /// Exists so a test can prove the warning is **emitted** on the skew path.
    /// Asserting on [`warn_if_out_of_dialect`] alone proves only that the
    /// renderer is correct and leaves the call deletable, with the suite still
    /// green — and a deleted dialect warning restores the exact silent skip
    /// #654 was filed about.
    pub fn action_reporting_to(
        &self,
        raw: Option<String>,
        out: &mut dyn std::io::Write,
    ) -> UnmetAction {
        let action = unmet_action(raw.clone());
        if action == UnmetAction::Skip {
            warn_if_out_of_dialect(self.env, raw.as_deref(), out);
        }
        action
    }

    /// Act on an unmet precondition: skip cleanly, or fail loudly.
    ///
    /// Returns `None` so a caller inside a `-> Option<_>` fixture can
    /// `return knob.report_unmet(action, &reason);` directly. Generic in the
    /// return type for exactly that reason — the value is never constructed.
    ///
    /// Omitting the `return` is a **compile error** (E0282, "type annotations
    /// needed"), not a silent fall-through, because `T` is unbounded and only
    /// the enclosing `fn`'s return type can infer it. That is the strongest
    /// argument for this shape: a non-generic `-> Option<()>` would let a
    /// dropped `return` compile and skip nothing.
    ///
    /// ⚠️ **Unless `T` is annotated.** `let _: Option<()> = knob.report_unmet(…);`
    /// names `T` itself, so the guarantee lapses there — which is exactly the
    /// spelling a `-> bool` helper needs, and the three in this crate all use
    /// it. That is safe where the next statement is `true`, and a trap if the
    /// line is copied into an `-> Option<Rig>` fixture, where a dropped
    /// `return` would then compile and fall through to the success path having
    /// printed a `[SKIP]`. Copy [`crate::skip::skip_if_no_supervisor`]'s shape
    /// rather than the annotation alone.
    ///
    /// # Panics
    ///
    /// Under [`UnmetAction::Fail`], naming both [`RequireKnob::env`] and
    /// `reason`. Both halves matter: the knob so the operator reads it as
    /// their own demand, and the reason so they know what to stage next.
    pub fn report_unmet<T>(&self, action: UnmetAction, reason: &str) -> Option<T> {
        self.report_unmet_to(action, reason, &mut std::io::stderr())
    }

    /// [`RequireKnob::report_unmet`] with the skip line written to `out`.
    ///
    /// Exists so a unit test can prove the Skip arm **emits** the line.
    /// Asserting on [`crate::skip::skip_line`] alone proves only that the
    /// renderer is correct, and leaves `eprint!` deletable, `print!`-able
    /// (stdout, which the audit never reads) or droppable with the suite still
    /// green. That mutation is worse than the bug the knob fixed: it would make
    /// `grep -c '^\[SKIP\]'` report a *clean* run rather than a misleading one.
    /// A test cannot pin it by calling the stderr form, because emitting a real
    /// `[SKIP]` line would inflate the very count it is protecting.
    ///
    /// # Panics
    ///
    /// Under [`UnmetAction::Fail`], exactly as [`RequireKnob::report_unmet`].
    pub fn report_unmet_to<T>(
        &self,
        action: UnmetAction,
        reason: &str,
        out: &mut dyn std::io::Write,
    ) -> Option<T> {
        match action {
            UnmetAction::Fail => self.panic_unmet(reason),
            UnmetAction::Skip => {
                let _ = write!(out, "{}", skip_line(reason));
                None
            }
        }
    }

    /// Abort the run: the operator demanded it and a precondition is unmet.
    ///
    /// The **one** place this sentence is written. A tier whose own helper
    /// returns `!` (`microvm::require_panic`) needs a diverging form, and
    /// writing the sentence out a second time there is precisely the drift the
    /// #680 review caught between two micro-VM call sites — so the diverging
    /// form is this method, not a second `panic!`.
    ///
    /// # Panics
    ///
    /// Always. That is its whole job; the return type says so.
    pub fn panic_unmet(&self, reason: &str) -> ! {
        panic!(
            "{} demanded a real {} end-to-end run, but a precondition is unmet: {}",
            self.env,
            self.tier,
            // Flattened for the same reason the `[SKIP]` line is: probe errors
            // embed a `\n\n` operator hint, and a panic whose first line stops
            // before the reason is exactly the archaeology this knob exists to
            // spare the operator.
            one_line(reason),
        )
    }

    /// Emit the positive control: `[E2E] <tier>: <detail>` on the **success**
    /// path of a precondition cascade.
    ///
    /// # Why only under [`UnmetAction::Fail`]
    ///
    /// A `[SKIP]` line is evidence that a test did *not* run. Its absence is
    /// not evidence that one *did* — which is the whole of [#664]: with the
    /// knob set and one typo in a `--test` name filter, `cargo test` reports
    /// `0 passed; 4 filtered out` and **exits 0**, having loaded no model and
    /// emitted no `[SKIP]`. The knob fires only from inside a test body, so it
    /// is structurally blind to a body that never ran.
    ///
    /// An affirmative line inverts that: `grep -c '^\[E2E\]'` over a demanded
    /// run is a **count that can be asserted against**, which is what
    /// `scripts/run-e2e-gate.sh` does. Emitting it only when the operator
    /// demanded the run keeps a casual `cargo test` quiet and makes every
    /// `[E2E]` line mean one thing: *a demanded precondition was actually met
    /// here*.
    ///
    /// [#664]: https://github.com/hherb/kastellan/issues/664
    pub fn announce(&self, action: UnmetAction, detail: &str) {
        self.announce_to(action, detail, &mut std::io::stderr());
    }

    /// [`RequireKnob::announce`] writing to `out`, so a test can read the bytes
    /// back without inflating a real run's `[E2E]` count.
    pub fn announce_to(&self, action: UnmetAction, detail: &str, out: &mut dyn std::io::Write) {
        if action == UnmetAction::Fail {
            let _ = write!(out, "{}", e2e_line(self.tier, detail));
        }
    }

    /// [`RequireKnob::announce`] for a success path that never computed an
    /// [`UnmetAction`], because nothing was unmet.
    ///
    /// The four original `announce` call sites sit in helpers that already
    /// read the knob to decide the *failure* arm, so passing the action back in
    /// costs nothing there. A cascade whose success path is a bare `false` or
    /// `Ok(value)` — every micro-VM combinator is one — would have to read the
    /// knob solely in order to announce, and the obvious spelling
    /// (`knob.action()`) re-emits the out-of-dialect `[WARN]` on a path that
    /// has nothing to warn about, once per precondition per test.
    ///
    /// So this reads the knob **without** the dialect warning: the warning
    /// belongs to the decision, and repeating it on every success line would
    /// drown the log a gate is grepping.
    pub fn announce_demanded(&self, detail: &str) {
        self.announce_demanded_to(detail, &mut std::io::stderr());
    }

    /// [`RequireKnob::announce_demanded`] writing to `out`, so a test can read
    /// the bytes back without inflating a real run's `[E2E]` count.
    pub fn announce_demanded_to(&self, detail: &str, out: &mut dyn std::io::Write) {
        self.announce_to(unmet_action(self.raw()), detail, out);
    }
}

/// The guard tier's knob.
///
/// Declared here rather than beside its tier — the exception to the rule the
/// other five follow — because `core/tests/guard_tier_e2e.rs` is an
/// integration-test binary, invisible to [`KNOBS`] and so to the test that pins
/// the vocabulary against the gate script. A knob the census cannot see is a
/// knob the census cannot defend, which is the whole failure [`KNOBS`] exists
/// to close.
///
/// ⚠️ It covers `bootstrap`'s **worker-binary** check only; the supervisor,
/// sandbox and Postgres checks beside it answer to their own knobs. See the
/// warning at its use site.
pub const GUARD_TIER_KNOB: RequireKnob =
    RequireKnob::new("KASTELLAN_GUARD_REQUIRE_E2E", "guard-tier");

/// Every REQUIRE knob this workspace defines.
///
/// # Why the list exists
///
/// `scripts/run-e2e-gate.sh` sets these variables by name, as shell literals;
/// the Rust side reads them through the consts. Nothing tied the two halves
/// together, so renaming either one left a profile setting a variable nothing
/// reads: [`RequireKnob::action`] returns [`UnmetAction::Skip`], the
/// precondition silently reverts to skip-as-pass, its `[E2E]` lines vanish —
/// and before the floors were per-tier, the gate still reported `✅`. A guard
/// sharing its census's blind spot, in the tree's own gate script.
///
/// `gate_script_tests` reads the script and pins both directions: every knob
/// the script names is one of these, and every one of these is named by some
/// profile — so a new tier cannot be added without a gate that demands it.
///
/// ⚠️ One variable can carry two knobs: [`crate::skip::SUPERVISOR_KNOB`] and
/// [`crate::skip::PG_KNOB`] differ only in the phrase they name, deliberately
/// (#714). Anything de-duplicating this list must key on
/// [`RequireKnob::env`], not on the knob — `PartialEq` here is `(env, tier)`
/// identity and would report two.
pub const KNOBS: &[RequireKnob] = &[
    crate::skip::SUPERVISOR_KNOB,
    crate::skip::PG_KNOB,
    crate::sandbox::SANDBOX_KNOB,
    GUARD_TIER_KNOB,
    crate::microvm::KNOB,
    crate::gliner_e2e::KNOB,
];

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KNOB: RequireKnob = RequireKnob::new("KASTELLAN_TEST_REQUIRE_E2E", "test-tier");

    /// The dialect, in both directions. A strict `Some("1")` rule is the skew
    /// #654 was filed about, so `true`/`YES`/` on ` must all demand.
    #[test]
    fn the_flag_dialect_decides_whether_a_run_is_demanded() {
        for truthy in ["1", "true", "TRUE", "yes", "on", "  on  "] {
            assert_eq!(
                unmet_action(Some(truthy.to_string())),
                UnmetAction::Fail,
                "{truthy:?} must demand a real run"
            );
        }
        for falsey in ["0", "false", "no", "off", ""] {
            assert_eq!(
                unmet_action(Some(falsey.to_string())),
                UnmetAction::Skip,
                "{falsey:?} must not demand"
            );
        }
        assert_eq!(unmet_action(None), UnmetAction::Skip, "unset is the default");
    }

    /// An out-of-dialect value is operator error, and silently treating it as
    /// unset hands back the green run the operator was ruling out.
    #[test]
    fn an_out_of_dialect_value_warns_and_names_the_knob() {
        let mut sink = Vec::new();
        warn_if_out_of_dialect("KASTELLAN_X_REQUIRE_E2E", Some("y"), &mut sink);
        let got = String::from_utf8(sink).expect("utf8");
        assert!(got.contains("[WARN]"), "must warn: {got:?}");
        assert!(got.contains("KASTELLAN_X_REQUIRE_E2E"), "must name the knob: {got:?}");
        assert!(got.contains("will NOT become failures"), "must name the consequence: {got:?}");
    }

    /// ...but a deliberate opt-out is not a typo, and warning on it would train
    /// the operator to ignore the warning.
    #[test]
    fn a_deliberate_opt_out_does_not_warn() {
        for quiet in [Some("0"), Some("false"), Some("OFF"), Some(""), Some("   "), None] {
            let mut sink = Vec::new();
            warn_if_out_of_dialect("KASTELLAN_X_REQUIRE_E2E", quiet, &mut sink);
            assert!(sink.is_empty(), "{quiet:?} must be silent, got {sink:?}");
        }
    }

    /// **The wiring, not the rule.** `action_reporting_to` must actually CALL
    /// the dialect warning — deleting that call leaves every unit test above
    /// green while restoring the silent skip.
    #[test]
    fn the_skip_arm_emits_the_dialect_warning_through_the_knob() {
        let mut sink = Vec::new();
        let action = TEST_KNOB.action_reporting_to(Some("y".into()), &mut sink);
        assert_eq!(action, UnmetAction::Skip, "an out-of-dialect value does not demand");
        let got = String::from_utf8(sink).expect("utf8");
        assert!(got.contains("KASTELLAN_TEST_REQUIRE_E2E"), "the knob warns by name: {got:?}");
    }

    /// A demanded run must NOT warn — there is nothing out of dialect, and a
    /// warning on the success path is noise in exactly the log a gate reads.
    #[test]
    fn a_demanded_run_emits_no_dialect_warning() {
        let mut sink = Vec::new();
        let action = TEST_KNOB.action_reporting_to(Some("1".into()), &mut sink);
        assert_eq!(action, UnmetAction::Fail);
        assert!(sink.is_empty(), "no warning on the demanded path: {sink:?}");
    }

    /// The Skip arm **emits** a `[SKIP]` line, not merely renders one. A
    /// dropped `eprint!` here would make a misleading run look clean.
    #[test]
    fn the_skip_arm_emits_a_skip_line_carrying_the_reason() {
        let mut sink = Vec::new();
        let got: Option<()> =
            TEST_KNOB.report_unmet_to(UnmetAction::Skip, "no cluster at /nowhere", &mut sink);
        assert!(got.is_none(), "the skip arm yields None so the caller returns");
        let rendered = String::from_utf8(sink).expect("utf8");
        assert!(rendered.contains("[SKIP]"), "must be greppable: {rendered:?}");
        assert!(rendered.contains("no cluster at /nowhere"), "carries the reason: {rendered:?}");
    }

    /// The Fail arm names **both** the knob and the reason. Naming only the
    /// knob sends the operator hunting; naming only the reason reads as a
    /// regression in whatever change is in flight.
    #[test]
    fn the_fail_arm_panics_naming_the_knob_the_tier_and_the_reason() {
        let err = std::panic::catch_unwind(|| {
            let _: Option<()> = TEST_KNOB.report_unmet(UnmetAction::Fail, "no cluster at /nowhere");
        })
        .expect_err("Fail must panic");
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .expect("panic payload is a String");
        assert!(msg.contains("KASTELLAN_TEST_REQUIRE_E2E"), "names the knob: {msg}");
        assert!(msg.contains("test-tier"), "names the tier: {msg}");
        assert!(msg.contains("no cluster at /nowhere"), "names the reason: {msg}");
    }

    /// A probe reason may span lines (both supervisor backends embed a `\n\n`
    /// operator hint). A panic whose first line stops before the reason is the
    /// archaeology the knob exists to spare the operator.
    #[test]
    fn the_fail_arm_flattens_a_multi_line_reason() {
        let err = std::panic::catch_unwind(|| {
            let _: Option<()> =
                TEST_KNOB.report_unmet(UnmetAction::Fail, "probe failed\n\n   start the manager");
        })
        .expect_err("Fail must panic");
        let msg = err.downcast_ref::<String>().map(String::as_str).expect("String payload");
        assert!(
            msg.contains("probe failed start the manager"),
            "the whole reason is on one line: {msg}"
        );
    }

    /// **The positive control.** A demanded run emits an `[E2E]` line naming
    /// the tier and what was resolved, so a gate can assert a COUNT rather than
    /// infer from the absence of `[SKIP]`.
    #[test]
    fn a_demanded_run_announces_itself_with_a_greppable_e2e_line() {
        let mut sink = Vec::new();
        TEST_KNOB.announce_to(UnmetAction::Fail, "cluster at /tmp/pg", &mut sink);
        let got = String::from_utf8(sink).expect("utf8");
        assert!(got.starts_with("\n[E2E] "), "must be its own greppable line: {got:?}");
        assert!(got.contains("test-tier"), "names the tier: {got:?}");
        assert!(got.contains("cluster at /tmp/pg"), "names what was resolved: {got:?}");
    }

    /// ...and a run nobody demanded stays quiet, so every `[E2E]` line in a
    /// gate log means one thing: a **demanded** precondition was met here.
    /// Without this, a plain `cargo test --workspace` would emit hundreds of
    /// lines and the count would stop being evidence of anything.
    #[test]
    fn an_undemanded_run_announces_nothing() {
        let mut sink = Vec::new();
        TEST_KNOB.announce_to(UnmetAction::Skip, "cluster at /tmp/pg", &mut sink);
        assert!(sink.is_empty(), "no [E2E] line without the knob: {sink:?}");
    }

    /// **The success-path read.** `announce_demanded` exists for cascades whose
    /// success arm never computed an action, so it must reach the environment
    /// itself — a constant here would make every micro-VM `[E2E]` line either
    /// unconditional or absent.
    #[test]
    fn announce_demanded_reads_the_environment_not_a_constant() {
        let _lock = crate::env::env_lock();

        let mut demanded = Vec::new();
        {
            let _set = crate::env::EnvVarGuard::set("KASTELLAN_TEST_REQUIRE_E2E", "1");
            TEST_KNOB.announce_demanded_to("fixture staged", &mut demanded);
        }
        let got = String::from_utf8(demanded).expect("utf8");
        assert!(got.contains("[E2E]"), "a demanded run announces: {got:?}");
        assert!(got.contains("fixture staged"), "carries the detail: {got:?}");

        let mut undemanded = Vec::new();
        {
            let _unset = crate::env::EnvVarGuard::unset("KASTELLAN_TEST_REQUIRE_E2E");
            TEST_KNOB.announce_demanded_to("fixture staged", &mut undemanded);
        }
        assert!(undemanded.is_empty(), "no knob, no line: {undemanded:?}");
    }

    /// ...and it must NOT warn on the success path. The dialect warning belongs
    /// to the decision; repeated once per precondition per test it would drown
    /// the log a gate greps.
    #[test]
    fn announce_demanded_does_not_emit_the_dialect_warning() {
        let _lock = crate::env::env_lock();
        let _set = crate::env::EnvVarGuard::set("KASTELLAN_TEST_REQUIRE_E2E", "y");

        let mut sink = Vec::new();
        TEST_KNOB.announce_demanded_to("fixture staged", &mut sink);
        assert!(sink.is_empty(), "out-of-dialect is not demanded, and does not warn here: {sink:?}");
    }

    /// A knob set to a non-UTF-8 value must not read as *unset*. `.ok()` would
    /// discard it, and `warn_if_out_of_dialect` returns early on `None`, so the
    /// operator would get a silent skip from a knob they demonstrably set.
    #[test]
    fn a_non_utf8_knob_value_warns_rather_than_reading_as_unset() {
        let _lock = crate::env::env_lock();

        let mut sink = Vec::new();
        let action = TEST_KNOB.action_reporting_to(Some("\u{fffd}".into()), &mut sink);
        assert_eq!(action, UnmetAction::Skip, "a non-UTF-8 value cannot be truthy");
        let got = String::from_utf8(sink).expect("utf8");
        assert!(got.contains("[WARN]"), "it must leave a trace: {got:?}");
    }

    /// The vocabulary is what `scripts/run-e2e-gate.sh` is pinned against, so a
    /// stray name or a duplicated tier would weaken that check silently.
    #[test]
    fn the_knob_vocabulary_is_well_formed() {
        assert!(!KNOBS.is_empty(), "an empty vocabulary would make every check over it vacuous");
        for knob in KNOBS {
            assert!(knob.env().starts_with("KASTELLAN_"), "{} is not one of ours", knob.env());
            assert!(knob.env().ends_with("_REQUIRE_E2E"), "{} is not a REQUIRE knob", knob.env());
            assert!(!knob.tier().is_empty(), "{} has no tier phrase", knob.env());
        }

        // Tiers must be distinct: the gate script counts `[E2E] <tier>:` lines
        // per tier, so two knobs sharing a phrase would make one floor
        // satisfiable by the other's evidence — the very substitution the
        // per-tier floors exist to prevent.
        let mut tiers: Vec<&str> = KNOBS.iter().map(RequireKnob::tier).collect();
        tiers.sort_unstable();
        let before = tiers.len();
        tiers.dedup();
        assert_eq!(before, tiers.len(), "two knobs share a tier phrase: {tiers:?}");
    }

    /// `[E2E]`, `[SKIP]` and `[WARN]` are counted separately when a run is
    /// audited, so none may be mistakable for another. A positive control that
    /// inflated the skip count would misattribute a test that actually ran.
    #[test]
    fn the_three_evidence_markers_are_mutually_distinguishable() {
        let e2e = e2e_line("tier", "detail");
        assert!(!e2e.contains("[SKIP]"), "a success is not a skip: {e2e:?}");
        assert!(!e2e.contains("[WARN]"), "a success is not a warning: {e2e:?}");
        assert!(!skip_line("r").contains("[E2E]"), "a skip is not a success");
        assert!(!warn_line("r").contains("[E2E]"), "a warning is not a success");
    }
}
