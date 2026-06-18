//! Unit tests for the production `StepDispatcher` (`tool_dispatch`).
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod
//! tests` block (Rust-2018 sibling-module pattern; precedents:
//! `audit/tests.rs`, `launchd_agents/tests.rs`, `macos_container/tests.rs`).
//! `use super::*` resolves to the parent `tool_dispatch` module, so the
//! re-exported pure helpers (`rpc_code_name`, `map_dispatch_result`,
//! moved to the `result_mapping` sibling) and the parent-private
//! `build_scheduler_step_failure_payload` / `ACTION_STEP_*` consts are
//! all reachable exactly as before the lift.

use super::*;
// `codes` + `ClientError` moved with the result-mapping helpers into the
// `result_mapping` sibling, and `ToolHostError` is no longer named in the
// parent's `use` glob — import them directly here (all were previously
// reached via `use super::*`).
use crate::tool_host::ToolHostError;
use kastellan_protocol::{client::ClientError, codes, RpcError};
use kastellan_sandbox::{Net, Profile};
use std::io;
use std::path::PathBuf;

// ----- rpc_code_name -----

#[test]
fn rpc_code_name_maps_known_codes() {
    // Each branch is pinned individually so a future rename in
    // `kastellan_protocol::codes` (e.g. renaming POLICY_DENIED) trips
    // a single specific assertion instead of a coalesced diff.
    assert_eq!(rpc_code_name(codes::PARSE_ERROR), "PARSE_ERROR");
    assert_eq!(rpc_code_name(codes::INVALID_REQUEST), "INVALID_REQUEST");
    assert_eq!(rpc_code_name(codes::METHOD_NOT_FOUND), "METHOD_NOT_FOUND");
    assert_eq!(rpc_code_name(codes::INVALID_PARAMS), "INVALID_PARAMS");
    assert_eq!(rpc_code_name(codes::INTERNAL_ERROR), "INTERNAL_ERROR");
    assert_eq!(rpc_code_name(codes::POLICY_DENIED), "POLICY_DENIED");
    assert_eq!(rpc_code_name(codes::OPERATION_FAILED), "OPERATION_FAILED");
}

#[test]
fn rpc_code_name_unknown_falls_back_to_generic() {
    // An app-level code the dispatcher hasn't been taught about
    // must surface as RPC_ERROR rather than an empty / panicking
    // mapping. The detail string still carries the worker's
    // original message.
    assert_eq!(rpc_code_name(-32099), "RPC_ERROR");
    assert_eq!(rpc_code_name(0), "RPC_ERROR");
    assert_eq!(rpc_code_name(i32::MAX), "RPC_ERROR");
}

// ----- map_dispatch_result -----

#[test]
fn map_dispatch_result_ok_preserves_value() {
    let v = serde_json::json!({"exit_code": 0, "stdout": "hi"});
    let out = map_dispatch_result(Ok(v.clone()));
    match out {
        StepOutcome::Ok(got) => assert_eq!(got, v),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn map_dispatch_result_protocol_rpc_uses_named_code() {
    // The worker rejected the call with POLICY_DENIED (-32001). The
    // dispatcher must surface the *name* not the integer, and
    // must preserve the worker's `message` verbatim so the audit
    // trail captures the underlying reason.
    let rpc = RpcError::new(codes::POLICY_DENIED, "argv not allowlisted");
    let err = ToolHostError::Protocol(ClientError::Rpc(rpc));
    let out = map_dispatch_result(Err(err));
    match out {
        StepOutcome::Err { code, detail } => {
            assert_eq!(code, "POLICY_DENIED");
            assert_eq!(detail, "argv not allowlisted");
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn map_dispatch_result_protocol_rpc_unknown_code_falls_back() {
    let rpc = RpcError::new(-32099, "custom worker error");
    let err = ToolHostError::Protocol(ClientError::Rpc(rpc));
    match map_dispatch_result(Err(err)) {
        StepOutcome::Err { code, detail } => {
            assert_eq!(code, "RPC_ERROR");
            assert_eq!(detail, "custom worker error");
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn map_dispatch_result_protocol_non_rpc_uses_protocol_error_code() {
    // ClientError::EarlyExit (worker exited before responding) is
    // a non-Rpc protocol failure — distinct from a structured
    // RPC error.
    let err = ToolHostError::Protocol(ClientError::EarlyExit);
    match map_dispatch_result(Err(err)) {
        StepOutcome::Err { code, detail } => {
            assert_eq!(code, "PROTOCOL_ERROR");
            // The Display string must contain *something*
            // operator-readable; pin the substring rather than
            // the exact form so a thiserror message tweak
            // doesn't churn this test.
            assert!(detail.contains("exited"), "detail: {detail:?}");
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn map_dispatch_result_io_error_is_distinct_from_protocol() {
    // A raw stdio I/O failure (e.g. broken pipe) is bucketed
    // as IO_ERROR, not PROTOCOL_ERROR. Operators triaging audit
    // logs can split host-side I/O issues from JSON-RPC issues.
    let io = io::Error::new(io::ErrorKind::BrokenPipe, "pipe down");
    let err = ToolHostError::Io(io);
    match map_dispatch_result(Err(err)) {
        StepOutcome::Err { code, detail } => {
            assert_eq!(code, "IO_ERROR");
            assert!(detail.contains("pipe"), "detail: {detail:?}");
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

// ----- ToolRegistry -----

fn fake_entry() -> ToolEntry {
    ToolEntry {
        binary: PathBuf::from("/usr/local/bin/fake"),
        policy: SandboxPolicy {
            mem_mb: 32,
            ..SandboxPolicy::default()
        },
        wall_clock_ms: Some(5_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}

#[test]
fn tool_registry_starts_empty() {
    let reg = ToolRegistry::new();
    assert!(reg.is_empty());
    assert_eq!(reg.len(), 0);
    assert!(reg.lookup("anything").is_none());
}

#[test]
fn tool_registry_insert_then_lookup_round_trip() {
    let mut reg = ToolRegistry::new();
    reg.insert("shell-exec", fake_entry());
    assert!(!reg.is_empty());
    assert_eq!(reg.len(), 1);
    let got = reg.lookup("shell-exec").expect("entry present");
    assert_eq!(got.binary, PathBuf::from("/usr/local/bin/fake"));
    assert!(reg.lookup("nope").is_none());
}

#[test]
fn tool_registry_insert_replaces_existing_entry() {
    // Re-inserting under the same name swaps the entry (HashMap
    // semantics). Documented here so a future split into a
    // multi-entry registry tripwires this expectation.
    let mut reg = ToolRegistry::new();
    reg.insert("shell-exec", fake_entry());
    let mut second = fake_entry();
    second.binary = PathBuf::from("/opt/kastellan/shell-exec");
    reg.insert("shell-exec", second);
    assert_eq!(reg.len(), 1);
    assert_eq!(
        reg.lookup("shell-exec").unwrap().binary,
        PathBuf::from("/opt/kastellan/shell-exec")
    );
}

#[test]
fn tool_names_returns_registered_names_sorted() {
    let mut reg = ToolRegistry::new();
    reg.insert("web-fetch", fake_entry());
    reg.insert("shell-exec", fake_entry());
    let names = reg.tool_names();
    assert!(names.contains("shell-exec"));
    assert!(names.contains("web-fetch"));
    assert_eq!(names.len(), 2);
    // BTreeSet is sorted: first element is the lexicographically smallest.
    assert_eq!(names.iter().next().map(String::as_str), Some("shell-exec"));
}

// ----- shell_exec_entry -----

#[test]
fn shell_exec_entry_carries_allowlist_in_env() {
    // The allowlist round-trips into the policy's env vec as
    // KASTELLAN_SHELL_ALLOWLIST = JSON array. The worker reads it
    // at startup; changing the env-var name or the encoding here
    // requires a coordinated change in `workers/shell-exec/src`.
    let binary = PathBuf::from("/usr/local/bin/kastellan-worker-shell-exec");
    let allowlist = vec![
        "/usr/bin/echo".to_string(),
        "/bin/echo".to_string(),
    ];
    let entry = shell_exec_entry(binary.clone(), &allowlist);

    assert_eq!(entry.binary, binary);
    assert_eq!(entry.wall_clock_ms, Some(30_000));

    // Policy invariants the threat-model relies on.
    assert!(matches!(entry.policy.net, Net::Deny),
            "shell-exec must default to network-denied");
    assert!(matches!(entry.policy.profile, Profile::WorkerStrict),
            "shell-exec must run under WorkerStrict (no socket() syscalls)");
    assert_eq!(entry.policy.fs_write, Vec::<PathBuf>::new(),
               "shell-exec entry should not pre-allocate writable scratch");
    assert!(entry.policy.fs_read.contains(&binary),
            "binary must be in fs_read so bwrap can mount it");

    // The allowlist env entry.
    let allow_env = entry.policy.env.iter()
        .find(|(k, _)| k == "KASTELLAN_SHELL_ALLOWLIST")
        .expect("allowlist env entry must be present");
    let parsed: Vec<String> = serde_json::from_str(&allow_env.1)
        .expect("allowlist value must be JSON-decodable");
    assert_eq!(parsed, allowlist);
}

#[test]
fn shell_exec_entry_empty_allowlist_is_valid_deny_all() {
    // An empty allowlist is the safest default — the worker
    // accepts no argv. The daemon admin opts programs in
    // explicitly. Worker-side handling (shell-exec/src) must
    // already reject "no allowlist" or "empty allowlist" with
    // POLICY_DENIED.
    let entry = shell_exec_entry(PathBuf::from("/x"), &[]);
    let allow_env = entry.policy.env.iter()
        .find(|(k, _)| k == "KASTELLAN_SHELL_ALLOWLIST")
        .expect("allowlist env entry must be present");
    assert_eq!(allow_env.1, "[]");
}

#[test]
fn shell_exec_entry_declares_single_use_lifecycle() {
    // Shell-exec must remain single-use forever — per-request isolation IS its
    // security model. If a future change to `shell_exec_entry` accidentally swaps
    // this for `IdleTimeout`, this test trips so the regression is caught at PR
    // time rather than in production. See
    // `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` §"The
    // two policies" for why shell-exec stays in the `single_use` category.
    let entry = shell_exec_entry(PathBuf::from("/x"), &[]);
    assert!(matches!(
        entry.lifecycle,
        crate::worker_lifecycle::Lifecycle::SingleUse
    ));
}

// The unknown-tool branch of `ToolHostStepDispatcher::dispatch_step`
// is covered end-to-end in `core/tests/scheduler_step_dispatch_e2e.rs`
// (the dispatcher needs a real `PgPool` to construct, so a pure unit
// test would be tautological). `tool_registry_starts_empty` above
// pins the underlying registry-miss contract.

// ----- build_scheduler_step_failure_payload -----

#[test]
fn build_payload_unknown_tool_shape_has_no_err_field() {
    // UNKNOWN_TOOL is a registry-miss; there is no underlying error
    // string to attach. The audit consumer's filter on
    // `payload ? 'err'` distinguishes this row from `step.spawn_failed`
    // by structure alone.
    let req = serde_json::json!({"url": "https://example.com"});
    let payload = build_scheduler_step_failure_payload(
        "web-fetch", "fetch", req.clone(), None, 0,
    );
    let obj = payload.as_object().expect("payload must be a JSON object");
    assert_eq!(obj.get("tool").and_then(|v| v.as_str()), Some("web-fetch"));
    assert_eq!(obj.get("method").and_then(|v| v.as_str()), Some("fetch"));
    assert_eq!(obj.get("req"), Some(&req));
    assert_eq!(obj.get("ms").and_then(|v| v.as_u64()), Some(0));
    assert!(
        !obj.contains_key("err"),
        "UNKNOWN_TOOL payload must omit `err`; got {payload:#}",
    );
    // Exactly the keys we expect — no accidental extras (which would
    // shift the audit-shape contract in a future refactor).
    let keys: std::collections::BTreeSet<&str> =
        obj.keys().map(|s| s.as_str()).collect();
    let expected: std::collections::BTreeSet<&str> =
        ["tool", "method", "req", "ms"].iter().copied().collect();
    assert_eq!(keys, expected, "unexpected keys in payload");
}

#[test]
fn build_payload_spawn_failed_shape_includes_err_string() {
    // SPAWN_FAILED carries the sandbox/IO error's `to_string()` so
    // operators can triage from the audit log alone.
    let req = serde_json::json!({"argv": ["/bin/echo", "hi"]});
    let payload = build_scheduler_step_failure_payload(
        "shell-exec",
        "shell.exec",
        req.clone(),
        Some("sandbox: policy paths must be absolute"),
        7,
    );
    let obj = payload.as_object().expect("payload must be a JSON object");
    assert_eq!(obj.get("tool").and_then(|v| v.as_str()), Some("shell-exec"));
    assert_eq!(obj.get("method").and_then(|v| v.as_str()), Some("shell.exec"));
    assert_eq!(obj.get("req"), Some(&req));
    assert_eq!(
        obj.get("err").and_then(|v| v.as_str()),
        Some("sandbox: policy paths must be absolute"),
    );
    assert_eq!(obj.get("ms").and_then(|v| v.as_u64()), Some(7));
    // No accidental extras here either.
    let keys: std::collections::BTreeSet<&str> =
        obj.keys().map(|s| s.as_str()).collect();
    let expected: std::collections::BTreeSet<&str> =
        ["tool", "method", "req", "err", "ms"].iter().copied().collect();
    assert_eq!(keys, expected, "unexpected keys in payload");
}

#[test]
fn shell_exec_entry_defaults_container_image_to_none() {
    // Pin the default so a future operator-config plumbing pass that
    // adds image-tag inheritance for non-container backends has to
    // update this test deliberately — it must not silently start
    // populating container_image on workers that don't use container.
    let entry = shell_exec_entry(
        PathBuf::from("/usr/bin/true"),
        &["/usr/bin/true".to_string()],
    );
    assert!(
        entry.container_image.is_none(),
        "shell_exec_entry must default container_image to None; got {:?}",
        entry.container_image,
    );
}

/// `shell_exec_entry` defaults `sandbox_backend` to `None` so the
/// shell-exec worker stays on the per-OS default backend (Seatbelt
/// on darwin, Bwrap on linux). A future explicit opt-in to
/// `Some(SandboxBackendKind::Container)` would be a deliberate
/// audit-trail change.
#[test]
fn shell_exec_entry_defaults_sandbox_backend_to_none() {
    let entry = shell_exec_entry(
        std::path::PathBuf::from("/usr/bin/true"),
        &["true".to_string()],
    );
    assert_eq!(entry.sandbox_backend, None);
}
