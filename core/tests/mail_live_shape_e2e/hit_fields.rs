//! #947's leg of the live localmail shape gate: the two hit fields the mail
//! worker's `hits::shape` rewrites (`attachments`, `date_local`), and the
//! `filters.attachment_name` filter its tool doc promises the planner.
//!
//! Its own file because `mail_live_shape_e2e.rs` is over the 500-line cap.
//! It is a `#[path]` module of that test, not a test target of its own: a
//! plain file in a subdirectory of `tests/` is not one cargo discovers.
//! `curl` and `ok_json` are the gate's own closures, passed in so every leg
//! reports a transport failure or a non-200 the same way.

use serde_json::Value;

/// One HTTP exchange: `(status, lowercased head, parsed JSON body)`.
pub type Reply = (u16, String, Option<Value>);

pub fn check(
    curl: &dyn Fn(&str, &str, Option<&str>) -> Reply,
    ok_json: &dyn Fn(&str, Reply) -> Value,
    fields: &[&str],
) {
    // Asked of mail that HAS attachments, so the `filename` check cannot pass
    // vacuously on attachment-less hits.
    let with_att = ok_json(
        "/v1/search with attachments",
        curl(
            "POST",
            "/v1/search",
            Some(
                &serde_json::json!({
                    "filters": {"has_attachment": true},
                    "limit": 5,
                    "fields": fields,
                    "snippet_chars": super::contract::SNIPPET_CHARS,
                })
                .to_string(),
            ),
        ),
    );
    let att_hits = with_att["results"].as_array().cloned().unwrap_or_default();
    let mut a_name: Option<(String, String)> = None; // (message_id, filename)
    for h in &att_hits {
        let list = h["attachments"].as_array().unwrap_or_else(|| {
            panic!("#947: a hit's `attachments` must be an array: {h}")
        });
        for a in list {
            let name = a["filename"].as_str().unwrap_or_else(|| {
                panic!("#947: `hits::shape` keeps `attachments[].filename`, a STRING: {a}")
            });
            a_name.get_or_insert((h["message_id"].as_str().unwrap_or("").to_owned(), name.to_owned()));
        }
        // `null` without a usable `Date:` header; otherwise an RFC 3339
        // instant with its offset, which `hits::shape` shows as `date`.
        let dl = &h["date_local"];
        assert!(
            dl.is_null() || dl.as_str().is_some_and(|s| s.len() >= 25 && s.contains('T')),
            "#947: `date_local` must be null or an RFC 3339 instant with an offset: {dl}"
        );
    }
    let (holder, name) = a_name
        .unwrap_or_else(|| panic!("#947: no attachment name in 5 has_attachment hits: {with_att}"));
    let by_name = ok_json(
        "/v1/search by attachment_name",
        curl(
            "POST",
            "/v1/search",
            Some(&serde_json::json!({"filters": {"attachment_name": name}, "limit": 50}).to_string()),
        ),
    );
    assert!(
        by_name["results"].as_array().into_iter().flatten().any(|h| h["message_id"] == holder.as_str()),
        "#947: `filters.attachment_name` must find message {holder}, which holds that file"
    );
}
