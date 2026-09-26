//! Ids in the two shapes localmail actually puts on the wire.
//!
//! localmail serialises **every** id as a JSON string — `"message_id":"37477"`
//! on `/v1/search` and `/v1/messages`, `"id":"1"` on `/v1/accounts` — while this
//! worker interpolates ids into URL paths and so must keep them strictly
//! numeric. Before this type the params took a bare `i64`, so the planner
//! copying an id straight out of a search hit (which is what it does, and the
//! only sane thing for it to do) produced `invalid type: string "17817",
//! expected i64`: 7 of the 14 live `mail.get_message` failures.
//!
//! [`LocalmailId`] widens the *accepted JSON types* while keeping the *validated
//! output* an `i64`, so the set of values that can reach a URL path is a strict
//! **subset** of what it was before — the old bare `i64` accepted negatives, and
//! `/v1/messages/-1` was reachable. This tightens the traversal guard; it does
//! not loosen it.
//!
//! The widening stops just past the two forms localmail emits (leading zeros are
//! also accepted, being unambiguous — see `leading_zeros_are_accepted`). No
//! trimming, no sign, no floats: the type stays a validator, not a repair layer.
//! A value that is not an id gets [`explain`], which is written for the planner
//! rather than for a log reader — `inner_loop` feeds a failed step's error back
//! on the next iteration, so this text is the only chance to correct the
//! mistake.

use std::fmt;

use serde::Deserialize;

/// A localmail row id, accepted as `37477` or `"37477"`, always rendered as a
/// bare decimal `i64`.
///
/// Construction is deliberately confined to this module's `deserialize_with`
/// entry points, so a validated value is the only kind that exists — there is
/// no way to obtain the inner `i64` back out, only to `Display` it into a URL.
/// If you ever need another constructor, make it **fallible**: an infallible
/// `From<i64>` would silently reintroduce the negatives that `/v1/messages/-1`
/// was once reachable on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalmailId(i64);

impl fmt::Display for LocalmailId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Which parameter an id arrived in.
///
/// [`explain`]'s advice is only actionable if it names the parameter that
/// actually failed, and serde attaches no field context to a
/// `deserialize_with` error — so the field has to be threaded in by hand.
/// Getting this wrong is not cosmetic: before #536's review, `{"account_ids":
/// ["abc"]}` was answered with *"re-read the hit's message_id … Expected the
/// numeric message_id"*, sending the planner to repair a parameter that was
/// never wrong while it re-sent the one that was.
///
/// `pub` so [`id_strings`] can take one instead of re-deriving it from a
/// string: a closed set of parameters is what this is, and passing the name as
/// `&str` bought an unreachable "not an id filter" arm whose message blamed the
/// *planner* for what could only ever be a worker-side typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdField {
    MessageId,
    AccountIds,
    FolderIds,
}

impl IdField {
    /// The parameter name exactly as `core`'s tool schema advertises it, so the
    /// planner can match the advice to the argument it wrote.
    pub fn name(self) -> &'static str {
        match self {
            Self::MessageId => "message_id",
            Self::AccountIds => "account_ids",
            Self::FolderIds => "folder_ids",
        }
    }

    /// A concrete id of the right shape for this parameter. Message ids are
    /// five digits in the live archive; account and folder ids are small.
    fn example(self) -> &'static str {
        match self {
            Self::MessageId => "37477",
            Self::AccountIds | Self::FolderIds => "1",
        }
    }

    /// Is a pasted `next_cursor` a plausible mistake in this parameter?
    ///
    /// Only in `message_id`: it is the one id that sits beside `next_cursor` in
    /// a search/list response, and all 3 measured cursor pastes were there.
    /// Offering the cursor diagnosis for an account id would spend the planner's
    /// clamp budget on a mistake that has never happened.
    fn cursor_adjacent(self) -> bool {
        matches!(self, Self::MessageId)
    }

    /// The generic tail: what a correct value looks like.
    ///
    /// Spells the parameter name exactly as [`name`](Self::name) does — the
    /// planner has to be able to match the advice to the argument it wrote, and
    /// prose like "numeric account ids" does not tell it which key to edit.
    fn want(self) -> &'static str {
        match self {
            Self::MessageId => "Expected the numeric message_id of a hit, e.g. 37477.",
            Self::AccountIds => "Expected numeric account_ids from mail.list_accounts, e.g. 1.",
            Self::FolderIds => "Expected numeric folder_ids, e.g. 1.",
        }
    }
}

/// `#[serde(deserialize_with)]` for `mail.get_message`'s `message_id`.
pub fn message_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<LocalmailId, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    parse_id(IdField::MessageId, &v)
        .map(LocalmailId)
        .map_err(serde::de::Error::custom)
}

/// `#[serde(deserialize_with)]` for an **optional** `message_id`.
///
/// `mail.get_attachment_text` addresses an attachment either by message or by
/// hash, so its `message_id` is optional — and an optional field is precisely
/// where a validator degrades quietly. Plain `Option::<LocalmailId>` would be
/// fine, but reaching for `Option<i64>` or swallowing the error into `None`
/// would make a *malformed* id indistinguishable from an *absent* one, and the
/// tool would then answer "name the attachment" for a value the planner did
/// name. Explicit `null` is absence; anything else is validated exactly as the
/// required form, [`explain`]'s repair advice included.
pub fn opt_message_id<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<LocalmailId>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => Ok(None),
        v => parse_id(IdField::MessageId, &v)
            .map(LocalmailId)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// `#[serde(deserialize_with)]` for `mail.list_messages`' optional `account_ids`.
pub fn account_ids<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<LocalmailId>>, D::Error> {
    id_list(d, IdField::AccountIds)
}

/// `#[serde(deserialize_with)]` for `mail.list_messages`' optional `folder_ids`.
pub fn folder_ids<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<LocalmailId>>, D::Error> {
    id_list(d, IdField::FolderIds)
}

/// Validate a JSON array of ids into their canonical digit-string form.
///
/// For `mail.search`'s `filters` object, whose `account_ids`/`folder_ids`
/// localmail types as `list[str]` — an integer there returns a raw FastAPI 422
/// the planner cannot act on. `field` names the parameter exactly as the tool
/// schema advertises it, so a rejection blames the argument the planner
/// actually wrote (the #536 lesson).
///
/// Returns validated [`LocalmailId`]s rather than bare strings: this does the
/// same work as [`id_list`], and discarding the type at the boundary would
/// leave "these were validated" recorded only in the function's name.
pub fn id_strings(field: IdField, v: &serde_json::Value) -> Result<Vec<LocalmailId>, String> {
    let key = field.name();
    let Some(items) = v.as_array() else {
        return Err(format!(
            "`{key}` must be a list of ids, e.g. [\"1\"]. {} Got: {}",
            field.want(),
            head(&v.to_string())
        ));
    };
    if items.is_empty() {
        return Err(format!(
            "`{key}` must not be an empty list — omit it entirely to search them all."
        ));
    }
    items.iter().map(|item| parse_id(field, item).map(LocalmailId)).collect()
}

/// An optional list of ids, every element validated and blamed on `field`.
///
/// An explicitly *empty* list is refused rather than passed on: `join_ids` would
/// render it as a bare `account_ids=`, which asks localmail to filter by nothing
/// and most plausibly returns the whole unfiltered archive — the caller asked
/// for one thing and would silently get another, which is the failure family
/// this module exists to close.
fn id_list<'de, D: serde::Deserializer<'de>>(
    d: D,
    field: IdField,
) -> Result<Option<Vec<LocalmailId>>, D::Error> {
    let Some(values) = Option::<Vec<serde_json::Value>>::deserialize(d)? else {
        return Ok(None);
    };
    if values.is_empty() {
        return Err(serde::de::Error::custom(format!(
            "`{}` must not be an empty list — omit it entirely to search them all.",
            field.name()
        )));
    }
    values
        .iter()
        .map(|v| parse_id(field, v).map(LocalmailId))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// Pure: a JSON value → a validated non-negative id, or the planner-facing
/// reason it is not one.
///
/// Accepts a non-negative integer JSON number, or a string of ASCII digits.
/// Everything else — signs, surrounding whitespace, the empty string, floats,
/// negatives, `i64` overflow, and every other JSON type (booleans, null, arrays,
/// objects) — is refused.
fn parse_id(field: IdField, v: &serde_json::Value) -> Result<i64, String> {
    match v {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) if i >= 0 => Ok(i),
            _ => Err(explain(field, v)),
        },
        serde_json::Value::String(s) => {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(explain(field, v));
            }
            // Digits-only and non-empty, so the only remaining failure is
            // overflowing i64.
            s.parse::<i64>().map_err(|_| explain(field, v))
        }
        _ => Err(explain(field, v)),
    }
}

/// What to tell the **planner** when a value is not a usable id.
///
/// Each arm names a mistake the live `audit_log` actually recorded, because this
/// string is fed back into the next planning iteration and a generic "expected
/// i64" has demonstrably not been enough: the same three mistakes recurred
/// across two months.
///
/// **The rejected value is quoted LAST in every arm, and that is load-bearing.**
/// `core` clamps this text to [`kastellan_protocol::STEP_ERR_DETAIL_MAX`] chars
/// before the planner ever sees it, so anything past the cut is dropped. The
/// value is the only part whose length varies with input, so putting it at the
/// end makes every word of advice sit at a *fixed* offset — the fit stops being
/// an arithmetic property that has to be re-checked whenever the prose or
/// [`HEAD_MAX`] changes, and becomes a structural one.
///
/// It was not always so. The first cut put the generic tail before the
/// class-specific diagnosis and lost all of it; the second put the value first
/// and delivered `e.g. 374` — a *truncated example that reads like a wrong id* —
/// for any value of 48 chars or more, which includes the 64-char hex cursors
/// `/v1/search` serves live. Both were found by review, not by a test, because
/// the tests probed with values short enough to fit either way.
fn explain(field: IdField, v: &serde_json::Value) -> String {
    // Truncate the RENDERED form, not the raw value: `{:?}` escapes, so a value
    // full of quotes or backslashes renders at up to twice the length `head`
    // would have thought it capped.
    let shown = match v {
        serde_json::Value::String(s) => head(&format!("{s:?}")),
        other => head(&other.to_string()),
    };
    let (want, example) = (field.want(), field.example());
    match v {
        serde_json::Value::String(s) if s.starts_with("{{") && s.ends_with("}}") => format!(
            "NO template substitution — write the literal id from the previous step's \
             output, e.g. {example}, not a placeholder. {want} Got: {shown}"
        ),
        serde_json::Value::String(_) if field.cursor_adjacent() => format!(
            "Not a number. If this came from next_cursor, that is an opaque paging token, \
             not an id — re-read the hit's message_id, e.g. 37477. {want} Got: {shown}"
        ),
        _ => format!("Not a valid `{}`. {want} Got: {shown}", field.name()),
    }
}

/// Longest rendered form of a rejected value that [`explain`] will quote.
///
/// Bounds the message so it cannot grow with its input — this text rides into
/// the planner's next prompt and into the audit log, and the cursor cases are
/// long base64/hex blobs (44 and 64 chars live). Since [`explain`] quotes the
/// value last, this no longer governs whether the *advice* survives the clamp;
/// it governs how much of the value the planner gets to see in order to
/// recognise which argument it wrote.
const HEAD_MAX: usize = 48;

fn head(s: &str) -> String {
    if s.chars().count() <= HEAD_MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(HEAD_MAX).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What the planner actually receives.
    ///
    /// `core` clamps a failed step's `detail` to
    /// [`kastellan_protocol::STEP_ERR_DETAIL_MAX`] chars before it reaches the
    /// planner prompt (`core::scheduler::inner_loop::summary`), and that
    /// `detail` is the worker's `RpcError.message` verbatim
    /// (`core::scheduler::tool_dispatch::result_mapping`) — which for this
    /// module is `"bad params: " + explain(...)` (see `handler::request::from_params`).
    /// So only this much of `explain`'s output is ever delivered.
    ///
    /// The const is imported, not mirrored. It used to be a hand-synced copy
    /// with "keep in sync by hand" in its doc comment; #536's review pointed
    /// out that lowering it in `core` would leave this side green while
    /// production silently ate the repair advice — the exact cross-component
    /// blindness that caused #527.
    fn as_the_planner_sees_it(explanation: &str) -> String {
        const BUDGET: usize =
            kastellan_protocol::STEP_ERR_DETAIL_MAX - "bad params: ".len();
        explanation.chars().take(BUDGET).collect()
    }

    /// The class-specific diagnosis must precede the generic tail, or the clamp
    /// is what eats it.
    #[track_caller]
    fn assert_diagnosis_precedes_generic(message: &str, repair_phrase: &str, field: IdField) {
        let repair = message.find(repair_phrase).expect("repair phrase present");
        let generic = message.find(field.want()).expect("generic text present");
        assert!(
            repair < generic,
            "the class-specific diagnosis must come FIRST, before the generic text, \
             or it is what core's clamp eats: {message}"
        );
    }

    #[track_caller]
    fn assert_explanation_is_bounded(field: IdField, v: &serde_json::Value) {
        let m = explain(field, v);
        assert!(
            m.chars().count() < 300,
            "explanation must not grow with the value; got {} chars: {m}",
            m.chars().count()
        );
        assert!(m.contains('…'), "the value should be visibly truncated; got: {m}");
    }

    const ALL_FIELDS: [IdField; 3] = [IdField::MessageId, IdField::AccountIds, IdField::FolderIds];

    /// A real base64 `next_cursor`, copied from a live `/v1/messages` response.
    const LIVE_CURSOR: &str = "ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0";

    // --- the accepted grammar, one test per row of the spec's table ---

    #[test]
    fn a_json_number_is_accepted() {
        assert_eq!(parse_id(IdField::MessageId, &json!(37477)), Ok(37477));
    }

    #[test]
    fn a_digit_string_is_accepted() {
        // The shape localmail actually emits, and the 7-of-14 failure case.
        assert_eq!(parse_id(IdField::MessageId, &json!("37477")), Ok(37477));
    }

    #[test]
    fn leading_zeros_are_accepted() {
        // localmail never emits this, but it is unambiguous.
        assert_eq!(parse_id(IdField::MessageId, &json!("0037")), Ok(37));
    }

    #[test]
    fn a_signed_string_is_rejected() {
        // Sign characters are not digits, and no row id is negative.
        assert!(parse_id(IdField::MessageId, &json!("-1")).is_err());
        assert!(parse_id(IdField::MessageId, &json!("+1")).is_err());
    }

    #[test]
    fn surrounding_whitespace_is_rejected_not_trimmed() {
        // Not trimming is deliberate: whitespace means the value came from
        // somewhere it should not have, and repairing it would hide that.
        assert!(parse_id(IdField::MessageId, &json!(" 37477")).is_err());
        assert!(parse_id(IdField::MessageId, &json!("37477 ")).is_err());
    }

    #[test]
    fn the_empty_string_is_rejected() {
        assert!(parse_id(IdField::MessageId, &json!("")).is_err());
    }

    #[test]
    fn a_float_is_rejected() {
        assert!(parse_id(IdField::MessageId, &json!(37.0)).is_err());
        assert!(parse_id(IdField::MessageId, &json!(3.5)).is_err());
    }

    #[test]
    fn a_negative_number_is_rejected() {
        // The traversal half of the guarantee: the old bare `i64` let
        // `/v1/messages/-1` be built.
        assert!(parse_id(IdField::MessageId, &json!(-1)).is_err());
    }

    #[test]
    fn an_overflowing_digit_string_is_rejected() {
        assert!(parse_id(IdField::MessageId, &json!("99999999999999999999999")).is_err());
    }

    #[test]
    fn non_scalars_and_null_are_rejected() {
        assert!(parse_id(IdField::MessageId, &json!(null)).is_err());
        assert!(parse_id(IdField::MessageId, &json!(true)).is_err());
        assert!(parse_id(IdField::MessageId, &json!([1])).is_err());
        assert!(parse_id(IdField::MessageId, &json!({"id": 1})).is_err());
    }

    // --- the planner-facing text: one test per live failure class ---

    #[test]
    fn a_template_placeholder_is_told_there_is_no_substitution() {
        // 4 of the 14 live failures were exactly this value.
        let m = explain(IdField::MessageId, &json!("{{message_id}}"));
        assert!(m.contains("NO template substitution"), "got: {m}");
        assert!(m.contains("literal id"), "got: {m}");
    }

    #[test]
    fn a_paging_cursor_is_named_as_a_cursor() {
        // 3 of the 14 live failures pasted next_cursor in here.
        let m = explain(IdField::MessageId, &json!(LIVE_CURSOR));
        assert!(m.contains("next_cursor"), "got: {m}");
        assert!(m.contains("paging token"), "got: {m}");
    }

    /// Every arm, for every field, must name its OWN parameter and give an
    /// example of the right shape — inside the planner's budget, not merely
    /// somewhere in the unclamped string.
    #[test]
    fn every_explanation_names_its_own_field_and_gives_an_example() {
        for field in ALL_FIELDS {
            for v in [json!("{{x}}"), json!(LIVE_CURSOR), json!(null), json!(-1), json!("abc")] {
                let seen = as_the_planner_sees_it(&explain(field, &v));
                assert!(
                    seen.contains(field.name()),
                    "{:?}/{v} must name its own field within the planner's budget; got: {seen:?}",
                    field
                );
                assert!(
                    seen.contains(field.example()),
                    "{:?}/{v} must give a concrete example within the planner's budget; got: {seen:?}",
                    field
                );
            }
        }
    }

    /// The #536 regression: `LocalmailId` is used for three parameters, but
    /// `explain` used to hardcode `message_id` in every arm. A planner that
    /// fumbled `account_ids` was told — three times over — to go fix
    /// `message_id`, and pointed at `next_cursor`, which no account id has ever
    /// been confused with. Misdirected advice is worse than generic advice,
    /// because `inner_loop` feeds it straight back and the planner acts on it.
    #[test]
    fn a_non_message_id_field_is_never_told_to_fix_message_id() {
        for field in [IdField::AccountIds, IdField::FolderIds] {
            for v in [json!("{{x}}"), json!(LIVE_CURSOR), json!(null), json!("abc"), json!(-1)] {
                let m = explain(field, &v);
                assert!(
                    !m.contains("message_id"),
                    "{field:?} must not send the planner to repair message_id: {m}"
                );
                assert!(
                    !m.contains("next_cursor"),
                    "the cursor diagnosis is measured on message_id only: {m}"
                );
            }
        }
    }

    /// Pins the actual guarantee rather than that a phrase appears *somewhere*
    /// in the full string: only the first
    /// `STEP_ERR_DETAIL_MAX - "bad params: ".len()` chars are ever delivered, so
    /// a phrase after that point is dropped as surely as if never written.
    ///
    /// The budget-window checks alone are NOT an ordering guard — with a short
    /// `want()` both pinned phrases fit whichever half comes first, so an edit
    /// that reverted the ordering would pass them and regress silently. The
    /// `.find(..) < .find(..)` checks are what pin the ordering, independent of
    /// how long `want()` is. Neither alone is sufficient.
    #[test]
    fn the_repair_phrase_survives_the_core_side_planner_clamp() {
        let cursor = explain(IdField::MessageId, &json!(LIVE_CURSOR));
        let cursor_head = as_the_planner_sees_it(&cursor);
        assert!(cursor_head.contains("next_cursor"), "clamped to: {cursor_head:?}");
        assert!(cursor_head.contains("paging token"), "clamped to: {cursor_head:?}");
        assert_diagnosis_precedes_generic(&cursor, "paging token", IdField::MessageId);

        let placeholder = explain(IdField::MessageId, &json!("{{message_id}}"));
        let placeholder_head = as_the_planner_sees_it(&placeholder);
        assert!(placeholder_head.contains("NO template substitution"), "clamped to: {placeholder_head:?}");
        assert!(placeholder_head.contains("literal id"), "clamped to: {placeholder_head:?}");
        assert_diagnosis_precedes_generic(&placeholder, "NO template substitution", IdField::MessageId);
    }

    /// The structural half of the clamp guarantee, and the one the previous cut
    /// lacked: the advice must fit for the LONGEST value `explain` can ever
    /// quote, not merely for the values the other tests happen to pass in.
    ///
    /// The worst case is derived from [`HEAD_MAX`] rather than hardcoded, so
    /// raising that ceiling — or lengthening the prose in front of the value —
    /// fails here instead of silently clipping the advice in production. That is
    /// exactly how the `e.g. 374` defect got in: the probe was a 4-char stand-in
    /// while the live hex cursor is 64 chars.
    #[test]
    fn the_advice_survives_the_clamp_for_the_longest_value_explain_can_quote() {
        let worst = "x".repeat(HEAD_MAX * 4);
        for field in ALL_FIELDS {
            for v in [json!(worst.clone()), json!(format!("{{{{{worst}}}}}"))] {
                let m = explain(field, &v);
                let seen = as_the_planner_sees_it(&m);
                assert!(seen.contains(field.name()), "clamped away the field name: {seen:?}");
                assert!(seen.contains(field.example()), "clamped away the example: {seen:?}");
                assert!(
                    seen.contains(field.want()),
                    "clamped away the generic tail, so nothing after the value survives: {seen:?}"
                );
                // The property that makes the three asserts above hold for ANY
                // input: the only variable-length part is last.
                let value_at = m.find("Got: ").expect("the value is quoted");
                assert!(
                    m.find(field.want()).expect("tail present") < value_at,
                    "the rejected value must be quoted LAST, after all advice: {m}"
                );
            }
        }
    }

    /// The optional form must be optional about *presence*, not about validity.
    #[test]
    fn an_optional_message_id_is_absent_or_validated_never_silently_dropped() {
        #[derive(Debug, Deserialize)]
        struct P {
            #[serde(default, deserialize_with = "opt_message_id")]
            message_id: Option<LocalmailId>,
        }
        let absent: P = serde_json::from_value(json!({})).unwrap();
        assert!(absent.message_id.is_none(), "an absent field is None");
        let null: P = serde_json::from_value(json!({"message_id": null})).unwrap();
        assert!(null.message_id.is_none(), "an explicit null is absence");
        let good: P = serde_json::from_value(json!({"message_id": "37413"})).unwrap();
        assert_eq!(good.message_id.unwrap().to_string(), "37413");

        // The half that a `None`-swallowing implementation would get wrong: a
        // bad value must still be refused, with the same advice as the required
        // form, rather than read as "the planner named no message".
        let e = serde_json::from_value::<P>(json!({"message_id": "{{message_id}}"})).unwrap_err();
        assert!(e.to_string().contains("NO template substitution"), "got: {e}");
    }

    /// `id_strings` is `mail.search`'s validator for ids nested in `filters`,
    /// and it had no direct test at all — its empty-list guard could be deleted
    /// without failing anything, which would send localmail a filter matching
    /// nothing and answer a question that had answers with zero results.
    #[test]
    fn id_strings_validates_coerces_and_refuses_the_empty_list() {
        let got = id_strings(IdField::AccountIds, &json!([1, "2"])).unwrap();
        assert_eq!(got.iter().map(ToString::to_string).collect::<Vec<_>>(), ["1", "2"]);

        let empty = id_strings(IdField::AccountIds, &json!([])).unwrap_err();
        assert!(empty.contains("account_ids"), "{empty}");
        assert!(empty.contains("omit it"), "must say what to do instead: {empty}");

        let not_a_list = id_strings(IdField::FolderIds, &json!("1")).unwrap_err();
        assert!(not_a_list.contains("folder_ids"), "blame the right parameter: {not_a_list}");
        assert!(not_a_list.contains("list of ids"), "{not_a_list}");

        // A bad element is blamed on the field it was written under, not on
        // `message_id` — the #536 rule.
        let bad = id_strings(IdField::FolderIds, &json!(["{{folder_id}}"])).unwrap_err();
        assert!(bad.contains("folder_ids"), "{bad}");
        assert!(!bad.contains("message_id"), "must not mis-attribute: {bad}");
    }

    #[test]
    fn a_long_rejected_string_is_truncated() {
        // This text rides into the planner's next prompt, so it must not grow
        // with the offending value. Bound on chars, not bytes: `head` truncates
        // by `chars().count()` and Debug-escaping can widen a value in ways
        // unrelated to the truncation this pins.
        assert_explanation_is_bounded(IdField::MessageId, &json!("x".repeat(500)));
    }

    #[test]
    fn a_long_rejected_non_scalar_is_also_truncated() {
        // The catch-all arm renders `v` itself (via `Display`), not just the two
        // string arms — an array or object must be bounded too, not formatted
        // unbounded onto the wire and into the audit log.
        let big = serde_json::Value::Array(vec![json!("x".repeat(500))]);
        assert_explanation_is_bounded(IdField::MessageId, &big);
    }

    /// A value quoted with `{:?}` can be twice its own length once escaped, so
    /// `head` truncates the *rendered* form. Truncating the raw value first
    /// would let a quote-laden input past the ceiling this test pins.
    #[test]
    fn an_escape_heavy_value_is_bounded_by_its_rendered_length() {
        assert_explanation_is_bounded(IdField::MessageId, &json!("\"\\".repeat(200)));
    }
}
