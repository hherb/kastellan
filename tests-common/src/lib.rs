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
//!
//! Nothing here is shipped at runtime. The crate is `publish = false`
//! and consumed only from `[dev-dependencies]`.

pub mod allowlist;
pub mod binaries;
pub mod embedding;
pub mod env;
pub mod guards;
pub mod pg;
pub mod sandbox;
pub mod serial;
pub mod skip;
pub mod temp;
pub mod wait;

pub use allowlist::seed_tool_allowlist;
pub use binaries::{cli_binary, core_binary, shell_exec_worker_binary, workspace_target_binary};
pub use embedding::text_to_embedding;
pub use env::{env_lock, EnvVarGuard};
pub use guards::{PathGuard, ServiceGuard};
pub use pg::{
    bring_up_pg_cluster, bring_up_pg_cluster_with_timeout, PgCluster, PG_BRING_UP_TIMEOUT_SECS,
};
pub use sandbox::{backend, policy_for_shell_exec, skip_if_sandbox_unavailable};
pub use serial::serial_lock;
pub use skip::{pg_bin_dir_or_skip, skip_if_no_supervisor};
pub use temp::{current_username, unique_suffix, unique_temp_root};
pub use wait::{wait_for_log_match, wait_for_socket, wait_for_status};
