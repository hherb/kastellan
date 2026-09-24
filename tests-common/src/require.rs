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
    ///
    /// ⚠️ **This is one of the TWO doors that install #742's panic hook**
    /// (#748). The other is [`RequireKnob::action_reporting_to`], for callers
    /// that supply the value themselves. `microvm::require_action_to` is the
    /// one production caller: until #755 it read the environment itself, with
    /// the lossy `std::env::var(..).ok()` this method replaces; it now reads
    /// through here and passes the value on, so it goes through BOTH doors. A
    /// knob read that goes through neither would leave the process on the
    /// default hook — which is exactly what the met-precondition path did:
    /// [`RequireKnob::announce_demanded`] reaches the knob only through here,
    /// never through `action_reporting_to`, so on a healthy micro-VM host every
    /// green gate run had the default hook. `knob_reads_install_the_panic_hook_e2e`
    /// pins each door alone, in its own process.
    pub(crate) fn raw(&self) -> Option<String> {
        crate::panic_hook::install_once();
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
    ///
    /// ⚠️ **This is one of the two doors that install #742's panic hook — the
    /// other is `raw()` (#748).** Neither is `action`, which merely calls both.
    /// The first version installed from `action`, and the **`microvm` gate profile bypassed it
    /// entirely**: `microvm::skip_unless_ready` → `report_unmet_microvm_to` →
    /// `require_action_to` called *this* method directly (it has called `raw()`
    /// first only since #755), so a micro-VM suite
    /// reached its knob without ever touching `action`. It was covered only
    /// when some co-set knob (the profile also sets the PG and sandbox ones)
    /// happened to be read first — coverage by accident, which is precisely
    /// what `install_once`'s doc claims to have replaced.
    ///
    /// ⚠️ **"Every knob read funnels through here" was the claim, and it was
    /// false** (#748): the met-precondition announcement reads the knob via
    /// `raw()` and never comes here. Hence the second door.
    pub fn action_reporting_to(
        &self,
        raw: Option<String>,
        out: &mut dyn std::io::Write,
    ) -> UnmetAction {
        crate::panic_hook::install_once();
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
mod tests;
