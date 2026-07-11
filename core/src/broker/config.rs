//! Daemon-level broker config: the discovered broker binary + the scratch root
//! under which each per-worker broker sidecar gets its UDS dir. Analogous to
//! [`crate::worker_lifecycle::force_route::ForceRoutingConfig`], but with no
//! daemon-level enable gate — the *manifest* opts a worker in, so the daemon
//! always tries to discover each kind's broker binary and holds a config iff it
//! is found.
//!
//! **Fail-closed semantics live at the spawn chokepoint, not here:** if a worker
//! carries `broker: Some(spec)` but the daemon has no matching `BrokerConfig`
//! (that kind's binary absent), the chokepoint refuses to spawn rather than
//! silently falling back to direct egress — the manifest already dropped the
//! backend host from the allowlist, so a silent fallback would break the worker's
//! backend route entirely.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::kind::BrokerKind;
use crate::worker_manifest::{discover_binary, ResolveCtx};

/// Everything core needs to spawn one broker sidecar of a given kind. Built once
/// at daemon startup (iff the kind's binary resolves) and shared behind an `Arc`.
pub struct BrokerConfig {
    /// Which broker kind this config spawns — supplies every per-kind string.
    pub(crate) kind: BrokerKind,
    /// Resolved, runnable path to this kind's broker binary.
    pub(crate) broker_bin: PathBuf,
    /// Directory under which each broker-backed worker gets a unique scratch
    /// subdir (`<prefix><pid>-<seq>`) holding the broker's UDS. Created per spawn,
    /// removed on teardown by [`super::spawn::BrokerSidecar`]'s `Drop`.
    pub(crate) scratch_root: PathBuf,
}

impl BrokerConfig {
    /// Bare constructor shared by [`from_env`] and the tests.
    pub fn new(kind: BrokerKind, broker_bin: PathBuf, scratch_root: PathBuf) -> Self {
        Self { kind, broker_bin, scratch_root }
    }
}

/// Daemon-level registry: one config slot per broker kind. A `None` slot means
/// that kind's binary was not discovered — a worker declaring it then fails
/// closed at the spawn chokepoint. Cheap to clone (two `Option<Arc<_>>`).
#[derive(Default, Clone)]
pub struct BrokerConfigs {
    pub embed: Option<Arc<BrokerConfig>>,
    pub search: Option<Arc<BrokerConfig>>,
}

impl BrokerConfigs {
    /// The config slot for `kind`, or `None` when that kind's binary was not
    /// discovered. The spawn chokepoint maps `None` to a fail-closed refusal.
    pub fn for_kind(&self, kind: BrokerKind) -> Option<&Arc<BrokerConfig>> {
        match kind {
            BrokerKind::Embed => self.embed.as_ref(),
            BrokerKind::Search => self.search.as_ref(),
        }
    }
}

/// Discover one kind's broker config from the environment. The `*_BIN` override
/// wins (fail-closed if set-but-invalid), else the exe-relative sibling default.
/// Scratch root defaults to the egress root so the #251 sweep reclaims leaks.
///
/// Returns `Some(Arc<config>)` when the binary resolves and `None` when absent.
/// `None` is not an error here — a deployment that never opts a worker into that
/// kind's broker mode simply never needs the binary. The fail-closed check (a
/// broker-wanting worker with no config) happens at the spawn chokepoint.
pub fn from_env(kind: BrokerKind, exe_dir: Option<&Path>) -> Option<Arc<BrokerConfig>> {
    let broker_bin = discover_broker_bin(kind, exe_dir)?;
    let scratch_root = std::env::var_os(kind.scratch_dir_env())
        .map(PathBuf::from)
        // Share the egress default so the force-routing sweep covers broker dirs.
        .unwrap_or_else(crate::worker_lifecycle::force_route::default_egress_scratch_root);
    Some(Arc::new(BrokerConfig::new(kind, broker_bin, scratch_root)))
}

/// Resolve a kind's broker binary the same way plain workers are found: the
/// `kind.bin_env()` override wins (fail-closed if set-but-invalid), else the
/// exe-relative sibling `kind.broker_bin_default()`. Like the egress proxy, the
/// broker is never registered as a callable tool — only spawned as a sidecar.
fn discover_broker_bin(kind: BrokerKind, exe_dir: Option<&Path>) -> Option<PathBuf> {
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &Path| p.exists();
    let is_dir = |p: &Path| p.is_dir();
    discover_broker_bin_with(kind, &get_env, &exists, &is_dir, exe_dir)
}

/// Dependency-injected core of [`discover_broker_bin`] (mirrors
/// `force_route::discover_egress_proxy_bin_with`): the env + path probes arrive as
/// closures so the discovery semantics are unit-testable without touching the
/// process environment or filesystem.
fn discover_broker_bin_with(
    kind: BrokerKind,
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
    discover_binary(&ctx, kind.bin_env(), kind.broker_bin_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_prefers_env_override_when_runnable() {
        let get_env =
            |k: &str| (k == BrokerKind::Embed.bin_env()).then(|| "/opt/broker".to_string());
        let exists = |_p: &Path| true;
        let is_dir = |_p: &Path| false;
        let got = discover_broker_bin_with(BrokerKind::Embed, &get_env, &exists, &is_dir, None);
        assert_eq!(got, Some(PathBuf::from("/opt/broker")));
    }

    #[test]
    fn discover_falls_back_to_exe_sibling() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| true;
        let is_dir = |_p: &Path| false;
        let exe_dir = PathBuf::from("/usr/local/lib/kastellan");
        let got = discover_broker_bin_with(
            BrokerKind::Embed,
            &get_env,
            &exists,
            &is_dir,
            Some(&exe_dir),
        );
        assert_eq!(got, Some(exe_dir.join(BrokerKind::Embed.broker_bin_default())));
    }

    #[test]
    fn discover_none_when_absent() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let is_dir = |_p: &Path| false;
        assert_eq!(
            discover_broker_bin_with(BrokerKind::Embed, &get_env, &exists, &is_dir, None),
            None
        );
    }
}
