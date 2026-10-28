//! What the daemon records about the guard tier at boot (issue [#627]).
//!
//! One boot produces one `info!` line, at most one `warn!`, and one
//! durable `policy / guard_tier.boot` audit row. All three report the
//! same facts, and until this module existed all three were built inline
//! in `core/src/main.rs` — a binary crate with no `#[cfg(test)]` module,
//! so the only tests naming that row asserted its **count**.
//!
//! That gap mattered because half of [#624]'s fix lives in the *report*
//! rather than in the probe. Taking the fastest of several samples is
//! worth nothing if the row cannot then distinguish a quiet host from a
//! busy one — and swapping [`BootRates::fastest_tok_per_s`] with
//! [`BootRates::slowest_tok_per_s`] **silences** the documented operator
//! query `slowest_tok_per_s < tok_per_s / 2`.
//!
//! Note the failure mode, because it is worse than an inversion and the
//! obvious guess is wrong. A swap does *not* make quiet hosts report as
//! busy: since `slowest <= fastest` always holds, a swapped row asks
//! `fastest < slowest / 2`, which no row can satisfy. The query returns
//! the **empty set on every host, forever** — a contended host stops
//! reporting as contended and nothing takes its place. Every key set,
//! type and non-null survives that mutation, so only a test that reads
//! the two values back can catch it.
//!
//! **Pure throughout** — no tier, no pool, no clock, no backend. The two
//! scalars and the budget [`boot_payload`] takes are its whole input
//! apart from [`super::policy::policy_digest`], and taking them rather
//! than a `&GuardTier` is the whole point: a `GuardTier` has no
//! constructor but [`super::GuardTier::from_router_config`], whose
//! `/props` verification is fatal — so building one in a unit test would
//! need a live guard endpoint. That is why this code was unreachable
//! from a unit test in the first place.
//!
//! [#624]: https://github.com/hherb/kastellan/issues/624
//! [#627]: https://github.com/hherb/kastellan/issues/627

use serde_json::{json, Value};

pub mod refusal;
pub mod reported;

pub use refusal::{
    record_refusal, refusal_kind, refusal_payload, REFUSAL_ROW_INTERVAL, REFUSED_ACTION,
};
pub use reported::ReportedRates;

use super::policy::policy_digest;
use super::tier::Unadjudicated;
use super::timeout::{GuardTimeout, TimeoutBasis};

/// The four probe-derived numbers a boot report carries.
///
/// Read out of the basis by one pure function, [`Self::from_basis`], and
/// so shared by the `info!` line, the `warn!` finding and the durable
/// row. Before this type they were an inline four-tuple destructured in
/// the binary, which is exactly the shape a transposition hides in.
///
/// Every field is an `Option` because every one of them is genuinely
/// absent on some basis, and absence must reach the row as JSON `null`
/// rather than as a fabricated `0`: a wedged backend really can measure
/// `0.0` tok/s, so a zero standing in for "never measured" would be
/// indistinguishable from an observation.
///
/// ⚠️ That contract depends on a guard **this module does not hold**.
/// `serde_json` maps a non-finite float to `null` rather than erroring,
/// so a `NaN` or infinite rate would reach the row as `"tok_per_s":
/// null` — indistinguishable from "never measured", which is the exact
/// confusion the `Option` exists to prevent. It cannot happen today
/// because [`super::timeout::derive_guard_timeout`] rejects a
/// non-finite rate before any `Probed` basis is built, and that is the
/// only production constructor of one.
///
/// The fields are `pub` within the crate and nothing here enforces
/// `slowest <= fastest` — that repair belongs to
/// [`super::timeout::derive_guard_timeout`], which builds the basis and
/// applies `.min(fastest)` as it does. (Not to [`super::timeout::summarise`],
/// which returns a [`super::timeout::ProbeSummary`] and never computes a
/// fastest at all.) The invariant therefore holds of a basis that
/// function produced; `TimeoutBasis`'s own fields are `pub`, so a
/// hand-built one — including every fixture in this module's tests —
/// carries whatever it was given. This type reports what the basis says;
/// it does not re-decide it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct BootRates {
    /// The **fastest** of the probe's measuring samples, which is the
    /// one that measures the *host* — prompt processing has a hardware
    /// ceiling and no floor, so contention can only ever make an
    /// observation slower ([#624]'s D11).
    ///
    /// Read straight off [`TimeoutBasis::Probed::fastest_tok_per_s`],
    /// whose doc carries the full rationale; the two were renamed
    /// together in [#632] so they cannot drift apart. **The name is not
    /// the wire key** — [`boot_payload`] still writes `"tok_per_s"`,
    /// and so do `main.rs`'s two tracing fields.
    ///
    /// [#624]: https://github.com/hherb/kastellan/issues/624
    /// [#632]: https://github.com/hherb/kastellan/issues/632
    pub fastest_tok_per_s: Option<f32>,
    /// The **slowest** of the samples that measured. Together with
    /// [`Self::fastest_tok_per_s`] this is the contention spread: two rates that
    /// agree are a measurement of the host, two that differ by 22x are a
    /// measurement of how busy it was at boot.
    ///
    /// (22x, not the 26x quoted elsewhere in the guard tree: 26x is
    /// ~7 000 tok/s measured directly against the 269.6 tok/s boot, and
    /// the 7 000 never appears in a row. The spread *this row* can show
    /// is 6 073 / 269.6 = 22.5x.)
    pub slowest_tok_per_s: Option<f32>,
    /// How many samples produced a usable rate — the numerator.
    pub measured_samples: Option<u32>,
    /// How many probe calls were actually made — the denominator
    /// [`Self::measured_samples`] is a numerator of.
    ///
    /// Present on every basis a probe produced, not only the measuring
    /// one: on `Saturated`/`Unprobed` it is the *strength of the
    /// evidence* behind the finding, and one failed call predicts a
    /// wholly fail-open tier far more weakly than three.
    ///
    /// Without it, `measured_samples: 1` reads three ways at once — one
    /// sample that worked, or three of which two were served from cache,
    /// or three of which two failed outright — and a reader cannot even
    /// recover `PROBE_SAMPLES`, which is a tunable. A row with
    /// `attempted_samples > measured_samples` and no `coverage_finding`
    /// is the one that says "read that boot's `warn!` lines".
    pub attempted_samples: Option<u32>,
}

impl BootRates {
    /// Read the reportable numbers out of a timeout's provenance.
    ///
    /// Pure and total: every basis maps, and the ones that measured
    /// nothing map to `None` rather than to a stand-in value.
    pub fn from_basis(basis: &TimeoutBasis) -> Self {
        // ONE match building `Self` per arm, rather than two matches
        // feeding a positional tuple. The tuple form was the first cut
        // and it reintroduced exactly the hazard this module exists to
        // close: `(Some(*fastest_tok_per_s), Some(*slowest_tok_per_s),
        // ..)` are same-typed neighbours, so transposing them compiles
        // in silence. Named field initialisers make the same mistake
        // read as `fastest_tok_per_s: Some(*slowest_tok_per_s)` — wrong
        // on its face.
        //
        // The cost is repeating `None` for the quiet arms, and it buys
        // the three legal shapes being visible in the code rather than
        // only in the prose above: `Probed` measures and counts,
        // `Saturated`/`Unprobed` ran a probe that measured nothing, and
        // an operator pin never probed at all.
        match basis {
            TimeoutBasis::Probed {
                fastest_tok_per_s,
                slowest_tok_per_s,
                measured_samples,
                attempted_samples,
                ..
            } => Self {
                fastest_tok_per_s: Some(*fastest_tok_per_s),
                slowest_tok_per_s: Some(*slowest_tok_per_s),
                measured_samples: Some(*measured_samples),
                attempted_samples: Some(*attempted_samples),
            },
            TimeoutBasis::Saturated { attempted_samples, .. }
            | TimeoutBasis::Unprobed { attempted_samples, .. } => Self {
                fastest_tok_per_s: None,
                slowest_tok_per_s: None,
                measured_samples: None,
                attempted_samples: Some(*attempted_samples),
            },
            TimeoutBasis::Operator { .. } => Self {
                fastest_tok_per_s: None,
                slowest_tok_per_s: None,
                measured_samples: None,
                attempted_samples: None,
            },
        }
    }
}

/// The budget in milliseconds, as every boot report spells it.
///
/// A one-line derivation, given its own name for one reason: the
/// `info!` line, the `warn!` finding and the durable row all report it,
/// and until this existed the log sites computed it from a local while
/// the payload computed it again. Two copies of `as_millis() as u64`
/// agree today and are a silent divergence the day one of them changes.
///
/// Saturates rather than truncates, matching
/// [`super::timeout::derive_guard_timeout`]'s deliberate choice three
/// modules over. Unreachable today — every `GuardTimeout` in the tree is
/// built with `Duration::from_millis(u64)`, so `as_millis()` returns
/// exactly that `u64` — but this function is `pub` and takes an
/// arbitrary budget, and a `Duration::from_secs`-built one would wrap a
/// bare `as u64` silently into the durable row.
pub fn timeout_ms(budget: &GuardTimeout) -> u64 {
    u64::try_from(budget.timeout.as_millis()).unwrap_or(u64::MAX)
}

/// The durable `policy / guard_tier.boot` payload for a **configured**
/// tier.
///
/// Takes `tau` and `n_ctx` as plain scalars, and the budget for its
/// provenance, rather than the tier that holds all three — see the
/// module docs for why that is the design and not a convenience.
///
/// Key notes for a reader of the stored rows:
///
/// * `timeout_basis` is [`TimeoutBasis::kind`], which folds
///   [`super::timeout::Clamped::ToCeiling`] into a bare `"probed"`. That
///   is why `coverage_finding` is a key of its own: without it, the row
///   for a host that cannot adjudicate a worst-case document looks
///   exactly like a healthy one.
/// * `coverage_finding` is `null` when the boot was routine, so the
///   query for affected hosts is
///   `WHERE payload->>'coverage_finding' IS NOT NULL`. Tracing logs
///   rotate; `audit_log` does not. It is passed straight through from
///   [`TimeoutBasis::coverage_finding`] for **every** basis, not only
///   the clamped one — five of the enum's states carry a finding, and a
///   row that reported only the `Probed`/`ToCeiling` one would return
///   the empty set for the three loudest (the probe never returned, the
///   probe failed, and both out-of-band operator pins).
/// * The key set never shrinks. A basis that measured nothing still
///   carries `tok_per_s`, `slowest_tok_per_s` and `measured_samples`,
///   holding `null` — so a reader querying one of them finds the key on
///   every row rather than only on the hosts that happened to probe.
pub fn boot_payload(tau: f32, n_ctx: u64, budget: &GuardTimeout) -> Value {
    // The ONE mapping from probe fields to reporting names (#643). It
    // used to be spelled out here and again, twice, in `main.rs`'s two
    // tracing lines; only this copy was ever guarded.
    let reported = ReportedRates::from_basis(&budget.basis);
    json!({
        "configured":        true,
        "tau":               tau,
        "timeout_ms":        timeout_ms(budget),
        "timeout_basis":     budget.basis.kind(),
        // ⚠️ These JSON keys are the DURABLE wire format — rows carrying
        // them are already in `audit_log` on live hosts, and operator
        // queries are written against them. They are string literals
        // here and `BootRates` has no `Serialize` derive, so the Rust
        // field names and the wire keys are decoupled on purpose. #632
        // exercised exactly that decoupling: it renamed the FIELD to
        // `fastest_tok_per_s` and left the KEY at `"tok_per_s"`, which
        // is why the two differ on the line below. That difference is
        // deliberate and must survive — `main.rs`'s two tracing fields
        // are frozen at `tok_per_s` for the same reason, so an operator
        // correlating a boot log line with its audit row reads one
        // vocabulary rather than two.
        "tok_per_s":         reported.tok_per_s,
        "slowest_tok_per_s": reported.slowest_tok_per_s,
        "measured_samples":  reported.measured_samples,
        "attempted_samples": reported.attempted_samples,
        "n_ctx":             n_ctx,
        "policy_digest":     policy_digest(),
        "coverage_finding":  budget.basis.coverage_finding(),
    })
}

/// The durable payload for a host with **no** guard tier configured.
///
/// Spells "no tier ran" with the same token the per-dispatch
/// `guard.state` vocabulary uses ([`Unadjudicated::NotConfigured`]), so
/// the question has one spelling across the audit log rather than a live
/// half and an orphaned half. This boot row is deliberately the ONLY
/// producer of it: a per-dispatch `not_configured` field would be a
/// constant on every row of an unconfigured host.
pub fn not_configured_payload() -> Value {
    json!({
        "configured": false,
        "state":      Unadjudicated::NotConfigured.as_str(),
    })
}

#[cfg(test)]
mod tests;
