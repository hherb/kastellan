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
        vec!["message_id", "filename", "sha256"],
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
    assert_eq!(names, vec!["message_id", "filename", "sha256"]);
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
