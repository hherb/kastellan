//! Typed [`ServiceSpec`] builders for the canonical kastellan services.
//!
//! Centralizes "what a real kastellan service looks like" so both the
//! Linux ([`crate::systemd_user`]) and macOS ([`crate::launchd_agents`])
//! backends emit the same semantics from one source of truth.
//!
//! Pure: no I/O, no env probing — the caller resolves the binary path
//! and the log directory and passes them in. That keeps these helpers
//! trivially unit-testable and means a test can synthesize a spec
//! without filesystem side effects.
//!
//! Today this module ships the agent-core and Postgres daemon specs, the
//! optional guard-model backend (`llama-server`, #609), plus the
//! `kastellan_target_spec` bundle that ties the canonical services together.

use std::path::Path;

use crate::{RestartBackoff, ServiceSpec, TargetSpec};

/// Canonical name used for the agent-core daemon's unit/agent file.
///
/// Same string on both OSes — the file becomes `kastellan-core.service`
/// on Linux and `kastellan-core.plist` on macOS, and the launchd `Label`
/// is the same. We deliberately don't use a reverse-DNS form
/// (`org.kastellan.core`) so the same name works through both backends
/// without per-OS branching in caller code (the supervisor lib.rs
/// doc-comment on `ServiceSpec.name` calls out that either style is
/// acceptable).
pub const CORE_SERVICE_NAME: &str = "kastellan-core";

/// Canonical name used for the per-user Postgres daemon's unit/agent
/// file. Same shape rationale as [`CORE_SERVICE_NAME`].
pub const POSTGRES_SERVICE_NAME: &str = "kastellan-postgres";

/// Canonical name of the optional guard-model backend (`llama-server` serving
/// the guard tier's model). Same shape rationale as [`CORE_SERVICE_NAME`].
pub const GUARD_SERVICE_NAME: &str = "kastellan-guard";

/// Context size the guard backend is started with, in tokens.
///
/// The daemon's guard tier refuses to boot below its D8 floor
/// (`REQUIRED_GUARD_N_CTX` in `kastellan-core`, 66 048 today — a worst-case
/// 64 KiB document plus the prompt); `kastellan-core` pins this value against
/// that floor in a test. 131 072 is what the live DGX guard has run since its
/// bring-up, with room to spare.
pub const GUARD_CONTEXT_TOKENS: u32 = 131_072;

/// Canonical name of the service bundle that brings up the whole agent.
/// Becomes `kastellan.target` on systemd; on launchd it names the member
/// set only. Same string on both OSes (see [`CORE_SERVICE_NAME`]).
pub const KASTELLAN_TARGET_NAME: &str = "kastellan";

/// Build a [`ServiceSpec`] for the agent-core daemon (`kastellan`
/// binary, see `core/src/main.rs`).
///
/// Arguments:
/// - `binary` — absolute path to the compiled `kastellan` binary.
///   Today this is `target/debug/kastellan` in dev. Production install
///   location is an open question (see HANDOVER "Open questions"
///   #6); when that lands, the *caller* changes, this helper does
///   not.
/// - `log_dir` — directory where stdout/stderr append logs go.
///   The supervisor backends require the parent directory of each
///   log path to exist; the caller must create `log_dir` before
///   calling [`crate::Supervisor::install`]. The two log files are
///   named `<CORE_SERVICE_NAME>.out` and `<CORE_SERVICE_NAME>.err`
///   under `log_dir`.
///
/// Choices baked into today's spec, with reasons:
/// - `args` is empty: the daemon parses no flags yet.
/// - `env` is empty: `core/src/main.rs` reads `RUST_LOG` from the
///   environment but defaults to `"info"` when unset (see the
///   `unwrap_or_else` in `main`), so we don't need to inject it.
///   When the daemon grows real config-via-env, populate this.
/// - `working_dir` is `None`: nothing in the daemon depends on cwd.
/// - `keep_alive` is `true`: the daemon now blocks on SIGTERM/SIGINT
///   and is meant to stay running until the supervisor stops it. On
///   systemd this becomes `Restart=on-failure` so a *crash* (non-zero
///   exit) triggers a respawn while a clean SIGTERM-induced exit does
///   not. On launchd this becomes `KeepAlive=true` with the same
///   intent (bootout removes the agent from the domain entirely, so
///   `stop` still ends the process for good). The regression test
///   ([`tests::core_service_spec_keep_alive_is_true`]) pins today's
///   value so a regression can't sneak in unnoticed.
/// - `restart_backoff` is `Some({ max_delay_sec: 300, steps: 8 })`: on
///   systemd a crash-looping daemon ramps the restart delay
///   `RestartSec=5` → `RestartMaxDelaySec=300` over `RestartSteps=8`
///   instead of hammering every 5 s; on launchd this is warned-and-ignored
///   (no equivalent knob). Pinned by
///   [`tests::core_service_spec_carries_expected_backoff_curve`].
pub fn core_service_spec(binary: &Path, log_dir: &Path) -> ServiceSpec {
    ServiceSpec {
        name: CORE_SERVICE_NAME.into(),
        program: binary.to_path_buf(),
        args: vec![],
        // Egress force-routing is on by default in the supervised deployment
        // (egress proxy slice #2): every `Net::Allowlist` worker is routed
        // through a per-worker egress-proxy sidecar with no direct network
        // route. **Fail-closed** — the daemon refuses to start if the
        // `kastellan-worker-egress-proxy` binary isn't installed beside it
        // (see `worker_lifecycle::force_route::from_env`), so a packaged
        // deployment must ship the proxy alongside the core binary. Operators
        // can override via the unit's `EnvironmentFile` (set to `0`) for a
        // bring-up without the proxy.
        env: vec![(
            "KASTELLAN_EGRESS_FORCE_ROUTING".to_string(),
            "1".to_string(),
        )],
        working_dir: None,
        keep_alive: true,
        stdout_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.out"))),
        stderr_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.err"))),
        after: vec![POSTGRES_SERVICE_NAME.to_string()],
        part_of: Some(KASTELLAN_TARGET_NAME.to_string()),
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
        environment_files: Vec::new(),
    }
}

/// Build a [`ServiceSpec`] for the per-user Postgres daemon (`postgres`
/// binary, see PGDG `postgresql-18` package on Linux / Homebrew
/// `postgresql@18` on macOS).
///
/// Arguments:
/// - `postgres_binary` — absolute path to the `postgres` executable.
///   Linux PGDG default: `/usr/lib/postgresql/18/bin/postgres`.
///   macOS Homebrew default:
///   `/opt/homebrew/opt/postgresql@18/bin/postgres` (Apple Silicon)
///   or `/usr/local/opt/postgresql@18/bin/postgres` (Intel).
///   Caller resolves which one — see [`kastellan_db::find_pg_bin_dir`]
///   in the `db` crate.
/// - `data_dir` — absolute path to the cluster data dir (the one that
///   `kastellan-db-init` populated; postgres is invoked with `-D <path>`).
/// - `log_dir` — directory where the supervisor appends stdout/stderr.
///   Caller must create the dir before [`crate::Supervisor::install`].
///   Files: `<POSTGRES_SERVICE_NAME>.out` and `.err`.
///
/// Choices baked in:
/// - **`args = ["-D", <data_dir>]`** — the only argument postgres needs.
///   The unix socket directory and `listen_addresses=''` come from
///   `postgresql.auto.conf` inside the data dir, so no `-k` flag is
///   needed at the supervisor layer (and we keep the spec minimal so
///   the same shape works whether the caller sets the socket inside
///   or outside the data dir).
/// - **`env` is empty** — postgres does not require any environment
///   variables to start cleanly when given `-D`. Locale defaults are
///   already baked into the cluster by `initdb`'s `--encoding=UTF8`.
///   When workers later need to override `LC_ALL` or set
///   `PGTZ`, populate this; today we deliberately pass nothing so the
///   process inherits a clean env from the supervisor.
/// - **`working_dir = None`** — postgres reads `data_dir` exclusively
///   from `-D` and writes its own pidfile/logs there. Cwd is irrelevant.
/// - **`keep_alive = true`** — postgres is a long-running daemon. On
///   systemd this is `Restart=on-failure RestartSec=5` (a crash means
///   we restart, a clean stop via SIGTERM does not). On launchd this
///   is `KeepAlive=true` (same intent; `bootout` removes the agent
///   from the domain entirely so `stop` still ends the process).
/// - **`restart_backoff = Some({ max_delay_sec: 300, steps: 8 })`** — on
///   systemd a crash-looping cluster ramps `RestartSec=5` →
///   `RestartMaxDelaySec=300` over `RestartSteps=8` rather than respawning
///   every 5 s; on launchd this is warned-and-ignored (no equivalent knob).
///   Pinned by [`tests::postgres_service_spec_carries_expected_backoff_curve`].
///
/// Pure: no I/O, no env probing. Same call → same spec every time.
pub fn postgres_service_spec(
    postgres_binary: &Path,
    data_dir: &Path,
    log_dir: &Path,
) -> ServiceSpec {
    ServiceSpec {
        name: POSTGRES_SERVICE_NAME.into(),
        program: postgres_binary.to_path_buf(),
        args: vec!["-D".into(), data_dir.to_string_lossy().into_owned()],
        env: vec![],
        working_dir: None,
        keep_alive: true,
        stdout_log: Some(log_dir.join(format!("{POSTGRES_SERVICE_NAME}.out"))),
        stderr_log: Some(log_dir.join(format!("{POSTGRES_SERVICE_NAME}.err"))),
        after: vec![],
        part_of: Some(KASTELLAN_TARGET_NAME.to_string()),
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
        environment_files: Vec::new(),
    }
}

/// Build a [`ServiceSpec`] for the guard-model backend: `llama-server`
/// serving the guard tier's model on loopback (#609).
///
/// Why it is supervised at all: before #609 the guard ran as a hand-started
/// process, so a reboot left it down — and because the daemon refuses to boot
/// with a configured but unreachable guard (wiring-spec D6), the *whole agent*
/// stayed down with it (1 500+ restarts on the DGX, 2026-10-19 → 24).
///
/// Arguments:
/// - `server` — absolute path to the operator's `llama-server`.
/// - `model` — absolute path to the guard model's GGUF file.
/// - `alias` — the model name the API answers to. Must equal the daemon's
///   `KASTELLAN_LLM_GUARD_MODEL`, which is what the installer passes.
/// - `port` — the loopback port. Must equal the port in the daemon's
///   `KASTELLAN_LLM_GUARD_URL`, likewise derived by the installer.
/// - `log_dir` — as for [`core_service_spec`].
///
/// Choices baked in:
/// - **`--host 127.0.0.1`**: the guard is a core-only dependency; nothing off
///   the host may reach it.
/// - **`-c` [`GUARD_CONTEXT_TOKENS`]**: must clear the daemon's D8 floor.
/// - **`-ngl 99`**: offload every layer when there is a GPU. On a host without
///   one `llama-server` warns and runs on the CPU — slower, but the tier still
///   works, so the flag is safe everywhere.
/// - **`env` empty**: `llama-server` needs none (measured on the DGX with
///   `env -i`; its libraries resolve through the binary's rpath).
/// - **`keep_alive` + the same backoff as core**: a crashed guard must come
///   back, or core cannot boot.
/// - **Not sandboxed**: like Postgres it is an operator-installed service, not
///   a tool worker, and it runs no untrusted *code*. It does parse untrusted
///   *input* — scoring attacker-controlled documents is what it is for — so
///   this is a deliberate choice with a known exposure: loopback-only, but
///   CORS `*` and no API key ([#907]).
///
/// [#907]: https://github.com/hherb/kastellan/issues/907
///
/// Pure: no I/O, no env probing.
pub fn guard_service_spec(
    server: &Path,
    model: &Path,
    alias: &str,
    port: u16,
    log_dir: &Path,
) -> ServiceSpec {
    ServiceSpec {
        name: GUARD_SERVICE_NAME.into(),
        program: server.to_path_buf(),
        args: vec![
            "-m".into(),
            model.to_string_lossy().into_owned(),
            "--alias".into(),
            alias.into(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            port.to_string(),
            "-c".into(),
            GUARD_CONTEXT_TOKENS.to_string(),
            "-ngl".into(),
            "99".into(),
        ],
        env: vec![],
        working_dir: None,
        keep_alive: true,
        stdout_log: Some(log_dir.join(format!("{GUARD_SERVICE_NAME}.out"))),
        stderr_log: Some(log_dir.join(format!("{GUARD_SERVICE_NAME}.err"))),
        after: vec![],
        part_of: Some(KASTELLAN_TARGET_NAME.to_string()),
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
        environment_files: Vec::new(),
    }
}

/// Build the canonical [`TargetSpec`] that brings up the whole agent.
///
/// Members in **start order**: Postgres first (the dependency leaf),
/// then core (which must start after Postgres). Inference is **not** a
/// member — it is an operator-managed external dependency that core's
/// startup probe health-checks — except the guard backend, which the
/// installer inserts before core when the operator configures one (#609).
/// Workers are **not** members either — `tool_host` spawns them on demand
/// inside sandboxes when core runs.
///
/// Pure: no I/O, same call → same value.
pub fn kastellan_target_spec() -> TargetSpec {
    TargetSpec {
        name: KASTELLAN_TARGET_NAME.into(),
        members: vec![
            POSTGRES_SERVICE_NAME.into(),
            CORE_SERVICE_NAME.into(),
        ],
    }
}

#[cfg(test)]
mod tests;
