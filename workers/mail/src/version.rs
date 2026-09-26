//! Which localmail this worker can talk to.
//!
//! localmail numbers its `/v1` additions with `api_minor` (served by the
//! unauthenticated `GET /v1/version`), and two of them are ones this worker
//! now depends on (#760):
//!
//! | `api_minor` | what it added | what uses it here |
//! | --- | --- | --- |
//! | 2 | `/v1/messages/{id}/attachments/{index}[/text]`, and `offset`/`limit` paging on both text routes | both attachment tools |
//! | 3 | `fields` + `snippet_chars` on `POST /v1/search` | `mail.search` |
//!
//! Why ask rather than just try: an older server answers the index route with
//! the same 404 it gives a missing message, and **ignores** `offset`/`limit`,
//! answering 200 with the whole text and no `next_offset`. So a worker that
//! merely tried would report "no such attachment" for one that exists, or hand
//! the planner a whole 2 MB document labelled as a first page. Neither failure
//! says "upgrade localmail", which is the only repair. The version is the one
//! signal localmail itself designed for this (`serve/routes/version.py`).
//!
//! There is deliberately **no fallback** to the pre-slice routes: one code path
//! per tool, and both deployed hosts serve `api_minor` 3 (measured 2026-09-26).
//! A server that is too old gets a refusal naming the upgrade.
//!
//! This module is the pure half — [`version_error`] reads a `/v1/version` body
//! and says what is wrong with it. The handler owns the request.

use serde_json::Value;

/// The `api_major` this worker speaks. A different major is a different API.
pub const API_MAJOR: u64 = 1;

/// The oldest `api_minor` this worker works against — slice E's `fields` /
/// `snippet_chars`, which is also the newest thing it uses. Compared with `>=`,
/// the way localmail's own clients pin it, so a later additive bump is fine.
pub const MIN_API_MINOR: u64 = 3;

/// `None` when the server described by `body` (a `GET /v1/version` response)
/// is one this worker can use; otherwise the planner-facing reason.
///
/// Every message names the repair (upgrade localmail) and the version found,
/// because the planner cannot fix this and the operator reading the task
/// transcript is the one who has to act.
pub fn version_error(body: &Value) -> Option<String> {
    let major = body.get("api_major").and_then(Value::as_u64);
    let minor = body.get("api_minor").and_then(Value::as_u64);
    match (major, minor) {
        (Some(API_MAJOR), Some(m)) if m >= MIN_API_MINOR => None,
        (Some(API_MAJOR), Some(m)) => Some(too_old(&format!("{API_MAJOR}.{m}"))),
        // A major is only "another API" when the body is otherwise well formed;
        // `1` with no minor is a malformed body, not a mismatch.
        (Some(other), Some(_)) if other != API_MAJOR => Some(format!(
            "localmail speaks API {other}.x and this mail worker speaks {API_MAJOR}.x — \
             mail is unavailable until the two match. Tell the operator."
        )),
        _ => Some(too_old("an unknown version (no api_major/api_minor)")),
    }
}

/// The refusal for a server that predates what this worker needs. Also used by
/// the handler when `/v1/version` itself is missing (a 404), which only a
/// localmail older than every `api_minor` gives.
pub fn too_old(found: &str) -> String {
    format!(
        "localmail is too old for this mail worker: it needs API \
         {API_MAJOR}.{MIN_API_MINOR} or newer, found {found}. Tell the operator to upgrade \
         localmail."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v(major: Value, minor: Value) -> Value {
        json!({"api_major": major, "api_minor": minor, "server_version": "0.3.0"})
    }

    /// The live answer on both hosts, 2026-09-26, and the later minors a `>=`
    /// pin exists to keep accepting.
    #[test]
    fn accepts_the_minimum_minor_and_every_later_one() {
        for m in [MIN_API_MINOR, MIN_API_MINOR + 1, 99] {
            assert_eq!(version_error(&v(json!(API_MAJOR), json!(m))), None, "minor {m}");
        }
    }

    /// Slice D's server (`api_minor` 2) has the index routes but not the
    /// search projection, so it is still refused: the worker sends `fields` on
    /// every search.
    #[test]
    fn refuses_an_older_minor_and_names_both_versions_and_the_repair() {
        let e = version_error(&v(json!(1), json!(2))).expect("1.2 is too old");
        assert!(e.contains("1.2") && e.contains("1.3"), "{e}");
        assert!(e.contains("upgrade"), "must name the repair: {e}");
    }

    #[test]
    fn refuses_another_major_even_with_a_high_minor() {
        let e = version_error(&v(json!(2), json!(9))).expect("2.x is another API");
        assert!(e.contains("2.x") && e.contains("1.x"), "{e}");
    }

    /// A version body is a contract, and a string `"3"` is not one this worker
    /// guesses its way past — `as_u64` refuses it, so it reads as "unknown".
    #[test]
    fn refuses_a_body_with_missing_or_mistyped_numbers() {
        for body in [json!({}), v(json!("1"), json!("3")), v(json!(1), Value::Null), json!([1, 3])] {
            let e = version_error(&body).unwrap_or_else(|| panic!("accepted {body}"));
            assert!(e.contains("unknown version"), "{e}");
        }
    }

    /// Every arm is shown to the planner whole: core clamps a failed step's
    /// detail, and the repair is the last clause.
    #[test]
    fn every_refusal_fits_the_planners_clamp() {
        let arms = [
            version_error(&v(json!(1), json!(2))),
            version_error(&v(json!(2), json!(0))),
            version_error(&json!({})),
            Some(too_old("HTTP 404 on /v1/version")),
        ];
        for e in arms.into_iter().map(Option::unwrap) {
            assert!(
                e.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
                "{} chars: {e}",
                e.chars().count()
            );
        }
    }
}
