//! End-to-end: the real `kastellan` daemon registers, advertises, and dispatches
//! `mail.*` — the daemon/planner leg. The mail worker runs under the real
//! sandbox against a plain-HTTP `mock_localmail`. Force-routing is off
//! (`KASTELLAN_EGRESS_FORCE_ROUTING=0`) so the daemon-spawned worker takes the
//! DIRECT path to the plain-HTTP mock (the force-routed path can't reach a
//! plain-HTTP/self-signed origin — the webpki wall, covered structurally by
//! `mail_e2e`'s tier 1b).
//!
//! - `daemon_planner_dispatches_mail_search_end_to_end` (always-on): a SCRIPTED
//!   planner plan calls `mail.search`; asserts registration + `<tools>`
//!   advertisement + dispatch + completion.
//! - `live_llm_selects_mail_unprompted` (`#[ignore]`): a REAL local LLM given a
//!   mail-ish question must reach for `mail.*` on its own.
//! - `mock_localmail_shapes_match_real_localmail` (`#[ignore]`, Mac-only): pins
//!   the mock against real localmail (closes the #487 drift failure mode).
//!
//! Skips as-pass without PG / supervisor / sandbox / the mail+cli+core binaries.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use kastellan_tests_common::daemon::{
    bring_up_daemon, DaemonGuards, DaemonHandle, DaemonSpec, LlmEndpoint,
};
use kastellan_tests_common::mock_localmail::{spawn_mock_localmail, MockLocalmail};
use kastellan_tests_common::scripted_llm::{
    embedding_envelope, envelope_for, plan_json, spawn_scripted_llm,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, core_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
    PgCluster,
};

/// The single plan step that calls the mail tool.
fn mail_search_step() -> serde_json::Value {
    serde_json::json!([{
        "tool":           "mail",
        "method":         "mail.search",
        "parameters":     {"query": "invoice"},
        "returns":        "results",
        "done_when":      "true",
        "classification": "Public",
    }])
}

/// `true` (caller should `return`) when any prerequisite binary / host facility
/// is missing — the shared skip-as-pass guard for both daemon tiers.
fn skip_prereqs() -> bool {
    for (label, p) in &[
        ("kastellan", core_binary()),
        ("kastellan-cli", cli_binary()),
        ("kastellan-worker-mail", workspace_target_binary("kastellan-worker-mail")),
    ] {
        if !p.exists() {
            eprintln!("\n[SKIP] {label} binary missing at {}; cargo build --workspace\n", p.display());
            return true;
        }
    }
    skip_if_no_supervisor() || skip_if_sandbox_unavailable() || pg_bin_dir_or_skip().is_none()
}

/// The live pieces a mail-daemon tier needs kept alive for its whole run. Field
/// order matters for drop (Rust drops fields top-to-bottom): the daemon guards
/// (stop + uninstall the service) drop BEFORE `cluster` (stops PG), so the
/// daemon tears down while its database is still up.
struct MailDaemon {
    daemon: DaemonHandle,
    _guards: DaemonGuards,
    _mock_mail: MockLocalmail,
    _token_dir: tempfile::TempDir,
    cluster: PgCluster,
}

/// Bring up the per-test PG cluster + a plain-HTTP mock localmail + the real
/// daemon with the mail worker registered (endpoint = mock, 0600 token file,
/// binary path) and force-routing OFF. `llm` is the endpoint the daemon's
/// router dials — `Base` for the mock, which owns a bare `host:port`, and
/// `from_operator_url` for the live-LLM leg, whose URL comes from an
/// operator env var that may or may not already carry `/v1`.
/// `model_override` replaces `DEFAULT_LLM_MODEL` when `Some`.
fn bring_up_mail_daemon(
    rt: &tokio::runtime::Runtime,
    suffix: &str,
    llm: LlmEndpoint,
    model_override: Option<&str>,
) -> MailDaemon {
    let cluster = bring_up_pg_cluster(
        &pg_bin_dir_or_skip().unwrap(),
        "maild-d",
        "maild-l",
        &format!("kastellan-supervisor-test-pg-maild-{suffix}"),
    );

    let mock_mail = rt.block_on(spawn_mock_localmail());

    // Migrations before the daemon boots (its own probe re-applies idempotently).
    // Mail derives its allowlist from the endpoint env, so there is NO
    // tool_allowlists seed (unlike shell-exec).
    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "test",
            "setup",
            serde_json::json!({"test": "mail_daemon_e2e_setup"}),
        )
        .await
        .expect("probe run");
    });

    // 0600 token file; its dir must outlive the daemon.
    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("mail-token");
    std::fs::write(&token_file, b"test-bearer-token").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod token 0600");
    }

    let extra_env = vec![
        ("KASTELLAN_MAIL_ENDPOINT".to_string(), mock_mail.base_url.clone()),
        ("KASTELLAN_MAIL_TOKEN_FILE".to_string(), token_file.to_string_lossy().into_owned()),
        (
            "KASTELLAN_MAIL_BIN".to_string(),
            workspace_target_binary("kastellan-worker-mail").to_string_lossy().into_owned(),
        ),
    ];

    // `DaemonSpec` sets KASTELLAN_DATA_DIR from the cluster's data_dir.
    // Force routing OFF through the named setter rather than an `env`
    // entry: it is a containment control, and `grep force_routing` should
    // find every test that opts out. The mock localmail origin is plain
    // HTTP on loopback, which the proxy's webpki-only MITM upstream cannot
    // reach.
    let mut spec = DaemonSpec::new("maild", &cluster.data_dir, llm)
        .force_routing(false)
        .envs(extra_env);
    // The live-LLM leg needs a real model in place of `DEFAULT_LLM_MODEL`.
    // Before #634 this went through `extra_env`'s later-wins ordering; the
    // setter says the same thing without depending on it.
    if let Some(model) = model_override {
        spec = spec.llm_model(model);
    }
    let (daemon, guards) = bring_up_daemon(&spec);

    MailDaemon {
        cluster,
        daemon,
        _guards: guards,
        _mock_mail: mock_mail,
        _token_dir: token_dir,
    }
}

/// Run `kastellan-cli ask <question>` against the daemon's cluster. `env_clear`
/// plus only the data-dir env: the operator CLI deliberately omits worker
/// registration (the #179 invariant) — only the daemon registers mail.
fn cli_ask(fixture: &MailDaemon, user: &str, question: &str) -> std::process::Output {
    Command::new(cli_binary())
        .arg("ask")
        .arg(question)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", user)
        .env("KASTELLAN_DATA_DIR", fixture.cluster.data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn kastellan-cli ask")
}

/// Panic with the daemon logs attached if the CLI did not exit 0.
fn assert_cli_ok(fixture: &MailDaemon, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "CLI must exit 0; got {:?}\n--- cli stderr ---\n{}\n--- daemon stdout ({}) ---\n{}\n--- daemon stderr ({}) ---\n{}\n",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        fixture.daemon.stdout_path.display(),
        std::fs::read_to_string(&fixture.daemon.stdout_path).unwrap_or_default(),
        fixture.daemon.stderr_path.display(),
        std::fs::read_to_string(&fixture.daemon.stderr_path).unwrap_or_default(),
    );
}

async fn count_audit(pool: &sqlx::PgPool, actor: &str, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE actor = $1 AND action = $2")
        .bind(actor)
        .bind(action)
        .fetch_one(pool)
        .await
        .expect("count audit rows")
}

/// Tier 2a — a SCRIPTED planner plan calls `mail.search`; the daemon registers +
/// advertises + dispatches it and the task completes.
#[test]
fn daemon_planner_dispatches_mail_search_end_to_end() {
    if skip_prereqs() {
        return;
    }
    let suffix = unique_suffix();
    let user = current_username();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    // Same 2-iteration shape as cli_ask_e2e's happy path: one embed + one plan
    // per iteration; plan A executes the mail.search step (result=None), plan B
    // is terminal.
    let scripted = rt.block_on(spawn_scripted_llm(
        vec![embedding_envelope(), embedding_envelope()],
        vec![
            envelope_for(&plan_json("act", mail_search_step(), None)),
            envelope_for(&plan_json(
                "task_complete",
                serde_json::json!([]),
                Some(serde_json::json!({"kind": "text", "body": "done"})),
            )),
        ],
    ));

    let fixture = bring_up_mail_daemon(
        &rt,
        &suffix,
        LlmEndpoint::Base(scripted.base_url.clone()),
        None,
    );
    let output = cli_ask(&fixture, &user, "find my latest invoice email");
    assert_cli_ok(&fixture, &output);

    rt.block_on(async {
        let pool = kastellan_db::pool::connect_runtime_pool(&fixture.cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        let (state, plan_count): (String, i32) =
            sqlx::query_as("SELECT state, plan_count FROM tasks ORDER BY id LIMIT 1")
                .fetch_one(&pool)
                .await
                .expect("select task");
        assert_eq!(state, "completed", "task must complete; got {state}");
        assert_eq!(plan_count, 2, "expected 2 plan rounds; got {plan_count}");

        // The planner advertised mail in its <tools> block.
        let chat0 = scripted.chat_requests.lock().unwrap().first().cloned().unwrap_or_default();
        assert!(
            chat0.contains("mail.search"),
            "planner <tools> block must advertise mail.search; first chat request:\n{chat0}"
        );

        // mail.search actually dispatched — audit (actor,action) =
        // ("tool:mail","mail.search"), the shape cli_ask asserts for shell-exec.
        assert_eq!(count_audit(&pool, "tool:mail", "mail.search").await, 1,
                   "expected exactly one tool:mail/mail.search dispatch row");
        assert_eq!(count_audit(&pool, "agent", "plan.formulate").await, 2,
                   "expected 2 agent/plan.formulate rows");

        pool.close().await;
    });
}

/// Tier 2b — a REAL local LLM, given a mail-ish question, must select `mail.*`
/// unprompted (proves the tool docs are good enough for real model selection).
/// Portable: the mock origin needs no localmail. Point the daemon at a local
/// OpenAI-compatible endpoint via `KASTELLAN_MAIL_LIVE_LLM_URL`
/// (e.g. `http://127.0.0.1:11434/v1` for Ollama) + `KASTELLAN_MAIL_LIVE_LLM_MODEL`.
#[test]
#[ignore = "needs a real local LLM (KASTELLAN_MAIL_LIVE_LLM_URL); non-deterministic"]
fn live_llm_selects_mail_unprompted() {
    let Ok(llm_url) = std::env::var("KASTELLAN_MAIL_LIVE_LLM_URL") else {
        eprintln!(
            "\n[SKIP] set KASTELLAN_MAIL_LIVE_LLM_URL to a local OpenAI-compatible \
             endpoint (with or without a trailing /v1)\n"
        );
        return;
    };
    if skip_prereqs() {
        return;
    }
    // `KASTELLAN_MAIL_LIVE_LLM_URL` has accepted BOTH shapes since this
    // test was written: pre-#634 it stripped a trailing `/v1` and let the
    // helper append one, so `http://127.0.0.1:11434` and
    // `http://127.0.0.1:11434/v1` both reached the daemon as `.../v1`. A
    // bare `Verbatim` here would have silently dropped the first — and the
    // bare form is the one this tree's own installer calls canonical
    // (`OLLAMA_LLM_URL`), so it is what an operator is likeliest to export.
    // `from_operator_url` keeps that tolerance somewhere a `tests-common`
    // unit test can reach it, which matters because this test is
    // `#[ignore]`d and runs on no PR and no DGX sweep.
    let model = std::env::var("KASTELLAN_MAIL_LIVE_LLM_MODEL")
        .unwrap_or_else(|_| "gemma3".to_string());

    let suffix = unique_suffix();
    let user = current_username();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    let fixture = bring_up_mail_daemon(
        &rt,
        &suffix,
        LlmEndpoint::from_operator_url(llm_url),
        Some(&model),
    );
    let output = cli_ask(
        &fixture,
        &user,
        "search my email for the invoice from north coast health service",
    );
    assert_cli_ok(&fixture, &output);

    rt.block_on(async {
        let pool = kastellan_db::pool::connect_runtime_pool(&fixture.cluster.conn_spec)
            .await
            .expect("connect runtime pool");
        // The model reached for SOME mail.* tool on its own (actor = "tool:mail",
        // any method). We assert reach, not wording.
        let dispatched: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE actor = 'tool:mail'")
                .fetch_one(&pool)
                .await
                .expect("count mail dispatch rows");
        assert!(dispatched >= 1, "the real planner must reach for mail.* unprompted (0 dispatches)");
        pool.close().await;
    });
}

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
    //    real attachment sha via list → message; skip this leg (printed note) if
    //    the archive carries no attachment.
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
    let mut sha: Option<String> = None;
    // The same attachment by position: (message id, index), for slice D's route.
    let mut at: Option<(String, usize)> = None;
    // Checked once, on the first detail actually fetched (see below).
    let mut detail_shape_checked = false;
    // Checked once, alongside the detail shape.
    let mut header_spelling_checked = false;
    for row in rows {
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
        // localmail reads a differently NAMED query parameter and derives
        // the flag from its VALUE (`serve/routes/messages.py::detail` →
        // `full_headers=(headers == "full")`, with `headers: str =
        // Query("compact")`). Until now that claim lived in a code comment
        // and in fixtures written from the same reading — our own mock
        // modelling the gate, and a unit test pinning the model. A
        // consistent misreading passed every test in the tree.
        //
        // It is an unvalidated bare string with a default, so a rename or a
        // changed sentinel makes localmail answer a header-less 200 and the
        // mail tool silently stops delivering headers. Both directions are
        // asserted: the spelling we send must work, and the spelling we used
        // to send must still NOT — if that one starts working, the service
        // gained an alias and `detail_path`'s translation deserves review.
        if !header_spelling_checked && msg.is_some() {
            header_spelling_checked = true;
            let full = ok_json(
                "/v1/messages/{id}?headers=full",
                curl("GET", &format!("/v1/messages/{id}?headers=full"), None),
            );
            assert!(
                full.get("headers").and_then(|h| h.as_object()).map(|o| !o.is_empty()).unwrap_or(false),
                "#500: `?headers=full` — the spelling handler::detail_path sends — must \
                 return a non-empty `headers` block; got keys {:?}",
                full.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
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
         `?headers=full` vs `?full_headers=true` was verified against nothing"
    );
    let Some(sha) = sha else {
        eprintln!("[NOTE] no attachment in the archive; skipping the attachment-text shape check");
        return;
    };
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
}
