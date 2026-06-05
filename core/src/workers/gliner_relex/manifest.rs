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
            Ok(env) => Resolution::Register(gliner_relex_entry(&env)),
            Err(ResolveSkipReason::Disabled) => Resolution::Disabled {
                detail: "HHAGENT_GLINER_RELEX_ENABLE != \"1\"".to_string(),
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
            "HHAGENT_GLINER_RELEX_ENABLE != \"1\"".to_string()
        }
        ResolveSkipReason::WeightsDirEnvMissing => {
            "HHAGENT_GLINER_RELEX_WEIGHTS_DIR unset".to_string()
        }
        ResolveSkipReason::WeightsDirNotADir { path } => {
            format!("weights dir missing on disk: {}", path.display())
        }
        ResolveSkipReason::VenvDirUnresolvable => {
            "venv dir unresolvable (HHAGENT_GLINER_RELEX_VENV_DIR, \
             HHAGENT_DATA_DIR, and HOME all unset)"
                .to_string()
        }
        ResolveSkipReason::ScriptShimMissing { path } => {
            format!("venv shim missing: {}", path.display())
        }
    }
}
