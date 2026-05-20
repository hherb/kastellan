//! Pin the unknown-action exit-code contract for the `hhagent-cli`
//! per-subcommand dispatchers that don't already have a dedicated
//! bad-args test (`entities` is already covered by
//! `cli_entities_e2e::cli_entities_bad_args_exit_code_two`).
//!
//! Why this file exists: [Issue #97][issue-97] moved the
//! `multi_thread_runtime` construction in 4 dispatchers from
//! *before* the action match to *inside* the known-action arms,
//! so an invalid action (`hhagent-cli tasks frobnicate`) no longer
//! spawns tokio worker threads it never uses. The structural change
//! is invisible from outside; the observable contract is
//! "invalid action -> exit 2 + the same `<dispatcher>: unknown ...`
//! stderr line as before." These tests pin that observable surface
//! so a future refactor cannot change the wording or the exit code
//! by accident.
//!
//! [issue-97]: https://github.com/hherb/hhagent/issues/97
//!
//! No Postgres or daemon required: the bad-action path must exit
//! before any DB connection is attempted, so passing
//! `HHAGENT_DATA_DIR=/nonexistent-...` proves the early-exit invariant.
//! Skips cleanly if the CLI binary hasn't been built.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use hhagent_tests_common::cli_binary;

/// Build a minimal env block that points the CLI at a non-existent
/// data dir. Used only as defence-in-depth: a bad-action path must
/// never reach `resolve_connect_spec`, so the value of
/// `HHAGENT_DATA_DIR` is irrelevant. If the early-exit invariant
/// regresses, the CLI would error on `connect_runtime_pool` and
/// produce a *different* stderr — the assertions below would catch
/// that as a contract change.
fn bad_args_env() -> Vec<(String, String)> {
    let mut env = vec![(
        "HHAGENT_DATA_DIR".to_string(),
        "/nonexistent-hhagent-cli-bad-args-test".to_string(),
    )];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    env
}

#[test]
fn cli_tasks_unknown_subcommand_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_tasks_unknown_subcommand_exits_two: hhagent-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["tasks", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli tasks frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`tasks frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("tasks: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}

#[test]
fn cli_memory_l1_unknown_action_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_memory_l1_unknown_action_exits_two: hhagent-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["memory", "l1", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli memory l1 frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`memory l1 frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("memory l1: unknown action"),
        "stderr must carry the dispatcher-prefixed unknown-action line; got: {stderr}",
    );
}

#[test]
fn cli_tools_allowlist_unknown_subcommand_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_tools_allowlist_unknown_subcommand_exits_two: hhagent-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["tools", "allowlist", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli tools allowlist frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`tools allowlist frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("tools allowlist: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}
