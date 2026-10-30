//! Unit tests for [`super`] — the mail worker's manifest and tool schema.

use super::*;

/// Minimal stub `ResolveCtx` driven by closures over fixed maps.
fn ctx<'a>(
    env: &'a dyn Fn(&str) -> Option<String>,
    exists: &'a dyn Fn(&Path) -> bool,
) -> ResolveCtx<'a> {
    ResolveCtx {
        get_env: env,
        exists,
        is_dir: &|_p| false,
        exe_dir: None,
        canonicalize: &|_p| None,
        allowlist: &|_t| Vec::new(),
    }
}

#[test]
fn disabled_when_endpoint_unset() {
    let env = |_k: &str| None;
    let exists = |_p: &Path| true;
    match MailManifest.resolve(&ctx(&env, &exists)) {
        Resolution::Disabled { .. } => {}
        _ => panic!("expected Disabled when endpoint unset"),
    }
}

#[test]
fn misconfigured_when_token_file_missing() {
    let env = |k: &str| match k {
        "KASTELLAN_MAIL_ENDPOINT" => Some("http://127.0.0.1:8000".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| false;
    match MailManifest.resolve(&ctx(&env, &exists)) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("KASTELLAN_MAIL_TOKEN_FILE"), "detail: {detail}");
        }
        _ => panic!("expected Misconfigured when token file missing"),
    }
}

#[test]
fn entry_allowlists_only_the_endpoint_and_binds_token_file() {
    let entry = mail_entry(
        PathBuf::from("/opt/kastellan-worker-mail"),
        "http://127.0.0.1:8000",
        "/run/kastellan/mail-token",
    );
    match &entry.policy.net {
        Net::Allowlist(entries) => {
            assert_eq!(entries, &vec!["127.0.0.1:8000".to_string()], "only the endpoint");
        }
        other => panic!("expected Net::Allowlist, got {other:?}"),
    }
    assert!(
        entry.policy.fs_read.contains(&PathBuf::from("/run/kastellan/mail-token")),
        "token file must be readable in the jail"
    );
    // Token PATH is in env; the plaintext is never in policy.env.
    assert!(entry
        .policy
        .env
        .iter()
        .any(|(k, v)| k == "KASTELLAN_MAIL_TOKEN_FILE" && v == "/run/kastellan/mail-token"));
    assert!(matches!(entry.lifecycle, crate::worker_lifecycle::Lifecycle::SingleUse));
}

#[test]
fn endpoint_with_explicit_port_and_default_port() {
    assert_eq!(net_entries_from_endpoint("http://127.0.0.1:8000"), vec!["127.0.0.1:8000"]);
    assert_eq!(net_entries_from_endpoint("https://mail.vpn.example"), vec!["mail.vpn.example:443"]);
    assert!(net_entries_from_endpoint("not a url").is_empty());
}

#[test]
fn advertises_all_six_tools() {
    let methods: Vec<&str> = MailManifest.tool_docs().iter().map(|d| d.method).collect();
    assert_eq!(
        methods,
        vec![
            "mail.search",
            "mail.get_message",
            "mail.list_messages",
            "mail.list_accounts",
            "mail.get_attachment_text",
            "mail.get_attachment",
        ]
    );
}

/// #698: a filter-only search ("every message with an attachment") is a
/// search, and the worker accepts it with no `query`. Advertising `query`
/// as required told the planner otherwise — and when it tried anyway (DGX
/// audit row 3777) the old worker refused it and cost a plan iteration.
/// The description has to say how to write one, or the planner invents
/// a query text to satisfy a slot it believes it must fill.
#[test]
fn search_advertises_query_as_optional_and_says_how_to_filter_only() {
    let docs = MailManifest.tool_docs();
    let d = docs.iter().find(|d| d.method == "mail.search").expect("mail.search");
    let query = d.params.iter().find(|p| p.name == "query").expect("query");
    assert!(!query.required, "a filter-only search needs no query");
    assert!(
        query.description.contains("filters") && query.description.contains("omit"),
        "must say a filter-only search omits query: {:?}",
        query.description
    );
    let sort = d.params.iter().find(|p| p.name == "sort").expect("sort");
    assert!(
        sort.description.contains("no query"),
        "must say what ordering a filter-only search gets: {:?}",
        sort.description
    );
}

/// localmail's plain search is `plainto_tsquery('simple', …)`: EVERY word
/// must appear in a hit, and `OR` is not an operator but one more word to
/// match. DGX task 211 (2026-10-29) asked for the most recent flight booking
/// with `flight booking OR flight confirmation OR airline OR reservation`,
/// matched only two 2025 emails, and answered with a 17-month-old booking
/// while `e-ticket itinerary` would have found the one from 28 Oct. The
/// description is the only place the planner can learn the dialect.
#[test]
fn search_query_says_every_word_must_match_and_or_is_not_an_operator() {
    let docs = MailManifest.tool_docs();
    let d = docs.iter().find(|d| d.method == "mail.search").expect("mail.search");
    let query = d.params.iter().find(|p| p.name == "query").expect("query");
    assert!(
        query.description.contains("EVERY word"),
        "must say all words are required: {:?}",
        query.description
    );
    assert!(
        query.description.contains("no OR"),
        "must say OR is not an operator: {:?}",
        query.description
    );
    assert!(
        query.description.contains("separate searches"),
        "must say how to search for alternatives: {:?}",
        query.description
    );
}

/// localmail's `sort: "date"` bypasses the hybrid pool: it is a keyword-only
/// walk, so the semantic arms never run. The old doc said every "most recent"
/// question MUST use it, so the planner never once searched by meaning. On the
/// DGX (2026-10-30) the ranked query "fee payable to council", bounded to
/// October, returned the council fee email and a "Payment overdue" notice in
/// its top three. Neither contains "invoice", which was the planner's
/// date-sorted query. The doc must say what each sort does and how to get
/// "the most recent X" out of the semantic one.
#[test]
fn search_says_date_sort_is_keyword_only_and_rank_is_semantic() {
    let docs = MailManifest.tool_docs();
    let d = docs.iter().find(|d| d.method == "mail.search").expect("mail.search");
    let sort = d.params.iter().find(|p| p.name == "sort").expect("sort");
    assert!(sort.description.contains("keyword-only"), "{:?}", sort.description);
    assert!(sort.description.contains("semantic"), "{:?}", sort.description);
    assert!(
        sort.description.contains("date_from"),
        "must say how to bound a ranked search to recent mail: {:?}",
        sort.description
    );
    // Unbounded, semantic rank puts the BEST match first, from any year, so
    // the newest relevant mail can be outranked. The window has to start
    // narrow and widen only when it finds nothing. (A narrow window is also
    // exact on localmail: Postgres sorts the window's chunks by distance
    // instead of taking the approximate HNSW top-K, EXPLAIN on the DGX
    // 2026-10-30.)
    assert!(
        sort.description.contains("narrow") && sort.description.contains("widen"),
        "must say to start with a narrow window and widen it: {:?}",
        sort.description
    );
    assert!(
        !sort.description.contains("must pass sort"),
        "must no longer force every recency question onto the keyword walk: {:?}",
        sort.description
    );
    let query = d.params.iter().find(|p| p.name == "query").expect("query");
    assert!(
        query.description.contains("describe"),
        "must say a ranked query should describe the mail: {:?}",
        query.description
    );
}

/// The live defect this parameter set exists to end (task 160, 2026-08-17):
/// the tool took exactly one parameter — a 64-char sha256 — and the planner,
/// which had the right hash in its prompt head, supplied a different one.
/// localmail 404'd, and the agent told the user that PDF extraction had
/// failed while the extracted text sat in the database.
///
/// The schema is where a planner learns there is a form that needs no hash,
/// and `assemble` renders params in **declaration order**, so the order is a
/// commitment rather than a formatting preference.
#[test]
fn get_attachment_text_offers_the_message_form_first_and_demands_no_hash() {
    let docs = MailManifest.tool_docs();
    let d = docs
        .iter()
        .find(|d| d.method == "mail.get_attachment_text")
        .expect("mail.get_attachment_text must be advertised");
    let names: Vec<&str> = d.params.iter().map(|p| p.name).collect();
    assert_eq!(
        names,
        vec!["message_id", "filename", "index", "sha256", "offset"],
        "the form that needs no hash must be read first"
    );
    assert!(
        d.params.iter().all(|p| !p.required),
        "either form suffices, so marking any one parameter required would be a lie"
    );
    assert!(
        d.summary.contains("message_id"),
        "the summary is read before the params: {:?}",
        d.summary
    );
    let sha = d.params.iter().find(|p| p.name == "sha256").expect("sha256 still accepted");
    assert!(
        sha.description.contains("message_id"),
        "a planner reaching for the hash must learn there is another way: {:?}",
        sha.description
    );
}

/// `mail.get_attachment` carries the same hallucinated-hash exposure as its
/// sibling, so it advertises the same message form — and its `filename`
/// description has to say that the parameter now does two jobs, because with
/// `sha256` it still means the output name.
#[test]
fn get_attachment_offers_the_message_form_and_explains_the_double_duty_filename() {
    let docs = MailManifest.tool_docs();
    let d = docs
        .iter()
        .find(|d| d.method == "mail.get_attachment")
        .expect("mail.get_attachment must be advertised");
    let names: Vec<&str> = d.params.iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["message_id", "filename", "index", "sha256"]);
    assert!(d.params.iter().all(|p| !p.required), "either form suffices");
    let fname = d.params.iter().find(|p| p.name == "filename").expect("filename");
    assert!(
        fname.description.contains("message_id") && fname.description.contains("sha256"),
        "must distinguish the two meanings: {:?}",
        fname.description
    );
}

/// Live task 161 spent its whole budget writing `account_ids` where
/// `mail.list_messages` takes it. The worker now accepts it there, and the
/// schema has to say so or the planner has no way to learn it.
#[test]
fn search_advertises_the_top_level_id_filters_the_planner_actually_writes() {
    let docs = MailManifest.tool_docs();
    let d = docs.iter().find(|d| d.method == "mail.search").expect("mail.search");
    let names: Vec<&str> = d.params.iter().map(|p| p.name).collect();
    for want in ["account_ids", "folder_ids"] {
        assert!(names.contains(&want), "mail.search must advertise {want}: {names:?}");
    }
    let acct = d.params.iter().find(|p| p.name == "account_ids").expect("account_ids");
    assert!(
        acct.description.contains("filters"),
        "must say the nested form exists and that both is an error: {:?}",
        acct.description
    );
}

/// The `message_id` description is load-bearing, not prose: the live audit
/// log recorded 4 dispatches sending the literal `{{message_id}}` and 3
/// sending `next_cursor`, and this tool is the ONLY one in the log that has
/// ever been given a `{{…}}` placeholder. So the description must name the
/// field, rule out the adjacent one, and forbid the placeholder.
///
/// Asserted by substring rather than by pinning the whole string: the exact
/// wording should stay editable, the three commitments should not.
#[test]
fn message_id_description_names_the_field_and_rules_out_the_two_live_mistakes() {
    let docs = MailManifest.tool_docs();
    let get = docs
        .iter()
        .find(|d| d.method == "mail.get_message")
        .expect("mail.get_message must be advertised");
    let p = get
        .params
        .iter()
        .find(|p| p.name == "message_id")
        .expect("message_id must be advertised");
    let d = p.description;
    assert!(d.contains("message_id"), "must name the field to read: {d:?}");
    assert!(d.contains("next_cursor"), "must rule out the adjacent cursor: {d:?}");
    assert!(
        d.contains("not a placeholder") || d.contains("literal"),
        "must forbid a template placeholder: {d:?}"
    );
}

/// #760: the attachment tools take an `index`, and the schema has to say
/// where it comes from (mail.get_message) and when it is the key to use —
/// otherwise the planner keeps reaching for a hash when names collide.
#[test]
fn attachment_tools_advertise_index_and_get_message_lists_it() {
    let docs = MailManifest.tool_docs();
    for method in ["mail.get_attachment_text", "mail.get_attachment"] {
        let d = docs.iter().find(|d| d.method == method).expect(method);
        let index = d.params.iter().find(|p| p.name == "index").expect("index");
        assert!(!index.required);
        assert!(
            index.description.contains("mail.get_message") && index.description.contains("share"),
            "{method}: {:?}",
            index.description
        );
    }
    let get = docs.iter().find(|d| d.method == "mail.get_message").expect("get_message");
    assert!(get.summary.contains("index"), "{:?}", get.summary);
}

/// DGX task 218 (2026-10-30): asked for the most recent invoice, the planner
/// fetched a council email whose whole body was "please find attached … the
/// amount payable", judged it "not an invoice" from the body alone, and went
/// on to older mail. The facts were in the PDF. The summary must say that a
/// body pointing at an attachment is not the content.
#[test]
fn get_message_says_to_read_an_attachment_the_body_points_to() {
    let docs = MailManifest.tool_docs();
    let get = docs.iter().find(|d| d.method == "mail.get_message").expect("get_message");
    assert!(
        get.summary.contains("attached") && get.summary.contains("mail.get_attachment_text"),
        "must say to read the attachment the body refers to: {:?}",
        get.summary
    );
    assert!(
        get.summary.contains("before ruling"),
        "must say not to rule the message out from its body alone: {:?}",
        get.summary
    );
}

/// Text is paged since #760, so the planner must learn that a page may not
/// be the whole document and how to get the next one.
#[test]
fn get_attachment_text_says_how_to_read_the_next_page() {
    let docs = MailManifest.tool_docs();
    let d = docs.iter().find(|d| d.method == "mail.get_attachment_text").expect("text");
    assert!(d.summary.contains("next_offset") && d.summary.contains("offset"), "{:?}", d.summary);
    let offset = d.params.iter().find(|p| p.name == "offset").expect("offset");
    assert!(offset.description.contains("next_offset"), "{:?}", offset.description);
}
