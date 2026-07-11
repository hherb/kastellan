//! The broker kind: which trusted sidecar a worker declares. One worker binds at
//! most one broker socket, so this is a plain enum, not a bitset. Each variant is
//! the single source of truth for that broker's binary name and its env / socket /
//! scratch naming contracts — `BrokerKind::Embed` reproduces every string the
//! merged embed-broker used, so web-research is byte-for-byte unaffected.

/// A trusted broker sidecar kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BrokerKind {
    /// Embedding broker: `embed{model,input}` → OpenAI-compatible backend. web-research.
    Embed,
    /// Search broker: `search{query,count}` → SearxNG backend. web-search.
    Search,
}

impl BrokerKind {
    /// Exe-relative default binary name (used when the `*_BIN` override is unset).
    pub const fn broker_bin_default(self) -> &'static str {
        match self {
            BrokerKind::Embed => "kastellan-worker-embed-broker",
            BrokerKind::Search => "kastellan-worker-search-broker",
        }
    }
    /// Operator override env for the broker binary path.
    pub const fn bin_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_BIN",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_BIN",
        }
    }
    /// Override env for this kind's per-worker scratch root.
    pub const fn scratch_dir_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_SCRATCH_DIR",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_SCRATCH_DIR",
        }
    }
    /// Env the *broker binary* reads for the backend URL it forwards to.
    pub const fn endpoint_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_ENDPOINT",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_ENDPOINT",
        }
    }
    /// Env core injects into the *worker* carrying the bound UDS path.
    pub const fn uds_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_UDS",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_UDS",
        }
    }
    /// Basename of the broker's UDS under its scratch dir.
    pub const fn uds_file(self) -> &'static str {
        match self {
            BrokerKind::Embed => "embed.sock",
            BrokerKind::Search => "search.sock",
        }
    }
    /// Scratch-subdir name prefix (`<prefix><pid>-<seq>`), matched by the #251 sweep.
    pub const fn scratch_prefix(self) -> &'static str {
        match self {
            BrokerKind::Embed => "embed-",
            BrokerKind::Search => "search-",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_constants_are_byte_identical_to_the_merged_broker() {
        // Web-research relies on these exact strings; a change is a silent break.
        assert_eq!(BrokerKind::Embed.broker_bin_default(), "kastellan-worker-embed-broker");
        assert_eq!(BrokerKind::Embed.bin_env(), "KASTELLAN_EMBED_BROKER_BIN");
        assert_eq!(BrokerKind::Embed.scratch_dir_env(), "KASTELLAN_EMBED_BROKER_SCRATCH_DIR");
        assert_eq!(BrokerKind::Embed.endpoint_env(), "KASTELLAN_EMBED_BROKER_ENDPOINT");
        assert_eq!(BrokerKind::Embed.uds_env(), "KASTELLAN_EMBED_BROKER_UDS");
        assert_eq!(BrokerKind::Embed.uds_file(), "embed.sock");
        assert_eq!(BrokerKind::Embed.scratch_prefix(), "embed-");
    }

    #[test]
    fn search_constants_are_distinct_and_well_formed() {
        assert_eq!(BrokerKind::Search.broker_bin_default(), "kastellan-worker-search-broker");
        assert_eq!(BrokerKind::Search.uds_env(), "KASTELLAN_SEARCH_BROKER_UDS");
        assert_eq!(BrokerKind::Search.uds_file(), "search.sock");
        assert_eq!(BrokerKind::Search.scratch_prefix(), "search-");
        // No shared strings between the two kinds (a copy-paste slip would collide).
        assert_ne!(BrokerKind::Embed.uds_env(), BrokerKind::Search.uds_env());
        assert_ne!(BrokerKind::Embed.uds_file(), BrokerKind::Search.uds_file());
    }
}
