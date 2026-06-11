//! Bounded per-host cache of prebuilt rustls server configs (one CA-signed leaf
//! each). Building a leaf does a keygen + signature, so we cache by host for the
//! life of the (short-lived, SingleUse) proxy. Bounded so a worker that connects
//! to many distinct hosts can't grow the map without limit; on overflow we clear
//! (the simplest bound — re-issue is cheap and the proxy is ephemeral).

use std::collections::HashMap;
use std::sync::Arc;

use rustls::ServerConfig;

use crate::ca::{issue_leaf, CaMaterial};

/// Upper bound on distinct host leaves held at once.
pub const MAX_CACHED_LEAVES: usize = 256;

/// Host → prebuilt server config (CA-signed leaf for that host).
pub struct LeafCache {
    map: HashMap<String, Arc<ServerConfig>>,
}

impl LeafCache {
    pub fn new() -> Self {
        Self { map: HashMap::new() }
    }

    /// Number of distinct host leaves currently cached. Used by the cache's
    /// own unit tests to assert reuse + the bound; the proxy never needs it.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Return the server config for `host`, issuing + caching it on first use.
    /// Clears the cache first if it is at the bound (cheap re-issue afterwards).
    pub fn get_or_issue(
        &mut self,
        ca: &CaMaterial,
        host: &str,
    ) -> Result<Arc<ServerConfig>, String> {
        if let Some(cfg) = self.map.get(host) {
            return Ok(Arc::clone(cfg));
        }
        if self.map.len() >= MAX_CACHED_LEAVES {
            self.map.clear();
        }
        let leaf = issue_leaf(ca, host).map_err(|e| format!("issue leaf for {host}: {e}"))?;
        let (chain, key) = leaf.into_rustls();
        let cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| format!("build server config for {host}: {e}"))?;
        let cfg = Arc::new(cfg);
        self.map.insert(host.to_string(), Arc::clone(&cfg));
        Ok(cfg)
    }
}

impl Default for LeafCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
