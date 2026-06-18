//! End-to-end test: agent core spawns the `python-exec` worker under the
//! platform's real sandbox backend and round-trips `python.exec` calls
//! **through `tool_host::dispatch`** (the sealed chokepoint — see
//! `shell_exec_e2e.rs` for why dispatch and not `worker.call`).
//!
//! What this pins beyond the worker's own `real_python.rs` suite: the
//! **production policy inside the real jail** — `python_exec_entry`'s
//! `Net::Deny` + `Profile::WorkerStrict` actually contain the CPython
//! child (a socket attempt dies), and the explicit
//! `KASTELLAN_LANDLOCK_RW=["/tmp"]` grant really lets code write the
//! jail's ephemeral scratch.
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the sandbox, the worker
//! binary, or a python3 interpreter is missing.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::{
    interpreter_extra_lib_dirs, python_exec_entry, PYTHON_CANDIDATES,
};
use kastellan_db::secrets::{MapKeyProvider, KEY_LEN};
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

/// Deterministic key-provider for the secret-scrub test — mirrors
/// `secret_vault_e2e.rs`. A fixed in-memory AES key is all the scrub test needs:
/// it `put`s a secret, `materialize`s it into a Vault, and asserts the plaintext
/// is redacted from the worker's output.
const TEST_KEY_ID: &str = "test-keyring";

fn test_key_provider() -> MapKeyProvider {
    MapKeyProvider::new(TEST_KEY_ID, [42u8; KEY_LEN])
}

/// The manifest's own per-OS candidate cascade (single source of truth —
/// on macOS that list deliberately excludes the `/usr/bin/python3` xcrun
/// shim, which cannot run inside the jail). Canonicalized like the
/// manifest does, so the framework-layout fs_read derivation sees the
/// real path.
fn find_python() -> Option<PathBuf> {
    for c in PYTHON_CANDIDATES {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(std::fs::canonicalize(&p).unwrap_or(p));
        }
    }
    eprintln!("\n[SKIP] no python3 interpreter on this host\n");
    None
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "python-exec-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

fn dispatch_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    python: PathBuf,
}

fn ready_or_skip() -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("kastellan-worker-python-exec");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return None;
    }
    let python = find_python()?;

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pyx-d",
        "pyx-l",
        &format!("kastellan-supervisor-test-pg-pythonexec-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        python,
    })
}

/// Spawn the python-exec worker under the **production** policy
/// (`python_exec_entry`) and run one `python.exec` dispatch with the
/// caller-supplied `vault` + `params`, returning the result value.
///
/// The explicit `vault` + `params` seam (vs. the code-only [`exec_in_jail`]
/// convenience wrapper) lets the secret-scrub test materialize a real secret
/// into a Vault it owns and pass the opaque `secret://` ref through the params
/// channel, exercising the substitution-in + scrub-out path of `dispatch`.
async fn dispatch_in_jail(
    pool: &sqlx::PgPool,
    env: &TestEnv,
    vault: &Vault,
    params: serde_json::Value,
) -> Result<serde_json::Value, kastellan_core::tool_host::ToolHostError> {
    // Mirror the manifest: bind the interpreter's out-of-prefix shared-lib dirs
    // (issue #284) so a pyenv/Homebrew-linked interpreter dyld-loads in the jail
    // without a manual KASTELLAN_*_EXTRA_FS_READ. Shares the manifest's seed
    // logic (interpreter_deps) so the two can't drift.
    let interpreter_lib_dirs = interpreter_extra_lib_dirs(
        &env.python,
        &|p| p.exists(),
        &|p| std::fs::canonicalize(p).ok(),
        &kastellan_core::workers::interpreter_deps::resolve_deps_via_tool,
    );
    let entry = python_exec_entry(
        env.worker_path.clone(),
        env.python.clone(),
        interpreter_lib_dirs,
        None,
    );
    let mut policy = entry.policy.clone();
    let scratch =
        kastellan_core::tool_host::prepare_ephemeral_scratch(&mut policy, entry.ephemeral_scratch)
            .expect("prepare scratch");
    // Capture the host scratch path (macOS; `None` on Linux) so we can assert
    // it is RAII-cleaned after `close()` — turns the "no leaked scratch dirs"
    // manual check into an in-band regression guard.
    let scratch_path = scratch.as_ref().map(|s| s.path().to_path_buf());
    let backend = backend();
    let worker_str = env.worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: None,
    };
    let mut sworker = spawn_worker(&*backend, &spec)
        .expect("spawn python-exec under sandbox")
        .with_scratch(scratch);
    let result = dispatch(pool, vault, &mut sworker, "python-exec", "python.exec", params).await;
    let _ = sworker.close();
    if let Some(p) = scratch_path {
        assert!(
            !p.exists(),
            "scratch dir must be RAII-cleaned after close(), but {} still exists",
            p.display()
        );
    }
    result
}

/// One jailed `python.exec` dispatch under the **production** policy, returning
/// the result value. Convenience wrapper over [`dispatch_in_jail`] with a fresh
/// (empty) vault and a code-only payload.
async fn exec_in_jail(
    pool: &sqlx::PgPool,
    env: &TestEnv,
    code: &str,
) -> Result<serde_json::Value, kastellan_core::tool_host::ToolHostError> {
    dispatch_in_jail(pool, env, &Vault::new(), serde_json::json!({ "code": code })).await
}

#[test]
fn print_round_trip_through_sandboxed_worker() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let r = exec_in_jail(&pool, &env, "print(6 * 7)")
            .await
            .expect("python.exec round trip");
        assert_eq!(r["exit_code"], 0, "stderr: {}", r["stderr"]);
        assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "42");
        pool.close().await;
    });
}

#[test]
fn socket_attempt_is_contained_by_the_jail() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        // Under seccomp `strict` the socket(2) syscall is not in the
        // allow-list → CPython dies with SIGSYS (exit_code null); under
        // Seatbelt the connect is denied → OSError (exit_code 1). Either
        // way: anything but success.
        let r = exec_in_jail(
            &pool,
            &env,
            "import socket\ns = socket.socket()\ns.connect(('127.0.0.1', 9))\nprint('escaped')",
        )
        .await
        .expect("dispatch itself must succeed");
        assert_ne!(r["exit_code"], 0, "socket attempt must not succeed: {r}");
        assert!(
            !r["stdout"].as_str().unwrap_or("").contains("escaped"),
            "network reached from inside the jail: {r}"
        );
        pool.close().await;
    });
}

#[test]
fn scratch_tmp_write_round_trip_inside_jail() {
    // Linux: bwrap's per-spawn `/tmp` tmpfs (#89). macOS: a host-created
    // per-spawn dir granted via Seatbelt `fs_write` + handed to the worker
    // through KASTELLAN_WORKER_SCRATCH (#283). Either way the agent code can
    // write + read a temp file inside the jail.
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let code = concat!(
            "import tempfile\n",
            "with tempfile.NamedTemporaryFile('w+', delete=True) as f:\n",
            "    f.write('jail-scratch-ok')\n",
            "    f.flush()\n",
            "    f.seek(0)\n",
            "    print(f.read())\n",
        );
        let r = exec_in_jail(&pool, &env, code)
            .await
            .expect("python.exec round trip");
        assert_eq!(r["exit_code"], 0, "stderr: {}", r["stderr"]);
        assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "jail-scratch-ok");
        pool.close().await;
    });
}

/// A params payload larger than the inline env threshold (64 KiB) must reach
/// the agent through the **file channel**: the worker writes
/// `<scratch>/params.json`, sets `KASTELLAN_PYTHON_PARAMS_FILE`, and the agent
/// reads the full value. If the file channel failed, `KASTELLAN_PYTHON_PARAMS`
/// would be the `"{}"` default → `KeyError` → non-zero exit, so a zero exit
/// with the correct length proves end-to-end delivery. Real worker, real jail,
/// production policy — runs on macOS (Seatbelt) + DGX (bwrap).
#[test]
fn large_param_round_trips_via_file_channel() {
    let Some(env) = ready_or_skip() else { return };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        // 100_000 bytes ≫ the 64 KiB inline threshold, ≪ the 1 MiB default
        // file ceiling → the File channel.
        let blob = "A".repeat(100_000);
        let code = concat!(
            "import json, os\n",
            "p = os.environ.get('KASTELLAN_PYTHON_PARAMS_FILE')\n",
            "if p:\n",
            "    with open(p) as f:\n",
            "        params = json.load(f)\n",
            "else:\n",
            "    params = json.loads(os.environ.get('KASTELLAN_PYTHON_PARAMS', '{}'))\n",
            "b = params['blob']\n",
            "print(len(b), b[:4], b[-4:])\n",
        );
        let params = serde_json::json!({ "code": code, "params": { "blob": blob } });
        let r = dispatch_in_jail(&pool, &env, &kastellan_core::secrets::Vault::new(), params)
            .await
            .expect("python.exec dispatch must succeed");
        assert_eq!(r["exit_code"].as_i64(), Some(0), "stderr: {}", r["stderr"]);
        assert_eq!(
            r["stdout"].as_str().unwrap().trim_end(),
            "100000 AAAA AAAA",
            "agent must read the full 100 KiB payload via the file channel"
        );
        pool.close().await;
    });
}

/// A secret materialized into a Vault and passed through the `params` channel as
/// an opaque `secret://` ref reaches the python-exec child as plaintext (so the
/// skill can use it), but the worker's stdout is **redacted** before `dispatch`
/// returns it — proving the python-exec output secret-scrub (design 2026-06-17)
/// end-to-end with the real worker, real jail, real Vault, and a real secret.
///
/// This is the in-process complement to the (deferred) full-daemon e2e: the
/// scrub lives in `tool_host::dispatch_with_sink`, which this path runs, so the
/// only thing not exercised here is the CLI→scheduler→l3py routing — and that
/// (which never touches the scrub) is covered by the param round-trip test in
/// `cli_memory_l3py_run_daemon_e2e.rs`. The full-daemon scrub e2e needs a
/// production seam to expose a Vault ref to the separate CLI process; it is
/// tracked as a follow-up issue. Without the scrub, the `!contains(plaintext)`
/// assertion below would fail — the secret would surface verbatim in stdout.
#[test]
fn materialized_secret_param_is_scrubbed_from_output() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let kp = test_key_provider();

        // A distinctive plaintext well over MIN_SECRET_LEN (8 bytes) so the leak
        // scanner can fingerprint it and an un-scrubbed leak would be unmistakable
        // in the assertion output.
        let plaintext = "SCRUBME-7c1f9a2b-secret-value-do-not-leak";
        kastellan_db::secrets::put(&pool, &kp, "scrub-secret", plaintext.as_bytes(), None)
            .await
            .expect("put secret");

        // Materialize into a Vault we own, then pass the opaque ref through the
        // params channel. `dispatch` substitutes `params.token` → plaintext before
        // the worker runs; the python code echoes it; the scrub redacts it on the
        // way back out.
        let vault = Vault::new();
        let secret_ref = vault
            .materialize(&pool, &kp, "scrub-secret", "test")
            .await
            .expect("materialize");

        let code = "import os, json\n\
                    p = json.loads(os.environ['KASTELLAN_PYTHON_PARAMS'])\n\
                    print('TOKEN:' + p['token'])\n";
        let params = serde_json::json!({
            "code": code,
            "params": { "token": secret_ref.as_str() },
        });

        let r = dispatch_in_jail(&pool, &env, &vault, params)
            .await
            .expect("python.exec dispatch must succeed");

        assert_eq!(r["exit_code"], 0, "stderr: {}", r["stderr"]);
        let stdout = r["stdout"].as_str().expect("stdout string");

        // The plaintext must NOT survive in the returned output...
        assert!(
            !stdout.contains(plaintext),
            "materialized secret plaintext leaked through python-exec output: {stdout:?}"
        );
        // ...and the scrub must have replaced it with the redaction marker.
        assert!(
            stdout.contains("[redacted:"),
            "expected a [redacted:<hex>] marker in scrubbed output, got: {stdout:?}"
        );

        // The redacted scrub audit row landed under the `policy` actor (hash/
        // offset/len only — never plaintext; that shape is unit-pinned in
        // tool_host::secret_scrub).
        let scrub_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log \
             WHERE actor='policy' AND action='secret.output_scrubbed'",
        )
        .fetch_one(&pool)
        .await
        .expect("count scrub rows");
        assert_eq!(scrub_rows, 1, "exactly one secret.output_scrubbed audit row");

        pool.close().await;
    });
}
