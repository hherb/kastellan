//! Subprocess-level pin for `hhagent-cli relations show <entity-id>`.
//!
//! Boots a per-test PG cluster, seeds a small clinical-style subgraph
//! directly via the `Graph` trait, then runs the real `hhagent-cli`
//! binary as a subprocess and inspects exit code + stdout + stderr.
//!
//! Key invariants pinned end-to-end (mirror of `cli_relations_e2e.rs`
//! patterns; runs side-by-side without colliding because each test
//! spins up its own cluster):
//!
//!   * **Happy path, depth=1, plain format.** `relations show <id>`
//!     prints the seed entity header, the outbound walk (with the
//!     correct `(kind, "name")` shape), the inbound walk, and exits 0.
//!     A quarantined endpoint is tagged `[Q]`; an approved endpoint
//!     is not.
//!
//!   * **JSON format.** `--format json` emits NDJSON: one
//!     `{"seed":...}` header line followed by one `{"direction":...}`
//!     line per edge. Parsing each line as JSON and pinning canonical
//!     fields catches a future renderer change that breaks downstream
//!     `jq` consumers.
//!
//!   * **Depth respected.** `--depth 2` surfaces a depth-2 edge that
//!     `--depth 1` does not. Sanity-pins the `walk_outbound_edges`
//!     `max_depth` argument is plumbed through end-to-end.
//!
//!   * **Unknown id.** `relations show <large-id>` exits 1 with a
//!     `"not found"` stderr line. (Exit 1 rather than 2 because the
//!     parser accepted the id syntactically; the failure is a
//!     runtime-time lookup miss, not an operator-fixable input
//!     fault.)
//!
//!   * **Bad-format.** `--format xml` exits 2 with the recognised-
//!     formats diagnostic, *before* connecting. Already pinned at the
//!     parser level by the unit tests; this case is structural so
//!     omitted from the e2e (no DB needed to verify and the unit
//!     coverage is precise).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use hhagent_db::graph::{Graph, PgGraph};
use hhagent_db::pool::connect_runtime_pool;
use hhagent_db::probe::run as probe_run;
use hhagent_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    unique_suffix,
};

/// Match a rendered edge row, tolerating dynamic column widths. The
/// renderer left-pads the `kind` column to the widest kind in the
/// result set, so a literal substring check against `--[treats]-->`
/// would break the moment a sibling row has a longer kind. This helper
/// asserts that an edge with `kind` (followed by *some* trailing
/// whitespace before `]-->`) ends at the rendered dst endpoint.
fn contains_edge_row(haystack: &str, kind: &str, dst_suffix: &str) -> bool {
    let needle_prefix = format!("--[{kind}");
    haystack.lines().any(|line| {
        if let Some(after_prefix) = line.find(&needle_prefix) {
            let tail = &line[after_prefix + needle_prefix.len()..];
            // Allow any amount of right-padding (including zero) before `]-->`.
            let rest = tail.trim_start_matches(' ');
            if let Some(after_close) = rest.strip_prefix("]-->") {
                return after_close.contains(dst_suffix);
            }
        }
        false
    })
}

/// Same env-construction shape as `cli_relations_e2e::cli_env`. Peer
/// auth keys off `$USER`; the cluster's bootstrap role IS the OS user,
/// so `$USER` must reach the subprocess intact.
fn cli_env(data_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("HHAGENT_DATA_DIR".to_string(), data_dir.display().to_string()),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    if let Some(user) = std::env::var_os("USER") {
        env.push(("USER".to_string(), user.to_string_lossy().into_owned()));
    } else {
        env.push(("USER".to_string(), current_username()));
    }
    env
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_relations_show_renders_outbound_inbound_walks_and_quarantine_tags() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_relations_show_renders_outbound_inbound_walks_and_quarantine_tags: \
             hhagent-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rs-d",
        "rs-l",
        &format!("hhagent-postgres-cli-relations-show-e2e-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_relations_show_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");
    let g = PgGraph::new(&pool);

    // ── Subgraph seed ──────────────────────────────────────────────────
    //
    //                                   Dr Smith
    //          ┌────────[treats]──────────┐
    //          │                          ▼
    //  jane ──[associated with]──▶ Dr Smith ──[has symptom (depth 2 outbound)]──▶ wheezing
    //                                   │
    //                                   └─[prescribed]─▶ salbutamol
    //
    // jane is depth-1 inbound from Dr Smith; wheezing is depth-2
    // outbound through the asthma chain (Dr Smith --treats--> asthma
    // --has symptom--> wheezing).
    let dr = g.upsert_entity("person", "Dr Smith", &serde_json::json!({})).await.unwrap();
    let asthma = g.upsert_entity("disease", "asthma", &serde_json::json!({})).await.unwrap();
    let salbutamol = g.upsert_entity("drug", "salbutamol", &serde_json::json!({})).await.unwrap();
    let wheezing = g.upsert_entity("symptom", "wheezing", &serde_json::json!({})).await.unwrap();
    let jane = g.upsert_entity("patient", "Jane Doe", &serde_json::json!({})).await.unwrap();

    // Approve Dr Smith — the rest stay quarantined so we can pin both
    // `[Q]` and unmarked endpoints in one run.
    sqlx::query("UPDATE entities SET quarantine = FALSE WHERE id = $1")
        .bind(dr)
        .execute(&pool)
        .await
        .expect("approve dr_smith");

    g.upsert_relation(dr, asthma, "treats", &serde_json::json!({})).await.unwrap();
    g.upsert_relation(dr, salbutamol, "prescribed", &serde_json::json!({})).await.unwrap();
    g.upsert_relation(asthma, wheezing, "has symptom", &serde_json::json!({})).await.unwrap();
    g.upsert_relation(jane, dr, "associated with", &serde_json::json!({})).await.unwrap();

    let env = cli_env(&cluster.data_dir);

    // ── 1. Plain format, depth=1 (default) ────────────────────────────
    let out = Command::new(&bin)
        .args(["relations", "show", &dr.to_string()])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli show plain");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "show plain exit: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status,
    );

    // Header line carries the (approved) seed without `[Q]`.
    assert!(
        stdout.contains(&format!("entity: id={dr} kind=person name=\"Dr Smith\"\n")),
        "header must show approved seed without [Q] tag; stdout:\n{stdout}",
    );
    assert!(stdout.contains("depth: 1"), "stdout:\n{stdout}");

    // Outbound section has 2 edges (treats + prescribed). Quarantined
    // endpoints carry `[Q]`. The kind column is dynamically padded
    // (10 wide here because `prescribed` is the longest), so substring
    // checks accept any positive amount of internal whitespace.
    assert!(stdout.contains("outbound (2):"), "stdout:\n{stdout}");
    assert!(
        contains_edge_row(&stdout, "treats", r#"(disease, "asthma") [Q]"#),
        "outbound must include treats→asthma edge with quarantine tag; stdout:\n{stdout}",
    );
    assert!(
        contains_edge_row(&stdout, "prescribed", r#"(drug, "salbutamol") [Q]"#),
        "outbound must include prescribed→salbutamol edge; stdout:\n{stdout}",
    );

    // Inbound section has 1 edge (jane→dr); jane is quarantined.
    assert!(stdout.contains("inbound (1):"), "stdout:\n{stdout}");
    assert!(
        stdout.contains(r#"(patient, "Jane Doe") [Q]"#),
        "inbound must include jane with quarantine tag; stdout:\n{stdout}",
    );
    assert!(
        contains_edge_row(&stdout, "associated with", r#"(person, "Dr Smith")"#),
        "inbound row must keep canonical jane→dr orientation \
         (kind=person without [Q] for the approved dr); stdout:\n{stdout}",
    );

    // ── 2. JSON format ─────────────────────────────────────────────────
    let out = Command::new(&bin)
        .args(["relations", "show", &dr.to_string(), "--format", "json"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli show json");
    assert!(
        out.status.success(),
        "show json exit: {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    // 1 header + 2 outbound + 1 inbound = 4 lines at depth=1.
    assert_eq!(lines.len(), 4, "expected 4 NDJSON lines, got:\n{stdout}");

    let header: serde_json::Value = serde_json::from_str(lines[0]).expect("header JSON");
    assert_eq!(header["seed"]["id"], dr);
    assert_eq!(header["seed"]["kind"], "person");
    assert_eq!(header["seed"]["name"], "Dr Smith");
    assert_eq!(header["seed"]["quarantine"], false);
    assert_eq!(header["depth"], 1);
    assert_eq!(header["outbound_count"], 2);
    assert_eq!(header["inbound_count"], 1);

    // Every non-header line must parse, carry a direction in
    // {outbound, inbound}, and have the canonical
    // {edge_id, depth, src, dst, kind} fields.
    let mut directions: Vec<String> = Vec::new();
    for line in &lines[1..] {
        let v: serde_json::Value = serde_json::from_str(line).expect("edge JSON");
        directions.push(v["direction"].as_str().expect("direction").to_string());
        assert!(v["edge_id"].is_i64(), "edge_id must be int: {line}");
        assert!(v["depth"].is_i64(), "depth must be int: {line}");
        assert!(v["src"].is_object(), "src must be object: {line}");
        assert!(v["dst"].is_object(), "dst must be object: {line}");
        assert!(v["kind"].is_string(), "kind must be string: {line}");
    }
    directions.sort();
    assert_eq!(
        directions,
        vec!["inbound".to_string(), "outbound".to_string(), "outbound".to_string()],
        "expected 2 outbound + 1 inbound",
    );

    // ── 3. Depth=2 surfaces the asthma → wheezing edge ────────────────
    let out = Command::new(&bin)
        .args(["relations", "show", &dr.to_string(), "--depth", "2"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli show depth=2");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("depth: 2"), "stdout:\n{stdout}");
    assert!(
        stdout.contains(r#"(symptom, "wheezing") [Q]"#),
        "depth=2 must surface the asthma→wheezing edge; stdout:\n{stdout}",
    );
    // `outbound (3)` (treats, prescribed, has symptom) — confirm the
    // count metadata picks up the new edge.
    assert!(stdout.contains("outbound (3):"), "stdout:\n{stdout}");

    // ── 4. Unknown entity-id exits 1 with not-found stderr ────────────
    // Pick an id far above the BIGSERIAL we just seeded.
    let nonexistent: i64 = 9_999_999_999;
    let out = Command::new(&bin)
        .args(["relations", "show", &nonexistent.to_string()])
        .env_clear()
        .envs(env)
        .output()
        .expect("spawn cli show unknown");
    assert_eq!(
        out.status.code(),
        Some(1),
        "unknown id must exit 1; got {:?}",
        out.status,
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains(&format!("relations show: id={nonexistent} not found")),
        "stderr must carry the not-found diagnostic; got: {stderr}",
    );

    pool.close().await;
}
