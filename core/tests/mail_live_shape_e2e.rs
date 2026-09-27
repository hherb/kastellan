//! The live localmail shape gate: our reading of localmail's `/v1` wire
//! shapes, checked against the real service. Moved out of
//! `mail_daemon_e2e.rs`, with which it shares nothing (#763).
//!
//! Run it with `scripts/mail/live-shape-gate.sh`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use kastellan_tests_common::live_localmail;

/// The mail worker's own wire constants — the values it actually sends and
/// requires — compiled in from its source rather than copied (#763). The mail
/// crate is bin-only, so this test cannot import them; a hand-copied list here
/// once meant a change to `HIT_FIELDS` would leave this gate checking the old
/// one, and passing. The file's own header says what it may contain.
mod contract {
    include!("../../workers/mail/src/localmail_contract.rs");
}

/// Fidelity gate: assert real localmail's `/v1` response SHAPES are the ones
/// `tests-common::mock_localmail` serves (the mock's own unit tests pin the same
/// shapes, and since #763 read the same `contract` constants), so the hermetic
/// mock cannot silently drift
/// (the #487 failure mode: mock served `hits`/`text-plain` while reality served
/// `results`/JSON, masking a real decode bug). Uses `curl -k` because the
/// dev-Mac localmail is HTTPS self-signed and the worker's transport is
/// webpki-only (that TLS path is NOT what this test checks).
///
/// **This is the only test in the tree that checks our reading of localmail's
/// wire shapes against the live service, and so the only one that can catch
/// "our belief about localmail is wrong" rather than "our fixtures disagree with
/// our code".** (`mail_e2e`'s `force_routed_search_against_real_localmail` also
/// reaches a live localmail, but tests the MITM/extra-CA path, not the shapes.)
/// Everything else — the mock's own
/// unit tests, the `PathFake` query assertions, the worker e2e — is written from
/// the same reading of the service, so a consistent misreading passes all of
/// them. #527 and #500 were both exactly that.
///
/// Run it via `scripts/mail/live-shape-gate.sh`, which finds the credentials
/// and runs the `mail-live` gate profile. That matters: this gate had itself
/// drifted undetected for months (asserting `results` for the list route, and
/// reading string ids with `as_i64()` so every row was skipped), and correcting
/// its assertions without changing how often it runs would leave the next rot
/// equally invisible. Under the profile's `KASTELLAN_MAIL_LIVE_REQUIRE_E2E` a
/// missing credential fails instead of skipping, and the `[E2E]` line it
/// announces is what the profile counts to prove the test ran at all (#763).
#[test]
#[ignore = "needs a live localmail — run scripts/mail/live-shape-gate.sh"]
fn mock_localmail_shapes_match_real_localmail() {
    let Some(credentials) = live_localmail::credentials_or_skip() else {
        return;
    };
    let (endpoint, token) = (credentials.endpoint(), credentials.token());

    // `curl -k` a path; return (status code, lowercased response headers,
    // parsed-JSON-or-none).
    //
    // curl's own failure — refused connection, DNS, TLS, a malformed URL — is
    // a panic here, with curl's exit code and its stderr (`-S`). Without that
    // it came back as status 0, and `ok_json` below blamed the token for a
    // localmail that was simply down.
    //
    // The status is returned — and checked at every leg via `ok_json` — because
    // discarding it misattributes every failure. An expired token makes
    // `/v1/messages` answer `{"detail":"Not authenticated"}`, and the shape
    // assert then reports a phantom schema drift, on the one gate whose entire
    // job is telling real drift from noise.
    let curl = |method: &str, path: &str, body: Option<&str>| -> (u16, String, Option<serde_json::Value>) {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sSk", "-D", "-",
            "-X", method,
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "Content-Type: application/json",
        ]);
        if let Some(b) = body {
            cmd.args(["--data", b]);
        }
        cmd.arg(format!("{endpoint}{path}"));
        let out = cmd.output().expect("curl");
        assert!(
            out.status.success(),
            "curl could not reach {endpoint}{path} ({}): {} — localmail is down, unreachable, \
             or refusing TLS; NOT an auth problem and NOT schema drift",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
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

    // A non-200 is an auth/endpoint problem, not schema drift, and has to say
    // so rather than surfacing as a confusing shape assertion. (A transport
    // failure never gets here: `curl` above panics on it.)
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

    // 1b. #760: the worker refuses a localmail below its minimum API and
    //     projects every search to exactly `HIT_FIELDS`, with `SNIPPET_CHARS`.
    //     All are claims about the live service, so they are pinned here —
    //     with the worker's own values (see `contract` above), so the gate
    //     checks what the worker sends rather than a copy of it.
    let version = ok_json("/v1/version", curl("GET", "/v1/version", None));
    assert_eq!(
        version["api_major"].as_u64(),
        Some(contract::API_MAJOR),
        "#760: the mail worker speaks API {}.x: {version}",
        contract::API_MAJOR
    );
    assert!(
        version["api_minor"].as_u64().is_some_and(|m| m >= contract::MIN_API_MINOR),
        "#760: the mail worker needs api_minor >= {}; got {version}",
        contract::MIN_API_MINOR
    );
    let fields = contract::HIT_FIELDS;
    let projected = ok_json(
        "/v1/search with fields",
        curl(
            "POST",
            "/v1/search",
            Some(
                &serde_json::json!({
                    "query": "invoice",
                    "limit": 3,
                    "fields": fields,
                    "snippet_chars": contract::SNIPPET_CHARS,
                })
                .to_string(),
            ),
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
    // …and `snippet_chars` must be honoured, not merely accepted: a localmail
    // that ignored it would serve its default window (200) and quietly undo
    // #760's payload saving while every key check above still passed. It may
    // add a `…` at each end, hence the 2.
    let snippets: Vec<&str> =
        projected["results"].as_array().into_iter().flatten().filter_map(|h| h["snippet"].as_str()).collect();
    assert!(!snippets.is_empty(), "#760: no projected hit carried a string `snippet`: {projected}");
    for s in snippets {
        assert!(
            s.chars().count() <= contract::SNIPPET_CHARS as usize + 2,
            "#760: localmail must cut `snippet` to snippet_chars = {} (+ an ellipsis each end); \
             got {} chars",
            contract::SNIPPET_CHARS,
            s.chars().count()
        );
    }

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
    // Asserted rather than defaulted: an empty fallback here would surface
    // below as "the archive has no attachment", blaming the data for drift.
    let attachment_rows = with_attachments["results"].as_array().cloned().unwrap_or_else(|| {
        panic!("filter-only /v1/search must key hits under `results`: {with_attachments}")
    });
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
        // Through `ok_json` like every other leg: an auth error body here used
        // to reach the shape asserts below and fail as "schema drift".
        let msg = ok_json("/v1/messages/{id}", curl("GET", &format!("/v1/messages/{id}"), None));

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
        if !header_spelling_checked {
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
            // The worker's own spelling, from `contract` — #500 was a query
            // spelling this gate had copied by hand.
            let list = ok_json(
                "/v1/messages/{id}?headers=list",
                curl("GET", &format!("/v1/messages/{id}?{}", contract::HEADER_LIST_QUERY), None),
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
        if let Some(atts) = msg["attachments"].as_array() {
            assert!(
                atts.iter().all(|a| a.get("index").is_none()),
                "#760: localmail now serves its own `index` in attachment entries, which \
                 detail::number_attachments overwrites with the position — check it means \
                 the same thing; message {id}"
            );
        }

        if let Some((i, s)) = msg
            .get("attachments")
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
    // meant to prevent. Fail loudly instead. (A failed per-id GET now fails at
    // its own `ok_json`, so what is left to catch here is a loop with no row.)
    assert!(
        detail_shape_checked,
        "the message-detail shape check never ran (no row with an id from /v1/messages or \
         the has_attachment search) — this anti-drift gate checked nothing; see the \
         mock_localmail drift this test exists to catch"
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
    // Every route below is spelled by `contract` — the functions the worker's
    // `attach::Picked` builds its paths with — never written out here (#767).
    // A hand copy is how #500 passed every test: the gate checked its own
    // spelling of a query the worker no longer sent.
    let by_sha = contract::attachment_by_sha_path(&sha);
    let (id, i) = at.expect("set beside sha");
    let by_index = contract::attachment_by_index_path(&id, i);

    // The text routes, in the paged form the worker sends. The worker copies
    // `next_offset` rather than computing it, and treats a body missing any
    // paging field as a fault — so all four must be on the wire, on BOTH
    // routes (#760 slice D). The hash route is the one a planner-typed
    // `sha256` reads; until #767 this gate fetched it unpaged, a request the
    // worker never makes.
    //
    // The window starts at 2, not 0: a paging parameter the service did not
    // recognise would be dropped and read as its default, and the default
    // offset IS 0 — so at 0 a respelled `offset` passed this check. At 2 a
    // dropped `offset` fails the echo, and a dropped `limit` the `next_offset`.
    const AT: u64 = 2;
    const WINDOW: u32 = 5;
    for blob in [&by_sha, &by_index] {
        let (status, head, page) = curl("GET", &contract::text_page_path(blob, AT, WINDOW), None);
        assert_eq!(status, 200, "{blob}/text must answer 200, headers:\n{head}");
        assert!(
            head.contains("application/json"),
            "{blob}/text must be application/json (the #487 contract), headers:\n{head}"
        );
        let page = page.unwrap_or_else(|| panic!("{blob}/text: expected a JSON body"));
        assert!(page["text"].is_string(), "{blob}/text must be a {{\"text\": …}} envelope: {page}");
        assert_eq!(page["offset"], AT, "#760: {blob}/text must echo the offset: {page}");
        let total = page["total"].as_u64().unwrap_or_else(|| panic!("#760: `total` must be a count: {page}"));
        let end = AT + u64::from(WINDOW);
        // Without this, a text that ends inside the window has `next_offset:
        // null` whether or not `limit` was honoured, and the leg below would
        // pass having checked nothing about it.
        assert!(
            total > end,
            "{blob}/text holds only {total} chars, so a dropped `limit` could not be told apart \
             from an honoured one — point the gate at an archive whose first stored attachment \
             has more than {end} chars of text"
        );
        assert_eq!(page["next_offset"], end, "#760: next_offset for a {WINDOW}-char window at {AT}: {page}");
    }

    // The bytes routes. localmail stores a blob under the sha256 of exactly
    // the bytes it serves (`attachments.py`); the worker relies on that twice.
    // By position, `attach::Picked::verify_bytes` checks the bytes against the
    // listed sha256 and refuses a mismatch. By hash, it does not re-check at
    // all, on the premise that localmail already matched them. Both are proven
    // here against the real service, since a hermetic fixture can only agree
    // with whoever wrote it.
    for blob in [&by_index, &by_sha] {
        assert_eq!(
            raw_sha256(endpoint, token, blob),
            sha,
            "#760: {blob} must serve bytes hashing to the sha256 get_message listed, or \
             verify_bytes refuses every message-resolved get_attachment (index route) and a \
             planner-typed hash saves unverified bytes (hash route)"
        );
    }
}

/// The sha256 of the raw bytes localmail serves at `path`, hex. Not through
/// the test's `curl` closure, which reads the body as lossy UTF-8.
fn raw_sha256(endpoint: &str, token: &str, path: &str) -> String {
    use sha2::{Digest, Sha256};
    let raw = Command::new("curl")
        .args(["-skf", "-H", &format!("Authorization: Bearer {token}")])
        .arg(format!("{endpoint}{path}"))
        .output()
        .expect("curl");
    assert!(raw.status.success(), "raw fetch of {path} failed ({})", raw.status);
    format!("{:x}", Sha256::digest(&raw.stdout))
}
