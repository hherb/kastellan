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
//!
//! The live localmail shape gate that used to sit here is
//! `mail_live_shape_e2e.rs`.
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
