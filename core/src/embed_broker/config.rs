//! Daemon-level embed-broker config: the discovered broker binary + the scratch
//! root under which each per-worker broker sidecar gets its UDS dir. Analogous to
//! [`crate::worker_lifecycle::force_route::ForceRoutingConfig`], but with no
//! daemon-level enable gate — the *manifest* opts a worker in
//! (`KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER`), so the daemon always tries to
//! discover the broker binary and holds a config iff it is found.
//!
//! **Fail-closed semantics live at the spawn chokepoint, not here:** if a worker
//! carries `embed_broker: Some(..)` but the daemon has no `EmbedBrokerConfig`
//! (binary absent), the chokepoint refuses to spawn rather than silently falling
//! back to direct egress — the manifest already dropped the embed host from the
//! allowlist, so a silent fallback would break hybrid ranking *and* leave the
//! worker with no embed route at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::worker_manifest::{discover_binary, ResolveCtx};

/// Override env var for the embed-broker binary path (mirrors `KASTELLAN_*_BIN`).
const ENV_BROKER_BIN: &str = "KASTELLAN_EMBED_BROKER_BIN";
/// Default sibling name of the embed-broker binary (exe-relative discovery).
const BROKER_BIN_DEFAULT: &str = "kastellan-worker-embed-broker";
/// Optional override for the per-worker broker sidecar scratch root. Defaults to
/// the *egress* scratch root so the existing force-routing startup sweep (#251)
/// reclaims leaked `embed-<pid>-<seq>` dirs for free (same root, same prefix
/// list — see [`crate::egress::scratch_sweep`]).
const ENV_SCRATCH_DIR: &str = "KASTELLAN_EMBED_BROKER_SCRATCH_DIR";

/// Everything core needs to spawn a per-worker embed-broker sidecar. Built once
/// at daemon startup (iff the broker binary resolves) and shared behind an `Arc`
/// across the lifecycle managers, exactly like `ForceRoutingConfig`.
pub struct EmbedBrokerConfig {
    /// Resolved, runnable path to the `kastellan-worker-embed-broker` binary.
    pub(crate) broker_bin: PathBuf,
    /// Directory under which each broker-backed worker gets a unique scratch
    /// subdir (`embed-<pid>-<seq>`) holding the broker's `embed.sock`. Created per
    /// spawn, removed on teardown by [`super::spawn::EmbedBrokerSidecar`]'s `Drop`.
    pub(crate) scratch_root: PathBuf,
}

impl EmbedBrokerConfig {
    /// Bare constructor shared by [`from_env`] and the tests.
    pub fn new(broker_bin: PathBuf, scratch_root: PathBuf) -> Self {
        Self { broker_bin, scratch_root }
    }
}

/// Build the daemon's embed-broker config from the process environment.
///
/// Discovers the broker binary (override [`ENV_BROKER_BIN`], else the exe-relative
/// sibling [`BROKER_BIN_DEFAULT`]); returns `Some(Arc<config>)` when found and
/// `None` when absent. `None` is not an error here — a deployment that never opts
/// a worker into broker mode simply never needs the binary. The fail-closed check
/// (a broker-wanting worker with no config) happens at the spawn chokepoint.
pub fn from_env(exe_dir: Option<&Path>) -> Option<Arc<EmbedBrokerConfig>> {
    let broker_bin = discover_broker_bin(exe_dir)?;
    let scratch_root = std::env::var_os(ENV_SCRATCH_DIR)
        .map(PathBuf::from)
        // Share the egress default so the force-routing sweep covers embed- dirs.
        .unwrap_or_else(crate::worker_lifecycle::force_route::default_egress_scratch_root);
    Some(Arc::new(EmbedBrokerConfig::new(broker_bin, scratch_root)))
}

/// Resolve the embed-broker binary the same way plain workers are found: the
/// [`ENV_BROKER_BIN`] override wins (fail-closed if set-but-invalid), else the
/// exe-relative sibling [`BROKER_BIN_DEFAULT`]. Like the egress proxy, the broker
/// is never registered as a callable tool — only spawned as a sidecar.
fn discover_broker_bin(exe_dir: Option<&Path>) -> Option<PathBuf> {
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &Path| p.exists();
    let is_dir = |p: &Path| p.is_dir();
    discover_broker_bin_with(&get_env, &exists, &is_dir, exe_dir)
}

/// Dependency-injected core of [`discover_broker_bin`] (mirrors
/// `force_route::discover_egress_proxy_bin_with`): the env + path probes arrive as
/// closures so the discovery semantics are unit-testable without touching the
/// process environment or filesystem.
fn discover_broker_bin_with(
    get_env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
    is_dir: &dyn Fn(&Path) -> bool,
    exe_dir: Option<&Path>,
) -> Option<PathBuf> {
    let allowlist = |_t: &str| Vec::new();
    let ctx = ResolveCtx {
        get_env,
        exists,
        is_dir,
        exe_dir,
        canonicalize: &|_p| None,
        allowlist: &allowlist,
    };
    discover_binary(&ctx, ENV_BROKER_BIN, BROKER_BIN_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_prefers_env_override_when_runnable() {
        let get_env = |k: &str| (k == ENV_BROKER_BIN).then(|| "/opt/broker".to_string());
        let exists = |_p: &Path| true;
        let is_dir = |_p: &Path| false;
        let got = discover_broker_bin_with(&get_env, &exists, &is_dir, None);
        assert_eq!(got, Some(PathBuf::from("/opt/broker")));
    }

    #[test]
    fn discover_falls_back_to_exe_sibling() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| true;
        let is_dir = |_p: &Path| false;
        let exe_dir = PathBuf::from("/usr/local/lib/kastellan");
        let got = discover_broker_bin_with(&get_env, &exists, &is_dir, Some(&exe_dir));
        assert_eq!(got, Some(exe_dir.join(BROKER_BIN_DEFAULT)));
    }

    #[test]
    fn discover_none_when_absent() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let is_dir = |_p: &Path| false;
        assert_eq!(discover_broker_bin_with(&get_env, &exists, &is_dir, None), None);
    }
}
