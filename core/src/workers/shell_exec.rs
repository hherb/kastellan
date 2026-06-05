//! Host-side manifest + `ToolEntry` constructor for the shell-exec worker.

use std::path::PathBuf;

use hhagent_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

/// Tool name the registry keys shell-exec on.
const TOOL_NAME: &str = "shell-exec";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "HHAGENT_SHELL_EXEC_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "hhagent-worker-shell-exec";

/// Build the [`ToolEntry`] for the shell-exec worker. The administrator
/// controls the argv allowlist (sourced from the `tool_allowlists` DB table by
/// the daemon); the LLM-supplied `step.parameters` cannot widen it.
///
/// Defaults: `Net::Deny`, `Profile::WorkerStrict` (no `socket(2)`), `cpu_ms =
/// 5_000`, `mem_mb = 256`, `wall_clock_ms = Some(30_000)`, `SingleUse`.
pub fn shell_exec_entry(binary: PathBuf, allowlist: &[String]) -> ToolEntry {
    let allow_json = serde_json::to_string(allowlist)
        .expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![binary.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
        cpu_quota_pct: None,
        tasks_max: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
}

/// shell-exec's manifest. Discovery: a set `HHAGENT_SHELL_EXEC_BIN` override is
/// authoritative (honoured iff it names a runnable file, else fails closed);
/// only when it is unset do we fall back to the exe-relative sibling
/// `hhagent-worker-shell-exec`. See [`discover_binary`].
pub struct ShellExecManifest;

impl WorkerManifest for ShellExecManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let binary = match discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME) {
            Some(b) => b,
            None => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "could not resolve worker binary: {BIN_ENV} set but not a \
                         runnable file, or unset with no sibling {DEFAULT_BIN_NAME} found"
                    ),
                };
            }
        };
        let allowlist = (ctx.allowlist)(TOOL_NAME);
        Resolution::Register(shell_exec_entry(binary, &allowlist))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir: None,
            allowlist,
        }
    }

    #[test]
    fn resolve_registers_with_byte_identical_policy() {
        let get_env = |k: &str| (k == BIN_ENV).then(|| "/opt/shell-exec".to_string());
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["ls".to_string(), "cat".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match ShellExecManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert_eq!(entry.binary, PathBuf::from("/opt/shell-exec"));
                assert_eq!(entry.policy.fs_read, vec![PathBuf::from("/opt/shell-exec")]);
                assert!(entry.policy.fs_write.is_empty());
                assert_eq!(entry.policy.cpu_ms, 5_000);
                assert_eq!(entry.policy.mem_mb, 256);
                assert_eq!(entry.wall_clock_ms, Some(30_000));
                let (k, v) = &entry.policy.env[0];
                assert_eq!(k, "HHAGENT_SHELL_ALLOWLIST");
                assert_eq!(v, r#"["ls","cat"]"#);
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_misconfigured_when_no_binary_found() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match ShellExecManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("hhagent-worker-shell-exec"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    fn outcome_label(r: &Resolution) -> &'static str {
        match r {
            Resolution::Register(_) => "Register",
            Resolution::Disabled { .. } => "Disabled",
            Resolution::Misconfigured { .. } => "Misconfigured",
        }
    }
}
