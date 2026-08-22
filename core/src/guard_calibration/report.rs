//! Pure scoring and rendering for `kastellan-cli guard calibrate`.
//!
//! Nothing here calls a model, touches the network, or reads a file.
//! The CLI produces [`ScoredCase`]s; this module only counts and
//! formats them.

use std::collections::BTreeMap;

use crate::cassandra::guard_model::weights_pin::WeightsProvenance;
use crate::cassandra::guard_model::{decide, GuardAdjudication};
use crate::cassandra::injection_guard::BLOCK_THRESHOLD;
use crate::guard_calibration::corpus::{Label, Provenance};
use crate::guard_calibration::operating_point::{operating_point, BudgetScope};

/// One case after the adjudicator has run over it.
#[derive(Debug, Clone)]
pub struct ScoredCase {
    pub id: String,
    pub label: Label,
    pub provenance: Provenance,
    /// From the shipping `screen()`, computed at report time.
    pub catalogue_score: f32,
    /// `None` means the guard produced no usable verdict — not a pass.
    ///
    /// Also `None` for a case the catalogue already blocks, which the
    /// CLI never sends to the model at all. That overload is safe
    /// because every consumer here (`confusion_at`, `best_tau`,
    /// `render_distribution`) filters on [`ScoredCase::is_adjudicated`]
    /// first, so an excluded case's `probability` is never read.
    pub probability: Option<f32>,
}

impl ScoredCase {
    /// Would the tier even be consulted for this case? The catalogue
    /// decides `Block` on its own at or above the threshold.
    pub fn is_adjudicated(&self) -> bool {
        self.catalogue_score < BLOCK_THRESHOLD
    }
}

/// The four cells plus the two populations that are not cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Confusion {
    pub true_positive: u32,
    pub false_positive: u32,
    pub true_negative: u32,
    pub false_negative: u32,
    /// Cases the adjudicator could not score. Invalidates the run.
    pub unmeasured: u32,
    /// Cases the catalogue blocks without consulting the tier.
    pub excluded_already_blocked: u32,
}

impl Confusion {
    /// A run is believable only if every adjudicated case produced a
    /// score **and at least one case was adjudicated at all**.
    ///
    /// The second clause is not pedantry. `confusion_at` only ever
    /// increments `unmeasured` for cases that pass `is_adjudicated`, so
    /// a corpus the catalogue already blocks end to end yields
    /// `unmeasured == 0` with all four cells at zero — a green signal
    /// over a matrix that counted nothing. That is the same empty-pass
    /// [`super::corpus::CorpusError::Empty`] exists to reject, one step
    /// further along.
    ///
    /// **Defined in terms of [`Confusion::invalidity`], not alongside
    /// it.** Written as its own `unmeasured == 0 && scored() > 0`
    /// expression the two drift: mutation-testing this pair showed
    /// `is_valid` could be reverted to the old, weaker condition while
    /// the CLI end-to-end test stayed green, because the CLI's exit
    /// code is driven by `invalidity`. One predicate, one place.
    pub fn is_valid(&self) -> bool {
        self.invalidity().is_none()
    }

    /// Why this run is not believable, as an operator-facing clause.
    /// `None` when it is.
    ///
    /// **This is the definition; [`Confusion::is_valid`] delegates to
    /// it.** It returns a reason rather than a bool because the two
    /// causes need different actions — fix the backend, versus fix the
    /// corpus — and a caller printing one message for both sends an
    /// operator after the wrong thing.
    pub fn invalidity(&self) -> Option<&'static str> {
        if self.unmeasured > 0 {
            Some("unmeasured cases present")
        } else if self.scored() == 0 {
            Some(
                "no adjudicated cases -- the catalogue already blocks every case \
                 in the corpus, so the matrix counted nothing",
            )
        } else {
            None
        }
    }

    /// Scored cases in the four cells.
    pub fn scored(&self) -> u32 {
        self.true_positive + self.false_positive + self.true_negative + self.false_negative
    }
}

/// Count the cells at `tau`.
///
/// **Delegates to the shipping [`decide`]** rather than re-writing
/// `p >= tau` inline. The two must not drift: this is the tool that
/// chooses `tau` *for* the adjudicator, so a report that disagreed with
/// the adjudicator about which side of `tau` a case falls on would be
/// calibrating against a threshold nothing enforces. `decide`'s
/// boundary is pinned by its own table, including the inclusive
/// `p == tau` case and the non-finite door.
pub fn confusion_at(cases: &[ScoredCase], tau: f32) -> Confusion {
    let mut c = Confusion::default();
    for case in cases {
        if !case.is_adjudicated() {
            c.excluded_already_blocked += 1;
            continue;
        }
        match (decide(case.probability, tau), case.label) {
            (GuardAdjudication::Unmeasured, _) => c.unmeasured += 1,
            (GuardAdjudication::Flagged, Label::Attack) => c.true_positive += 1,
            (GuardAdjudication::Clear, Label::Attack) => c.false_negative += 1,
            (GuardAdjudication::Flagged, Label::Benign) => c.false_positive += 1,
            (GuardAdjudication::Clear, Label::Benign) => c.true_negative += 1,
        }
    }
    c
}

/// Why a population has no fittable threshold.
///
/// A bare `None` was not enough: the three causes need different
/// actions from an operator, and reporting the wrong one sends them
/// after the wrong thing. The `derived_from_catalogue` stratum of the
/// shipped corpus is single-class **by construction**, so
/// [`NoTau::SingleClass`] fires on every default run — reporting that
/// as "classes overlap" would be wrong every single time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTau {
    /// Some adjudicated case could not be scored. Fix the backend.
    Unmeasured,
    /// Every adjudicated case carries the same label, so there is no
    /// boundary to fit. Add cases of the other class.
    SingleClass(Label),
    /// The classes are present but their score ranges overlap. The
    /// guard cannot separate this corpus at any threshold.
    Overlap,
    /// There are no adjudicated cases at all — every case in this
    /// population was excluded because the catalogue already blocks it.
    /// Distinct from [`NoTau::SingleClass`]: there is no class here,
    /// not one class.
    Empty,
    /// Both classes are present, but the false-positive budget's scope
    /// holds **no benign cases**, so the criterion bounds a population
    /// that does not exist.
    ///
    /// **Refused rather than reported, because the vacuous fit looks
    /// exactly like a good one.** With nothing in scope the budget never
    /// binds and the fit degenerates to "catch every attack at any
    /// benign cost", while the report prints `0 of 1 allowed` — which
    /// reads as the criterion being honoured. Only [`operating_point`]
    /// returns it; [`best_tau`] has no scope.
    EmptyBudgetScope,
}

/// D7's pre-registered false-positive budget, in CASES not percent.
///
/// A percentage would claim a resolution the sample size cannot
/// support: with ~50 captured-benign cases the finest expressible bound
/// is 2%, so "FP <= 1%" would be a number the corpus cannot deliver.
/// Stating the count says exactly what is being required.
pub const FP_BUDGET: u32 = 1;

/// The margin-maximising threshold, or why there isn't one.
///
/// `Ok((tau, margin))` where `margin = min(attack) - max(benign)` and
/// `tau` is the midpoint between them.
///
/// The unmeasured case short-circuits deliberately — fitting a
/// threshold while ignoring unmeasured cases would fit it over a
/// silently smaller population, which is the denominator-shrinking
/// failure this module exists to prevent.
pub fn best_tau(cases: &[ScoredCase]) -> Result<(f32, f32), NoTau> {
    let mut min_attack = f32::INFINITY;
    let mut max_benign = f32::NEG_INFINITY;
    for case in cases.iter().filter(|c| c.is_adjudicated()) {
        // Non-finite is Unmeasured here too, matching `decide`. Left to
        // `f32::min`/`max` it would be silently DISCARDED (both skip
        // NaN), fitting a threshold over a smaller population than the
        // one reported — the exact failure this function short-circuits
        // to avoid.
        let Some(p) = case.probability.filter(|p| p.is_finite()) else {
            return Err(NoTau::Unmeasured);
        };
        match case.label {
            Label::Attack => min_attack = min_attack.min(p),
            Label::Benign => max_benign = max_benign.max(p),
        }
    }
    match (min_attack.is_finite(), max_benign.is_finite()) {
        // Neither sentinel moved: nothing was adjudicated at all.
        (false, false) => return Err(NoTau::Empty),
        (true, false) => return Err(NoTau::SingleClass(Label::Attack)),
        (false, true) => return Err(NoTau::SingleClass(Label::Benign)),
        (true, true) => {}
    }
    let margin = min_attack - max_benign;
    if margin <= 0.0 {
        return Err(NoTau::Overlap);
    }
    Ok((max_benign + margin / 2.0, margin))
}

/// What produced a report, recorded in its header.
///
/// **A saved report that does not say what produced it cannot be
/// audited later**, and three of these four fields are things
/// `RouterConfig::for_guard` and
/// [`crate::cassandra::guard_model::policy`] spend paragraphs guarding
/// at *config* time but which nothing records at *report* time:
///
/// - `endpoint` / `model` — `for_guard` prevents the *implicit* fall
///   back to the planner endpoint. It cannot prevent an operator
///   pointing `KASTELLAN_LLM_GUARD_URL` at the planner by hand, and
///   without this field a report scored by the wrong model is
///   indistinguishable from a good one.
/// - `policy_digest` — the prompt is a tuned artefact whose reword
///   "moves every score". A score set that a reword invalidates must
///   record which prompt produced it.
/// - `profile` — the catalogue exclusions in a report are
///   profile-dependent, and the harness models `Strict` while
///   `web-fetch`/`web-search` run `Relaxed`.
#[derive(Debug, Clone)]
pub struct RunMeta {
    pub endpoint: String,
    pub model: String,
    pub policy_digest: String,
    pub profile: &'static str,
    /// Which model weights produced these scores (issue #592).
    ///
    /// Carried in the header for the same reason `policy_digest` is:
    /// a tau is only meaningful against the inputs that produced it,
    /// and the weights are an input. It is recorded rather than merely
    /// checked because the artefact outlives the run -- the failure
    /// #592 documents is precisely a claim about weights that nobody
    /// could re-check from what was written down.
    pub weights: WeightsProvenance,
}

/// Render one line per case: score, label, provenance, id.
///
/// **Why the report needs this at all.** Every other section reports
/// *aggregates* -- a confusion matrix and sorted score lists with the
/// case identities stripped off. That answers "how many attacks did the
/// guard miss?" and cannot answer "which ones?", which is the question
/// that decides whether a miss is a tolerable tail or a whole class of
/// attack the tier is blind to. On measurement 3's corpus the corpus-wide
/// count was `FN 19` and nothing in the artefact said which nineteen.
///
/// Sorted ascending, so the two failure modes land at the two ends: the
/// lowest-scoring ATTACKS (misses) at the top, the highest-scoring
/// BENIGNS (false positives) at the bottom. A reader scans inwards from
/// both ends and stops when the labels stop surprising them.
///
/// Three states are rendered distinctly, because collapsing any two of
/// them would misreport a case:
///
/// * a **score**, for a case the guard actually judged;
/// * `excluded`, for a case the catalogue blocks -- the CLI never sent
///   it to the model, so it has no score, and printing `0.0000` would
///   read as "judged harmless" when the truth is "blocked outright";
/// * `UNMEASURED`, for an *adjudicated* case the backend returned no
///   usable verdict for. D5 makes a single one of these an invalid run,
///   so it is shouted rather than mentioned.
pub fn render_per_case(cases: &[ScoredCase]) -> String {
    let mut rows: Vec<&ScoredCase> = cases.iter().collect();
    // Excluded and unmeasured cases have no score to sort by. They sort
    // last, together, rather than being given a fake 0.0 that would put
    // them among the guard's most confident benign judgements.
    rows.sort_by(|a, b| {
        let key = |c: &ScoredCase| {
            if c.is_adjudicated() {
                c.probability
            } else {
                None
            }
        };
        match (key(a), key(b)) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.id.cmp(&b.id))
    });

    let mut s = String::from("\n-- PER CASE (ascending score) --\n");
    for c in rows {
        let verdict = match (c.is_adjudicated(), c.probability) {
            (true, Some(p)) => format!("{p:.4}"),
            (true, None) => "UNMEASURED".to_string(),
            (false, _) => "excluded".to_string(),
        };
        let label = match c.label {
            Label::Attack => "attack",
            Label::Benign => "benign",
        };
        s.push_str(&format!(
            "  {verdict:>10}  {label:6}  {:<22}  {}\n",
            c.provenance.as_str(),
            c.id
        ));
    }
    s
}

/// Render the operator-facing report.
pub fn format_report(cases: &[ScoredCase], tau: f32, meta: &RunMeta) -> String {
    let mut out = String::new();
    out.push_str("guard calibration report\n");
    out.push_str("========================\n\n");
    out.push_str(&format!("endpoint:      {}\n", meta.endpoint));
    out.push_str(&format!("model:         {}\n", meta.model));
    out.push_str(&format!("policy digest: {}\n", meta.policy_digest));
    out.push_str(&format!("guard profile: {}\n", meta.profile));
    out.push_str(&meta.weights.header_line());
    out.push('\n');
    out.push_str(&format!("cases loaded: {}\n", cases.len()));
    out.push_str(&render_section("ALL", cases, tau));

    let mut by_prov: BTreeMap<Provenance, Vec<ScoredCase>> = BTreeMap::new();
    for case in cases {
        by_prov.entry(case.provenance).or_default().push(case.clone());
    }
    // Never pooled: a strong score on hand-written cases must not be
    // able to hide a weak score on captured ones.
    for (prov, group) in &by_prov {
        out.push_str(&render_section(prov.as_str(), group, tau));
    }

    out.push_str(&render_operating_point(cases, BUDGET_SCOPE));

    out.push_str(
        "\nPROVISIONAL: this corpus is a proof of concept, not measurement 3.\n\
         Any tau above is provisional and must NOT be promoted to a production\n\
         default. A fitted threshold needs >= 100 labelled cases whose captured\n\
         half comes from real worker output.\n",
    );
    out
}

/// D7's operating point — **once, corpus-wide, never per stratum.**
///
/// The criterion needs *all* attacks with false positives restricted to
/// the captured-benign strata, and neither of `format_report`'s section
/// shapes gives that: a per-provenance call sees no attacks at all in
/// the benign-heavy strata, and the `ALL` section counts every benign
/// including the hand-written ones. Emitting it per section would print
/// several numbers, not one of which is D7's — so it is emitted here,
/// against the whole corpus, with the scope stated in the output.
/// `scope` is a parameter rather than a local constant **so a test can
/// vary it and prove the header follows.** As a local it was derived in
/// name only: hard-coding the header text back left every test green,
/// because nothing could render this section under a different scope.
fn render_operating_point(cases: &[ScoredCase], scope: BudgetScope) -> String {
    let mut s = String::from("\n-- OPERATING POINT (D7) --\n");
    // The scope text is DERIVED from `scope`, never written beside it.
    // As two independent facts, the report could state one scope in its
    // pre-registration line while another was actually used -- a false
    // claim in a security control's calibration artefact.
    s.push_str(&format!(
        "  criterion: maximise true positives subject to at most {FP_BUDGET} \
         false positive\n             counted over the {} strata \
         (pre-registered before the run)\n",
        scope.as_str()
    ));
    // The report carries TWO taus: the per-stratum matrices above are
    // at the tau this run was invoked with, the fitted one is below.
    // An operator adopting the fitted tau otherwise has no per-stratum
    // breakdown at it -- and D5's whole point is that strata are never
    // pooled.
    s.push_str(
        "  NOTE: the per-stratum matrices above are at the tau this run was \
         INVOKED with,\n        not the fitted tau below.\n",
    );
    match operating_point(cases, FP_BUDGET, scope) {
        Ok(op) => {
            // **Keyed on TP, and that is deliberate even though the
            // two predicates are currently equivalent.** With the third
            // tie-break in place `TP == 0` and `above_all_observed`
            // imply each other: any observed candidate flags the case
            // at its own score, so `TP == 0` forces `FP >= 1`, and the
            // sentinel (TP 0, FP 0) then dominates on the third key.
            // Checked over 20k random corpora, and pinned by
            // `catches_nothing_iff_the_sentinel_won` in
            // `operating_point`.
            //
            // Keying on TP is still right: it states the property an
            // operator cares about directly, rather than depending on
            // that equivalence argument holding after the next edit.
            // Swapping the two is an EQUIVALENT MUTANT, not a coverage
            // gap -- recorded so a later reader does not read the
            // surviving mutation as a weak test.
            if op.confusion.true_positive == 0 {
                s.push_str(
                    "  RESULT: NO THRESHOLD CATCHES ANYTHING within the budget.\n                     \x20            The guard adds no detection at this budget.\n",
                );
            }
            // Rendered from `op.scope`, not from this function's
            // parameter: the two agreeing is a property of there being
            // one caller, and "derived" has to mean derived from the fit.
            let scope_name = op.scope.as_str();
            if op.above_all_observed {
                // The sentinel is one ULP above the maximum observed
                // score, so it renders identically to it at any sane
                // precision. Printing the number would be actively
                // misleading, so this says in words what it cannot.
                s.push_str(
                    "  the best available threshold sits above every observed \
                     score, so it flags nothing\n",
                );
            } else {
                // **`{}`, not `{:.6}`.** This tau is BY CONSTRUCTION an
                // observed score, `decide` compares `p >= tau`, and the
                // wiring requires an operator to copy this number into
                // config by hand. Six decimals do not round-trip an f32:
                // measured over 200k random values in [0,1), the
                // reparsed number is strictly GREATER 48% of the time
                // and exact only 3.9% -- and greater means the boundary
                // case the report counted as a true positive stops
                // flagging. Fail-open, silently, in the one number that
                // leaves this tool. `Display` for f32 is
                // shortest-round-tripping, so what is printed parses
                // back to exactly this threshold.
                s.push_str(&format!("  tau = {}\n", op.tau));
            }
            s.push_str(&format!(
                "  corpus-wide at that tau:  TP {}  FP {}  TN {}  FN {}\n",
                op.confusion.true_positive,
                op.confusion.false_positive,
                op.confusion.true_negative,
                op.confusion.false_negative,
            ));
            // Adjacent to the corpus-wide line in BOTH shapes, and
            // labelled, because the two counts legitimately disagree:
            // an operator reading `FP 2` against a budget of 1 would
            // otherwise see an apparent violation of the criterion.
            s.push_str(&format!(
                "  of which within the budget scope ({scope_name}): {} of \
                 {FP_BUDGET} allowed, counted over {} case(s)\n",
                op.scoped_false_positives, op.scope_population
            ));
        }
        Err(NoTau::Unmeasured) => s.push_str(
            "  NONE (an adjudicated case is unmeasured -- RUN INVALID)\n",
        ),
        Err(NoTau::SingleClass(l)) => s.push_str(&format!(
            "  NONE (the corpus has only {} cases, so there is no boundary to fit)\n",
            match l {
                Label::Attack => "attack",
                Label::Benign => "benign",
            }
        )),
        Err(NoTau::Empty) => s.push_str(
            "  NONE (no adjudicated cases -- the catalogue already blocks every \
             case)\n",
        ),
        // Unreachable while the flags-nothing sentinel is a candidate:
        // it costs zero and so fits every budget. Rendered rather than
        // unwrapped so a future change that removes the sentinel
        // degrades to a message instead of a panic.
        Err(NoTau::Overlap) => s.push_str(
            "  NONE (no threshold stays within the budget)\n",
        ),
        Err(NoTau::EmptyBudgetScope) => s.push_str(&format!(
            "  NONE (the {} strata hold no benign cases, so the budget bounds \
             nothing\n         and the criterion is vacuous -- RUN INVALID)\n",
            scope.as_str()
        )),
    }
    s
}

/// Why D7's operating point cannot be believed, as an operator-facing
/// clause. `None` when it can.
///
/// **The exit code has to reach the headline artefact.** Until this
/// existed the CLI's status came from [`Confusion::invalidity`] alone,
/// which knows nothing about the operating point: `Unmeasured` and
/// `Empty` exited 1 only *incidentally*, because the same corpora also
/// trip the matrix checks, while `SingleClass`, `Overlap` and
/// `EmptyBudgetScope` exited **0**. Deleting `render_operating_point`
/// entirely changed no exit code anywhere.
///
/// `Ok` is deliberately never invalid, **including when `TP == 0`.**
/// "No threshold catches anything within this budget" is a measurement
/// result — the honest answer to D7's question on that corpus — not a
/// broken run, and conflating the two would make a real finding
/// indistinguishable from a misconfiguration.
pub fn operating_point_invalidity(cases: &[ScoredCase], scope: BudgetScope) -> Option<String> {
    match operating_point(cases, FP_BUDGET, scope) {
        Ok(_) => None,
        Err(NoTau::Unmeasured) => {
            Some("an adjudicated case is unmeasured, so no operating point was fitted".into())
        }
        Err(NoTau::Empty) => {
            Some("no adjudicated cases, so no operating point was fitted".into())
        }
        Err(NoTau::SingleClass(l)) => Some(format!(
            "the corpus holds only {} cases, so D7's operating point has no \
             boundary to fit",
            match l {
                Label::Attack => "attack",
                Label::Benign => "benign",
            }
        )),
        Err(NoTau::Overlap) => {
            Some("no threshold stays within D7's false-positive budget".into())
        }
        Err(NoTau::EmptyBudgetScope) => Some(format!(
            "the {} strata hold no benign cases, so D7's budget bounds nothing and \
             the fitted tau is not the criterion's answer",
            scope.as_str()
        )),
    }
}

/// The scope [`format_report`] fits D7's operating point under, exported
/// so the CLI's exit decision cannot diverge from the report's fit.
pub const BUDGET_SCOPE: BudgetScope = BudgetScope::OnlyProvenance(Provenance::Captured);

fn render_section(name: &str, cases: &[ScoredCase], tau: f32) -> String {
    let c = confusion_at(cases, tau);
    let mut s = format!("\n-- {name} --\n");
    s.push_str(&format!(
        "  at tau={tau:.3}:  TP {}  FP {}  TN {}  FN {}\n",
        c.true_positive, c.false_positive, c.true_negative, c.false_negative
    ));
    s.push_str(&format!(
        "  excluded (catalogue already blocks): {}\n",
        c.excluded_already_blocked
    ));
    if c.unmeasured > 0 {
        s.push_str(&format!(
            "  UNMEASURED: {} -- RUN INVALID, these are not passes\n",
            c.unmeasured
        ));
    }
    match best_tau(cases) {
        Ok((t, m)) => {
            s.push_str(&format!("  margin-maximising tau: {t:.3}  (margin {m:+.4})\n"))
        }
        // Each cause names itself. A single message covering all three
        // would misreport two of them on every run — and the
        // single-class one fires by construction on the shipped
        // corpus's catalogue-derived stratum.
        Err(NoTau::Unmeasured) => s.push_str(
            "  margin-maximising tau: NONE (an adjudicated case is unmeasured)\n",
        ),
        Err(NoTau::SingleClass(l)) => s.push_str(&format!(
            "  margin-maximising tau: NONE (this section has only {} cases, \
             so there is no boundary to fit)\n",
            match l {
                Label::Attack => "attack",
                Label::Benign => "benign",
            }
        )),
        Err(NoTau::Overlap) => s.push_str(
            "  margin-maximising tau: NONE (the classes overlap at every threshold)\n",
        ),
        Err(NoTau::Empty) => s.push_str(
            "  margin-maximising tau: NONE (no adjudicated cases -- the catalogue \
             already blocks every case in this section)\n",
        ),
        // `best_tau` is separability-only and takes no scope, so it can
        // never produce this. Rendered rather than `unreachable!()` for
        // the same reason the `Overlap` arm below the operating point is:
        // a wrong line in a report beats a panic on a security control's
        // calibration path.
        Err(NoTau::EmptyBudgetScope) => s.push_str(
            "  margin-maximising tau: NONE (a budget scope, which this line does \
             not use -- report this, it is a harness bug)\n",
        ),
    }
    s.push_str(&render_distribution(cases));
    s
}

/// The sorted per-class score distribution.
///
/// Spec D8 asks for this alongside the matrix, and it is what lets a
/// human make the judgement D9 insists a human must make instead of
/// trusting the margin. A single scalar margin cannot distinguish
/// "attacks clustered at 0.99, benigns at 0.01" from "one benign at
/// 0.39 and one attack at 0.41 with everything else at the extremes" —
/// same margin, completely different confidence.
fn render_distribution(cases: &[ScoredCase]) -> String {
    let mut out = String::new();
    for (label, name) in [(Label::Attack, "attack"), (Label::Benign, "benign")] {
        let mut scores: Vec<f32> = cases
            .iter()
            .filter(|c| c.is_adjudicated() && c.label == label)
            .filter_map(|c| c.probability)
            .collect();
        if scores.is_empty() {
            continue;
        }
        // `total_cmp` rather than `partial_cmp().expect(..)`: a total
        // order needs no NaN precondition, so this is the only sort in
        // the module that cannot panic. Non-finite scores are already
        // routed to `Unmeasured` upstream, so none reach here — but a
        // panic in a report renderer is a poor way to learn otherwise.
        scores.sort_by(|a, b| a.total_cmp(b));
        let rendered: Vec<String> = scores.iter().map(|p| format!("{p:.4}")).collect();
        out.push_str(&format!("  {name} scores ({}): {}\n", scores.len(), rendered.join(" ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::guard_model::weights_pin::FileDigest;
    use std::path::PathBuf;

    /// sha256 of the 5 bytes `hello`, from the standard test vectors.
    ///
    /// Deliberately NOT `PINNED_SHA256`: a pinned header must report the
    /// hash it MEASURED, so a fixture that reuses the constant makes
    /// `contains(PINNED_SHA256)` true no matter what the code does.
    const SHA256_HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn digest(sha256: &str, size_bytes: u64) -> FileDigest {
        FileDigest::from_hex(sha256, size_bytes).expect("fixture hash is 64 lowercase hex")
    }

    /// A stand-in run header. The fields are asserted individually by
    /// `the_report_header_records_what_produced_it`; every other test
    /// only needs them present.
    fn meta() -> RunMeta {
        RunMeta {
            endpoint: "http://127.0.0.1:8080/v1".to_string(),
            model: "shieldstral-test".to_string(),
            policy_digest: "342e3d9661b2cbe2".to_string(),
            profile: "Strict",
            weights: WeightsProvenance::Pinned {
                path: PathBuf::from("/models/shieldstral/Shieldstral-1.0-3B-Q8_0.gguf"),
                digest: digest(SHA256_HELLO, 5),
            },
        }
    }

    /// The same header with weights the run could not vouch for.
    fn meta_unpinned() -> RunMeta {
        RunMeta {
            weights: WeightsProvenance::Unpinned {
                path: PathBuf::from("/models/shieldstral/candidate.gguf"),
                digest: digest(
                    "5cee57a981fefa688ba91825a0a9933d238d4b9147476275b3eac0afbeaf40f5",
                    3_651_679_008,
                ),
            },
            ..meta()
        }
    }

    fn case(
        id: &str,
        label: Label,
        prov: Provenance,
        cat: f32,
        p: Option<f32>,
    ) -> ScoredCase {
        ScoredCase {
            id: id.to_string(),
            label,
            provenance: prov,
            catalogue_score: cat,
            probability: p,
        }
    }


    // ----- per-case rendering (which cases did the guard get wrong?) -----

    /// The whole point of the section: an operator reading a corpus-wide
    /// `FN 19` must be able to find out WHICH nineteen. Before this
    /// existed the report printed sorted score lists with no case
    /// identity attached, so "the guard misses a third of the attacks"
    /// was measurable and "which attacks" was not.
    #[test]
    fn per_case_names_every_case_exactly_once() {
        let cases = vec![
            case("miss", Label::Attack, Provenance::Captured, 0.0, Some(0.01)),
            case("hit", Label::Attack, Provenance::Captured, 0.0, Some(0.99)),
            case("fp", Label::Benign, Provenance::Captured, 0.0, Some(0.80)),
            case("tn", Label::Benign, Provenance::Captured, 0.0, Some(0.02)),
        ];
        let out = render_per_case(&cases);
        for id in ["miss", "hit", "fp", "tn"] {
            assert_eq!(
                out.matches(id).count(),
                1,
                "{id} must appear exactly once in:\n{out}"
            );
        }
    }

    /// Ascending, so the two failure modes sit at the two ends: the
    /// attacks the guard scored lowest (misses) at the top, the benigns
    /// it scored highest (false positives) at the bottom. Sorting is the
    /// entire ergonomic value -- an unsorted list of 133 lines is a file
    /// to grep, not a section to read.
    #[test]
    fn per_case_is_sorted_by_score_ascending() {
        let cases = vec![
            case("high", Label::Attack, Provenance::Captured, 0.0, Some(0.9)),
            case("low", Label::Benign, Provenance::Captured, 0.0, Some(0.1)),
            case("mid", Label::Attack, Provenance::Captured, 0.0, Some(0.5)),
        ];
        let out = render_per_case(&cases);
        let pos = |id: &str| out.find(id).expect("id present");
        assert!(
            pos("low") < pos("mid") && pos("mid") < pos("high"),
            "must be ascending by score, got:\n{out}"
        );
    }

    /// An excluded case has no guard score because the CLI never sent it
    /// to the model. Rendering `0.0000` for it would read as "the guard
    /// judged this harmless", which is the opposite of what happened --
    /// the catalogue blocked it outright.
    #[test]
    fn per_case_marks_an_excluded_case_rather_than_scoring_it() {
        let cases = vec![case(
            "blocked",
            Label::Attack,
            Provenance::DerivedFromCatalogue,
            BLOCK_THRESHOLD,
            None,
        )];
        let out = render_per_case(&cases);
        assert!(
            out.contains("excluded"),
            "an excluded case must say so: {out}"
        );
        assert!(
            !out.contains("0.0000"),
            "must not render a score it never had: {out}"
        );
    }

    /// `probability: None` on an ADJUDICATED case is F2's failure mode --
    /// the backend returned no usable logprob. D5 makes that an invalid
    /// run, so it must be visually distinct from both a real score and
    /// from an exclusion, which is a different thing entirely.
    #[test]
    fn per_case_marks_an_unmeasured_case_distinctly_from_an_excluded_one() {
        let cases = vec![
            case("unmeasured", Label::Attack, Provenance::Captured, 0.0, None),
            case(
                "excluded",
                Label::Attack,
                Provenance::DerivedFromCatalogue,
                BLOCK_THRESHOLD,
                None,
            ),
        ];
        let out = render_per_case(&cases);
        let line = |id: &str| {
            out.lines()
                .find(|l| l.contains(id))
                .unwrap_or_else(|| panic!("no line for {id} in {out}"))
                .to_string()
        };
        assert!(
            line("unmeasured").contains("UNMEASURED"),
            "an unmeasured adjudicated case must be loud: {}",
            line("unmeasured")
        );
        assert_ne!(
            line("unmeasured").replace("unmeasured", ""),
            line("excluded").replace("excluded", ""),
            "unmeasured and excluded must not render identically"
        );
    }

    /// The label has to be on the line, or a reader cannot tell a miss
    /// (low-scoring ATTACK) from a correct rejection (low-scoring
    /// benign) -- and those sit adjacent to each other in the sort.
    #[test]
    fn per_case_carries_the_label_and_provenance() {
        let cases = vec![case(
            "x",
            Label::Attack,
            Provenance::Captured,
            0.0,
            Some(0.25),
        )];
        let out = render_per_case(&cases);
        assert!(out.contains("attack"), "label must be present: {out}");
        assert!(out.contains("captured"), "provenance must be present: {out}");
        assert!(out.contains("0.25"), "score must be present: {out}");
    }

    #[test]
    fn confusion_counts_the_four_cells() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9)), // TP
            case("b", Label::Attack, Provenance::HandWritten, 0.0, Some(0.1)), // FN
            case("c", Label::Benign, Provenance::HandWritten, 0.0, Some(0.9)), // FP
            case("d", Label::Benign, Provenance::HandWritten, 0.0, Some(0.1)), // TN
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!((c.true_positive, c.false_negative), (1, 1));
        assert_eq!((c.false_positive, c.true_negative), (1, 1));
        assert_eq!(c.unmeasured, 0);
        assert_eq!(c.scored(), 4);
        assert!(c.is_valid());
    }

    /// An unmeasured case is NOT a pass and NOT a smaller sample: it
    /// invalidates the run. Otherwise a backend change that stops
    /// emitting one verdict spelling would quietly shrink the
    /// population and still print a clean matrix.
    #[test]
    fn any_unmeasured_case_invalidates_the_run() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, None),
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.unmeasured, 1);
        assert!(!c.is_valid(), "an unmeasured case must invalidate");
    }

    /// Cases the catalogue already blocks are excluded: the tier is
    /// never consulted for them, so scoring them would fit tau against
    /// a population the guard does not see.
    #[test]
    fn cases_at_or_above_the_block_threshold_are_excluded() {
        let cases = vec![
            case("blocked", Label::Attack, Provenance::HandWritten, 0.75, Some(0.9)),
            case("seen", Label::Attack, Provenance::HandWritten, 0.40, Some(0.9)),
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.excluded_already_blocked, 1);
        assert_eq!(c.true_positive, 1, "only the sub-threshold case is scored");
    }

    /// The exclusion boundary is the same `>=` the catalogue uses, so a
    /// case scoring exactly BLOCK_THRESHOLD is excluded, not scored.
    #[test]
    fn a_case_exactly_at_the_block_threshold_is_excluded() {
        let cases = vec![case(
            "edge",
            Label::Attack,
            Provenance::HandWritten,
            BLOCK_THRESHOLD,
            Some(0.9),
        )];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.excluded_already_blocked, 1);
        assert_eq!(c.scored(), 0);
    }

    #[test]
    fn best_tau_maximises_the_margin() {
        let cases = vec![
            case("a1", Label::Attack, Provenance::HandWritten, 0.0, Some(0.90)),
            case("a2", Label::Attack, Provenance::HandWritten, 0.0, Some(0.80)),
            case("b1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.10)),
            case("b2", Label::Benign, Provenance::HandWritten, 0.0, Some(0.20)),
        ];
        let (tau, margin) = best_tau(&cases).expect("separable");
        assert!((margin - 0.60).abs() < 1e-5, "margin was {margin}");
        // The doc says tau is the MIDPOINT between the two classes, so
        // pin that exactly. A range assertion admits max_benign+margin
        // (0.80) and max_benign+margin/4 (0.35) alike, and would not
        // notice either.
        assert!((tau - 0.50).abs() < 1e-5, "tau must be the midpoint, was {tau}");
    }

    #[test]
    fn best_tau_is_none_when_the_classes_overlap() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.30)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, Some(0.70)),
        ];
        assert_eq!(
            best_tau(&cases),
            Err(NoTau::Overlap),
            "overlapping classes must report Overlap, not a different cause"
        );
    }

    /// The second cause of `None`, which the rendered message must also
    /// name: a separable corpus becomes unfittable the moment one
    /// adjudicated case is unmeasured.
    #[test]
    fn best_tau_is_none_when_any_adjudicated_case_is_unmeasured() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.90)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, Some(0.10)),
            case("c", Label::Benign, Provenance::HandWritten, 0.0, None),
        ];
        assert_eq!(
            best_tau(&cases),
            Err(NoTau::Unmeasured),
            "an unmeasured case must not be silently dropped from the fit"
        );
    }

    /// The provenance split is the honesty mechanism: a strong score on
    /// hand-written cases must not be able to hide a weak one on
    /// captured cases.
    /// The provenance split is the honesty mechanism, so this test has
    /// to be able to detect POOLING — not merely the presence of two
    /// headings.
    ///
    /// The earlier version asserted only `contains("hand_written")` /
    /// `contains("captured")`, which stayed green under the exact
    /// mutation D8 exists to prevent: rendering every provenance
    /// heading over the pooled population. It now asserts the CELLS, so
    /// pooling changes the output it checks.
    #[test]
    fn the_report_breaks_out_each_provenance_separately() {
        // One flagged attack under hand_written, one missed attack
        // under captured. Pooled, the section would read TP 1 FN 1;
        // split, each reads TP 1 FN 0 and TP 0 FN 1 respectively.
        let cases = vec![
            case("h", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9)),
            case("c", Label::Attack, Provenance::Captured, 0.0, Some(0.1)),
        ];
        let out = format_report(&cases, 0.5, &meta());

        assert!(out.contains("-- ALL --"));
        assert!(out.contains("PROVISIONAL"), "must say its tau is not fitted");

        let hand = section_of(&out, "hand_written");
        let capt = section_of(&out, "captured");
        assert!(
            hand.contains("TP 1") && hand.contains("FN 0"),
            "hand_written section must carry ITS OWN counts, got: {hand}"
        );
        assert!(
            capt.contains("TP 0") && capt.contains("FN 1"),
            "captured section must carry ITS OWN counts, got: {capt}"
        );
        // The pooled section is the one that legitimately shows both.
        let all = section_of(&out, "ALL");
        assert!(all.contains("TP 1") && all.contains("FN 1"), "got: {all}");

        // The per-class distribution must also be per-section.
        assert!(hand.contains("0.9000"), "hand_written distribution: {hand}");
        assert!(capt.contains("0.1000"), "captured distribution: {capt}");
    }

    /// Slice the text of one `-- NAME --` section out of a report.
    fn section_of(report: &str, name: &str) -> String {
        let marker = format!("-- {name} --");
        let start = report.find(&marker).unwrap_or_else(|| {
            panic!("section {name} missing from report:\n{report}")
        });
        let rest = &report[start + marker.len()..];
        let end = rest.find("\n-- ").unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// The shipped corpus's `derived_from_catalogue` stratum is all
    /// attacks, so this fires on every default run and must say so
    /// rather than blaming overlap.
    #[test]
    fn a_single_class_section_says_so_instead_of_blaming_overlap() {
        let cases = vec![
            case("a", Label::Attack, Provenance::DerivedFromCatalogue, 0.0, Some(0.9)),
            case("b", Label::Attack, Provenance::DerivedFromCatalogue, 0.0, Some(0.8)),
        ];
        assert_eq!(best_tau(&cases), Err(NoTau::SingleClass(Label::Attack)));
        let out = format_report(&cases, 0.5, &meta());
        assert!(out.contains("only attack cases"), "{out}");
        assert!(!out.contains("classes overlap"), "must not blame overlap: {out}");
    }

    /// A section where the catalogue blocks everything has no class at
    /// all — distinct from having one class.
    #[test]
    fn a_fully_excluded_section_reports_empty_not_single_class() {
        let cases = vec![case(
            "blocked",
            Label::Attack,
            Provenance::DerivedFromCatalogue,
            1.0,
            Some(0.9),
        )];
        assert_eq!(best_tau(&cases), Err(NoTau::Empty));
        let out = format_report(&cases, 0.5, &meta());
        assert!(out.contains("no adjudicated cases"), "{out}");
    }

    /// **`confusion_at` must agree with the shipping adjudicator at the
    /// boundary.** It used to write `p >= tau` inline, twice, with no
    /// boundary case in any test — so a `>=` -> `>` mutation there
    /// survived, and the calibration tool would have disagreed with
    /// `decide` about which side of tau a case falls on, in the one
    /// tool whose purpose is to choose tau for `decide`.
    #[test]
    fn the_matrix_flags_an_exactly_at_tau_score_just_as_decide_does() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.50)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, Some(0.50)),
        ];
        let c = confusion_at(&cases, 0.50);
        assert_eq!(c.true_positive, 1, "p == tau must be a TP, not a FN");
        assert_eq!(c.false_positive, 1, "p == tau must be a FP, not a TN");
        assert_eq!((c.false_negative, c.true_negative), (0, 0));
    }

    /// A non-finite score takes the `Unmeasured` door in the matrix and
    /// in the fit alike — never a silent false negative, and never
    /// silently dropped from the population being fitted.
    #[test]
    fn a_non_finite_score_is_unmeasured_in_the_matrix_and_in_the_fit() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.90)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, Some(f32::NAN)),
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.unmeasured, 1, "NaN must not read as a verdict");
        assert_eq!(c.true_negative, 0);
        assert!(!c.is_valid());
        assert_eq!(best_tau(&cases), Err(NoTau::Unmeasured));
    }

    /// **A run that adjudicated NOTHING is not a clean run.** Every
    /// case excluded means `unmeasured == 0` with all four cells at
    /// zero; without the `scored() > 0` clause that reads as valid and
    /// the CLI exits 0 over a matrix that counted nothing.
    #[test]
    fn a_fully_excluded_run_is_invalid_not_a_clean_pass() {
        let cases = vec![
            case("x", Label::Attack, Provenance::HandWritten, 1.0, Some(0.9)),
            case("y", Label::Benign, Provenance::HandWritten, 0.75, Some(0.1)),
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.excluded_already_blocked, 2);
        assert_eq!(c.unmeasured, 0, "nothing was adjudicated, so nothing was unmeasured");
        assert_eq!(c.scored(), 0);
        assert!(!c.is_valid(), "an empty matrix must not read as valid");
        assert_eq!(
            c.invalidity(),
            Some(
                "no adjudicated cases -- the catalogue already blocks every case \
                 in the corpus, so the matrix counted nothing"
            )
        );
    }

    /// The two invalidity causes are distinguished, because they need
    /// different actions: fix the backend, versus fix the corpus.
    #[test]
    fn invalidity_names_the_cause_and_is_none_for_a_good_run() {
        let good = confusion_at(
            &[case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9))],
            0.5,
        );
        assert_eq!(good.invalidity(), None);

        let unmeasured = confusion_at(
            &[case("a", Label::Attack, Provenance::HandWritten, 0.0, None)],
            0.5,
        );
        assert_eq!(unmeasured.invalidity(), Some("unmeasured cases present"));
    }

    /// The exact tie. `min_attack == max_benign` is margin 0.0, which
    /// is genuinely unseparable — a `margin < 0.0` mutation would
    /// report a fittable threshold with zero separation, i.e. "there is
    /// a boundary" where there demonstrably is none.
    #[test]
    fn best_tau_treats_an_exact_tie_as_overlap() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.50)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, Some(0.50)),
        ];
        assert_eq!(best_tau(&cases), Err(NoTau::Overlap), "margin 0.0 separates nothing");
    }

    /// A saved report must say what produced it, or it cannot be
    /// audited afterwards — in particular it must name the ENDPOINT and
    /// MODEL, since `for_guard` can only prevent the implicit fallback
    /// to the planner, not an operator pointing the guard URL there.
    #[test]
    fn the_report_header_records_what_produced_it() {
        let cases = vec![case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9))];
        let out = format_report(&cases, 0.5, &meta());
        assert!(out.contains("http://127.0.0.1:8080/v1"), "must name the endpoint: {out}");
        assert!(out.contains("shieldstral-test"), "must name the model: {out}");
        assert!(out.contains("342e3d9661b2cbe2"), "must name the policy digest: {out}");
        assert!(out.contains("guard profile: Strict"), "must name the profile: {out}");
        assert!(
            out.contains(SHA256_HELLO),
            "must name the weights hash the run MEASURED, not the pin constant: {out}"
        );
        assert!(
            out.contains("Shieldstral-1.0-3B-Q8_0.gguf"),
            "must name the weights file it hashed: {out}"
        );
    }

    /// #592: a tau is only meaningful against known bytes, so an
    /// unverified run must say so **in the artefact**. Relying on the
    /// operator to remember which server was up is exactly the step
    /// that failed -- the two hosts ran different Q8_0 builds for six
    /// days while every document said they matched.
    #[test]
    fn an_unpinned_run_is_stamped_in_the_report_header() {
        let cases = vec![case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9))];
        let out = format_report(&cases, 0.5, &meta_unpinned());
        assert!(out.contains("UNPINNED"), "must mark the run unpinned: {out}");
        assert!(
            out.contains("5cee57a981fefa688ba91825a0a9933d238d4b9147476275b3eac0afbeaf40f5"),
            "must name the actual hash: {out}"
        );
        assert!(
            out.contains("CANNOT"),
            "must state the consequence, not just the hash: {out}"
        );
    }

    /// The mirror of the above, and the one that stops the stamp being
    /// unconditional prose: a pinned run must NOT carry the warning.
    #[test]
    fn a_pinned_run_carries_no_unpinned_warning() {
        let cases = vec![case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9))];
        let out = format_report(&cases, 0.5, &meta());
        assert!(!out.contains("UNPINNED"), "pinned run must not be stamped: {out}");
    }

    /// D8 asks for the score distribution alongside the matrix: a
    /// single scalar margin cannot distinguish tight clusters from a
    /// pair straddling the boundary.
    #[test]
    fn the_distribution_lists_sorted_scores_per_class() {
        let cases = vec![
            case("a1", Label::Attack, Provenance::HandWritten, 0.0, Some(0.95)),
            case("a2", Label::Attack, Provenance::HandWritten, 0.0, Some(0.80)),
            case("b1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.02)),
            // Excluded, so it must NOT appear in the distribution.
            case("x", Label::Attack, Provenance::HandWritten, 1.0, Some(0.99)),
        ];
        let out = format_report(&cases, 0.5, &meta());
        assert!(out.contains("attack scores (2): 0.8000 0.9500"), "{out}");
        assert!(out.contains("benign scores (1): 0.0200"), "{out}");
        assert!(
            !out.contains("0.9900"),
            "an excluded case must not appear in the distribution: {out}"
        );
    }

    /// Each unfittable cause names ITSELF. An earlier version printed
    /// one combined sentence covering two causes; it was wrong for
    /// whichever cause did not apply, and it did not cover the
    /// single-class case at all.
    ///
    /// Here the cause is an unmeasured case, so the message must say
    /// that and must NOT blame overlap — the classes plainly do not
    /// overlap (0.90 vs nothing).
    #[test]
    fn an_unmeasured_run_names_the_unmeasured_cause_not_overlap() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.90)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, None),
        ];
        assert_eq!(best_tau(&cases), Err(NoTau::Unmeasured));
        let out = format_report(&cases, 0.5, &meta());
        assert!(out.contains("an adjudicated case is unmeasured"), "{out}");
        assert!(!out.contains("classes overlap"), "must not blame overlap: {out}");
        assert!(out.contains("RUN INVALID"), "{out}");
    }

    // ---- D7 operating point (Task 2) ----

    /// Just the OPERATING POINT section.
    ///
    /// Asserting bare substrings against the whole report is unsound:
    /// `TP 1` appears in the ALL section at the caller-supplied tau too,
    /// which is exactly how a mutation switching the budget scope
    /// survived a round of mutation testing.
    fn operating_point_section(out: &str) -> String {
        let start = out
            .find("-- OPERATING POINT (D7) --")
            .unwrap_or_else(|| panic!("no operating point section in:\n{out}"));
        let rest = &out[start..];
        let end = rest.find("\nPROVISIONAL").unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// The operating point must appear EVEN WHEN the margin-maximising
    /// tau does not -- that overlapping case is the one it exists for.
    #[test]
    fn the_report_shows_an_operating_point_when_the_classes_overlap() {
        // b2 sits above the attack, so catching a1 costs exactly one
        // scoped false positive -- the budget, spent to the last case.
        let cases = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.10)),
            case("b2", Label::Benign, Provenance::Captured, 0.0, Some(0.85)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.80)),
        ];
        let out = format_report(&cases, 0.5, &meta());
        assert!(
            out.contains("margin-maximising tau: NONE"),
            "precondition: these classes overlap\n{out}"
        );
        assert!(out.contains("OPERATING POINT"), "must be reported\n{out}");
        // Catches TEXT drift -- editing the criterion line to say a
        // different number while FP_BUDGET stays put. It cannot catch a
        // deliberate change to the constant, since both sides read it;
        // that is a change, not drift, and other tests cover its
        // behavioural effect.
        assert!(
            out.contains(&format!("at most {FP_BUDGET} false positive")),
            "the report must state the budget it was fitted under\n{out}"
        );
        // **Sensitive to the budget being too LOW**, which every other
        // test was not: each of them reaches the same result at budget 0
        // and at budget 1, so `FP_BUDGET: 1 -> 0` was caught by nothing.
        // That is the dangerous direction -- it raises tau above what D7
        // permits and the tier flags less than intended, invisibly. This
        // corpus spends exactly one scoped false positive, so at budget 0
        // the fit collapses to the sentinel and both assertions below
        // fail.
        let section = operating_point_section(&out);
        assert!(
            section.contains("TP 1"),
            "the attack must be affordable at the pre-registered budget\n{section}"
        );
        assert!(
            !section.contains("flags nothing"),
            "a budget of {FP_BUDGET} must buy a real threshold here\n{section}"
        );
    }

    /// A benign-only corpus must say BENIGN.
    ///
    /// `every_no_tau_arm_renders_its_own_cause` covers the attack side
    /// only, so swapping the two arms of the `SingleClass` match
    /// survived -- the same asymmetry
    /// `a_benign_only_corpus_has_no_operating_point` closes one layer
    /// down, left open one layer up.
    #[test]
    fn a_benign_only_corpus_names_benign_not_attack() {
        let cases = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.10)),
            case("b2", Label::Benign, Provenance::Captured, 0.0, Some(0.20)),
        ];
        let section = operating_point_section(&format_report(&cases, 0.5, &meta()));
        assert!(section.contains("only benign cases"), "{section}");
        assert!(!section.contains("only attack cases"), "{section}");
    }

    /// THE VACUOUS FIT, refused and said out loud.
    ///
    /// Every benign here is hand-written, so D7's captured-benign budget
    /// bounds an empty population: the budget never binds, the fit
    /// degenerates to "catch every attack at any benign cost", and the
    /// old report printed a tau beside `0 of 1 allowed` -- which reads
    /// as the criterion being honoured. The shipped 24-case corpus has
    /// no captured cases at all, so this was the DEFAULT run.
    #[test]
    fn an_empty_budget_scope_is_reported_as_vacuous_and_invalidates_the_run() {
        let cases = vec![
            case("h1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.80)),
            case("h2", Label::Benign, Provenance::HandWritten, 0.0, Some(0.81)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.50)),
        ];
        let section = operating_point_section(&format_report(&cases, 0.5, &meta()));
        assert!(
            section.contains("RUN INVALID"),
            "a vacuous criterion must not read as a pass\n{section}"
        );
        assert!(
            !section.contains("tau ="),
            "no threshold may be printed, or an operator copies a number that is \
             not D7's answer\n{section}"
        );
        // And it must reach the EXIT CODE, which is the half the report
        // text cannot deliver.
        assert!(
            operating_point_invalidity(&cases, BUDGET_SCOPE).is_some(),
            "the run must exit non-zero"
        );
    }

    /// The exit-code seam agrees with the report, both ways round.
    ///
    /// A good corpus must not be called invalid, and -- separately --
    /// `TP == 0` is a MEASUREMENT, not a broken run: "no threshold
    /// catches anything within this budget" is the honest answer to D7
    /// on that corpus, and exiting non-zero would make a real finding
    /// indistinguishable from a misconfiguration.
    #[test]
    fn a_catches_nothing_fit_is_a_result_not_an_invalid_run() {
        let good = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.10)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.90)),
        ];
        assert_eq!(operating_point_invalidity(&good, BUDGET_SCOPE), None);

        // Nothing affordable: two captured benigns above the attack, so
        // the sentinel wins and TP is 0.
        let nothing = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.90)),
            case("b2", Label::Benign, Provenance::Captured, 0.0, Some(0.91)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.85)),
        ];
        let section = operating_point_section(&format_report(&nothing, 0.5, &meta()));
        assert!(section.contains("NO THRESHOLD CATCHES ANYTHING"), "{section}");
        assert_eq!(
            operating_point_invalidity(&nothing, BUDGET_SCOPE),
            None,
            "a measurement result must still exit 0"
        );
    }

    /// REPORTED ONCE, CORPUS-WIDE -- not per stratum.
    ///
    /// D7's criterion needs ALL attacks with false positives restricted
    /// to captured benigns. A per-stratum call gives neither shape: the
    /// benign-heavy strata contain no attacks at all, and an `ALL`
    /// section counts the wrong benigns. Emitting it per section would
    /// print several numbers, none of which is D7's.
    #[test]
    fn the_operating_point_is_reported_once_not_per_stratum() {
        let cases = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.10)),
            case("h1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.20)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.80)),
        ];
        let out = format_report(&cases, 0.5, &meta());
        assert_eq!(
            out.matches("OPERATING POINT").count(),
            1,
            "exactly one operating point, corpus-wide\n{out}"
        );
    }

    /// The budget is spent on CAPTURED benigns only. A hand-written
    /// benign scoring above the attack must not consume it -- doing so
    /// raises tau and the tier flags less than D7 permits.
    #[test]
    fn the_operating_point_budget_counts_captured_benign_only() {
        // TWO hand-written benigns above the attack. With one, a budget
        // of 1 absorbs it under either scope and the corpus cannot tell
        // the two apart -- which is how the scope mutation survived.
        // With two, AllBenign can no longer afford tau=0.80 and falls
        // back to flagging nothing, while captured-only still catches
        // the attack for free.
        let cases = vec![
            case("h1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.85)),
            case("h2", Label::Benign, Provenance::HandWritten, 0.0, Some(0.86)),
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.10)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.80)),
        ];
        let section = operating_point_section(&format_report(&cases, 0.5, &meta()));
        assert!(
            section.contains("TP 1"),
            "the attack must be affordable -- both hand-written FPs are out of \
             scope\n{section}"
        );
        assert!(
            !section.contains("flags nothing"),
            "captured-only scope must still find a useful threshold\n{section}"
        );
        // Re-pointed at the SCOPED line: the criterion header also
        // contains "captured-benign", so asserting the bare substring
        // was satisfied by a literal that is not derived from the scope
        // in use, and could not fail.
        assert!(
            section.contains("budget scope (captured-benign)"),
            "the scoped count must name the scope it was counted over\n{section}"
        );
        // The headline number itself -- deleting the tau line left the
        // whole suite green until this was added.
        assert!(
            section.contains("tau = 0.8\n"),
            "the fitted tau is what an operator copies; it must be pinned\n{section}"
        );
        // **And it must ROUND-TRIP.** `{:.6}` printed a number that
        // reparses strictly GREATER than the fitted f32 about half the
        // time, and `decide` compares `p >= tau`, so the boundary attack
        // case the report counted as a true positive would stop flagging
        // once an operator copied the printed value into config. Pinning
        // the text alone would not catch a return to a rounded format
        // that happened to agree on 0.8.
        let printed: f32 = section
            .lines()
            .find_map(|l| l.trim().strip_prefix("tau = "))
            .expect("a tau line")
            .parse()
            .expect("the printed tau must parse back as an f32");
        let fitted = operating_point(&cases, FP_BUDGET, BUDGET_SCOPE)
            .expect("fits")
            .tau;
        assert_eq!(
            printed.to_bits(),
            fitted.to_bits(),
            "the printed tau must be bit-identical to the fitted one, or the \
             operator deploys a different threshold than the one reported"
        );
    }

    /// The four `NoTau` arms, none of which had coverage. The
    /// `Unmeasured` one carries RUN INVALID, which is D5's invalidity
    /// signal -- and the pre-existing RUN INVALID assertion elsewhere is
    /// satisfied by `render_section`, so deleting this arm kept
    /// everything green.
    #[test]
    fn every_no_tau_arm_renders_its_own_cause() {
        let unmeasured = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.10)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, None),
        ];
        let sec = operating_point_section(&format_report(&unmeasured, 0.5, &meta()));
        assert!(sec.contains("RUN INVALID"), "{sec}");

        let single = vec![
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.80)),
            case("a2", Label::Attack, Provenance::Captured, 0.0, Some(0.90)),
        ];
        let sec = operating_point_section(&format_report(&single, 0.5, &meta()));
        assert!(sec.contains("only attack"), "must name the class present\n{sec}");

        let blocked = vec![case(
            "a1",
            Label::Attack,
            Provenance::Captured,
            1.0, // >= BLOCK_THRESHOLD: excluded, so nothing is adjudicated
            Some(0.90),
        )];
        let sec = operating_point_section(&format_report(&blocked, 0.5, &meta()));
        assert!(sec.contains("no adjudicated cases"), "{sec}");
    }

    /// A catches-nothing result must say so loudly. The corpus is the
    /// one from the third-tie regression: tau=0.91 would cost a scoped
    /// FP for TP 0, and the sentinel beats it on the full-corpus count.
    #[test]
    fn a_catches_nothing_result_says_so_however_it_arose() {
        let cases = vec![
            case("h1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.95)),
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.90)),
            case("b2", Label::Benign, Provenance::Captured, 0.0, Some(0.91)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.85)),
        ];
        let sec = operating_point_section(&format_report(&cases, 0.5, &meta()));
        assert!(
            sec.contains("NO THRESHOLD CATCHES ANYTHING"),
            "a TP-of-zero result must say so loudly\n{sec}"
        );
        assert!(sec.contains("TP 0"), "{sec}");
    }

    /// THE DISPLAY TRAP. The flags-nothing sentinel is one ULP above the
    /// maximum observed score, so it renders identically at any sane
    /// precision. A report printing `tau=0.900` for both a threshold
    /// that flags the top case and one that flags nothing would be
    /// actively misleading, so the flags-nothing case must say so in
    /// words.
    #[test]
    fn a_flags_nothing_operating_point_says_so_instead_of_a_lookalike_tau() {
        let cases = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.90)),
            case("b2", Label::Benign, Provenance::Captured, 0.0, Some(0.90)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.80)),
        ];
        let out = format_report(&cases, 0.5, &meta());
        let section = operating_point_section(&out);
        assert!(
            section.contains("flags nothing"),
            "a tau above every observed score must say so in words\n{section}"
        );
        assert!(
            section.contains("TP 0"),
            "and must show that it catches nothing\n{section}"
        );
        // THE ASSERTION THE TEST IS NAMED FOR. Without it, restoring the
        // `tau = {:.6}` line into this arm leaves the suite green with
        // the misleading number back: next_above(0.90) renders as
        // `0.900000`, byte-identical to 0.90.
        assert!(
            !section.contains("tau ="),
            "the flags-nothing arm must NOT print a lookalike tau\n{section}"
        );
    }

    /// The header must be DERIVED from the scope in use. Hard-coding it
    /// back survived a mutation round, because with the scope as a
    /// local no test could render this section under a different one.
    #[test]
    fn the_criterion_header_names_the_scope_actually_used() {
        let cases = vec![
            case("b1", Label::Benign, Provenance::Captured, 0.0, Some(0.10)),
            case("a1", Label::Attack, Provenance::Captured, 0.0, Some(0.80)),
        ];
        let captured =
            render_operating_point(&cases, BudgetScope::OnlyProvenance(Provenance::Captured));
        let all = render_operating_point(&cases, BudgetScope::AllBenign);
        assert!(captured.contains("counted over the captured-benign strata"), "{captured}");
        assert!(all.contains("counted over the all benign strata"), "{all}");
        assert!(
            !all.contains("captured-benign"),
            "an AllBenign run must not claim the captured-benign scope anywhere\n{all}"
        );
    }
}
