//! gliner-relex's host-side [`WorkerManifest`](crate::worker_manifest::WorkerManifest)
//! implementation.
//!
//! [`GlinerRelexManifest`] is the uniform self-description the daemon's
//! [`registry_build`](crate::registry_build) iterates over. It wraps the
//! pure [`resolve_env`](super::resolve::resolve_env) (env →
//! [`GlinerRelexEnv`](super::resolve::GlinerRelexEnv)) +
//! [`gliner_relex_entry`](super::entry::gliner_relex_entry) (env →
//! `ToolEntry`), mapping the resolver's typed skip reasons onto the
//! uniform [`Resolution`](crate::worker_manifest::Resolution) outcomes.

use super::client::Client;
use super::entry::gliner_relex_entry;
use super::resolve::{resolve_env, ResolveSkipReason};

/// gliner-relex's host-side manifest. Wraps the existing pure `resolve_env`
/// (env → `GlinerRelexEnv`) + `gliner_relex_entry` (env → `ToolEntry`),
/// mapping its typed skip reasons onto the uniform [`Resolution`] outcomes.
///
/// [`Resolution`]: crate::worker_manifest::Resolution
pub struct GlinerRelexManifest;

impl crate::worker_manifest::WorkerManifest for GlinerRelexManifest {
    fn name(&self) -> &'static str {
        Client::TOOL_NAME
    }

    // No argv allowlist: gliner-relex is a single stateless inference service,
    // not an argv-dispatch worker. (allowlist_tool defaults to None.)

    fn resolve(
        &self,
        ctx: &crate::worker_manifest::ResolveCtx<'_>,
    ) -> crate::worker_manifest::Resolution {
        use crate::worker_manifest::Resolution;
        match resolve_env(
            |k| (ctx.get_env)(k),
            |p| (ctx.is_dir)(p),
            |p| (ctx.exists)(p),
        ) {
            Ok(mut env) => {
                // Host mode: bind the venv's external interpreter prefix + its
                // out-of-prefix shared-lib dirs (issue #284) so CPython
                // dyld-loads in the jail. Needs `canonicalize` + the otool/ldd
                // dep tool, so it lives here (not in the pure `resolve_env`).
                // Container mode bakes the interpreter into the image — skip.
                if !env.use_container_backend {
                    let (root, dirs) = super::resolve::resolve_host_interpreter_binds(
                        &env.venv_dir,
                        |p| (ctx.exists)(p),
                        |p| (ctx.canonicalize)(p),
                        crate::workers::interpreter_deps::resolve_deps_via_tool,
                    );
                    env.interpreter_root = root;
                    env.interpreter_lib_dirs = dirs;
                }
                // Linux: gliner-relex is a pure-Python venv worker bwrap spawns
                // directly, so it needs the lockdown-exec shim to actually apply
                // its ml_client seccomp filter. Fail-closed if the shim is
                // missing — never register an unfiltered torch worker. macOS uses
                // Seatbelt (applied from the parent), so no shim.
                #[cfg(target_os = "linux")]
                {
                    match crate::worker_manifest::discover_binary(
                        ctx,
                        "KASTELLAN_LOCKDOWN_EXEC_BIN",
                        "kastellan-worker-lockdown-exec",
                    ) {
                        Some(shim) => {
                            Resolution::Register(gliner_relex_entry(&env, Some(shim)))
                        }
                        None => Resolution::Misconfigured {
                            detail: "lockdown-exec shim not found (KASTELLAN_LOCKDOWN_EXEC_BIN unset/invalid and no exe-relative sibling); gliner-relex requires it for worker-side seccomp on Linux".to_string(),
                        },
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Resolution::Register(gliner_relex_entry(&env, None))
                }
            }
            Err(ResolveSkipReason::Disabled) => Resolution::Disabled {
                detail: "KASTELLAN_GLINER_RELEX_ENABLE != \"1\"".to_string(),
            },
            Err(other) => Resolution::Misconfigured {
                detail: gliner_skip_detail(&other),
            },
        }
    }
}

/// Human-readable detail for a non-`Disabled` skip reason. Mirrors the
/// messages the deleted `registry_build::log_gliner_relex_skip` emitted, so
/// the operator log wording is unchanged.
fn gliner_skip_detail(reason: &ResolveSkipReason) -> String {
    match reason {
        ResolveSkipReason::Disabled => {
            // Handled by the Disabled arm above; included for exhaustiveness.
            "KASTELLAN_GLINER_RELEX_ENABLE != \"1\"".to_string()
        }
        ResolveSkipReason::WeightsDirEnvMissing => {
            "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR unset".to_string()
        }
        ResolveSkipReason::WeightsDirNotADir { path } => {
            format!("weights dir missing on disk: {}", path.display())
        }
        ResolveSkipReason::VenvDirUnresolvable => {
            "venv dir unresolvable (KASTELLAN_GLINER_RELEX_VENV_DIR, \
             KASTELLAN_DATA_DIR, and HOME all unset)"
                .to_string()
        }
        ResolveSkipReason::ScriptShimMissing { path } => {
            format!("venv shim missing: {}", path.display())
        }
    }
}
