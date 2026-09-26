//! Ordering resolution and planner-facing ordering advice for `mail.search`.
//!
//! # Why this module exists
//!
//! `mail.search` ranks by relevance unless asked otherwise, and relevance order
//! is emphatically not date order. Measured against the live 37k-message archive
//! (issue #559), a `Qantas` search returns hits dated 2019 → 2025 interleaved,
//! with marketing mail scoring above actual booking confirmations. Two live runs
//! asked "what was my *most recent* Qantas flight booking?", neither passed
//! `sort`, and both answered from a relevance ranking — giving two different
//! wrong answers.
//!
//! The capability was never missing: `sort: "date"` exists, works, and was
//! already advertised. What was missing is anything that makes the *default's
//! unsuitability* visible at the moment it matters.
//!
//! # Why the advice rides in the response, not only in the parameter docs
//!
//! Hardening a parameter description is the cheap fix, and this change makes it
//! too (see `core/src/workers/mail.rs`). But on this exact tool that remedy has
//! a measured failure: #536 rewrote `mail.get_message`'s `message_id`
//! description to say "use the literal value from the previous step's output,
//! **not a placeholder**", shipped 2026-08-09 — and the planner still invented a
//! 16-hex `message_id` in both later runs. What *did* work in one of those runs
//! was [`crate::ids`]'s error-time `explain` text: the planner read it and
//! repaired itself on the next plan.
//!
//! So the lever that has actually moved this planner is text delivered **where
//! it reads results**, not text in the advertisement. Hence [`annotate`].
//!
//! # Two constraints the wording and the key name were chosen under
//!
//! When this module was written, a successful step reached the planner through
//! `core`'s `injection_guard::extract_scannable_text`, which kept **only string
//! values, with their keys discarded**, capped at 4 KiB. Since #677 the planner
//! reads a pruned copy of the JSON with its keys kept, so neither constraint is
//! strictly required any more. Both are kept as defence in depth, because the
//! pruned view still drops an object's alphabetically last keys first when a
//! budget forces it to narrow one:
//!
//! 1. **The advice is a self-describing sentence,** so it reads correctly even
//!    if its key is ever lost again. It is prose, so under budget pressure the
//!    view may trim it like any snippet: its point belongs in its first words.
//! 2. **The key sorts early.** `serde_json::Map` is a `BTreeMap` here (no
//!    `preserve_order` feature in this workspace), and a view narrowing an
//!    object keeps its alphabetically first keys, so `ordering_note` outlives
//!    `results` and `sort_applied`. It is not first — `next_cursor` sorts
//!    before it — so a view narrowed to one key keeps only the cursor.
//!    [`ordering_key_sorts_before_results`] pins the part this module controls.
//!    `core`'s `an_ordering_note_reaches_the_planner_wherever_its_key_sorts`
//!    shows the note reaching the planner at the production budget, where no
//!    object is narrowed; nothing tests the narrowed case end to end.
//!
//! # Why paging is a third case and not a default
//!
//! Sending `sort` unconditionally looks tidier, and the first cut did exactly
//! that. It is wrong on a **paging** request: localmail's cursor already encodes
//! its own ordering (measured — a date page yields the keyset cursor
//! `d|<ts>|<id>`, a rank page an opaque `<session>:<page>`), so a defaulted
//! `sort` does not *choose* an ordering there, it *contradicts* one. localmail
//! resolves that by discarding the cursor and silently restarting at page one
//! ([#561](https://github.com/hherb/kastellan/issues/561)) — `http 200`, no
//! warning, and no error text for the `ids::explain`-style repair loop to act on.
//!
//! The bug predates this module and survives it either way, but defaulting a
//! sort would have *constrained how it can be fixed*: it defeats "infer the sort
//! from the cursor when none is given" outright, and turns "reject a mismatch"
//! into a hard error on every paged request the planner did not annotate. So
//! [`plan_sort`] sends nothing when paging, which leaves all of those open.

use serde_json::Value;

/// The sort this worker asks for when the planner names none **and is not
/// paging** — see [`plan_sort`] for why the paging case sends nothing instead.
///
/// localmail's own default is already `rank`, so sending it explicitly changes
/// no result. It is sent anyway so that what we *advertise* as the default and
/// what we *request* are the same fact, established here rather than inherited
/// from another service's unpinned behaviour — and so [`annotate`] can describe
/// the ordering truthfully without assuming anything about the server.
pub const DEFAULT_SORT: &str = "rank";

/// The sort this worker asks for when the planner names none, is not paging,
/// **and the query is blank** — a filter-only search (#698).
///
/// Not a preference: localmail has nothing to rank a textless query against,
/// always answers it date-ordered, and refuses a *stated* `rank` for it with a
/// 400. So defaulting [`DEFAULT_SORT`] here would turn every filter-only search
/// into an error. `date` is the ordering such a query is served with anyway.
pub const TEXTLESS_SORT: &str = "date";

/// Whether `query` has no free text, in the sense localmail uses to pick its
/// textless ordering (`free_text.strip()` is empty there).
///
/// **It cannot see a query made only of search operators** (`has:attachment`,
/// `subject:invoice`): localmail lifts those out before judging, and this
/// worker deliberately does not parse localmail's query language. Such a query
/// still gets [`DEFAULT_SORT`], localmail refuses it, and its refusal — which
/// names `sort: "date"` as the fix — reaches the planner as a repairable error.
/// The advertised way to write a filter-only search is `filters` with no
/// `query`, which this function does see.
pub fn is_textless(query: &str) -> bool {
    query.trim().is_empty()
}

/// Response key carrying [`ordering_note`]'s sentence.
///
/// **Must sort lexicographically before `"results"`** — see the module docs.
pub const ORDERING_KEY: &str = "ordering_note";

/// What to do about `sort` on one `/v1/search` request.
///
/// Three cases, not two — see [`plan_sort`] for why paging is its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortPlan<'a> {
    /// Send `sort: <this>`, and describe that ordering in the note.
    Send(&'a str),
    /// Send **no** `sort` at all: this is a paging request and the cursor
    /// already carries the ordering.
    DeferToCursor,
}

/// Decide the `sort` field for a request, given what the planner asked for,
/// whether it is paging, and the query text.
///
/// Pure. Four cases, checked in this order:
///
/// - **Planner named a sort** → send it verbatim, cursor or not. An explicit
///   request is never second-guessed; unknown values are passed through rather
///   than corrected, because localmail validates the field server-side (a bogus
///   sort is a `422`, measured) and silently rewriting one would answer a
///   question the planner did not ask.
/// - **No sort, but a cursor** → [`SortPlan::DeferToCursor`]. **This is the case
///   that needs the care.** localmail's cursor already encodes its ordering
///   (measured: a date page yields the keyset cursor `d|<ts>|<id>`, a rank page
///   yields an opaque `<session>:<page>`), so defaulting a sort here does not
///   pick an ordering — it *contradicts* one. localmail resolves that
///   contradiction by discarding the cursor and silently restarting at page one
///   ([#561](https://github.com/hherb/kastellan/issues/561)); sending nothing
///   leaves the service free to honour its own cursor.
/// - **Neither, and the query is blank** → [`TEXTLESS_SORT`] (#698). A
///   filter-only search has nothing to rank, and localmail refuses a stated
///   `rank` for it, so the default below would make it an error.
/// - **Neither** → the advertised default, so what we advertise and what we
///   request are the same fact rather than one inherited from another service.
///
/// **Deliberately not done here: detecting a sort/cursor mismatch.** The two
/// cursor formats are distinguishable, so this worker *could* refuse one — but
/// only by parsing an opaque paging token whose shape is localmail's private
/// business and can change with no notice, which would fail silently and late.
/// That check belongs to the service that owns the format (#561).
pub fn plan_sort<'a>(requested: Option<&'a str>, has_cursor: bool, query: &str) -> SortPlan<'a> {
    match requested {
        Some(s) if !s.is_empty() => SortPlan::Send(s),
        _ if has_cursor => SortPlan::DeferToCursor,
        _ if is_textless(query) => SortPlan::Send(TEXTLESS_SORT),
        _ => SortPlan::Send(DEFAULT_SORT),
    }
}

/// One sentence telling the planner what order it is looking at, and what to do
/// about it if that is the wrong order for the question.
///
/// Written for the planner rather than for a log reader, in the same spirit as
/// [`crate::ids`]'s `explain`. The `rank` arm is the load-bearing one: it is the
/// default, so it is the arm that fires on the query that has already gone wrong
/// twice, and it names both the parameter and the value to pass.
///
/// The [`SortPlan::DeferToCursor`] arm is the honest one: this worker is
/// stateless per call, so on a paging request it genuinely does not know which
/// ordering the cursor was issued with, and says so rather than guessing. That
/// matters more than it looks — claiming an ordering here would be exactly the
/// "silent lie to the planner" shape, and the *correct* recovery for a caller
/// that needs a known order is to re-run the search rather than page on.
pub fn ordering_note(plan: SortPlan<'_>) -> String {
    let sort = match plan {
        SortPlan::DeferToCursor => {
            return "These results continue whatever ordering the cursor was issued with — \
                    this tool cannot tell which. For a guaranteed newest-first answer, call \
                    mail.search again with sort: \"date\" instead of paging."
                .to_string()
        }
        SortPlan::Send(s) => s,
    };
    match sort {
        "date" => "These results are in date order, newest first.".to_string(),
        "rank" => "These results are in rank order (best match first), NOT date order: \
                   hits may be from any year and the first hit is not the most recent. \
                   To answer a 'most recent' or 'latest' question, call mail.search \
                   again with sort: \"date\"."
            .to_string(),
        other => format!(
            "These results use the requested sort {other:?}, whose ordering this worker \
             cannot describe. If the question is about recency, call mail.search again \
             with sort: \"date\"."
        ),
    }
}

/// Attach [`ordering_note`] to a `/v1/search` response under [`ORDERING_KEY`].
///
/// A no-op unless `response` is a JSON object, and it never overwrites a key
/// localmail already serves: if the service one day describes its own ordering,
/// that answer is authoritative and this worker's inference is not. Everything
/// else in the response is passed through untouched — the worker reshapes no
/// part of `results`, which is what keeps a fabricated id attributable to the
/// planner rather than to us.
pub fn annotate(response: &mut Value, plan: SortPlan<'_>) {
    let Some(map) = response.as_object_mut() else { return };
    if map.contains_key(ORDERING_KEY) {
        return;
    }
    map.insert(ORDERING_KEY.to_string(), Value::String(ordering_note(plan)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_or_empty_sort_resolves_to_the_advertised_default() {
        assert_eq!(plan_sort(None, false, "qantas"), SortPlan::Send("rank"));
        assert_eq!(plan_sort(Some(""), false, "qantas"), SortPlan::Send("rank"));
    }

    #[test]
    fn an_explicit_sort_is_passed_through() {
        assert_eq!(plan_sort(Some("date"), false, "q"), SortPlan::Send("date"));
        // Not corrected here — localmail 422s an unknown sort (measured live).
        assert_eq!(plan_sort(Some("newest"), false, "q"), SortPlan::Send("newest"));
    }

    /// #561: on a paging request the cursor already carries the ordering, so a
    /// defaulted sort would contradict it and localmail would silently discard
    /// the cursor. Send nothing.
    #[test]
    fn paging_without_a_named_sort_defers_to_the_cursor() {
        assert_eq!(plan_sort(None, true, "q"), SortPlan::DeferToCursor);
        assert_eq!(plan_sort(Some(""), true, "q"), SortPlan::DeferToCursor);
        // A textless page defers too: the cursor still carries the ordering.
        assert_eq!(plan_sort(None, true, ""), SortPlan::DeferToCursor);
    }

    /// An explicit sort is still honoured while paging — the planner may
    /// legitimately be re-ordering, and this worker does not adjudicate the
    /// mismatch (that needs the cursor's format, which is localmail's).
    #[test]
    fn an_explicit_sort_wins_over_a_cursor() {
        assert_eq!(plan_sort(Some("date"), true, "q"), SortPlan::Send("date"));
    }

    /// #698: a filter-only search has no text to rank, and localmail refuses
    /// a *stated* `rank` for it. Defaulting `rank` there turned every
    /// filter-only search into an error, so a blank query defaults to `date` —
    /// the ordering localmail would serve it with anyway.
    #[test]
    fn a_blank_query_without_a_named_sort_resolves_to_date() {
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(plan_sort(None, false, blank), SortPlan::Send("date"), "{blank:?}");
            assert_eq!(plan_sort(Some(""), false, blank), SortPlan::Send("date"), "{blank:?}");
        }
    }

    /// The blank-query default must not override the planner: an explicit
    /// `rank` on a blank query is sent as asked, and localmail's refusal —
    /// which names `sort: "date"` as the repair — reaches the planner.
    #[test]
    fn an_explicit_sort_on_a_blank_query_is_still_passed_through() {
        assert_eq!(plan_sort(Some("rank"), false, ""), SortPlan::Send("rank"));
    }

    /// The paging note must not name an ordering it cannot know.
    #[test]
    fn the_paging_note_admits_it_cannot_name_the_ordering() {
        let note = ordering_note(SortPlan::DeferToCursor);
        assert!(note.contains("cannot tell which"), "{note}");
        assert!(note.contains("\"date\""), "must still offer the recovery: {note}");
        for claim in ["in date order", "in rank order"] {
            assert!(!note.contains(claim), "must not claim an ordering: {note}");
        }
    }

    /// The default's note has to be *actionable*, not merely accurate: #559's
    /// whole point is that `'rank' (default) or 'date'` was already true and
    /// still left the planner with no reason to change anything.
    #[test]
    fn the_rank_note_names_both_the_parameter_and_the_value_to_pass() {
        let note = ordering_note(SortPlan::Send("rank"));
        assert!(note.contains("sort"), "note must name the parameter: {note}");
        assert!(note.contains("\"date\""), "note must name the value: {note}");
        assert!(note.contains("NOT date order"), "note must state the consequence: {note}");
    }

    #[test]
    fn the_date_note_states_newest_first() {
        let note = ordering_note(SortPlan::Send("date"));
        assert!(note.contains("newest first"), "{note}");
    }

    /// An unrecognised sort must not silently claim an ordering this worker
    /// cannot vouch for.
    #[test]
    fn an_unknown_sort_note_admits_it_cannot_describe_the_order() {
        let note = ordering_note(SortPlan::Send("sideways"));
        assert!(note.contains("cannot describe"), "{note}");
        assert!(note.contains("sideways"), "note must quote the sort it got: {note}");
    }

    /// The placement invariant, pinned locally. `serde_json::Map` is a
    /// `BTreeMap` in this workspace, so key order is alphabetical, and when the
    /// planner's pruned view (#677) must narrow an object it keeps the first
    /// keys. Before #677 anything sorting after `results` was clipped outright.
    /// This test is what fails first, in the crate where someone would rename
    /// the key.
    #[test]
    fn ordering_key_sorts_before_results() {
        assert!(
            ORDERING_KEY < "results",
            "{ORDERING_KEY} must sort before `results`, or a narrowed planner view drops it first"
        );
    }

    #[test]
    fn annotate_adds_a_self_describing_sentence() {
        let mut v = json!({"results": [], "next_cursor": null});
        annotate(&mut v, SortPlan::Send("rank"));
        let note = v[ORDERING_KEY].as_str().expect("note must be a string");
        assert_eq!(note, ordering_note(SortPlan::Send("rank")));
    }

    /// If localmail ever describes its own ordering, the service wins.
    #[test]
    fn annotate_never_overwrites_a_key_the_service_already_served() {
        let mut v = json!({"results": [], ORDERING_KEY: "served by localmail"});
        annotate(&mut v, SortPlan::Send("rank"));
        assert_eq!(v[ORDERING_KEY], json!("served by localmail"));
    }

    #[test]
    fn annotate_leaves_a_non_object_response_alone() {
        let mut v = json!(["not", "an", "object"]);
        annotate(&mut v, SortPlan::Send("rank"));
        assert_eq!(v, json!(["not", "an", "object"]));
    }

    /// The whole point of the sentence form: the planner sees values with their
    /// keys stripped, so the text has to carry its own subject.
    #[test]
    fn the_note_is_intelligible_with_its_key_removed() {
        let note = ordering_note(SortPlan::Send("rank"));
        assert!(
            note.starts_with("These results"),
            "note must name its own subject, since the key is discarded: {note}"
        );
    }
}
