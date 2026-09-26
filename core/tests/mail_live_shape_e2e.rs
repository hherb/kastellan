//! The live localmail shape gate: our reading of localmail's `/v1` wire
//! shapes, checked against the real service. Moved out of
//! `mail_daemon_e2e.rs`, with which it shares nothing (#763).
//!
//! Run it with `scripts/mail/live-shape-gate.sh`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

/// Fidelity gate: assert real localmail's `/v1` response SHAPES still match
/// `tests-common::mock_localmail`, so the hermetic mock cannot silently drift
/// (the #487 failure mode: mock served `hits`/`text-plain` while reality served
/// `results`/JSON, masking a real decode bug). Uses `curl -k` because the
/// dev-Mac localmail is HTTPS self-signed and the worker's transport is
/// webpki-only (that TLS path is NOT what this test checks).
///
/// **This is the only test in the tree that talks to the live service, and so
/// the only one that can catch "our belief about localmail is wrong" rather than
/// "our fixtures disagree with our code".** Everything else — the mock's own
/// unit tests, the `PathFake` query assertions, the worker e2e — is written from
/// the same reading of the service, so a consistent misreading passes all of
/// them. #527 and #500 were both exactly that.
///
/// Run it via `scripts/mail/live-shape-gate.sh`, which refuses to run without
/// the env rather than letting the skip-as-pass below report a meaningless
/// green. That matters: this gate had itself drifted undetected for months
/// (asserting `results` for the list route, and reading string ids with
/// `as_i64()` so every row was skipped), and correcting its assertions without
/// changing how often it runs would leave the next rot equally invisible.
#[test]
#[ignore = "needs real localmail (KASTELLAN_MAIL_ENDPOINT + KASTELLAN_MAIL_TOKEN); Mac-only"]
fn mock_localmail_shapes_match_real_localmail() {
    let (Ok(endpoint), Ok(token)) = (
        std::env::var("KASTELLAN_MAIL_ENDPOINT"),
        std::env::var("KASTELLAN_MAIL_TOKEN"),
    ) else {
        eprintln!("\n[SKIP] set KASTELLAN_MAIL_ENDPOINT + KASTELLAN_MAIL_TOKEN to the live localmail\n");
        return;
    };

    // `curl -k` a path; return (status code, lowercased response headers,
    // parsed-JSON-or-none).
    //
    // The status is returned — and checked at every leg via `ok_json` — because
    // discarding it misattributes every failure. An expired token makes
    // `/v1/messages` answer `{"detail":"Not authenticated"}`, and the shape
    // assert then reports a phantom schema drift, on the one gate whose entire
    // job is telling real drift from noise.
    let curl = |method: &str, path: &str, body: Option<&str>| -> (u16, String, Option<serde_json::Value>) {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sk", "-D", "-",
            "-X", method,
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "Content-Type: application/json",
        ]);
        if let Some(b) = body {
            cmd.args(["--data", b]);
        }
        cmd.arg(format!("{endpoint}{path}"));
        let out = cmd.output().expect("curl");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (head, body_text) = match text.split_once("\r\n\r\n") {
            Some((h, b)) => (h.to_string(), b.to_string()),
            None => (text.clone(), String::new()),
        };
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0);
        (status, head.to_lowercase(), serde_json::from_str(&body_text).ok())
    };

    // A non-200 is a transport/auth/endpoint problem, not schema drift, and has
    // to say so rather than surfacing as a confusing shape assertion.
    let ok_json = |what: &str, r: (u16, String, Option<serde_json::Value>)| -> serde_json::Value {
        let (status, _head, json) = r;
        assert_eq!(
            status, 200,
            "{what}: live localmail must answer 200 — this is an auth/token/endpoint \
             problem, NOT schema drift (token expiry is the usual cause)"
        );
        json.unwrap_or_else(|| panic!("{what}: expected a JSON body"))
    };

    // 1. search → object with a `results` array (NOT `hits`).
    let search = ok_json("/v1/search", curl("POST", "/v1/search", Some("{\"query\":\"invoice\"}")));
    assert!(
        search.get("results").map(|r| r.is_array()).unwrap_or(false),
        "real localmail search must key hits under `results`: {search}"
    );
    assert!(search.get("hits").is_none(), "real localmail must NOT use `hits` (the #487 drift)");
    // The route the planner copies ids out of, and the source of 7 of the 14
    // live `mail.get_message` failures — and, until now, the ONE id-bearing
    // route this gate did not pin. `/v1/accounts` and `/v1/messages` had their
    // string-ness asserted here while `/v1/search`'s was asserted only in
    // `mock_localmail`'s own unit tests: a claim about the live service checked
    // against our own fixture, which is precisely the circularity that let #527
    // through.
    //
    // Requires a non-empty result set deliberately: guarding the assert on
    // `results[0]` existing would let an archive that matches nothing skip the
    // check and still report success.
    let first_hit = search
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|rows| rows.first())
        .unwrap_or_else(|| panic!(
            "/v1/search for `invoice` matched nothing, so the id-shape check could not run; \
             this gate expects the live archive to contain at least one such message: {search}"
        ));
    assert!(
        first_hit.get("message_id").map(|id| id.is_string()).unwrap_or(false),
        "real localmail /v1/search results[0].message_id must be a STRING (measured live: \
         \"20973\"), not a bare JSON number; got {:?}",
        first_hit.get("message_id")
    );

    // 1b. #760: the worker refuses a localmail below API 1.3 and projects every
    //     search to exactly these keys. Both are claims about the live service,
    //     so they are pinned here (the worker crate is bin-only; its
    //     `version::MIN_API_MINOR` and `search_params::HIT_FIELDS` are mirrored).
    let version = ok_json("/v1/version", curl("GET", "/v1/version", None));
    assert_eq!(version["api_major"], 1, "#760: the mail worker speaks API 1.x: {version}");
    assert!(
        version["api_minor"].as_u64().is_some_and(|m| m >= 3),
        "#760: the mail worker needs api_minor >= 3 (slice E); got {version}"
    );
    let fields = ["message_id", "account", "subject", "from", "date", "has_attachments", "snippet"];
    let projected = ok_json(
        "/v1/search with fields",
        curl(
            "POST",
            "/v1/search",
            Some(&serde_json::json!({"query": "invoice", "limit": 3, "fields": fields, "snippet_chars": 120}).to_string()),
        ),
    );
    let hit = projected["results"].get(0).and_then(|h| h.as_object()).unwrap_or_else(|| {
        panic!("#760: projected /v1/search for `invoice` matched nothing: {projected}")
    });
    let mut got: Vec<&str> = hit.keys().map(String::as_str).collect();
    let mut want = fields.to_vec();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want, "#760: a projected hit must carry exactly the named keys");

    // 2. accounts → JSON array.
    let accounts = ok_json("/v1/accounts", curl("GET", "/v1/accounts", None));
    assert!(accounts.is_array(), "real localmail /v1/accounts must be a JSON array");
    // #527/#500's central discovery, pinned against the live service directly:
    // until now this was asserted only in the mock's own unit tests, which is
    // a claim ABOUT the live service verified against the live service
    // nowhere — precisely this gate's job.
    assert!(
        accounts.get(0).and_then(|a| a.get("id")).map(|id| id.is_string()).unwrap_or(false),
        "real localmail /v1/accounts[0].id must be a STRING (measured live: \"1\"), not a bare \
         JSON number; got {:?}",
        accounts.get(0)
    );

    // 3. attachment text → application/json {"text": …} (NOT text/plain). Find a
    //    real attachment sha via list → message, then via a filter-only
    //    `has_attachment` search (#698) — the 50 newest messages can easily
    //    carry none, which is how this leg once checked nothing while passing.
    //    Finding none at all is a failure, asserted below the loop.
    let list = ok_json("/v1/messages", curl("GET", "/v1/messages?limit=50", None));
    // The LIST route keys rows under `messages` and the SEARCH route under
    // `results` — they differ, and this gate asserted `results` for both until
    // 2026-08-09. Measured live: `/v1/messages` returns exactly
    // ["messages", "next_cursor"]. get_message's shape is pinned below.
    let rows = list
        .get("messages")
        .and_then(|r| r.as_array())
        .unwrap_or_else(|| panic!("real localmail /v1/messages must key rows under `messages`: {list}"));
    // #527/#500's central discovery, pinned against the live service directly
    // (see the /v1/accounts assert above for why this gate, not the mock's own
    // unit tests, is where the claim belongs). Keep the lenient
    // `as_i64().or_else(as_str)` extraction in the loop below for robustness —
    // this assert is the guard.
    let first_message_id = rows.first().and_then(|row| row.get("message_id"));
    assert!(
        first_message_id.map(|id| id.is_string()).unwrap_or(false),
        "real localmail /v1/messages[0].message_id must be a STRING (measured live: \"37477\"), \
         not a bare JSON number; got {first_message_id:?}"
    );
    let with_attachments = ok_json(
        "/v1/search (has_attachment)",
        curl(
            "POST",
            "/v1/search",
            Some(r#"{"query":"","filters":{"has_attachment":true},"sort":"date","limit":20}"#),
        ),
    );
    let attachment_rows = with_attachments["results"].as_array().cloned().unwrap_or_default();
    let mut sha: Option<String> = None;
    // The same attachment by position: (message id, index), for slice D's route.
    let mut at: Option<(String, usize)> = None;
    // Checked once, on the first detail actually fetched (see below).
    let mut detail_shape_checked = false;
    // Checked once, alongside the detail shape.
    let mut header_spelling_checked = false;
    for row in rows.iter().chain(attachment_rows.iter()) {
        // localmail serves ids as STRINGS. `as_i64()` alone returns None for
        // every row, so this loop used to skip the whole archive and exercise
        // nothing — the silent pass the assert below the loop exists to catch.
        let Some(id) = row
            .get("message_id")
            .or_else(|| row.get("id"))
            .and_then(|v| v.as_i64().map(|i| i.to_string()).or_else(|| v.as_str().map(str::to_owned)))
        else {
            continue;
        };
        let (_status, _h, msg) = curl("GET", &format!("/v1/messages/{id}"), None);

        // 3a. get_message's own field shape. Previously this loop used the
        // detail response only to discover an attachment sha, so the
        // message-detail fields were the ONE surface this anti-drift gate
        // did not pin — which is exactly how `mock_localmail` was able to
        // drift into a mail-tool-only shape (`"from"` as a bare string,
        // `"body"` instead of `"body_text"`) that `workers/email-in`
        // cannot parse at all. That drift is silent by construction:
        // `build_event` reads `from.address`, gets `None`, and records the
        // message as `skipped` rather than erroring. Both asserts below
        // fail loudly on exactly that shape — indexing a JSON string with
        // `["address"]` yields `Null`, so `is_string()` is `false`.
        if let Some(msg) = msg.as_ref() {
            if !detail_shape_checked {
                detail_shape_checked = true;
                assert!(
                    msg["from"]["address"].is_string(),
                    "real localmail /v1/messages/{{id}} must serve `from` as an ADDRESS \
                     OBJECT (`_address()` → {{address, name}}), not a bare string — \
                     email-in reads `from.address`; got from = {}",
                    msg["from"]
                );
                assert!(
                    msg.get("body_text").is_some(),
                    "real localmail /v1/messages/{{id}} must name the plain-text body \
                     `body_text` (not `body`); got keys {:?}",
                    msg.as_object().map(|o| o.keys().collect::<Vec<_>>())
                );
            }
        }

        // 3b. #500, pinned against the live service for the first time.
        //
        // localmail reads a differently NAMED query parameter, `headers`,
        // whose VALUE picks the shape (`serve/routes/messages.py::detail`,
        // `headers: str = Query("compact")`). Until #500 that claim lived in a
        // code comment and in fixtures written from the same reading — our
        // own mock modelling the gate, and a unit test pinning the model. A
        // consistent misreading passed every test in the tree.
        //
        // Two spellings are live: email-in sends `?headers=full` (a name-keyed
        // object for its DMARC check) and the mail worker `?headers=list`
        // (#760, localmail #381: one `{name, value}` per occurrence, in wire
        // order). Both must deliver, the list with exactly the two keys
        // `headers::header_list_error` accepts — a third key there would turn
        // every `full_headers` read into a fault. And the spelling the worker
        // used to send must still NOT work — if it starts to, the service
        // gained an alias and `detail_path`'s translation deserves review.
        if !header_spelling_checked && msg.is_some() {
            header_spelling_checked = true;
            let full = ok_json(
                "/v1/messages/{id}?headers=full",
                curl("GET", &format!("/v1/messages/{id}?headers=full"), None),
            );
            assert!(
                full.get("headers").and_then(|h| h.as_object()).map(|o| !o.is_empty()).unwrap_or(false),
                "#500: `?headers=full` — the spelling email-in sends — must return a \
                 non-empty `headers` object; got keys {:?}",
                full.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
            let list = ok_json(
                "/v1/messages/{id}?headers=list",
                curl("GET", &format!("/v1/messages/{id}?headers=list"), None),
            );
            let entries = list.get("headers").and_then(|h| h.as_array()).cloned().unwrap_or_default();
            assert!(
                !entries.is_empty(),
                "#760: `?headers=list` — the spelling handler::detail_path sends — must \
                 return a non-empty `headers` array; got keys {:?}",
                list.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
            for e in &entries {
                let keys: Vec<&String> = e.as_object().map(|o| o.keys().collect()).unwrap_or_default();
                assert!(
                    keys.len() == 2 && e["name"].is_string() && e["value"].is_string(),
                    "#760: every `?headers=list` entry must be exactly {{name, value}} strings \
                     (headers::header_list_error refuses anything else); got keys {keys:?}"
                );
            }
            // #760's point is one entry PER OCCURRENCE: the shape check above
            // would pass a server that kept one value per name, or dropped the
            // `Received` chain. `full` holds every occurrence grouped by name,
            // so `list` must hold exactly as many, and each name's values in
            // the same order.
            let grouped = full["headers"].as_object().expect("checked non-empty above");
            let occurrences: usize = grouped.values().map(|v| v.as_array().map_or(0, Vec::len)).sum();
            assert_eq!(
                entries.len(),
                occurrences,
                "#760: `?headers=list` must carry one entry per occurrence — `?headers=full` \
                 holds {occurrences} across {} names; message {id}",
                grouped.len()
            );
            for (name, values) in grouped {
                let from_list: Vec<&serde_json::Value> =
                    entries.iter().filter(|e| e["name"].as_str() == Some(name.as_str())).map(|e| &e["value"]).collect();
                let from_full: Vec<&serde_json::Value> =
                    values.as_array().map(|a| a.iter().collect()).unwrap_or_default();
                assert_eq!(
                    from_list.len(),
                    from_full.len(),
                    "#760: a header's occurrence count differs between `list` and `full`; message {id}"
                );
                assert!(
                    from_list == from_full,
                    "#760: a header's values differ, or are in a different order, between `list` \
                     and `full`; message {id}"
                );
            }
            let wrong = ok_json(
                "/v1/messages/{id}?full_headers=true",
                curl("GET", &format!("/v1/messages/{id}?full_headers=true"), None),
            );
            assert!(
                wrong.get("headers").is_none(),
                "#500: `?full_headers=true` is the spelling this worker used to send, and \
                 localmail DROPS it — that asymmetry is the whole bug. If it now yields \
                 headers the service changed; got keys {:?}",
                wrong.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
        }

        // #760: the worker writes each entry's position in as `index`
        // (`detail::number_attachments`), overwriting any `index` the service
        // served. Pinned here, where a localmail that starts sending one of its
        // own would first be seen.
        if let Some(atts) = msg.as_ref().and_then(|m| m["attachments"].as_array()) {
            assert!(
                atts.iter().all(|a| a.get("index").is_none()),
                "#760: localmail now serves its own `index` in attachment entries, which \
                 detail::number_attachments overwrites with the position — check it means \
                 the same thing; message {id}"
            );
        }

        if let Some((i, s)) = msg
            .as_ref()
            .and_then(|m| m.get("attachments"))
            .and_then(|a| a.as_array())
            .and_then(|atts| {
                atts.iter()
                    .enumerate()
                    .find_map(|(i, a)| a.get("sha256").and_then(|s| s.as_str()).map(|s| (i, s)))
            })
        {
            sha = Some(s.to_string());
            at = Some((id.clone(), i));
            break;
        }
    }
    // This gate exists specifically to catch the shared `mock_localmail` test
    // double drifting from the real service's field shape (see the comment
    // above `detail_shape_checked`'s first use) — that drift already happened
    // once and was fixed on this branch. `detail_shape_checked` is set inside
    // the loop above but was never asserted afterwards: if `/v1/messages`
    // returned zero rows, or every per-id `GET` above failed (`msg` is
    // `None`), the loop runs to completion having exercised nothing and the
    // test would still report success — exactly the silent pass this gate is
    // meant to prevent. Fail loudly instead.
    assert!(
        detail_shape_checked,
        "the message-detail shape check never ran (zero rows from /v1/messages, or every \
         per-id GET to /v1/messages/{{id}} failed) — this anti-drift gate checked nothing; \
         see the mock_localmail drift this test exists to catch"
    );
    // Same reasoning as `detail_shape_checked` above, for #500's half: a leg
    // that never ran must not read as a leg that passed.
    assert!(
        header_spelling_checked,
        "the #500 header-spelling check never ran — no message detail was fetched, so \
         `?headers=full`, `?headers=list` and `?full_headers=true` were verified against nothing"
    );
    // A leg that never ran must not read as a leg that passed — the same rule
    // as the two asserts above. This used to print a `[NOTE]` and return, which
    // no marker count sees, so an archive without a stored attachment passed
    // the whole attachment half (and #760's slice D leg) having checked nothing.
    let sha = sha.expect(
        "no message among the listed rows carries a stored attachment, so the attachment \
         text and slice D index-route checks verified nothing — point the gate at an archive \
         (or token) that can see one",
    );
    let (status, head, text) = curl("GET", &format!("/v1/attachments/{sha}/text"), None);
    assert_eq!(status, 200, "attachment text must answer 200, headers:\n{head}");
    assert!(
        head.contains("application/json"),
        "attachment text must be application/json (the #487 contract), headers:\n{head}"
    );
    assert!(
        text.and_then(|v| v.get("text").map(|t| t.is_string())).unwrap_or(false),
        "attachment text must be a JSON {{\"text\": …}} envelope"
    );

    // #760 (slice D): the same attachment by position, paged. The worker
    // copies `next_offset` rather than computing it, and treats a body missing
    // any paging field as a fault — so all four must be on the wire.
    let (id, i) = at.expect("set beside sha");
    let page = ok_json(
        "/v1/messages/{id}/attachments/{index}/text",
        curl("GET", &format!("/v1/messages/{id}/attachments/{i}/text?offset=0&limit=5"), None),
    );
    assert!(page["text"].is_string(), "#760: paged text must carry `text`: {page}");
    assert_eq!(page["offset"], 0, "#760: {page}");
    let total = page["total"].as_u64().unwrap_or_else(|| panic!("#760: `total` must be a count: {page}"));
    let want_next = if total > 5 { serde_json::json!(5) } else { serde_json::Value::Null };
    assert_eq!(page["next_offset"], want_next, "#760: next_offset for a 5-char window: {page}");
    let (status, head, _) = curl("GET", &format!("/v1/messages/{id}/attachments/{i}"), None);
    assert_eq!(status, 200, "#760: the index bytes route must answer 200, headers:\n{head}");

    // #760: the worker checks bytes fetched by position against the sha256 the
    // listing gave (`attach::Picked::verify_bytes`), on the premise that
    // localmail stores a blob under the hash of exactly the bytes this route
    // serves. Proven here against the real service — a hermetic fixture can
    // only agree with whoever wrote it. Raw bytes, so not through `curl` above
    // (which reads the body as lossy UTF-8).
    let raw = Command::new("curl")
        .args(["-skf", "-H", &format!("Authorization: Bearer {token}")])
        .arg(format!("{endpoint}/v1/messages/{id}/attachments/{i}"))
        .output()
        .expect("curl");
    assert!(raw.status.success(), "#760: raw fetch of the index bytes route failed");
    use sha2::{Digest, Sha256};
    assert_eq!(
        format!("{:x}", Sha256::digest(&raw.stdout)),
        sha,
        "#760: the index route's bytes must hash to the sha256 get_message listed at that \
         position, or verify_bytes refuses every message-resolved get_attachment"
    );
}
