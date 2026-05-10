//! End-to-end test for the prompt ledger: `load_prompts_from_dir` writes hashes
//! into `agent_prompts`, cache entries round-trip, and both versions of an
//! edited file persist (append-only by GRANT from migration 0006).
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see them.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/audit_dispatch_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_core::scheduler::prompts::load_prompts_from_dir;
use hhagent_db::agent_prompts::hash_content;

// ---------------------------------------------------------------------------
// Bring-up boilerplate (adapted from core/tests/audit_dispatch_e2e.rs)
// Issue #15: hoist to a shared fixture once Phase 3 tests land.
// ---------------------------------------------------------------------------

fn skip_if_no_supervisor() -> bool {
    match hhagent_supervisor::default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match hhagent_db::find_pg_bin_dir(&hhagent_db::default_pg_bin_dir_candidates()) {
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
    sup: Box<dyn hhagent_supervisor::Supervisor>,
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

fn wait_for_status<F: Fn(hhagent_supervisor::ServiceStatus) -> bool>(
    sup: &dyn hhagent_supervisor::Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<hhagent_supervisor::ServiceStatus, String> {
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

fn wait_for_socket(socket_dir: &std::path::Path, timeout: Duration) -> Result<(), String> {
    let target = socket_dir.join(".s.PGSQL.5432");
    let start = Instant::now();
    loop {
        if target.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timeout {:?} waiting for {}",
                timeout,
                target.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Bring up a per-test PG cluster. Returns the connection spec and
/// cleanup guards. Mirrors `bring_up_pg_cluster` in `audit_dispatch_e2e.rs`
/// with a short label to keep socket paths under the 108-byte limit.
fn bring_up_pg_cluster(
    bin_dir: &std::path::Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    use hhagent_db::{
        build_initdb_argv, build_postgresql_auto_conf, default_socket_dir, InitDbOptions,
        PgConfigOptions,
    };
    use hhagent_supervisor::{default_supervisor, specs::postgres_service_spec, ServiceStatus};

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // Short label — socket path must fit in sockaddr_un.sun_path (108 bytes on Linux).
    let data_root = unique_temp_root("ap-d");
    let data_guard = PathGuard {
        path: data_root.clone(),
    };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("ap-l");
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
    spec.name = format!("hhagent-sched-test-pg-ap-{suffix}");
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

/// Async helper: bring up a PG cluster, run migrations, return pool +
/// guards. Returns `None` when PG or supervisor is unavailable (skip).
async fn bring_up_pg(
    label: &str,
) -> Option<(sqlx::PgPool, (ServiceGuard, PathGuard, PathGuard))> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    // bring_up_pg_cluster is sync (spawns initdb, uses systemd/launchd).
    // ServiceGuard holds Box<dyn Supervisor> which is not Send, so we
    // cannot use spawn_blocking. Use block_in_place instead — it yields
    // the async worker thread for the duration of the blocking call
    // without requiring the return value to be Send.
    let (conn_spec, guards) =
        tokio::task::block_in_place(|| bring_up_pg_cluster(&bin_dir, &suffix));

    hhagent_db::probe::run(
        &conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "agent-prompts-e2e"}),
    )
    .await
    .ok()?;

    let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
        .await
        .ok()?;

    // Single guard tuple so all three Drop impls run in declaration order
    // (ServiceGuard first to stop PG, then PathGuards to remove dirs).
    // A flat 4-tuple destructure would invert the order via reverse-LIFO
    // local drops and PG would still be writing to the data dir while it
    // gets remove_dir_all'd.
    Some((pool, guards))
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Verifies that `load_prompts_from_dir` writes the SHA-256 hash into the
/// `agent_prompts` ledger, the cache entry matches, and both versions of
/// an edited prompt file persist (append-only by GRANT, migration 0006).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_hash_lands_in_ledger_and_audit_payload() {
    let Some((pool, _guards)) = bring_up_pg("ap").await else {
        eprintln!("\n[SKIP] prompt_hash_lands_in_ledger_and_audit_payload: no PG\n");
        return;
    };

    // Create a temporary directory with one prompt file.
    let tmp = tempfile::tempdir().expect("tempdir");
    let prompt_path = tmp.path().join("agent_planner.md");

    // --- Version 1 ---
    let v1_content = "version 1 content\n";
    std::fs::write(&prompt_path, v1_content).expect("write v1");

    let cache = load_prompts_from_dir(&pool, tmp.path())
        .await
        .expect("load v1");

    // Cache entry must carry the correct sha and content.
    let v1_hash = hash_content(v1_content);
    let entry = cache
        .get("agent_planner")
        .expect("agent_planner missing from cache");
    assert_eq!(
        entry.sha256, v1_hash,
        "cache sha256 must match hash_content(v1)"
    );
    assert_eq!(
        entry.content, v1_content,
        "cache content must match what was written"
    );

    // DB must have exactly 1 row for this name after v1 load.
    let count_v1: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_prompts WHERE name = 'agent_planner'",
    )
    .fetch_one(&pool)
    .await
    .expect("count v1 rows");
    assert_eq!(count_v1, 1, "expected 1 row after loading v1");

    // --- Version 2 ---
    let v2_content = "version 2 content\n";
    std::fs::write(&prompt_path, v2_content).expect("write v2");

    let cache2 = load_prompts_from_dir(&pool, tmp.path())
        .await
        .expect("load v2");

    // Cache must now reflect the new sha.
    let v2_hash = hash_content(v2_content);
    let entry2 = cache2
        .get("agent_planner")
        .expect("agent_planner missing from cache after v2 load");
    assert_eq!(
        entry2.sha256, v2_hash,
        "cache sha256 must match hash_content(v2)"
    );
    assert_ne!(v1_hash, v2_hash, "v1 and v2 must have distinct hashes");

    // DB must have 2 rows total (both versions persist — append-only by GRANT).
    let count_v2: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_prompts WHERE name = 'agent_planner'",
    )
    .fetch_one(&pool)
    .await
    .expect("count v2 rows");
    assert_eq!(count_v2, 2, "expected 2 rows after loading v2 (both versions persist)");

    // V1 row must still be present by its hash.
    let count_v1_by_hash: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_prompts WHERE sha256 = $1")
            .bind(&v1_hash)
            .fetch_one(&pool)
            .await
            .expect("count v1 by hash");
    assert_eq!(
        count_v1_by_hash, 1,
        "v1 row must still exist after loading v2 (ledger is append-only)"
    );

    eprintln!(
        "\n[PASS] prompt_hash_lands_in_ledger_and_audit_payload: \
         v1_hash={} v2_hash={}\n",
        &v1_hash[..8],
        &v2_hash[..8]
    );
}
