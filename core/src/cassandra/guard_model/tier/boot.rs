//! Building the guard tier: the boot sequence, and what one adjudication
//! produces (wiring-spec D6/D8/D9).
//!
//! Split from [`super`] so the decisions stay separable from the IO that
//! feeds them. Everything in the parent is pure and testable without a
//! server; everything here needs one, and the split is what lets the arm
//! logic be pinned exhaustively at the unit layer.

use std::sync::Arc;
use std::time::Duration;

use kastellan_llm_router::{RouterConfig, RouterError};

use super::super::context_pin::{self, GuardContextError};
use super::super::timeout::{self, GuardTimeout};
use super::super::GuardClient;
use super::{
    error_kind, outcome_and_score, probe, props_wait, resolve, validate_tau, GuardErrorKind, GuardOutcome,
    GuardReading, TauError,
};

/// Why the guard tier could not be built.
///
/// **Every variant stops the daemon** (wiring-spec D6). The
/// counter-argument — a down daemon protects nothing — was weighed and
/// rejected: "loud error at boot" is precisely the thing that gets
/// scrolled past, and the failure being guarded against is *silent
/// deactivation of a security control*.
///
/// **What these variants do NOT cover, and why [`Self::Required`]
/// exists.** The hazard D6 cites by name is `kastellan-cli install`
/// regenerating `kastellan.env` and dropping hand-added keys — but a
/// regeneration drops *every* hand-added key, not one of a pair, so the
/// realistic outcome is all three guard keys gone. That lands on
/// [`GuardTier::from_router_config`]'s `Ok(None)` arm, which is the one
/// arm that is **not** fatal: the daemon boots clean and screens with
/// the catalogue alone. The variants below cover a *half*-drop;
/// `install`'s own `env_diff` covers the full one; and an operator who
/// wants the control to be load-bearing sets `KASTELLAN_REQUIRE_GUARD=1`
/// and gets [`Self::Required`]. The same shape as
/// `KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR` (#388).
#[derive(Debug)]
pub enum GuardTierError {
    /// The URL/model pair is half-configured, τ is missing while a
    /// guard is configured, or the HTTP client could not be built.
    Config(RouterError),
    /// τ was supplied and is not a usable threshold.
    Tau(TauError),
    /// `/props` could not be reached or parsed, so nothing about the
    /// backend could be verified.
    PropsUnavailable(RouterError),
    /// The backend's context cannot hold a worst-case document.
    Context(GuardContextError),
    /// The operator pinned a timeout that cannot work.
    Timeout(timeout::TimeoutError),
    /// `KASTELLAN_REQUIRE_GUARD=1` is set and no tier is configured.
    Required,
}

impl std::fmt::Display for GuardTierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "guard tier configuration: {e}"),
            Self::Tau(e) => write!(f, "{e}"),
            Self::PropsUnavailable(e) => write!(
                f,
                "the guard tier is configured but its backend's /props is unreachable: \
                 {e}\n(A backend still starting — connection refused, HTTP 503 — is waited \
                 for up to {wait} s first.) The tier cannot be verified, and starting \
                 without it would turn a security control off behind a correct-looking log \
                 line. Start the guard backend — or have `kastellan-cli install` supervise \
                 it by setting KASTELLAN_GUARD_SERVER_BIN and KASTELLAN_GUARD_SERVER_MODEL \
                 in kastellan.env.local — or unset KASTELLAN_LLM_GUARD_URL and \
                 KASTELLAN_LLM_GUARD_MODEL to run without the tier deliberately.",
                wait = props_wait::PROPS_WAIT_BUDGET_MS / 1000
            ),
            Self::Context(e) => write!(f, "{e}"),
            Self::Timeout(e) => write!(f, "{e}"),
            Self::Required => write!(
                f,
                "KASTELLAN_REQUIRE_GUARD is set but no guard tier is configured. \
                 Set KASTELLAN_LLM_GUARD_URL, KASTELLAN_LLM_GUARD_MODEL and \
                 KASTELLAN_LLM_GUARD_TAU, or unset KASTELLAN_REQUIRE_GUARD to run \
                 on catalogue-only screening deliberately."
            ),
        }
    }
}

impl std::error::Error for GuardTierError {}

/// What one adjudication produced, in the shape the audit row wants.
///
/// `p` is `None` on **every** unadjudicated door — a failed call has no
/// score, and an `Unmeasured` one had no usable verdict pair. `tau`
/// rides along so the row is self-describing: a score without the
/// threshold it was compared against cannot be re-read months later.
///
/// The invariant that matters is the biconditional, and
/// [`super::outcome_and_score`] establishes it by deriving `p` from the
/// adjudication rather than forwarding it from the raw call:
///
/// ```text
/// p.is_some()  <=>  !matches!(outcome, AllowUnadjudicated { .. })
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardReport {
    pub outcome: GuardOutcome,
    /// The raw probability, recorded on **cleared** documents as well
    /// as blocked ones (D5). It is a float and carries no document
    /// content.
    pub p: Option<f32>,
    pub tau: f32,
    pub ms: u64,
    /// Bytes of scannable text the model was actually shown.
    ///
    /// Recorded because `p` is uninterpretable without it: a score over
    /// a 1 KiB result and a score over a 64 KiB truncation of a 10 MB
    /// one are different populations, and D5's whole purpose is a score
    /// distribution someone can read later.
    pub body_byte_len: usize,
    /// **Why** the call failed, when it did (issue [#616]).
    ///
    /// `Some` exactly on the [`super::Unadjudicated::RouterError`] door
    /// and `None` everywhere else — including the other two
    /// unadjudicated doors, which did not involve a failing call.
    ///
    /// A closed discriminant, never the backend's error text: the rule
    /// that no backend-controlled message may reach a durable row still
    /// holds, and [`super::GuardErrorKind`] carries no bytes a backend
    /// chose. Without it a timeout, a refused connection and an HTTP 400
    /// were one string, so the fail-open [#612] is about could not be
    /// counted — only inferred, by correlating `router_error` rows
    /// against `body_byte_len` and `ms` across a rotating log.
    ///
    /// [#612]: https://github.com/hherb/kastellan/issues/612
    /// [#616]: https://github.com/hherb/kastellan/issues/616
    pub error_kind: Option<GuardErrorKind>,
    /// Did `SCAN_BYTE_CAP` cut the document short?
    ///
    /// **Recorded on the ALLOW half, which is the half that needed it.**
    /// The forensic `injection.blocked` row has carried
    /// `body_truncated_at_64kib` since Item 30, but only on a Block — so
    /// a `state: "clear"` row on a 10 MB worker result read as "the model
    /// judged this document clear" when the model judged 1.5% of it, and
    /// an unadjudicated 98% was indistinguishable from a working tier.
    pub truncated: bool,
}

impl GuardReport {
    /// The `guard` sub-object for the per-dispatch tool audit row.
    ///
    /// Lives here rather than at the emission site so the field names
    /// are fixed in one place — a forensic query written against them
    /// should not depend on which chokepoint emitted the row.
    pub fn audit_value(&self) -> serde_json::Value {
        serde_json::json!({
            "state":         self.outcome.as_str(),
            "p":             self.p,
            "tau":           self.tau,
            "ms":            self.ms,
            "body_byte_len": self.body_byte_len,
            "truncated":     self.truncated,
            // Emitted on EVERY row, `null` when the call did not fail —
            // the same shape `p` already has, and for the same reason.
            // A key that appears only on failures cannot distinguish "the
            // call succeeded" from "this row predates the field", which
            // is the absence-vs-loss ambiguity #614 spent a branch
            // closing one layer up. So the query for #612's fail-opens is
            // a plain equality: `payload->'guard'->>'error_kind' = 'timeout'`.
            "error_kind":    self.error_kind.map(|k| k.as_str()),
        })
    }
}

/// A configured, verified guard tier.
///
/// Built once at boot and shared; holds no mutable state.
///
/// The manual [`std::fmt::Debug`] prints the tuning, not the client: a
/// `Router` holds a `reqwest::Client` whose debug output is noise, and the
/// three numbers below are the ones anyone debugging this actually wants.
pub struct GuardTier {
    client: GuardClient,
    tau: f32,
    /// The per-request budget and how it was arrived at. Kept whole
    /// rather than reduced to a `Duration` so the boot line can report
    /// the basis, and so a `Clamped::ToCeiling` finding survives to be
    /// logged.
    timeout: GuardTimeout,
    /// The context size the backend reported and D8 accepted. Logged so
    /// a boot line states what was verified rather than what was
    /// required.
    n_ctx: u64,
}

impl std::fmt::Debug for GuardTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardTier")
            .field("tau", &self.tau)
            .field("timeout", &self.timeout)
            .field("n_ctx", &self.n_ctx)
            .finish_non_exhaustive()
    }
}

impl GuardTier {
    /// Build the tier from the router config, verifying the backend.
    ///
    /// The boot sequence, in order, and the order is load-bearing:
    ///
    /// 1. **Tri-state on the URL/model pair.** Neither key set is a
    ///    deliberate opt-out and yields `Ok(None)`.
    /// 2. **τ is required.** A guard configured without one is a
    ///    misconfiguration, not an opt-out — the same argument
    ///    `for_guard` makes about the URL/model pair. There is
    ///    deliberately no default (D1).
    /// 3. **`/props`, then the D8 context check — fatal.** Cheap,
    ///    deterministic, and it is what makes the runtime fail-open on
    ///    HTTP 400 defensible. A backend that is still *starting*
    ///    (connection refused, HTTP 503) is waited for, for up to
    ///    [`props_wait::PROPS_WAIT_BUDGET_MS`], before the refusal (#609).
    /// 4. **The D9 throughput probe — never fatal.** It picks a number;
    ///    it does not verify a control, so it must not undo a boot that
    ///    step 3 already allowed. Skipped entirely when the operator
    ///    pinned `KASTELLAN_LLM_GUARD_TIMEOUT_MS`.
    ///
    /// Two clients are built: the probe spends
    /// [`timeout::PROBE_BUDGET_MS`], which is also what makes
    /// [`timeout::ProbeOutcome::Saturated`] observable — a probe that overruns
    /// its budget arrives as a transport timeout, and that *is* the
    /// measurement.
    pub async fn from_router_config(
        cfg: &RouterConfig,
        cache_buster: &str,
    ) -> Result<Option<Self>, GuardTierError> {
        // Step 1 + 2. `for_guard` owns the URL/model tri-state; the
        // probe budget is a placeholder here and never reaches
        // production, since the client below is rebuilt at the real
        // timeout.
        let probe_budget = Duration::from_millis(timeout::PROBE_BUDGET_MS);
        let probe_client = match GuardClient::from_config(cfg, probe_budget)
            .map_err(GuardTierError::Config)?
        {
            None => return Ok(None),
            Some(c) => c,
        };
        let tau = match cfg.guard_tau {
            None => {
                return Err(GuardTierError::Config(RouterError::Config(
                    "KASTELLAN_LLM_GUARD_URL and KASTELLAN_LLM_GUARD_MODEL are set but \
                     KASTELLAN_LLM_GUARD_TAU is not. The guard tier has no default \
                     threshold on purpose: a provisional value promoted to a default is \
                     an unfitted number wearing the appearance of a sanctioned one. \
                     Measurement 3's fitted value is 0.79552656."
                        .to_string(),
                )))
            }
            Some(t) => validate_tau(t).map_err(GuardTierError::Tau)?,
        };

        // Step 3 — fatal, once a backend that is still starting has had
        // its bounded wait (#609).
        let props = props_wait::wait_for_props(
            || probe_client.props(),
            Duration::from_millis(props_wait::PROPS_WAIT_BUDGET_MS),
            Duration::from_millis(props_wait::PROPS_RETRY_INTERVAL_MS),
        )
        .await
        .map_err(GuardTierError::PropsUnavailable)?;
        let n_ctx = context_pin::verify_guard_context(context_pin::n_ctx_from_props(&props))
            .map_err(GuardTierError::Context)?;

        // Step 4. The probe is never fatal; the operator's own value IS
        // validated, because a pinned 0 would silently disable the tier.
        //
        // The two arms are kept apart rather than folded into one helper
        // taking both: an earlier revision passed a fabricated
        // `ProbeOutcome` on the override path for the callee to ignore,
        // which is a value that means nothing travelling through a
        // signature that implies it means something.
        let timeout = match cfg.guard_timeout_ms {
            Some(ms) => {
                timeout::validate_operator_timeout(ms).map_err(GuardTierError::Timeout)?
            }
            None => {
                let summary = probe::run_probe(&probe_client, cache_buster).await;
                timeout::derive_guard_timeout(&summary)
            }
        };

        let client = GuardClient::from_config(cfg, timeout.timeout)
            .map_err(GuardTierError::Config)?
            .expect("for_guard already returned Some for this config");
        Ok(Some(Self { client, tau, timeout, n_ctx }))
    }

    /// The validated threshold.
    pub fn tau(&self) -> f32 {
        self.tau
    }

    /// The per-request budget and how it was arrived at.
    pub fn timeout(&self) -> &GuardTimeout {
        &self.timeout
    }

    /// The backend context size D8 verified.
    pub fn n_ctx(&self) -> u64 {
        self.n_ctx
    }

    /// Adjudicate one document.
    ///
    /// Returns the outcome, the raw probability when there was one, and
    /// the wall clock — all three of which the audit row carries (D5).
    ///
    /// **`p` is returned on cleared documents too**, and that is the
    /// decision with the most leverage in the slice: recording it makes
    /// production itself the source of a real-world score distribution,
    /// which is the only route out of measurement 3's catalogue-selected
    /// corpus. It is a float and carries no document content.
    ///
    /// Calls `probability()` + `decide()` rather than `adjudicate()`
    /// because the latter discards `p`. `probability` remains the only
    /// request-building path, which is the property that actually
    /// matters.
    pub async fn adjudicate_document(&self, body: &str, truncated: bool) -> GuardReport {
        let started = std::time::Instant::now();
        let probability = self.client.probability(body).await;
        let ms = started.elapsed().as_millis() as u64;
        let (outcome, p, error_kind) = match probability {
            // The mapping is `tier::outcome_and_score`, which derives
            // `p` from the adjudication rather than forwarding it from
            // the call — see there for why that difference is load
            // bearing, and for the invariant it pins.
            Ok(raw) => {
                // **An `Ok(None)` is a fail-open too, and it used to be
                // the silent one.** The call reached the backend and came
                // back 2xx, but carried no usable YES/NO verdict pair, so
                // `decide` takes the `Unmeasured` door and the document
                // goes through unjudged. The `Err` arm below has warned
                // since #616; this arm warned nowhere, which meant the
                // most likely *whole-deployment* failure — logprobs off,
                // the wrong quant served, a chat template that shifts the
                // verdict token — produced a clean boot and a per-dispatch
                // silence. `error_kind` stays `None` because no call
                // FAILED; the door is named by `guard.state = "unmeasured"`,
                // which is what counts these.
                if raw.is_none() {
                    tracing::warn!(
                        target: "kastellan::guard_model",
                        ms,
                        body_byte_len = body.len(),
                        "guard adjudication returned no usable verdict pair; failing OPEN \
                         to catalogue-only screening. The backend answered but produced no \
                         YES/NO logit pair -- check that logprobs are enabled and that the \
                         served model is the calibrated one"
                    );
                }
                let (outcome, p) = outcome_and_score(raw, self.tau);
                (outcome, p, None)
            }
            Err(e) => {
                // The error TEXT is logged here and never carried into
                // the verdict or the row, so no backend message can
                // influence a containment decision. The closed
                // DISCRIMINANT does ride along (issue #616): it carries
                // no bytes a backend chose, and without it every failure
                // mode reads as one `router_error` string and the
                // fail-open #612 is about cannot be counted.
                let kind = error_kind::classify(&e);
                tracing::warn!(
                    target: "kastellan::guard_model",
                    error = %e,
                    error_kind = kind.as_str(),
                    ms,
                    "guard adjudication failed; failing OPEN to catalogue-only screening"
                );
                (resolve(GuardReading::Failed), None, Some(kind))
            }
        };
        GuardReport {
            outcome,
            p,
            tau: self.tau,
            ms,
            body_byte_len: body.len(),
            truncated,
            error_kind,
        }
    }

    /// The report for a result carrying **no scannable text**.
    ///
    /// The model is not asked, and the door is named rather than
    /// silent — see [`super::Unadjudicated::NoScannableText`] for why an
    /// empty `<Document>` must not be sent and why returning no report
    /// at all would be the wrong way to skip it.
    ///
    /// `ms: 0` is honest: no call was made.
    pub fn no_scannable_text(&self) -> GuardReport {
        GuardReport {
            outcome: GuardOutcome::AllowUnadjudicated {
                reason: super::Unadjudicated::NoScannableText,
            },
            p: None,
            tau: self.tau,
            ms: 0,
            body_byte_len: 0,
            truncated: false,
            // No call was made, so there is no failure to classify. The
            // door is named by `outcome`, not here.
            error_kind: None,
        }
    }
}

/// Refuse to run without a tier when the operator demanded one.
///
/// Pure, so the decision is a unit test rather than a daemon boot. The
/// caller reads `KASTELLAN_REQUIRE_GUARD` through the canonical
/// `worker_lifecycle::force_route::env_flag_enabled`, so the truthy
/// spellings match every other daemon-wide opt-in.
///
/// **Deliberately not folded into [`GuardTier::from_router_config`].**
/// That function is about whether a *configured* tier is usable; this is
/// about whether being unconfigured is acceptable on this host, which is
/// a deployment question with a different answer per host. Keeping them
/// apart is also what lets `from_router_config` stay free of env reads.
pub fn require_tier(tier: Option<&GuardTier>, required: bool) -> Result<(), GuardTierError> {
    match (tier, required) {
        (None, true) => Err(GuardTierError::Required),
        _ => Ok(()),
    }
}

/// Share one tier across the dispatcher and whatever else needs it.
pub type SharedGuardTier = Arc<GuardTier>;
