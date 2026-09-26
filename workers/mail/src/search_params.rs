//! Getting `mail.search`'s id filters into the shape localmail accepts.
//!
//! Two independent traps, both measured live on 2026-08-17 (task 161, which
//! spent its entire six-iteration budget here and returned a 2022 flight for a
//! question about the most recent one).
//!
//! **1. Where the ids go.** `mail.list_messages` takes `account_ids` and
//! `folder_ids` at the *top level*; `/v1/search` wants them nested inside
//! `filters`. The planner carried the shape across from one tool to its
//! sibling — twice — and got `unknown field \`account_ids\`` both times. That
//! is not a planner mistake so much as an inconsistency in the surface it was
//! given, so the top-level form is now accepted here and folded inward.
//!
//! **2. What type they are.** localmail's `SearchFiltersModel` types both as
//! `list[str]` (`serve/routes/search.py`), so an integer id — the obvious thing
//! to write, and what `mail.list_messages` itself emits into a query string —
//! comes back as a raw FastAPI 422 validation envelope:
//! `{"detail":[{"type":"string_type","loc":["body","filters","account_ids",0],…}]}`.
//! That envelope is not planner-facing advice; it is barely reader-facing. Ids
//! are coerced to their canonical digit-string form here instead.
//!
//! Both forms are *validated*, not merely reshaped: an id that is not an id is
//! refused with [`crate::ids`]'s repair text, so widening the accepted shape
//! does not widen what can reach localmail.

use crate::ids::{IdField, LocalmailId};

/// Fold the top-level id filters into `filters`, and normalise any already
/// nested there, returning the object to send as `filters` (or `None`).
///
/// Naming an id filter in *both* places is refused rather than resolved by
/// precedence: the two could disagree, and silently preferring one would run a
/// search the planner did not ask for while telling it nothing — the same
/// argument `attach::choose` makes about two selectors.
pub fn normalize_filters(
    filters: Option<serde_json::Value>,
    account_ids: Option<Vec<LocalmailId>>,
    folder_ids: Option<Vec<LocalmailId>>,
) -> Result<Option<serde_json::Value>, String> {
    let mut obj = match filters {
        None => serde_json::Map::new(),
        Some(serde_json::Value::Object(m)) => m,
        Some(_other) => {
            return Err("`filters` must be an object, e.g. \
                        {\"account_ids\": [\"1\"], \"has_attachment\": true}."
                .to_string())
        }
    };

    for (field, top) in [(IdField::AccountIds, account_ids), (IdField::FolderIds, folder_ids)] {
        let key = field.name();
        // An explicit `null` is absence, matching `ids::opt_message_id` — the
        // convention this crate sets one module over. Treating it as a shape
        // error would refuse `{"account_ids": null}`, which localmail itself
        // accepts as "no filter", and would make the two optional-parameter
        // spellings disagree for no reason the planner could infer.
        let nested = obj.get(key).filter(|v| !v.is_null());
        match (top, nested) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "`{key}` was given both at the top level and inside `filters` — \
                     pass it once. Either place works."
                ))
            }
            (Some(ids), None) => {
                let as_strings: Vec<serde_json::Value> =
                    ids.iter().map(|i| serde_json::json!(i.to_string())).collect();
                obj.insert(key.to_string(), serde_json::Value::Array(as_strings));
            }
            (None, Some(v)) => {
                // Already nested, but possibly as numbers — which localmail
                // answers with a 422 the planner cannot act on.
                let coerced = crate::ids::id_strings(field, v)?;
                let as_values: Vec<serde_json::Value> =
                    coerced.into_iter().map(|i| serde_json::Value::String(i.to_string())).collect();
                obj.insert(key.to_string(), serde_json::Value::Array(as_values));
            }
            // A `null` placeholder must not reach localmail as a filter value.
            (None, None) => {
                obj.remove(key);
            }
        }
    }

    Ok(if obj.is_empty() { None } else { Some(serde_json::Value::Object(obj)) })
}

/// The keys each `mail.search` hit is projected to — localmail's `fields`
/// (slice E, `api_minor` 3), sent on every search (#760).
///
/// Every hit used to carry eleven keys, and the planner reads a step through a
/// 16 KiB pruned view: #677's real searches came back at 27 KB and 17 KB, so
/// the tail of a `limit: 50` page never reached it. Dropped, with the reason:
///
/// * `folder`, `to` — always `null` / `[]` on the search path (localmail's own
///   census, and seen live 2026-09-26);
/// * `score`, `matched_arms` — ranking internals nothing downstream reads;
/// * `snippet_html` — replaced by `snippet`, the same plain text under an
///   honest name (it never held HTML).
///
/// Kept: the id to fetch with, the account (the search filters on it), and
/// what a reader needs to pick a message — subject, sender, date, whether it
/// has attachments, and a snippet. localmail refuses an unknown name with a
/// 400, so a typo here fails every search loudly rather than dropping a key.
pub const HIT_FIELDS: [&str; 7] =
    ["message_id", "account", "subject", "from", "date", "has_attachments", "snippet"];

/// Snippet window, in characters, sent as `snippet_chars`. localmail's default
/// is 200 and it may add a `…` at each end. 120 keeps enough of a sentence to
/// recognise a message; with [`HIT_FIELDS`] it took two live 50-hit pages from
/// 25.8 KB / 26.6 KB to 18.1 KB / 18.8 KB (Mac archive, 2026-09-26). Must stay within
/// localmail's `1 ..= snippet_max_chars` (default 1000), or every search 400s.
pub const SNIPPET_CHARS: u32 = 120;

#[cfg(test)]
mod tests {
    use super::*;

    /// localmail's permitted names (`api/search_projection.py::HIT_FIELDS`,
    /// slice E spec). An entry outside this set is a 400 on every search.
    const LOCALMAIL_HIT_FIELDS: [&str; 12] = [
        "message_id", "account", "folder", "subject", "from", "to", "date", "snippet_html",
        "has_attachments", "score", "matched_arms", "snippet",
    ];

    #[test]
    fn every_projected_field_is_one_localmail_accepts() {
        for f in HIT_FIELDS {
            assert!(LOCALMAIL_HIT_FIELDS.contains(&f), "localmail would refuse {f:?}");
        }
    }

    /// `message_id` is not implied by localmail's projection (decision 4 of its
    /// spec): leave it out and every hit loses the one value the next step needs.
    #[test]
    fn the_projection_keeps_the_id_every_follow_up_call_needs() {
        assert!(HIT_FIELDS.contains(&"message_id"));
    }

    #[test]
    fn the_projection_drops_the_keys_that_are_always_empty_or_duplicated() {
        for dropped in ["folder", "to", "score", "matched_arms", "snippet_html"] {
            assert!(!HIT_FIELDS.contains(&dropped), "{dropped} should not be sent");
        }
    }

    #[test]
    fn the_snippet_width_is_inside_localmails_default_range() {
        assert!((1..=1000).contains(&SNIPPET_CHARS));
    }
    use serde_json::json;

    /// Build validated ids the way the params struct does.
    fn ids(vals: &[i64]) -> Vec<LocalmailId> {
        #[derive(serde::Deserialize)]
        struct P {
            #[serde(default, deserialize_with = "crate::ids::account_ids")]
            account_ids: Option<Vec<LocalmailId>>,
        }
        let p: P = serde_json::from_value(json!({ "account_ids": vals })).unwrap();
        p.account_ids.unwrap()
    }

    fn as_the_planner_sees_it(s: &str) -> String {
        s.chars().take(kastellan_protocol::STEP_ERR_DETAIL_MAX).collect()
    }

    /// The live shape from task 161, twice: `account_ids` where
    /// `mail.list_messages` takes it. It is now folded into `filters`.
    #[test]
    fn top_level_account_ids_are_folded_into_filters() {
        let out = normalize_filters(None, Some(ids(&[1])), None).unwrap().unwrap();
        assert_eq!(out["account_ids"], json!(["1"]));
    }

    #[test]
    fn top_level_folder_ids_are_folded_into_filters() {
        let out = normalize_filters(None, None, Some(ids(&[7, 9]))).unwrap().unwrap();
        assert_eq!(out["folder_ids"], json!(["7", "9"]));
    }

    /// The other half of task 161: nested but numeric, which localmail answers
    /// with a raw 422. Strings are what its model accepts.
    #[test]
    fn numeric_ids_already_inside_filters_are_coerced_to_strings() {
        let out = normalize_filters(Some(json!({"account_ids": [1, 2]})), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(out["account_ids"], json!(["1", "2"]));
    }

    #[test]
    fn string_ids_inside_filters_are_left_as_strings() {
        let out = normalize_filters(Some(json!({"account_ids": ["1"]})), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(out["account_ids"], json!(["1"]));
    }

    /// Everything that is not an id filter is forwarded untouched — this
    /// function normalises two keys, it does not own the filter vocabulary.
    #[test]
    fn other_filters_are_passed_through_unchanged() {
        let out = normalize_filters(
            Some(json!({"has_attachment": true, "subject": "flight", "account_ids": [1]})),
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(out["has_attachment"], json!(true));
        assert_eq!(out["subject"], json!("flight"));
        assert_eq!(out["account_ids"], json!(["1"]));
    }

    #[test]
    fn no_filters_at_all_stays_absent() {
        assert!(normalize_filters(None, None, None).unwrap().is_none());
    }

    /// An empty object must not become `filters: {}` on the wire — absent and
    /// "filter by nothing" should look the same to localmail.
    #[test]
    fn an_empty_filters_object_collapses_to_absent() {
        assert!(normalize_filters(Some(json!({})), None, None).unwrap().is_none());
    }

    #[test]
    fn naming_an_id_filter_twice_is_refused_rather_than_resolved_by_precedence() {
        let e = normalize_filters(Some(json!({"account_ids": ["1"]})), Some(ids(&[2])), None)
            .unwrap_err();
        assert!(
            as_the_planner_sees_it(&e).contains("account_ids"),
            "must name the doubled parameter: {e}"
        );
    }

    #[test]
    fn a_non_object_filters_is_refused_with_an_example() {
        let e = normalize_filters(Some(json!("account_ids=1")), None, None).unwrap_err();
        let seen = as_the_planner_sees_it(&e);
        assert!(seen.contains("object"), "{seen}");
        assert!(seen.contains("account_ids"), "must show the shape: {seen}");
    }

    /// Widening the accepted shape must not widen what reaches localmail: a
    /// nested value that is not an id is still refused, with `ids`' repair text.
    #[test]
    fn a_nested_non_id_is_still_refused_with_repair_advice() {
        let e = normalize_filters(Some(json!({"account_ids": ["{{account_id}}"]})), None, None)
            .unwrap_err();
        assert!(
            as_the_planner_sees_it(&e).contains("NO template substitution"),
            "got: {e}"
        );
        let e2 =
            normalize_filters(Some(json!({"folder_ids": [-1]})), None, None).unwrap_err();
        assert!(as_the_planner_sees_it(&e2).contains("folder_ids"), "got: {e2}");
    }

    /// A nested id filter that is not even a list is a shape error, not a panic
    /// — and the message names the parameter and shows the shape, like every
    /// other planner-facing string here. Asserting only `!e.is_empty()` pinned
    /// Err-vs-Ok and nothing else.
    #[test]
    fn a_nested_id_filter_that_is_not_a_list_is_refused_with_the_shape() {
        let e = normalize_filters(Some(json!({"account_ids": "1"})), None, None).unwrap_err();
        let seen = as_the_planner_sees_it(&e);
        assert!(seen.contains("account_ids"), "must name the parameter: {seen}");
        assert!(seen.contains("list of ids"), "must show the shape: {seen}");
    }

    /// An explicit `null` is absence, exactly as `ids::opt_message_id` treats
    /// it — and it must not survive into the body as a filter value localmail
    /// would then have to interpret.
    #[test]
    fn an_explicit_null_id_filter_is_absence_not_a_shape_error() {
        assert!(normalize_filters(Some(json!({"account_ids": null})), None, None)
            .unwrap()
            .is_none());
        let out = normalize_filters(Some(json!({"account_ids": null, "subject": "flight"})), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(out["subject"], json!("flight"));
        assert!(out.get("account_ids").is_none(), "no null on the wire: {out}");
    }

    /// A `null` beside a top-level value is absence, not a doubled parameter —
    /// otherwise the planner is refused for writing the placeholder it was
    /// allowed to write.
    #[test]
    fn a_null_nested_filter_does_not_collide_with_the_top_level_form() {
        let out = normalize_filters(Some(json!({"account_ids": null})), Some(ids(&[2])), None)
            .unwrap()
            .unwrap();
        assert_eq!(out["account_ids"], json!(["2"]));
    }

    /// An empty list asks localmail to filter by nothing. `id_list` refuses the
    /// same shape for `mail.list_messages`; the twin inherited the rule and not
    /// the test, so deleting the check survived a mutation run.
    #[test]
    fn an_empty_nested_id_list_is_refused_rather_than_filtering_by_nothing() {
        let e = normalize_filters(Some(json!({"account_ids": []})), None, None).unwrap_err();
        let seen = as_the_planner_sees_it(&e);
        assert!(seen.contains("account_ids"), "{seen}");
        assert!(seen.contains("omit it"), "must say what to do instead: {seen}");
    }

    /// The doubled-parameter message interpolates `{key}`. Only `account_ids`
    /// was ever tested, so hardcoding that name survived — and `folder_ids`
    /// doubling would then be blamed on the wrong parameter, which is the exact
    /// #536 mis-attribution this module is written against.
    #[test]
    fn a_doubled_folder_ids_is_blamed_on_folder_ids() {
        let e = normalize_filters(Some(json!({"folder_ids": ["1"]})), None, Some(ids(&[2])))
            .unwrap_err();
        let seen = as_the_planner_sees_it(&e);
        assert!(seen.contains("folder_ids"), "must name the doubled parameter: {seen}");
        assert!(!seen.contains("account_ids"), "must not blame the sibling: {seen}");
    }

    #[test]
    fn no_message_grows_past_the_planner_clamp() {
        let long = "x".repeat(400);
        let cases = vec![
            normalize_filters(Some(json!(long.clone())), None, None).unwrap_err(),
            normalize_filters(Some(json!({"account_ids": ["1"]})), Some(ids(&[2])), None)
                .unwrap_err(),
            normalize_filters(Some(json!({"account_ids": [long.clone()]})), None, None)
                .unwrap_err(),
        ];
        for m in &cases {
            assert!(
                m.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
                "{} chars exceeds the clamp: {m}",
                m.chars().count()
            );
        }
    }
}
