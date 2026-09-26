use super::*;
use super::search::BodyEchoFake;

// --- get_attachment_text returns one page of text ---

/// Answers every text request with a page whose text is the request's own
/// path and query, so a test reads back exactly what the worker asked for.
struct TextFake;
impl HttpGet for TextFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn get_authed(&self, url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
        let asked = format!("{}?{}", url.path(), url.query().unwrap_or(""));
        Ok(json_resp(
            serde_json::json!({
                "text": asked, "offset": 0, "limit": 8000, "total": 20000, "next_offset": 8000
            })
            .to_string()
            .as_bytes(),
        ))
    }
}

#[test]
fn get_attachment_text_asks_for_the_first_page_and_returns_it_with_its_paging_fields() {
    let mut h = MailHandler::with_client(client_with(Box::new(TextFake)));
    let sha = "a".repeat(64);
    let out = h.call("mail.get_attachment_text", serde_json::json!({"sha256": sha})).unwrap();
    assert_eq!(
        out["text"],
        format!("/v1/attachments/{sha}/text?offset=0&limit={}", text_page::TEXT_PAGE_CHARS)
    );
    assert_eq!(out["total"], 20000);
    assert_eq!(out["next_offset"], 8000);
    assert!(out["more"].as_str().unwrap().contains("offset: 8000"), "{out}");
}

/// The planner continues by sending back the `next_offset` it was given.
#[test]
fn get_attachment_text_forwards_the_planners_offset() {
    let mut h = MailHandler::with_client(client_with(Box::new(TextFake)));
    let out = h
        .call("mail.get_attachment_text", serde_json::json!({"sha256": "a".repeat(64), "offset": 8000}))
        .unwrap();
    assert!(out["text"].as_str().unwrap().contains("offset=8000&"), "{out}");
}

/// A body that is not localmail's paged text envelope is a service fault. It
/// used to be surfaced raw, which is how a server that does not page would
/// hand the planner a whole document looking like one complete page.
struct RawBodyFake(&'static [u8]);
impl HttpGet for RawBodyFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn get_authed(&self, _url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
        Ok(RawResponse { status: 200, location: None, content_type: "text/plain".into(), body: self.0.to_vec() })
    }
}

#[test]
fn a_text_body_that_is_not_a_paged_envelope_is_an_operation_failure() {
    for body in [&b"raw text"[..], br#"{"other":"x"}"#, br#"{"text":"whole, unpaged"}"#] {
        let mut h = MailHandler::with_client(client_with(Box::new(RawBodyFake(body))));
        let err = h
            .call("mail.get_attachment_text", serde_json::json!({"sha256": "a".repeat(64)}))
            .unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED, "{}", err.message);
        assert!(err.message.contains("service fault"), "{}", err.message);
    }
}

#[test]
fn bad_sha256_is_invalid_params() {
    let mut h = MailHandler::with_client(client_with(Box::new(TextFake)));
    let err = h.call("mail.get_attachment_text", serde_json::json!({"sha256": "../etc/passwd"})).unwrap_err();
    assert_eq!(err.code, codes::INVALID_PARAMS);
}

// --- get_attachment_text addressed by message rather than by hash ---

/// The sha256 that message 37413 really carries in the live archive.
const LIVE_SHA: &str = "71aac4580932cffe7649dda9c4cc10e2997de81d80105eafd448a64763f4a73b";
/// Its filename there, download prefix and all.
const LIVE_NAME: &str = "Download 470989752-e-ticket-DQXK68.pdf";
/// A second attachment, so that `filename` is load-bearing rather than
/// decorative: with only one attachment in the message every filename — and
/// none at all — resolves to the same sha, and a test written against that
/// fixture passes whether or not the worker reads the parameter.
const DECOY_NAME: &str = "boarding-pass.pdf";
const DECOY_SHA: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Distinguishable bodies, so a test can tell *which* attachment was read.
const E_TICKET_TEXT: &str = "GST Paid 146.81 AUD";
const DECOY_TEXT: &str = "boarding pass only";

/// Serves message detail *and* extracted text, with a **different body per
/// sha**.
///
/// That is what makes the selection observable: the test reads which
/// document came back, so "the filename picked the wrong attachment" is a
/// failed assertion on the text rather than a silent pass. An in-fake
/// `assert!` on the URL would instead abort the test through a panic, which
/// no `unwrap_err` can inspect and no `Err` arm can distinguish.
struct ArchiveFake {
    text_status: u16,
    /// Status for the *blob* route (`/v1/attachments/{sha}`), which is a
    /// different 404 from the text route's and needs its own advice.
    blob_status: u16,
    /// Status for the message-detail route. localmail returns 404 both for
    /// "no such message" and for an ACL denial.
    message_status: u16,
    /// Serve a message detail whose `attachments` is not an array — a
    /// contract violation that must not be reported as "no attachments".
    attachments_malformed: bool,
    /// `(filename, sha256)` pairs the message carries.
    attachments: Vec<(String, String)>,
}
impl ArchiveFake {
    fn new(text_status: u16, attachments: Vec<(String, String)>) -> Self {
        Self {
            text_status,
            blob_status: 200,
            message_status: 200,
            attachments_malformed: false,
            attachments,
        }
    }
    /// Two attachments, so a filename has work to do.
    fn ok() -> Self {
        Self::new(
            200,
            vec![
                (DECOY_NAME.to_string(), DECOY_SHA.to_string()),
                (LIVE_NAME.to_string(), LIVE_SHA.to_string()),
            ],
        )
    }
    /// One attachment — message 37413's real shape.
    fn single() -> Self {
        Self::new(200, vec![(LIVE_NAME.to_string(), LIVE_SHA.to_string())])
    }
    /// One attachment whose text localmail does not have.
    fn single_without_text() -> Self {
        Self::new(404, vec![(LIVE_NAME.to_string(), LIVE_SHA.to_string())])
    }
    /// One attachment listed on the message whose *blob* localmail cannot
    /// serve — the second step of `missing_text_advice`'s advice chain.
    fn single_missing_blob() -> Self {
        Self { blob_status: 404, ..Self::single() }
    }
    /// The message itself is unreadable: absent, or outside the ACL.
    fn no_such_message() -> Self {
        Self { message_status: 404, ..Self::single() }
    }
    fn attachments_not_an_array() -> Self {
        Self { attachments_malformed: true, ..Self::single() }
    }
    fn not_found(detail: &str) -> RawResponse {
        RawResponse {
            status: 404,
            location: None,
            content_type: "application/problem+json".into(),
            body: format!(r#"{{"detail": "{detail}"}}"#).into_bytes(),
        }
    }
}
/// localmail's text-route body: one page, with its paging fields (slice D).
/// These fixtures' texts are short, so every page is the whole and last one.
fn text_page_body(text: &str) -> RawResponse {
    json_resp(
        serde_json::json!({
            "text": text, "offset": 0, "limit": 8000,
            "total": text.chars().count(), "next_offset": null
        })
        .to_string()
        .as_bytes(),
    )
}

impl ArchiveFake {
    /// Serve the text or blob route for one attachment. Both routes — by hash
    /// and by position — land here, so a test's assertions hold whichever the
    /// worker used; which one it used is pinned separately.
    fn serve_attachment(&self, sha: &str, text: bool) -> RawResponse {
        match text {
            true if self.text_status == 404 => Self::not_found("no extracted text for attachment"),
            true => text_page_body(if sha == LIVE_SHA { E_TICKET_TEXT } else { DECOY_TEXT }),
            false if self.blob_status == 404 => Self::not_found(&format!("attachment {sha} not found")),
            false => RawResponse {
                status: 200,
                location: None,
                content_type: "application/pdf".into(),
                body: b"%PDF-1.7 body".to_vec(),
            },
        }
    }
}

impl HttpGet for ArchiveFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn get_authed(&self, url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
        let path = url.path().to_string();
        // Text route and blob route are distinct upstream 404s needing
        // opposite repairs, so the fake has to tell them apart too.
        if let Some(rest) = path.strip_prefix("/v1/attachments/") {
            let (sha, text) = match rest.strip_suffix("/text") {
                Some(sha) => (sha, true),
                None => (rest, false),
            };
            return Ok(self.serve_attachment(sha, text));
        }
        if let Some(rest) = path.strip_prefix("/v1/messages/37413/attachments/") {
            if self.message_status == 404 {
                return Ok(Self::not_found("message not found"));
            }
            let (i, text) = match rest.strip_suffix("/text") {
                Some(i) => (i, true),
                None => (rest, false),
            };
            let entry = i.parse::<usize>().ok().and_then(|i| self.attachments.get(i));
            return Ok(match entry {
                Some((_, sha)) => self.serve_attachment(sha, text),
                None => Self::not_found(&format!("attachment {i} of message 37413 not found")),
            });
        }
        if self.message_status == 404 {
            return Ok(Self::not_found("message not found"));
        }
        if self.attachments_malformed {
            return Ok(json_resp(
                br#"{"id":"37413","subject":"E-Ticket","attachments":"none"}"#,
            ));
        }
        let entries: Vec<String> = self
            .attachments
            .iter()
            .map(|(name, sha)| {
                format!(
                    r#"{{"filename":"{name}","sha256":"{sha}","content_type":"application/pdf","size":56112}}"#
                )
            })
            .collect();
        let body = format!(
            r#"{{"id":"37413","subject":"E-Ticket","attachments":[{}]}}"#,
            entries.join(",")
        );
        Ok(json_resp(body.as_bytes()))
    }
}

/// The live failure this whole change exists for (task 160, 2026-08-17): the
/// planner had the message and a filename, and had to retype a 64-char hash
/// to read the attachment. It got the hash wrong, localmail 404'd, and the
/// agent told the user PDF extraction had failed.
#[test]
fn get_attachment_text_resolves_the_sha_from_a_message_and_filename() {
    // Two attachments, so the filename decides which one — a fixture with
    // one would pass even if the parameter were never read.
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
    let out = h
        .call(
            "mail.get_attachment_text",
            serde_json::json!({"message_id": 37413, "filename": "e-ticket-DQXK68.pdf"}),
        )
        .unwrap();
    assert_eq!(out["text"], E_TICKET_TEXT);
    assert_eq!(out["sha256"], LIVE_SHA, "the resolved sha is reported back");
}

/// The other half of the same property: naming the *other* attachment reads
/// the other document. Without this, the test above is consistent with a
/// worker that always picks `attachments[1]`.
#[test]
fn a_different_filename_in_the_same_message_selects_the_other_attachment() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
    let out = h
        .call(
            "mail.get_attachment_text",
            serde_json::json!({"message_id": 37413, "filename": DECOY_NAME}),
        )
        .unwrap();
    assert_eq!(out["text"], DECOY_TEXT);
    assert_eq!(out["sha256"], DECOY_SHA);
}

/// The single-attachment case — which is what the live failure was — needs
/// no filename at all, so the planner copies exactly one short integer.
#[test]
fn get_attachment_text_needs_no_filename_when_the_message_has_one_attachment() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::single())));
    let out = h
        .call("mail.get_attachment_text", serde_json::json!({"message_id": 37413}))
        .unwrap();
    assert_eq!(out["text"], E_TICKET_TEXT);
}

/// Params naming no attachment at all must be repairable, not a bare
/// deserialization complaint about a missing required field.
#[test]
fn get_attachment_text_without_any_selector_is_invalid_params() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
    let err = h.call("mail.get_attachment_text", serde_json::json!({})).unwrap_err();
    assert_eq!(err.code, codes::INVALID_PARAMS);
    assert!(err.message.contains("message_id"), "got: {}", err.message);
}

/// A 404 on a hash the *planner* supplied is most often a mistyped hash, and
/// the advice says so — the live failure's agent instead concluded that
/// server-side extraction was broken and reported that to the user.
#[test]
fn a_404_on_a_planner_supplied_sha_points_at_the_addressing_not_at_extraction() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::single_without_text())));
    let err = h
        .call("mail.get_attachment_text", serde_json::json!({"sha256": LIVE_SHA}))
        .unwrap_err();
    assert!(err.message.contains("message_id"), "got: {}", err.message);
    assert!(err.message.contains("filename"), "got: {}", err.message);
}

/// The mirror: a hash this worker resolved is right by construction, so the
/// planner must not be sent to re-copy a parameter it never supplied (#536).
#[test]
fn a_404_on_a_resolved_sha_does_not_send_the_planner_to_re_copy_a_hash() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::single_without_text())));
    let err = h
        .call("mail.get_attachment_text", serde_json::json!({"message_id": 37413}))
        .unwrap_err();
    assert!(
        !err.message.contains("message_id"),
        "the message was already named correctly: {}",
        err.message
    );
    assert!(err.message.contains("mail.get_attachment"), "got: {}", err.message);
}

/// A message_id that is not an id gets `ids::explain`'s repair text, exactly
/// as `mail.get_message`'s does — the optional form must not quietly degrade
/// to "expected i64" or, worse, to `None`.
#[test]
fn a_bad_message_id_here_gets_the_same_repair_advice_as_get_message() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
    let err = h
        .call("mail.get_attachment_text", serde_json::json!({"message_id": "{{message_id}}"}))
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_PARAMS);
    assert!(err.message.contains("NO template substitution"), "got: {}", err.message);
}

/// Serialises the tests that drive `mail.get_attachment`.
///
/// `KASTELLAN_WORKER_OUT` is process-global while Rust runs tests in
/// parallel threads, so a second test's `remove_var` lands between the
/// first's `set_var` and its `call` — which is exactly how this crate's
/// full-workspace run failed with "no task output dir" while the same
/// tests passed run alone. The guard, not the ordering, is what makes them
/// deterministic; a poisoned lock is recovered rather than cascading, since
/// one failing test should not turn the others into failures of their own.
///
/// Cleanup runs from `Drop`, not from the tail of this function: `body` is
/// an assertion-bearing closure, so any failing test in here unwinds, and a
/// tail-position `remove_var` would leave the variable set process-wide for
/// whatever ran next — turning one real failure into a run whose other
/// results cannot be trusted.
fn with_out_dir<T>(tag: &str, body: impl FnOnce(&std::path::Path) -> T) -> T {
    static OUT_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            std::env::remove_var("KASTELLAN_WORKER_OUT");
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    let _guard = OUT_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("KASTELLAN_WORKER_OUT", &dir);
    let _restore = Restore(dir.clone());
    body(&dir)
}

// --- get_attachment writes original bytes to out/ safely ---
struct PdfFake;
impl HttpGet for PdfFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn get_authed(&self, _url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
        Ok(RawResponse { status: 200, location: None, content_type: "application/pdf".into(), body: b"%PDF-1.7 body".to_vec() })
    }
}

#[test]
fn get_attachment_writes_to_out_dir_safely() {
    with_out_dir("mailout", |out| {
        let mut h = MailHandler::with_client(client_with(Box::new(PdfFake)));
        let sha = "a".repeat(64);
        let out_json = h
            .call(
                "mail.get_attachment",
                serde_json::json!({"sha256": sha, "filename": "../evil/booking.pdf"}),
            )
            .unwrap();
        let path = std::path::PathBuf::from(out_json["path"].as_str().unwrap());
        assert!(path.starts_with(out), "must stay within out dir: {path:?}");
        assert!(path.exists(), "file written");
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.7 body");
        assert_eq!(out_json["size"], 13);
        assert!(out_json.get("data_base64").is_none(), "no bytes in the result");
        assert!(!path.to_string_lossy().contains(".."), "no traversal in name");
    });
}

/// `get_attachment` carries the same hallucinated-hash exposure as
/// `get_attachment_text`, so it takes the same message form.
#[test]
fn get_attachment_resolves_a_message_and_filename() {
    with_out_dir("mailsel", |_dir| {
        let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
        let out = h
            .call(
                "mail.get_attachment",
                serde_json::json!({"message_id": 37413, "filename": "e-ticket-DQXK68.pdf"}),
            )
            .unwrap();
        assert_eq!(out["sha256"], LIVE_SHA, "the decoy must not be saved");
        // Saved under the ARCHIVE's name, not the substring that selected it.
        let name = out["filename"].as_str().unwrap();
        assert!(name.contains("e-ticket-DQXK68.pdf"), "got {name}");
        assert!(name.starts_with(&LIVE_SHA[..12]), "sha-prefixed: {name}");
        assert!(
            name.contains("Download") || name.contains("470989752"),
            "archive name: {name}"
        );
    });
}

/// Live task 4 (Mac): the planner sent `message_id` *and* the hash, was
/// refused, retried with the bare hash, and the file landed as
/// `71aac4580932_attachment` — the sha form has no archive name to save
/// under. The pair is now taken as the message form, so the file keeps the
/// archive's own name. Covered end-to-end because that filename is the
/// user-visible half.
#[test]
fn get_attachment_by_message_and_sha_saves_under_the_archive_name() {
    with_out_dir("mailboth", |_dir| {
        let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
        let out = h
            .call(
                "mail.get_attachment",
                serde_json::json!({"message_id": 37413, "sha256": LIVE_SHA}),
            )
            .unwrap();
        assert_eq!(out["sha256"], LIVE_SHA);
        let name = out["filename"].as_str().unwrap();
        assert!(name.contains("e-ticket-DQXK68.pdf"), "archive name, not `_attachment`: {name}");
    });
}

/// The pre-existing contract, unchanged: with a `sha256` the `filename`
/// still means the *output* name.
#[test]
fn get_attachment_by_sha_still_treats_filename_as_the_output_name() {
    with_out_dir("mailsha", |_dir| {
        let mut h = MailHandler::with_client(client_with(Box::new(PdfFake)));
        let sha = "a".repeat(64);
        let out = h
            .call(
                "mail.get_attachment",
                serde_json::json!({"sha256": sha, "filename": "chosen.pdf"}),
            )
            .unwrap();
        assert_eq!(out["filename"], "aaaaaaaaaaaa_chosen.pdf");
    });
}

#[test]
fn get_attachment_without_any_selector_is_invalid_params() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
    let err = h.call("mail.get_attachment", serde_json::json!({})).unwrap_err();
    assert_eq!(err.code, codes::INVALID_PARAMS);
    assert!(err.message.contains("message_id"), "got: {}", err.message);
}

/// `get_attachment_text`'s resolved-hash advice ends with "use
/// mail.get_attachment instead", so this tool is the second step of an
/// advice chain. It used to forward localmail's bare `404: attachment
/// <sha> not found` — the ambiguous upstream sentence the sibling tool was
/// fixed for, reached by a planner that did exactly what it was told.
#[test]
fn a_404_from_get_attachment_carries_repair_advice_not_the_upstream_sentence() {
    with_out_dir("mail404", |_dir| {
        let mut h =
            MailHandler::with_client(client_with(Box::new(ArchiveFake::single_missing_blob())));
        let err = h
            .call("mail.get_attachment", serde_json::json!({"message_id": 37413}))
            .unwrap_err();
        assert!(!err.message.contains("localmail 404"), "raw upstream: {}", err.message);
        assert!(err.message.contains("not stored"), "got: {}", err.message);
        // Resolved hash: the planner is not sent to re-copy what it never typed.
        assert!(!err.message.contains("mistyped"), "got: {}", err.message);
    });
}

/// The planner-supplied arm of the same 404.
#[test]
fn a_404_from_get_attachment_on_a_typed_hash_points_at_the_hash() {
    with_out_dir("mail404b", |_dir| {
        let mut h =
            MailHandler::with_client(client_with(Box::new(ArchiveFake::single_missing_blob())));
        let err = h
            .call("mail.get_attachment", serde_json::json!({"sha256": LIVE_SHA}))
            .unwrap_err();
        assert!(err.message.contains("mistyped"), "got: {}", err.message);
        assert!(err.message.contains("message_id"), "got: {}", err.message);
    });
}

/// localmail answers the same 404 for "no such message" and "your ACL
/// excludes it" — deliberately, so permission state is not enumerable. The
/// worker used to forward it, so the agent told the user the mail did not
/// exist.
#[test]
fn an_unreadable_message_id_is_repairable_not_a_bare_upstream_404() {
    let mut h = MailHandler::with_client(client_with(Box::new(ArchiveFake::no_such_message())));
    let err = h
        .call("mail.get_attachment_text", serde_json::json!({"message_id": 99999}))
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_PARAMS);
    assert!(err.message.contains("99999"), "name the id that failed: {}", err.message);
    assert!(err.message.contains("ACL"), "both causes, not just one: {}", err.message);
    assert!(!err.message.contains("localmail 404"), "raw upstream: {}", err.message);
}

/// A 200 that is not a message detail must not be reported as a fact about
/// the archive ("this message has no attachments").
#[test]
fn a_non_array_attachments_field_is_a_service_fault_not_an_empty_message() {
    let mut h =
        MailHandler::with_client(client_with(Box::new(ArchiveFake::attachments_not_an_array())));
    let err = h
        .call("mail.get_attachment_text", serde_json::json!({"message_id": 37413}))
        .unwrap_err();
    assert_eq!(err.code, codes::OPERATION_FAILED);
    assert!(err.message.contains("service fault"), "got: {}", err.message);
}

/// The params structs gained three optional fields each, so the
/// misspelling guard needs pinning on the widened surface too: without
/// `deny_unknown_fields`, `acount_ids` is silently dropped and the search
/// runs unfiltered.
#[test]
fn a_misspelled_parameter_on_a_widened_schema_is_still_refused() {
    let mut h = MailHandler::with_client(client_with(Box::new(BodyEchoFake)));
    let err = h
        .call("mail.search", serde_json::json!({"query": "x", "acount_ids": [1]}))
        .unwrap_err();
    assert_eq!(err.code, codes::INVALID_PARAMS);

    let mut h2 = MailHandler::with_client(client_with(Box::new(ArchiveFake::ok())));
    for method in ["mail.get_attachment_text", "mail.get_attachment"] {
        let err = h2
            .call(method, serde_json::json!({"message_id": 37413, "filenam": "x.pdf"}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS, "{method} accepted a typo'd field");
    }
}
