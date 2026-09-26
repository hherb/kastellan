//! One page of an attachment's extracted text, as `mail.get_attachment_text`
//! returns it (#760).
//!
//! localmail served extracted text whole until slice D (`api_minor` 2): p99
//! 129 KB, max 2.1 MB on the live archive. Whole, a long PDF overflowed the
//! planner's 16 KiB per-step view and landed in the handoff stash and the
//! truncation path (#678), so the planner saw a head and a marker instead of
//! the text it asked for. Now the worker asks for one page of
//! [`TEXT_PAGE_CHARS`] characters and hands back localmail's own paging fields,
//! so the planner reads a page and can ask for the next.
//!
//! ⚠️ **`next_offset` is copied from localmail, never computed here.** Offsets
//! count Unicode code points (Postgres `substring()`); any arithmetic on the
//! returned text in another unit skips or repeats text on exactly the documents
//! with characters above U+FFFF (8 of 9,303 live extractions). The server's
//! value is the only correct one.

use serde_json::{json, Value};

use crate::attach::Picked;

/// Characters per page. The planner reads a step through a 16 KiB view
/// (`core`'s `STEP_OK_SUMMARY_MAX`), and a page is meant to arrive in it whole.
/// Characters are not bytes, so no character count guarantees that: 8,000 CJK
/// characters are 24 KB, and 8,000 newlines escape to 16 KB. What 8,000 does
/// guarantee is the common case — Latin-script text at ordinary newline and
/// quote density, pinned by a test at 1 escape per 8 characters — with room for
/// the envelope. A page that still overflows takes core's existing path for an
/// oversized result, exactly as every whole text did before. localmail's p50
/// extraction is 3.1 KB and its p95 40 KB, so most attachments are one page
/// and a p95 one is five.
pub const TEXT_PAGE_CHARS: u32 = 8_000;

/// Build the tool result from a text route's response body.
///
/// `Err` is a service fault (planner-facing): a body without the text or
/// without the paging fields. Before slice D this fell back to the raw body,
/// which made sense when the contract was loose; now the version gate has
/// established a server that sends all of them, and serving a page with no
/// `next_offset` would tell the planner it had the whole document when it
/// might have the first 8,000 characters of two million.
///
/// The fields must also be **consistent**, with each other and with
/// `requested` (the `offset` this worker asked for): localmail echoes the
/// requested offset, and its `next_offset` is null or advances strictly within
/// `total` (`text_window.py`). A server that ignored `offset` would otherwise
/// hand back page one on every call while the planner believed it was reading
/// on, and a `next_offset` that does not advance would send it round the same
/// page to the iteration cap. Comparing the server's own numbers is not
/// computing an offset, so the rule above still holds.
pub fn page_result(picked: &Picked, requested: u64, body: &[u8]) -> Result<Value, String> {
    let fault = |what: &str| {
        format!(
            "localmail returned attachment text {what} — a service fault, not a missing \
             attachment. Tell the operator."
        )
    };
    let v: Value = serde_json::from_slice(body).map_err(|_| fault("that is not JSON"))?;
    let text = v.get("text").and_then(Value::as_str).ok_or_else(|| fault("without a `text` string"))?;
    let offset = v.get("offset").and_then(Value::as_u64);
    let total = v.get("total").and_then(Value::as_u64);
    // Present-and-null is the last page; absent is a server that does not page.
    let next = match v.get("next_offset") {
        Some(Value::Null) => Some(None),
        Some(n) => n.as_u64().map(Some),
        None => None,
    };
    let (Some(offset), Some(total), Some(next_offset)) = (offset, total, next) else {
        return Err(fault("without its paging fields (offset, total, next_offset)"));
    };
    let advances = match next_offset {
        Some(n) => offset < n && n <= total,
        None => true,
    };
    if offset != requested || !advances {
        let next = next_offset.map_or("null".to_string(), |n| n.to_string());
        return Err(fault(&format!(
            "with inconsistent paging (asked {requested}; got offset {offset}, next_offset \
             {next}, total {total})"
        )));
    }

    let mut out = json!({
        "sha256": picked.sha256(),
        "text": text,
        "offset": offset,
        "total": total,
        "next_offset": next_offset,
    });
    if let Some(name) = picked.save_name() {
        out["filename"] = json!(name);
    }
    if let Some(i) = picked.index() {
        out["index"] = json!(i);
    }
    if let Some(n) = next_offset {
        out["more"] = json!(format!(
            "{} of {total} characters shown. For the rest, call mail.get_attachment_text \
             again for the same attachment with offset: {n}.",
            n - offset
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha_picked() -> Picked {
        Picked::from_planner_sha(&"a".repeat(64)).unwrap()
    }

    #[test]
    fn a_middle_page_carries_the_servers_next_offset_and_says_how_to_continue() {
        let body = br#"{"text":"page two","offset":8000,"limit":8000,"total":30000,"next_offset":16000}"#;
        let out = page_result(&sha_picked(), 8000, body).unwrap();
        assert_eq!(out["text"], "page two");
        assert_eq!(out["offset"], 8000);
        assert_eq!(out["total"], 30000);
        assert_eq!(out["next_offset"], 16000);
        let more = out["more"].as_str().expect("a continuing page says so");
        assert!(more.contains("offset: 16000") && more.contains("8000 of 30000"), "{more}");
    }

    /// The server's `next_offset` wins even where arithmetic on the text would
    /// disagree — here a 1-char text claiming a 2-code-point advance, the
    /// astral-plane case the module docs warn about.
    #[test]
    fn next_offset_is_copied_not_computed_from_the_text() {
        let body = br#"{"text":"x","offset":0,"limit":2,"total":5,"next_offset":2}"#;
        assert_eq!(page_result(&sha_picked(), 0, body).unwrap()["next_offset"], 2);
    }

    #[test]
    fn the_last_page_has_a_null_next_offset_and_no_continuation_note() {
        let body = br#"{"text":"end","offset":0,"limit":8000,"total":3,"next_offset":null}"#;
        let out = page_result(&sha_picked(), 0, body).unwrap();
        assert!(out["next_offset"].is_null());
        assert!(out.get("more").is_none(), "nothing more to fetch: {out}");
    }

    #[test]
    fn a_planner_typed_hash_carries_no_filename_or_index() {
        let body = br#"{"text":"t","offset":0,"limit":1,"total":1,"next_offset":null}"#;
        let out = page_result(&sha_picked(), 0, body).unwrap();
        assert_eq!(out["sha256"], "a".repeat(64));
        assert!(out.get("filename").is_none() && out.get("index").is_none(), "{out}");
    }

    /// A body without paging fields is what a pre-slice-D server sends: the
    /// whole text, looking exactly like a complete first page. Refused rather
    /// than served, because the planner would stop reading.
    #[test]
    fn a_body_without_paging_fields_is_a_fault_not_a_complete_document() {
        for body in [
            &br#"{"text":"whole"}"#[..],
            br#"{"text":"t","offset":0,"total":9}"#,
            br#"{"text":"t","offset":0,"total":9,"next_offset":"9"}"#,
        ] {
            let e = page_result(&sha_picked(), 0, body).unwrap_err();
            assert!(e.contains("paging fields"), "{e}");
        }
    }

    /// Present-but-inconsistent paging is refused too: a server that ignored
    /// the requested offset, a `next_offset` that does not advance (the
    /// planner would re-read one page to the iteration cap), and one past the
    /// end. Each alone would otherwise pass as a well-formed page.
    #[test]
    fn inconsistent_paging_is_a_fault_not_a_page() {
        for (requested, body) in [
            (8000, &br#"{"text":"t","offset":0,"total":30000,"next_offset":8000}"#[..]),
            (8000, br#"{"text":"t","offset":8000,"total":30000,"next_offset":8000}"#),
            (8000, br#"{"text":"t","offset":8000,"total":30000,"next_offset":4000}"#),
            (0, br#"{"text":"t","offset":0,"total":10,"next_offset":11}"#),
        ] {
            let e = page_result(&sha_picked(), requested, body).unwrap_err();
            assert!(e.contains("inconsistent paging") && e.contains("service fault"), "{e}");
            assert!(e.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX, "{e}");
        }
    }

    /// The boundaries that must still pass: the last page (`next_offset`
    /// exactly `total` is not what localmail sends, but is not inconsistent),
    /// and an offset past the end, which localmail answers with empty text and
    /// a null `next_offset` echoing the requested offset.
    #[test]
    fn consistent_edge_pages_are_accepted() {
        page_result(&sha_picked(), 0, br#"{"text":"t","offset":0,"total":10,"next_offset":10}"#)
            .unwrap();
        let out = page_result(&sha_picked(), 50, br#"{"text":"","offset":50,"total":10,"next_offset":null}"#)
            .unwrap();
        assert!(out.get("more").is_none(), "{out}");
    }

    #[test]
    fn a_body_without_text_or_not_json_is_a_fault() {
        assert!(page_result(&sha_picked(), 0, b"raw text").unwrap_err().contains("not JSON"));
        assert!(page_result(&sha_picked(), 0, br#"{"other":"x"}"#).unwrap_err().contains("`text`"));
    }

    #[test]
    fn every_fault_fits_the_planners_clamp() {
        for body in [&b"x"[..], br#"{}"#, br#"{"text":"t"}"#] {
            let e = page_result(&sha_picked(), 0, body).unwrap_err();
            assert!(e.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX, "{e}");
        }
    }

    /// The common case the page size is chosen for: Latin text with a JSON
    /// escape (newline or quote) every 8 characters — denser than prose — fits
    /// core's 16 KiB step view whole, envelope and continuation note included.
    #[test]
    fn a_page_of_ordinary_text_fits_the_planners_step_view() {
        let text: String = "invoice\n".repeat(TEXT_PAGE_CHARS as usize / 8);
        assert_eq!(text.chars().count(), TEXT_PAGE_CHARS as usize);
        let body = serde_json::to_vec(&json!({
            "text": text, "offset": 0, "limit": TEXT_PAGE_CHARS,
            "total": 10 * TEXT_PAGE_CHARS, "next_offset": TEXT_PAGE_CHARS
        }))
        .unwrap();
        let out = serde_json::to_vec(&page_result(&sha_picked(), 0, &body).unwrap()).unwrap();
        assert!(out.len() < 16 * 1024, "{} bytes", out.len());
    }
}
