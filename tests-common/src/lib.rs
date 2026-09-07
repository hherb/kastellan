//! Shared test fixtures for `kastellan` integration tests.
//!
//! Hoisted from byte-duplicated copies that previously lived in eight
//! separate `tests/*.rs` files (issue #15). The point of consolidation
//! is correctness — a fix to (say) socket-dir permissions or the
//! `sun_path`-aware unique-suffix scheme now lands in one place instead
//! of drifting across N copies.
//!
//! # Module layout
//!
//! * [`skip`] — `[SKIP]` early-return helpers wrapping the supervisor +
//!   pg-binary + sandbox probes. Each returns `bool` (`true` = skip),
//!   and prints a `[SKIP]` line to stderr so `cargo test -- --nocapture`
//!   makes the skip visible (a green run with `[SKIP]` lines means
//!   tests skipped, not that the containment actually held).
//! * [`guards`] — `ServiceGuard` + `PathGuard` RAII cleanup so a
//!   panicking test cannot leave a stale systemd unit or 200 MB of
//!   `pg_wal` behind.
//! * [`temp`] — `unique_suffix` + `unique_temp_root` + `current_username`.
//! * [`wait`] — poll-loop helpers (`wait_for_status`, `wait_for_socket`,
//!   `wait_for_log_match`).
//! * [`pg`] — `PgCluster` + `bring_up_pg_cluster` — the initdb +
//!   `postgresql.auto.conf` + supervisor install/start dance.
//! * [`sandbox`] — `skip_if_sandbox_unavailable` + cfg-gated `backend()`
//!   factory + `policy_for_shell_exec` helper used by tests that spawn
//!   the shell-exec worker.
//! * [`binaries`] — workspace target-dir-aware binary discovery for
//!   integration tests that exec the `kastellan`, `kastellan-cli`, and
//!   `kastellan-worker-shell-exec` binaries.
//! * [`serial`] — macOS-only `serial_lock()` that mutexes the launchd
//!   `gui/<uid>` domain across daemon-spawning tests.
//! * [`embedding`] — `text_to_embedding` deterministic SHA-256-seeded
//!   L2-normalised seed vector used by the memory-recall tests.
//! * [`env`] — `env_lock()` + `EnvVarGuard` for unit tests that mutate
//!   process-wide environment variables (issue #127).
//! * [`daemon`] — `MockLlm` + `spawn_inert_mock` + `DaemonSpec` +
//!   `bring_up_daemon` — the real-`kastellan`-daemon-under-the-supervisor
//!   bring-up shared by all six daemon e2es (`cli_memory_l3*_run_daemon_e2e`,
//!   `mail_daemon_e2e`, `cli_ask_e2e`, `observation_capture`,
//!   `guard_boot_row_e2e`; the last three were hand-rolled copies until #634).
//! * [`scripted_llm`] — `ScriptedLlm` + `spawn_scripted_llm` + plan/embedding
//!   envelope builders: the queued multi-shot mock LLM shared by the daemon
//!   e2e tests that drive a scripted planner (lifted from `cli_ask_e2e.rs`).
//! * [`mock_localmail`] — a canned-response localmail `/v1` origin for the
//!   mail-worker e2e tiers (real response shapes; pinned by the Mac-only contract
//!   test), in a plain-HTTP flavour (direct transport) and a self-signed-HTTPS one
//!   (the force-routed MITM tier, via the proxy's upstream extra CA).
//! * [`egress_forcing`] — `short_scratch_root` / `minted_uds` /
//!   `assert_connect_established` / `UDS_FILE_NAME`: the force-routing e2e
//!   scaffolding shared by `egress_force_routing_e2e.rs` + `mail_e2e.rs`.
//! * [`audit`] — `NoopAuditSink`, the no-Postgres `AuditSink` shared by the
//!   `dispatch_with_sink`-based worker e2e tests.
//! * [`gliner_weights`] — `weights_dir_candidate` (pure) +
//!   `weights_dir_or_reason` / `resolve_weights_dir_or_skip`: the one
//!   `multi-v1.0` snapshot lookup for the three gliner-relex e2es, mirrored by
//!   the worker's own `tests/live_support.py`.
//! * [`gliner_e2e`] — `gliner_host_env`: the host-mode precondition cascade
//!   for those three suites, in one place, plus the
//!   `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` knob that turns each of its
//!   `[SKIP]`s into a failure (#653) and the #459 flag dialect the fixtures
//!   used to miss (#654). The suites' own `bring_up_pg` routes the supervisor
//!   and Postgres preconditions through the same knob via `report_unmet`.
//! * [`venv_interpreter`] — `venv_interpreter_binds`: the #284 interpreter
//!   binds for the three gliner-relex host-mode fixtures, with the `None`
//!   return checked so a `.venv` staged on another host fails loudly instead
//!   of spawning a jail with no interpreter.
//!
//! Nothing here is shipped at runtime. The crate is `publish = false`
//! and consumed only from `[dev-dependencies]`.

pub mod allowlist;
pub mod audit;
pub mod binaries;
pub mod daemon;
pub mod embedding;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod egress_forcing;
pub mod env;
pub mod gliner_e2e;
pub mod gliner_weights;
pub mod guard_pin;
pub mod guards;
pub mod installable;
pub mod microvm;
pub mod mock_localmail;
pub mod pg;
pub mod provisioning;
pub mod sandbox;
pub mod scripted_llm;
pub mod serial;
pub mod signal_death;
pub mod skip;
pub mod temp;
pub mod tls_origin;
pub mod venv_interpreter;
pub mod wait;
pub mod watchdog;

pub use allowlist::seed_tool_allowlist;
pub use audit::NoopAuditSink;
pub use binaries::{
    cli_binary, cli_command, core_binary, egress_proxy_bin_or_reason, egress_proxy_bin_or_skip,
    shell_exec_worker_binary, workspace_binary_or_reason, workspace_target_binary,
};
pub use daemon::{
    assert_cli_failure, assert_cli_success, bring_up_daemon, guard_tier_boot_payload,
    spawn_inert_mock, stderr_tail, DaemonGuards, DaemonHandle, DaemonSpec, LlmEndpoint, MockLlm,
    COMPAT_SEGMENT,
};
pub use embedding::text_to_embedding;
pub use env::{env_lock, EnvVarGuard};
pub use gliner_e2e::{
    enable_gate_unmet, first_unmet_precondition, gliner_host_env, gliner_host_lockdown_shim,
    host_env_from, report_unmet, report_unmet_to, require_action, unmet_action, venv_dir_of,
    venv_shim_or_reason, venv_shim_path, workspace_root, EnableFlag, UnmetAction, ENABLE_ENV,
    LOCKDOWN_SHIM_BIN, MODEL_ID, REQUIRE_ENV, VENV_SHIM_SUBPATH,
};
pub use gliner_weights::{
    resolve_weights_dir_or_skip, weights_dir_candidate, weights_dir_or_reason, WEIGHTS_SUBPATH,
};
pub use guards::{PathGuard, ServiceGuard};
pub use pg::{
    bring_up_pg_cluster, bring_up_pg_cluster_with_timeout, PgCluster, PG_BRING_UP_TIMEOUT_SECS,
};
pub use sandbox::{
    backend, policy_for_shell_exec, sandbox_unavailable_reason, skip_if_sandbox_unavailable,
};
pub use serial::serial_lock;
pub use signal_death::{
    assert_contained_by_signal, assert_contained_signal_death, assert_is_signal_death,
    assert_nonzero_exit, stdout_of,
};
pub use skip::{
    one_line, origin_unreachable_reason, origin_unreachable_reason_at, pg_bin_dir_or_reason,
    pg_bin_dir_or_skip, skip_if_no_supervisor, skip_line, supervisor_unavailable_reason, warn_line,
};
pub use temp::{current_username, unique_suffix, unique_temp_root};
pub use venv_interpreter::venv_interpreter_binds;
pub use wait::{wait_for_log_match, wait_for_socket, wait_for_status};
pub use watchdog::{await_within, close_pool, close_pool_bounded, DEFAULT_POOL_CLOSE_TIMEOUT};
