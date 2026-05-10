//! End-to-end smoke for `memory::recall` — the first non-trivial
//! sqlx query path in `core`, and the first real consumer of the
//! `memories` table.
//!
//! What this test proves against a per-test PG cluster:
//!
//!   1. `db::memories::insert_memory` writes rows with a `vector(1024)`
//!      embedding via the text-cast path; no pgvector Rust crate
//!      required.
//!   2. `db::memories::semantic_search` ranks the embedding-matched
//!      memory first under cosine distance.
//!   3. `db::memories::lexical_search` ranks the lexically-matched
//!      memory first under `ts_rank`.
//!   4. `core::memory::recall(modes = ALL)` fuses the two via RRF and
//!      returns the same memory as top-1 when both lanes vote
//!      consistently for it.
//!
//! ## How the test creates "matching" embeddings without an embedding
//! worker
//!
//! Three memories are seeded with bodies that share no surface words
//! (`"alpha bravo charlie"`, `"delta echo foxtrot"`, `"golf hotel
//! india"`). The test embedding helper [`text_to_embedding`] hashes
//! the body text with SHA-256 and uses the digest to seed a deterministic
//! pseudo-random unit vector of length [`EMBEDDING_DIM`]. Two
//! consequences:
//!
//!   * Same text → same vector → cosine similarity 1.0 (distance 0).
//!   * Different texts → near-orthogonal vectors → cosine similarity
//!     ≈ 0 (distance ≈ 1).
//!
//! So the query embedding for one body is *exactly* equal to the row's
//! embedding, putting it at distance 0 (rank 1), while the other two
//! rows are at distance ~1. The same body's lexical query
//! (a unique word like "alpha") matches its tsvector and only its
//! tsvector, so lexical also ranks it first.
//!
//! ## Why this is a strict test, not a flaky correlation
//!
//! Both lanes are tested against the *exact* matching document, not a
//! noisy near-neighbour. If either lane mis-ranks the canonical match,
//! either pgvector is producing a non-zero distance for an identical
//! vector pair (impossible) or `tsv` isn't being maintained (a schema
//! regression). The test is one assertion away from "the system is
//! fundamentally broken" rather than a calibration check.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_db::memories::{
    insert_memory, lexical_search, semantic_search, EMBEDDING_DIM,
};
use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_supervisor::specs::postgres_service_spec;
use hhagent_supervisor::{default_probe, default_supervisor, ServiceStatus, Supervisor};

use hhagent_core::memory::{recall, RecallModes, RecallParams};

fn skip_if_no_supervisor() -> bool {
    match default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match find_pg_bin_dir(&default_pg_bin_dir_candidates()) {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("\n[SKIP] no Postgres install found: {e}\n");
            None
        }
    }
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

fn unique_temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hhagent-{}-{}", label, unique_suffix()))
}

fn current_username() -> String {
    if let Some(u) = std::env::var_os("USER") {
        let s = u.to_string_lossy().into_owned();
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(out) = Command::new("whoami").output() {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    "hhagent".into()
}

struct ServiceGuard {
    sup: Box<dyn Supervisor>,
    name: String,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = self.sup.stop(&self.name);
        let _ = self.sup.uninstall(&self.name);
    }
}

struct PathGuard {
    path: PathBuf,
}
impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn wait_for_status<F: Fn(ServiceStatus) -> bool>(
    sup: &dyn Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<ServiceStatus, String> {
    let start = Instant::now();
    let mut last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    loop {
        if predicate(last) {
            return Ok(last);
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout {:?}; last={last:?}", timeout));
        }
        std::thread::sleep(Duration::from_millis(50));
        last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    }
}

fn wait_for_socket(socket_dir: &Path, timeout: Duration) -> Result<(), String> {
    let target = socket_dir.join(".s.PGSQL.5432");
    let start = Instant::now();
    loop {
        if target.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout {:?} waiting for {}", timeout, target.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Bring up a per-test PG cluster (initdb + auto.conf + supervisor
/// install + start). Returns the connection spec and the cleanup
/// guards. Same shape as the helper in `audit_dispatch_e2e.rs` and
/// `supervisor_e2e.rs` — issue #15 will eventually hoist this into a
/// shared `tests-common` dev-dep crate.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // Short labels — the cluster socket path
    // `<data_dir>/sockets/.s.PGSQL.5432` must fit in `sockaddr_un.sun_path`
    // (108 bytes on Linux). Mirrors the audit_dispatch_e2e label
    // discipline.
    let data_root = unique_temp_root("recall-d");
    let data_guard = PathGuard {
        path: data_root.clone(),
    };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("recall-l");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_guard = PathGuard {
        path: log_dir.clone(),
    };

    let user = current_username();
    let argv = build_initdb_argv(
        &initdb,
        &InitDbOptions {
            data_dir: data_dir.clone(),
            username: user.clone(),
            ..InitDbOptions::default()
        },
    );
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .output()
        .expect("spawn initdb");
    assert!(
        out.status.success(),
        "initdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::create_dir(&socket_dir).expect("create socket dir");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&socket_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&socket_dir, perms).unwrap();
    }
    std::fs::write(
        data_dir.join("postgresql.auto.conf"),
        build_postgresql_auto_conf(&PgConfigOptions {
            socket_dir: socket_dir.clone(),
            ..PgConfigOptions::default()
        }),
    )
    .expect("write postgresql.auto.conf");

    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = format!("hhagent-supervisor-test-pg-recall-{suffix}");
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install pg");
    sup.start(&spec.name).expect("start pg");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("pg active");
    wait_for_socket(&socket_dir, Duration::from_secs(15)).expect("pg socket");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).unwrap(),
        ServiceStatus::Active,
        "pg flap"
    );

    let conn_spec = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };
    (conn_spec, (service_guard, data_guard, log_guard))
}

/// Deterministic, dependency-free embedding stub for tests.
///
/// Hashes the input text with SHA-256 to produce a 32-byte seed, then
/// runs an xorshift64 PRNG to fill 1024 floats in `[-1, 1]`, and
/// finally L2-normalises so the cosine-similarity calculation is
/// numerically clean. Two pins:
///
///   * Same text → same vector → `cos(emb(t), emb(t)) == 1.0` → cosine
///     distance `<=>` returns 0.
///   * Different texts → independent xorshift streams → cosine
///     similarity ≈ 0 (the central limit theorem applied to 1024
///     uniform random variables in `[-1, 1]`), so the matching pair
///     wins by ~1.0 distance margin.
///
/// Why a hand-rolled PRNG instead of the `rand` crate: avoids adding a
/// dev-dep for one helper. xorshift64 (Marsaglia 2003) is a 5-line
/// PRNG with excellent equidistribution properties for this purpose;
/// we are not using it for any cryptographic claim.
fn text_to_embedding(text: &str) -> Vec<f32> {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(text.as_bytes());
    // Seed xorshift64 from the first 8 bytes of the digest. A zero
    // seed is invalid for xorshift64 (would produce all-zero output);
    // SHA-256 of any non-empty input is overwhelmingly unlikely to be
    // zero in any 8-byte window, but we OR in 1 to defend against the
    // theoretical case.
    let mut seed: u64 = 0;
    for (i, b) in digest[..8].iter().enumerate() {
        seed |= (*b as u64) << (i * 8);
    }
    if seed == 0 {
        seed = 1;
    }

    let mut state = seed;
    let mut v: Vec<f32> = Vec::with_capacity(EMBEDDING_DIM);
    for _ in 0..EMBEDDING_DIM {
        // Marsaglia xorshift64.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // Map u64 → f32 in [-1, 1]. The top 24 bits give ~7 decimal
        // digits of significand which is more than enough for
        // discrimination.
        let bits = (state >> 40) as u32; // 24 bits
        let unit = (bits as f32) / ((1u32 << 24) as f32); // [0, 1)
        v.push(unit * 2.0 - 1.0);
    }

    // L2-normalise so cosine similarity equals dot product.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

const BODY_A: &str = "alpha bravo charlie gathered for the briefing";
const BODY_B: &str = "delta echo foxtrot ran aground at midnight";
const BODY_C: &str = "golf hotel india signaled clear at dawn";

#[test]
fn recall_seeds_three_docs_and_ranks_target_first_per_mode_and_fused() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let (conn_spec, _guards) = bring_up_pg_cluster(&bin_dir, &suffix);

    // recall is async + uses sqlx — needs a real tokio runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        // Probe applies migrations 0001 + 0002 + 0003 + 0004 and writes
        // the bring-up audit row.
        hhagent_db::probe::run(
            &conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "memory-recall"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
            .await
            .expect("connect runtime pool");

        // ---- seed three memories ----
        let emb_a = text_to_embedding(BODY_A);
        let emb_b = text_to_embedding(BODY_B);
        let emb_c = text_to_embedding(BODY_C);
        assert_eq!(emb_a.len(), EMBEDDING_DIM);

        let id_a = insert_memory(&pool, BODY_A, &serde_json::json!({}), Some(&emb_a))
            .await
            .expect("insert A");
        let id_b = insert_memory(&pool, BODY_B, &serde_json::json!({}), Some(&emb_b))
            .await
            .expect("insert B");
        let id_c = insert_memory(&pool, BODY_C, &serde_json::json!({}), Some(&emb_c))
            .await
            .expect("insert C");
        assert_ne!(id_a, id_b);
        assert_ne!(id_b, id_c);

        // ---- semantic-only: target embedding == BODY_A's embedding,
        // so distance 0; the other two rows are ~1.0 distance away.
        let semantic_hits = semantic_search(&pool, &emb_a, 10)
            .await
            .expect("semantic_search");
        assert_eq!(
            semantic_hits.first().copied(),
            Some(id_a),
            "semantic top-1 must be A: {semantic_hits:?}"
        );

        // ---- lexical-only: query "alpha" appears only in BODY_A's
        // tsvector, so the result set has exactly one row.
        let lexical_hits = lexical_search(&pool, "alpha", 10)
            .await
            .expect("lexical_search");
        assert_eq!(
            lexical_hits,
            vec![id_a],
            "lexical for 'alpha' must return only A: {lexical_hits:?}"
        );

        // ---- recall(SEMANTIC_ONLY): equivalent to the lane query
        // through the public surface, hydrated.
        let semantic_only = recall(
            &pool,
            &RecallParams {
                query_text: None,
                query_embedding: Some(&emb_a),
                k: 5,
                modes: RecallModes::SEMANTIC_ONLY,
            },
        )
        .await
        .expect("recall semantic-only");
        assert_eq!(
            semantic_only.first().map(|m| m.id),
            Some(id_a),
            "recall(SEMANTIC_ONLY) top-1 must be A"
        );
        assert_eq!(semantic_only.first().map(|m| m.body.as_str()), Some(BODY_A));

        // ---- recall(LEXICAL_ONLY): only A matches "alpha", so
        // exactly one hydrated result.
        let lexical_only = recall(
            &pool,
            &RecallParams {
                query_text: Some("alpha"),
                query_embedding: None,
                k: 5,
                modes: RecallModes::LEXICAL_ONLY,
            },
        )
        .await
        .expect("recall lexical-only");
        assert_eq!(lexical_only.len(), 1);
        assert_eq!(lexical_only[0].id, id_a);

        // ---- recall(ALL): both lanes vote for A; RRF fuses; top-1
        // must still be A. The two non-matching memories appear in
        // semantic but not lexical, so they share the lower fused
        // score deterministically.
        let fused = recall(
            &pool,
            &RecallParams {
                query_text: Some("alpha"),
                query_embedding: Some(&emb_a),
                k: 5,
                modes: RecallModes::ALL,
            },
        )
        .await
        .expect("recall fused");
        assert!(
            !fused.is_empty(),
            "fused recall returned empty result set"
        );
        assert_eq!(
            fused[0].id, id_a,
            "fused top-1 must be A; got {:?}",
            fused.iter().map(|m| m.id).collect::<Vec<_>>()
        );
        assert_eq!(fused[0].body, BODY_A);

        // The fused list should also include the two semantic-only
        // candidates somewhere below A — proves RRF is fusing rather
        // than intersecting. This is a soft assertion (depends on
        // semantic returning all three rows above some threshold,
        // which our test embedding guarantees: every row has a
        // non-NULL embedding so all three appear in the cosine
        // ORDER BY).
        let fused_ids: Vec<i64> = fused.iter().map(|m| m.id).collect();
        assert!(
            fused_ids.contains(&id_b) && fused_ids.contains(&id_c),
            "fused list should include B and C below A; got {fused_ids:?}"
        );

        pool.close().await;
    });
}
