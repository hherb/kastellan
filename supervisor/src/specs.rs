//! Typed [`ServiceSpec`] builders for the canonical hhagent services.
//!
//! Centralizes "what a real hhagent service looks like" so both the
//! Linux ([`crate::systemd_user`]) and macOS ([`crate::launchd_agents`])
//! backends emit the same semantics from one source of truth.
//!
//! Pure: no I/O, no env probing — the caller resolves the binary path
//! and the log directory and passes them in. That keeps these helpers
//! trivially unit-testable and means a test can synthesize a spec
//! without filesystem side effects.
//!
//! Today this module ships the agent-core daemon spec only. More will
//! land here as services are added (Postgres, inference router, etc.).

use std::path::Path;

use crate::ServiceSpec;

/// Canonical name used for the agent-core daemon's unit/agent file.
///
/// Same string on both OSes — the file becomes `hhagent-core.service`
/// on Linux and `hhagent-core.plist` on macOS, and the launchd `Label`
/// is the same. We deliberately don't use a reverse-DNS form
/// (`org.hhagent.core`) so the same name works through both backends
/// without per-OS branching in caller code (the supervisor lib.rs
/// doc-comment on `ServiceSpec.name` calls out that either style is
/// acceptable).
pub const CORE_SERVICE_NAME: &str = "hhagent-core";

/// Build a [`ServiceSpec`] for the agent-core daemon (`hhagent`
/// binary, see `core/src/main.rs`).
///
/// Arguments:
/// - `binary` — absolute path to the compiled `hhagent` binary.
///   Today this is `target/debug/hhagent` in dev. Production install
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
/// - `keep_alive` is `false`: the daemon is currently a placeholder
///   that emits one log line and exits 0. `Restart=on-failure`
///   (systemd's translation of `keep_alive=true`) wouldn't restart
///   on a clean exit anyway, so flipping this becomes meaningful
///   only when the daemon is rewritten as a long-running event
///   loop. There is a regression test ([`tests::core_service_spec_keep_alive_is_false_for_now`])
///   pinning today's value so the change can't sneak in unnoticed.
pub fn core_service_spec(binary: &Path, log_dir: &Path) -> ServiceSpec {
    ServiceSpec {
        name: CORE_SERVICE_NAME.into(),
        program: binary.to_path_buf(),
        args: vec![],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.out"))),
        stderr_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.err"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Pin the canonical name so a typo can't silently rename the
    /// service in a future refactor.
    #[test]
    fn core_service_spec_uses_canonical_name() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/var/log/hhagent"),
        );
        assert_eq!(spec.name, "hhagent-core");
        assert_eq!(spec.name, CORE_SERVICE_NAME);
    }

    /// The caller's binary path must flow through verbatim — both
    /// backends require an absolute `program`, but it's the caller's
    /// job to pass one (this helper is pure).
    #[test]
    fn core_service_spec_program_is_caller_supplied() {
        let bin = PathBuf::from("/opt/hhagent/bin/hhagent");
        let spec = core_service_spec(&bin, Path::new("/tmp/logs"));
        assert_eq!(spec.program, bin);
    }

    /// Defends against accidentally injecting unintended argv.
    #[test]
    fn core_service_spec_args_are_empty() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(spec.args.is_empty(), "daemon takes no flags yet");
    }

    /// Defends against accidentally injecting env that would override
    /// the daemon's RUST_LOG default or leak host config.
    #[test]
    fn core_service_spec_env_is_empty() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(spec.env.is_empty(), "no env injection by default");
    }

    /// The daemon doesn't depend on cwd; not setting one keeps the
    /// supervisor unit shape small.
    #[test]
    fn core_service_spec_does_not_set_working_dir() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(spec.working_dir.is_none());
    }

    /// Pinning today's value: when the daemon becomes a real
    /// long-running event loop, flip this to `true` here AND in the
    /// helper at the same time.
    #[test]
    fn core_service_spec_keep_alive_is_false_for_now() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(!spec.keep_alive);
    }

    /// stdout / stderr are appended to predictable filenames under
    /// `log_dir`, so an operator can `tail -F` them without guessing.
    #[test]
    fn core_service_spec_emits_log_paths_under_log_dir() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/var/log/hhagent"),
        );
        assert_eq!(
            spec.stdout_log,
            Some(PathBuf::from("/var/log/hhagent/hhagent-core.out"))
        );
        assert_eq!(
            spec.stderr_log,
            Some(PathBuf::from("/var/log/hhagent/hhagent-core.err"))
        );
    }

    /// Distinct destinations so an operator can separate normal logs
    /// from error spam.
    #[test]
    fn core_service_spec_log_paths_are_distinct() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert_ne!(spec.stdout_log, spec.stderr_log);
    }
}
