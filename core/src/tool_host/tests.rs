//! Unit tests for `tool_host`'s parent-module items.
//!
//! Lifted out of `tool_host.rs` (HANDOVER Next-TODO item 5, the file-size
//! sibling-lift). These pin the [`crate::tool_host::WorkerCommand`] seal —
//! the module-private constructor (issue #16 fix, 2026-05-13) that only
//! `tool_host` and its descendants may reach. `use super::*` resolves to the
//! parent `tool_host` module, so the otherwise-unreachable `WorkerCommand::new`
//! constructor and its private fields are visible here (this module is a
//! descendant of `tool_host`).
//!
//! The watchdog and lockdown-env tests live co-located with their code in the
//! `watchdog` / `lockdown_env` sibling modules.

use super::*;

#[test]
fn worker_command_new_carries_method_and_params() {
    // In-module sanity check: the module-private constructor (see
    // issue #16 fix 2026-05-13 — narrowed from `pub(crate)` to
    // module-private) preserves both the method name (any
    // `Into<String>` form) and the serde_json value verbatim.
    // Tests inside this `tests` module are descendants of `tool_host`
    // and therefore have access to its private items, so this
    // assertion compiles; sibling modules of `tool_host` (e.g.
    // `scheduler`) do not have that access and the build would refuse
    // a hypothetical `WorkerCommand::new(...)` from there. The
    // `compile_fail` doctest on `WorkerCommand` is the regression pin
    // for the out-of-crate side; the workspace build is the regression
    // pin for the in-crate sibling-module side.
    let cmd = WorkerCommand::new("shell.exec", serde_json::json!({"argv": ["/bin/echo", "hi"]}));
    assert_eq!(cmd.method, "shell.exec");
    assert_eq!(cmd.params["argv"][0], "/bin/echo");
    assert_eq!(cmd.params["argv"][1], "hi");
}

#[test]
fn worker_command_new_accepts_owned_string() {
    // The `impl Into<String>` parameter shape lets dispatch pass
    // its `&str` `method` parameter without a redundant owned
    // allocation at the call site, while still letting an owned
    // `String` flow through. Pin both shapes so a refactor to a
    // narrower bound (e.g. `&str`-only) trips this test.
    let owned: String = "shell.exec".to_string();
    let cmd = WorkerCommand::new(owned, serde_json::Value::Null);
    assert_eq!(cmd.method, "shell.exec");
    assert!(cmd.params.is_null());
}
