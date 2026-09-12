//! tool_host/post_process: the post-`worker.call` half of the dispatch
//! chokepoint.
//!
//! Lifted out of [`super::dispatch_with_sink`] (Item 9b prod-split) so the
//! parent module stays under the LOC cap. It was byte-identical at that
//! point; the guard-model wiring slice then added tier 2, so diffing this
//! against `dispatch_with_sink`'s history no longer shows unchanged logic.
//! It runs, in order:
//!
//! 1. **python-exec output secret-scrub** — for a worker that runs
//!    agent-authored code, redact every secret materialized into this
//!    dispatch's params out of the result before it is screened, audited, or
//!    returned. No-op (byte-identical) for every other worker.
//! 2. **Prompt-injection output screen** — [`screen_result`]: the
//!    deterministic catalogue, then (when a tier is configured and the
//!    catalogue allowed) the Shieldstral guard model. A placeholder is
//!    substituted on a Block so the planner gets an intelligible "withheld"
//!    signal.
//! 3. **Audit-emission arms** — the tool row (carrying the placeholder on a
//!    Block, and the `guard` sub-object whenever the tier ran), one
//!    `policy / secret.redeemed` row per substitution, and the forensic
//!    `policy / injection.blocked` row on a Block. All best-effort (a
//!    transient audit-insert failure is logged, never propagated).
//!
//! **The two screening tiers are ordered, and the order is a security
//! property.** A catalogue Block short-circuits: the model is not consulted,
//! so it cannot turn a decision that has already been made back into an
//! allow. The tier is escalate-up only — see
//! [`crate::cassandra::guard_model::tier`].

use sha2::{Digest, Sha256};

use kastellan_protocol::client::ClientError;

use super::{injection_blocked_placeholder, secret_scrub, AuditSink, ToolHostError};
use crate::cassandra::guard_model::tier::{consults_model, GuardReport};
use crate::cassandra::guard_model::SharedGuardTier;
use crate::cassandra::injection_guard::{
    extract_scannable_text, screen_with_profile, GuardProfile, InjectionDecision, InjectionVerdict,
    SCAN_BYTE_CAP,
};
use crate::secrets::RedemptionEvent;

/// Which tier withheld a document — the `tier` field on the
/// `policy / injection.blocked` row.
///
/// Both tiers reuse that one event name and carry this as a field rather than
/// splitting into two event names. The operator-facing question is "what was
/// withheld from the planner", and splitting its answer means every forensic
/// query written before this slice silently under-reports the moment the tier
/// is switched on (D5).
pub const TIER_CATALOGUE: &str = "catalogue";
/// See [`TIER_CATALOGUE`].
pub const TIER_GUARD_MODEL: &str = "guard_model";

/// Everything the audit arms need about a withheld document.
struct BlockedMeta {
    /// [`TIER_CATALOGUE`] or [`TIER_GUARD_MODEL`].
    tier: &'static str,
    verdict: InjectionVerdict,
    /// The text that was scanned — hashed for the forensic row, never
    /// written to any audit column in the clear.
    body: String,
    truncated: bool,
}

/// What the screening step concluded about one worker result.
struct ScreenOutcome {
    /// What the caller gets: the worker's own value, or the placeholder.
    value: serde_json::Value,
    /// `Some` iff the document was withheld.
    blocked: Option<BlockedMeta>,
    /// `Some` iff the guard tier actually ran — on a **cleared** document as
    /// well as a flagged one, which is the point of D5: recording `p` on the
    /// cleared half is what makes production the source of a real-world score
    /// distribution rather than a catalogue-selected corpus.
    guard: Option<GuardReport>,
}

/// Screen one worker result through both tiers.
///
/// Holds the policy so [`finalize`] does not: that function awaits, emits and
/// returns, and every decision about *what* to withhold lives here or in the
/// pure functions it calls.
///
/// `guard` is `None` when no tier is configured. That case is reported **once
/// at boot** rather than per dispatch — a per-call line on the chokepoint hot
/// path is its own denial of service (slice-1 D1).
async fn screen_result(
    tool: &str,
    guard: Option<&SharedGuardTier>,
    value: serde_json::Value,
) -> ScreenOutcome {
    let (body, truncated) = extract_scannable_text(&value, SCAN_BYTE_CAP);
    // Per-tool sensitivity (issue #142): doc-fetching net workers use the
    // Relaxed profile so quoted chat-template tokens in fetched documentation
    // do not auto-Block; every other worker (incl. shell-exec and any unknown)
    // stays Strict, fail-closed.
    let verdict = screen_with_profile(&body, GuardProfile::for_tool(tool));

    // ── Tier 1: the catalogue. A Block short-circuits. ──
    //
    // The model is not consulted here, and that is a security property rather
    // than an optimisation: it saves seconds per document, and it means a model
    // that says "clear" can never appear to overturn a decision the catalogue
    // has already made.
    if matches!(verdict.decision, InjectionDecision::Block) {
        // The placeholder carries a human-readable `note` string — the only
        // field the planner-summary render surfaces (extract_scannable_text
        // emits string leaves only), so the planner gets an intelligible
        // "withheld" signal rather than a silent gap (#340). Structured fields
        // stay for audit-shape parity with fetch_screen.
        let value = injection_blocked_placeholder(verdict.score, &verdict.reason_codes);
        return ScreenOutcome {
            value,
            blocked: Some(BlockedMeta { tier: TIER_CATALOGUE, verdict, body, truncated }),
            guard: None,
        };
    }

    // ── Tier 2: the guard model. ──
    //
    // `consults_model` delegates to the catalogue's own `decision_for_score`
    // rather than re-testing the threshold, so the two cannot drift about where
    // it is. It is asserted here as well as branched on above because the
    // short-circuit is what makes it true. Note this is a CANARY, not a control:
    // `debug_assert!` compiles out of the release build the daemon actually
    // runs, so what makes the short-circuit correct is the `return` above --
    // this only catches a reordering during development.
    debug_assert!(
        consults_model(verdict.score),
        "the catalogue-Block short-circuit above should already have returned"
    );
    let Some(tier) = guard else {
        return ScreenOutcome { value, blocked: None, guard: None };
    };
    // A result with no scannable text is not a document. `extract_scannable_text`
    // emits string leaves only, so `{"ok": true, "count": 3}` — the ordinary
    // shape for `kv.*` and for most workers' structured replies — arrives here
    // as `""`. Asking the model about it costs a round trip on EVERY such
    // dispatch and puts an undefined verdict on an empty `<Document>` in the
    // decision path: a `p >= tau` there would withhold a result that contained
    // nothing to inject. It would also seed D5's score distribution — the
    // corpus this slice exists to collect — with scores for empty documents.
    //
    // The door is NAMED, not silent: `guard: None` would spell this the same
    // way as an unconfigured host.
    if body.is_empty() {
        return ScreenOutcome { value, blocked: None, guard: Some(tier.no_scannable_text()) };
    }
    let report = tier.adjudicate_document(&body, truncated).await;
    if report.outcome.blocks() {
        // The planner sees the SAME note whichever tier withheld the document:
        // its available action is identical, and naming the tier would tell a
        // compromised planner which defence fired. The structured fields do
        // differ, because they must stay truthful — a guard Block carries the
        // model's `p`, not the catalogue's sub-threshold score and its
        // (possibly empty) class list.
        //
        // `p` is always `Some` here: `GuardOutcome::Block` is reachable only
        // from `GuardAdjudication::Flagged`, which `decide` returns only for a
        // finite `Some(p)`. The fallback is defensive and deliberately 1.0 —
        // the most-blocked value — so a refactor that broke that invariant
        // would over-report severity rather than under-report it.
        let value = injection_blocked_placeholder(report.p.unwrap_or(1.0), &[TIER_GUARD_MODEL]);
        return ScreenOutcome {
            value,
            blocked: Some(BlockedMeta { tier: TIER_GUARD_MODEL, verdict, body, truncated }),
            guard: Some(report),
        };
    }
    ScreenOutcome { value, blocked: None, guard: Some(report) }
}

/// Finalize a dispatch after `worker.call` has returned: scrub + screen the
/// result, then emit the audit rows, and return the caller-facing value (the
/// `injection_blocked_placeholder` on a Block, the worker's own value on Allow,
/// the worker's error on a call failure).
///
/// `elapsed_ms` is measured by the caller immediately after `worker.call`
/// returns so the audit rows carry the true dispatch latency; `req_for_audit`
/// is the pre-substitution snapshot (issue #147) — its opaque `secret://` refs
/// are still present for scrub fingerprinting and are what the tool row records.
///
/// `guard` is the model tier, threaded from the dispatcher exactly as `vault`
/// is (wiring-spec D3).
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize(
    sink: &dyn AuditSink,
    vault: &crate::secrets::Vault,
    guard: Option<&SharedGuardTier>,
    tool: &str,
    method: &str,
    req_for_audit: &serde_json::Value,
    redemption_events: &[RedemptionEvent],
    call_result: Result<serde_json::Value, ClientError>,
    elapsed_ms: u64,
) -> Result<serde_json::Value, ToolHostError> {
    // Prompt-injection screen on successful results. Errors are not
    // text-channel content (the planner sees them as failure codes,
    // not as text), so they can't carry injection — skip.
    // Fingerprints of every secret materialized into THIS dispatch's params
    // (`req_for_audit` is the pre-substitution snapshot, so its `secret://`
    // refs are still present). Shared by both arms below.
    let fps = secret_scrub::fingerprints_for_dispatch(req_for_audit, vault);
    let (final_result, screened) = match call_result {
        Ok(mut v) => {
            // ── python-exec output secret-scrub (design 2026-06-17). ──
            // For a worker that runs agent-authored code, redact every secret
            // materialized into THIS dispatch's params out of the result before
            // it is screened, audited (tool row + JSONL mirror), or returned to
            // the operator's InvokeReport. No-op (byte-identical) for every other
            // worker and for any call with no scannable secrets. `req_for_audit`
            // is the pre-substitution snapshot, so its `secret://` refs are still
            // present for fingerprinting.
            //
            // Security audit 2026-09-02 (finding H1): the scrub now runs for
            // EVERY tool, and on the error arm below too. The old gate
            // (`python-exec` only) reasoned about which worker to trust with a
            // secret, not about where the worker's bytes end up — and they
            // end up in the planner prompt (`StepOutcome::detail`, the
            // plan-so-far summary) and in `audit_log` (`payload.result` /
            // `payload.err`) plus its JSONL mirror. shell-exec's honest
            // `argv[0] "…" not in allowlist` denial therefore handed a
            // substituted `secret://` ref's plaintext straight to the LLM.
            // `fps` is empty when the call redeemed nothing, so the common
            // case costs one JSON walk. Verbatim-contiguous only (an encoded
            // or split secret evades it — `leak-scan` documents that); this is
            // the floor, not the per-tool secret policy.
            if !fps.is_empty() {
                let hits = secret_scrub::scrub_result_value(&mut v, &fps);
                secret_scrub::emit_scrub_audit(sink, tool, &hits).await;
            }

            let outcome = screen_result(tool, guard, v).await;
            (Ok(outcome.value), Some((outcome.blocked, outcome.guard)))
        }
        Err(mut e) => {
            // Same scrub on the error: an `RpcError` message/data (or a
            // mismatched-id echo) is rendered into the audit `err` string and
            // the planner-bound step detail exactly like a result would be.
            if !fps.is_empty() {
                let hits = secret_scrub::scrub_client_error(&mut e, &fps);
                secret_scrub::emit_scrub_audit(sink, tool, &hits).await;
            }
            (Err(e), None)
        }
    };
    let (blocked_meta, guard_report) = screened.unwrap_or((None, None));

    // Tool audit row (existing) — now carrying the placeholder on Block, and
    // the `guard` sub-object whenever the tier ran. No new rows: the row count
    // per dispatch is unchanged.
    let actor = format!("tool:{tool}");
    // Shape and key spellings live in `build_tool_audit_payload`, which is
    // pure and therefore testable against `kastellan-db`'s truncation rule
    // without a worker or a database (issue #617). The two keys the db
    // crate READS BACK (`req`, `guard`) are spelled as that crate's own
    // `const`s, so a rename there is a compile error here rather than a
    // silent stop to preservation past the payload cap; `ms`, `result` and
    // `err` are local literals, because nothing across the crate boundary
    // looks them up.
    let audit_payload = match &final_result {
        Ok(v) => build_tool_audit_payload(
            req_for_audit,
            Ok(v),
            guard_report.as_ref().map(|r| r.audit_value()),
            elapsed_ms,
        ),
        // The error arm never carries a guard verdict: an error is not
        // text-channel content, so the screen above skipped it entirely.
        Err(e) => build_tool_audit_payload(req_for_audit, Err(&e.to_string()), None, elapsed_ms),
    };
    // ── Emit `secret.redeemed` audit rows (one per substitution). ──
    //
    // Best-effort: a transient audit insert failure is logged but
    // does not propagate. The plaintext is already substituted into
    // params and the worker already ran; turning the dispatch into
    // an error because the audit log was unreachable would be worse
    // than missing rows. (Materialize-time audit IS hard-fail; see
    // Vault::materialize and spec §5.4 for the asymmetry rationale.)
    for event in redemption_events {
        let payload = serde_json::json!({
            "tool":     tool,
            "method":   method,
            "ref_hash": event.ref_hash,
            "ms":       elapsed_ms,
        });
        if let Err(e) = sink.insert("policy", "secret.redeemed", payload).await {
            tracing::error!(
                tool = %tool,
                ref_hash = %event.ref_hash,
                error = %e,
                "secret.redeemed audit insert failed"
            );
        }
    }

    if let Err(audit_err) = sink.insert(&actor, method, audit_payload).await {
        tracing::error!(
            tool = %tool,
            method = %method,
            error = %audit_err,
            "audit_log INSERT failed; tool result still propagated"
        );
    }

    // Forensic policy row on Block. SHA-256 of the body that was
    // scanned (which may have been truncated at SCAN_BYTE_CAP).
    // The raw body is never written to any audit column — only the
    // hash, byte length, score, class codes, the deciding `tier`, and
    // (on the guard-model arm) `p` and `tau`.
    if let Some(meta) = blocked_meta {
        let mut hasher = Sha256::new();
        hasher.update(meta.body.as_bytes());
        let body_sha256 = format!("{:x}", hasher.finalize());
        let body_byte_len = meta.body.len();
        let mut policy_payload = serde_json::json!({
            "tool":                    tool,
            "method":                  method,
            // The CATALOGUE score, on both tiers. On a guard Block it is the
            // sub-threshold score the catalogue gave the same body, which is
            // the useful forensic pairing: it says how far apart the two tiers
            // were on this document.
            "score":                   meta.verdict.score,
            "decision":                "block",
            "tier":                    meta.tier,
            "reason_codes":            meta.verdict.reason_codes,
            "body_sha256":             body_sha256,
            "body_byte_len":           body_byte_len,
            "body_truncated_at_64kib": meta.truncated,
        });
        // `p` and `tau` ride on the guard arm only, so a query filtering on
        // their presence separates the tiers even without reading `tier`.
        //
        // They sit at TOP LEVEL here, not under `GUARD_KEY`, so they are
        // NOT carried through `truncate_payload` the way the tool row's
        // `guard` object is -- and `PRESERVED_KEYS`' own argument for
        // admitting only the cleared half leans on this row surviving.
        // Safe because every field above is bounded by construction:
        // `reason_codes` is a deduped set drawn from a fixed catalogue,
        // `body_sha256` is 64 hex chars, and the rest are scalars, so this
        // payload cannot approach `PAYLOAD_MAX_BYTES`. Adding an unbounded
        // field (a body head, an error string) would break that silently
        // -- see the note in `db::audit::PRESERVED_KEYS`.
        if let Some(report) = &guard_report {
            policy_payload["p"] = serde_json::json!(report.p);
            policy_payload["tau"] = serde_json::json!(report.tau);
        }
        if let Err(e) = sink.insert("policy", "injection.blocked", policy_payload).await {
            tracing::error!(
                tool = %tool,
                method = %method,
                error = %e,
                "policy audit insert failed"
            );
        }
    }

    Ok(final_result?)
}

/// Build the `audit_log` payload for one tool dispatch.
///
/// Pure — no I/O, no clock, no global state — so the wire shape the
/// chokepoint stores is unit-testable without a worker, a vault or a
/// database. It was inline in [`finalize`] until issue #617 needed the
/// shape asserted against `kastellan-db`'s truncation rule.
///
/// * `outcome` is `Ok(result)` or `Err(error text)`.
/// * `guard_audit_value` is the guard tier's verdict when it ran. Passed as
///   a plain `Value` rather than a `GuardReport` so a test never has to
///   *construct* one — which would mean booting a guard tier. (It does not
///   remove a dependency: `GuardReport` is same-crate, and this function
///   already names `kastellan_db::audit`'s key `const`s below.)
///
/// `outcome` borrows and `guard_audit_value` is owned because that is what
/// each caller has: [`finalize`] matches on `&final_result`, so it holds
/// only a borrow, while `GuardReport::audit_value` mints a fresh `Value`.
///
/// The request goes under [`kastellan_db::audit::REQ_KEY`] and the verdict
/// under [`kastellan_db::audit::GUARD_KEY`] — both `const`s owned by the
/// crate that reads them, so a rename there is a compile error here rather
/// than a silent change of behaviour past the payload cap.
pub(super) fn build_tool_audit_payload(
    req_for_audit: &serde_json::Value,
    outcome: Result<&serde_json::Value, &str>,
    guard_audit_value: Option<serde_json::Value>,
    elapsed_ms: u64,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        (kastellan_db::audit::REQ_KEY): req_for_audit,
        "ms": elapsed_ms,
    });
    match outcome {
        Ok(v) => payload["result"] = v.clone(),
        Err(text) => payload["err"] = serde_json::Value::String(text.to_string()),
    }
    if let Some(verdict) = guard_audit_value {
        payload[kastellan_db::audit::GUARD_KEY] = verdict;
    }
    payload
}

#[cfg(test)]
mod tests;
