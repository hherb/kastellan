//! Unit tests for [`super`] — the plan-summary renderer.
//!
//! Lifted verbatim (de-indented one level) from the inline `mod tests`
//! block at the tail of `summary.rs`, which sat over the 500-LOC cap.
//! `use super::*` reaches every private item of `summary`.

use super::*;

/// An elidable `Ok` step carrying `text` of the given size.
fn ok(text: &str) -> RenderedStep {
    RenderedStep { text: format!("ok: {text}"), elidable: true }
}
/// A (non-elidable) error step.
fn err(detail: &str) -> RenderedStep {
    RenderedStep { text: format!("err: CODE: {detail}"), elidable: false }
}
fn total(plans: &[Vec<RenderedStep>]) -> usize {
    plans.iter().flatten().map(|s| s.text.len()).sum()
}

/// #559: `workers/mail` attaches a plain-language ordering note to every
/// `mail.search` response, because a successful step reaches the planner as
/// [`extract_scannable_text`]'s output — string values only, **keys
/// discarded**, capped at [`STEP_OK_SUMMARY_MAX`].
///
/// This pins the half that lives in *this* crate: whether such a note is
/// readable at all depends entirely on where its key sorts, because
/// `serde_json::Map` is a `BTreeMap` here and a full page of hits exhausts
/// the 4 KiB budget by itself. `ordering_note` sorts before `results` and
/// survives; the tidier-sounding `sort_applied` sorts after it and is
/// silently clipped — the same trap that swallowed #536's repair advice.
///
/// Deliberately structural rather than a check on the worker's exact
/// sentence: `core` owns the cap, `workers/mail` owns the wording, and
/// there is no shared constant between them (a leaf worker must not gain a
/// dependency edge to `core`). `workers/mail`'s own
/// `ordering_key_sorts_before_results` fails first if the key is renamed.
#[test]
fn a_note_keyed_before_results_survives_the_planner_head_cap() {
    let hits: Vec<serde_json::Value> = (0..50)
        .map(|i| {
            serde_json::json!({
                "message_id": format!("{}", 37000 + i),
                "subject": "x".repeat(200),
                "date": "2026-08-14T12:13:53+00:00",
            })
        })
        .collect();
    let note = "These results are in rank order, NOT date order.";
    let v = serde_json::json!({
        "ordering_note": note,
        "results": hits,
        "sort_applied": "clipped-because-it-sorts-after-results",
    });

    let rendered = render_step_outcome("mail", "mail.search", &StepOutcome::Ok(v));

    assert!(
        rendered.text.contains(note),
        "the ordering note must reach the planner; got {} chars starting {:?}",
        rendered.text.len(),
        rendered.text.chars().take(120).collect::<String>()
    );
    assert!(
        !rendered.text.contains("clipped-because-it-sorts-after-results"),
        "a key sorting after `results` should NOT survive — if this now fits, the \
         cap or the fixture grew and this test has stopped proving anything"
    );
}

#[test]
fn budget_is_a_no_op_when_under() {
    let mut plans = vec![vec![ok("small"), err("nope")]];
    let elided = apply_summary_budget(&mut plans, 1024);
    assert_eq!(elided, 0);
    assert_eq!(plans[0][0].text, "ok: small");
    assert_eq!(plans[0][1].text, "err: CODE: nope");
}

#[test]
fn budget_elides_oldest_ok_heads_first() {
    let big = "y".repeat(1000);
    // plans[0] is the oldest, plans[2] the most recent.
    let mut plans = vec![vec![ok(&big)], vec![ok(&big)], vec![ok(&big)]];
    let elided = apply_summary_budget(&mut plans, 1100);
    assert!(elided >= 1, "expected at least one elision");
    // Oldest elided, most-recent intact.
    assert_eq!(plans[0][0].text, OK_ELIDED_MARKER);
    assert_eq!(plans[2][0].text, format!("ok: {big}"));
    assert!(total(&plans) <= 1100, "total {} over budget", total(&plans));
}

#[test]
fn budget_never_elides_errors_or_decisions() {
    let big = "z".repeat(1000);
    // Oldest step is an error (non-elidable); the elidable Ok is newest.
    let mut plans = vec![vec![err("kept")], vec![ok(&big)]];
    apply_summary_budget(&mut plans, 50);
    assert_eq!(plans[0][0].text, "err: CODE: kept", "error must be preserved");
    assert_eq!(plans[1][0].text, OK_ELIDED_MARKER, "the only elidable head is elided");
}

#[test]
fn budget_never_elides_withheld_marker() {
    let withheld = RenderedStep {
        text: "ok: [withheld: failed injection screen]".to_string(),
        elidable: false,
    };
    let big = "q".repeat(1000);
    let mut plans = vec![vec![withheld], vec![ok(&big)]];
    apply_summary_budget(&mut plans, 50);
    assert_eq!(plans[0][0].text, "ok: [withheld: failed injection screen]");
}

#[test]
fn budget_never_grows_a_tiny_ok() {
    // "ok: 9" is shorter than the elided marker; eliding would *increase*
    // size, so the tiny head is left untouched even under a 0 budget.
    let mut plans = vec![vec![ok("9")]];
    let elided = apply_summary_budget(&mut plans, 0);
    assert_eq!(elided, 0);
    assert_eq!(plans[0][0].text, "ok: 9");
}

#[test]
fn budget_is_idempotent() {
    let big = "y".repeat(1000);
    let mut plans = vec![vec![ok(&big)], vec![ok(&big)], vec![ok(&big)]];
    apply_summary_budget(&mut plans, 1100);
    let snapshot: Vec<Vec<String>> =
        plans.iter().map(|p| p.iter().map(|s| s.text.clone()).collect()).collect();
    let elided_again = apply_summary_budget(&mut plans, 1100);
    assert_eq!(elided_again, 0, "second pass should be a no-op");
    let after: Vec<Vec<String>> =
        plans.iter().map(|p| p.iter().map(|s| s.text.clone()).collect()).collect();
    assert_eq!(snapshot, after);
}

#[test]
fn budget_lands_within_budget_when_all_heads_are_elidable() {
    // Worst case *for the elision pass*: every step is a full-size elidable
    // Ok head, so the budget can always be met. (A summary made entirely of
    // non-elidable errors can exceed the budget — by design, since errors
    // are load-bearing; that path is bounded instead by the per-error
    // `STEP_ERR_DETAIL_MAX` clamp, not by this pass.)
    let big = "y".repeat(STEP_OK_SUMMARY_MAX);
    // 40 plans × one full-size head each — far over a 32 KiB budget.
    let mut plans: Vec<Vec<RenderedStep>> =
        (0..40).map(|_| vec![ok(&big)]).collect();
    apply_summary_budget(&mut plans, PLANS_SUMMARY_BUDGET);
    assert!(
        total(&plans) <= PLANS_SUMMARY_BUDGET,
        "total {} exceeds budget {}",
        total(&plans),
        PLANS_SUMMARY_BUDGET
    );
}

#[test]
fn render_step_outcome_marks_ok_elidable_and_err_not() {
    let ok_step =
        render_step_outcome("shell-exec", "shell.exec", &StepOutcome::Ok(serde_json::json!("hello")));
    assert_eq!(ok_step.text, "ok: hello");
    assert!(ok_step.elidable);

    let err_step = render_step_outcome(
        "shell-exec",
        "shell.exec",
        &StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "no".into() },
    );
    assert_eq!(err_step.text, "err: POLICY_DENIED: no");
    assert!(!err_step.elidable);
}

use crate::workers::web_search::WEB_SEARCH_BATCH_METHOD;

/// A `web.search_batch`-shaped Ok value with `n` per-query elements.
fn batch_value(n: usize) -> serde_json::Value {
    let elements: Vec<serde_json::Value> = (0..n)
        .map(|i| serde_json::json!({ "query": format!("q{i}"), "results": [], "count": 0 }))
        .collect();
    serde_json::json!({ "results": elements })
}

#[test]
fn ok_summary_cap_is_flat_for_non_batch_methods() {
    let v = batch_value(8); // shape is irrelevant for a non-batch method
    assert_eq!(ok_summary_cap("web.search", &v), STEP_OK_SUMMARY_MAX);
    assert_eq!(ok_summary_cap("shell.exec", &v), STEP_OK_SUMMARY_MAX);
    assert_eq!(ok_summary_cap("", &v), STEP_OK_SUMMARY_MAX);
}

#[test]
fn ok_summary_cap_scales_with_query_count() {
    assert_eq!(
        ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(2)),
        2 * BATCH_PER_QUERY_SUMMARY_BYTES
    );
    assert_eq!(
        ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(8)),
        STEP_OK_BATCH_SUMMARY_MAX // 8 * 3 KiB == 24 KiB
    );
}

#[test]
fn ok_summary_cap_clamps_low_and_high() {
    // 1 query → 3 KiB < 4 KiB → clamped up to the single-step floor.
    assert_eq!(ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(1)), STEP_OK_SUMMARY_MAX);
    // 16 queries → 48 KiB → clamped down to the hard ceiling.
    assert_eq!(ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(16)), STEP_OK_BATCH_SUMMARY_MAX);
}

#[test]
fn ok_summary_cap_degrades_to_flat_on_malformed_results() {
    // Missing `results` → 0 elements → floor.
    assert_eq!(ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &serde_json::json!({})), STEP_OK_SUMMARY_MAX);
    // Non-array `results` → 0 → floor.
    assert_eq!(
        ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &serde_json::json!({ "results": "nope" })),
        STEP_OK_SUMMARY_MAX
    );
}

#[test]
fn batch_step_surfaces_more_than_a_single_search_head() {
    // An 8-query × 10-hit batch whose scannable text far exceeds 4 KiB.
    let hit = |i: usize| {
        serde_json::json!({
            "title": format!("title number {i} about a topic"),
            "url": format!("https://example.com/results/{i}"),
            "snippet": "s".repeat(300),
            "engine": "google",
        })
    };
    let elements: Vec<serde_json::Value> = (0..8)
        .map(|q| {
            serde_json::json!({
                "query": format!("query number {q}"),
                "results": (0..10).map(hit).collect::<Vec<_>>(),
                "count": 10,
            })
        })
        .collect();
    let val = serde_json::json!({ "results": elements });

    let batch =
        render_step_outcome("web-search", WEB_SEARCH_BATCH_METHOD, &StepOutcome::Ok(val.clone()));
    let single = render_step_outcome("web-search", "web.search", &StepOutcome::Ok(val));

    // Batch surfaces well over the flat 4 KiB; a single web.search of the
    // SAME value stays at the flat cap (regression pin: single search
    // untouched). Framing = "ok: " (4) + "…" (3 bytes) ≤ 8.
    assert!(
        batch.text.len() > STEP_OK_SUMMARY_MAX,
        "batch head {} should exceed 4 KiB",
        batch.text.len()
    );
    assert!(
        single.text.len() <= STEP_OK_SUMMARY_MAX + 8,
        "single-search head {} should stay ~4 KiB",
        single.text.len()
    );
    // Batch head is bounded by the hard ceiling (+ framing).
    assert!(batch.text.len() <= STEP_OK_BATCH_SUMMARY_MAX + 8);
}
