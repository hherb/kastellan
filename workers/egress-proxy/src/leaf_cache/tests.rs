use super::{LeafCache, MAX_CACHED_LEAVES};
use crate::ca::generate_ca;

/// rustls' `ServerConfig::builder()` needs a process-default CryptoProvider.
/// Production installs it in `main`; unit tests must install it themselves
/// (idempotent — only the first call in the test binary wins, rest are no-ops).
fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[test]
fn same_host_returns_a_cached_arc() {
    install_provider();
    let ca = generate_ca().unwrap();
    let mut cache = LeafCache::new();
    let a = cache.get_or_issue(&ca, "api.example.com").expect("issue");
    let b = cache.get_or_issue(&ca, "api.example.com").expect("cached");
    assert!(std::sync::Arc::ptr_eq(&a, &b), "same host must reuse the Arc");
}

#[test]
fn distinct_hosts_get_distinct_leaves() {
    install_provider();
    let ca = generate_ca().unwrap();
    let mut cache = LeafCache::new();
    let a = cache.get_or_issue(&ca, "a.example.com").unwrap();
    let b = cache.get_or_issue(&ca, "b.example.com").unwrap();
    assert!(!std::sync::Arc::ptr_eq(&a, &b));
    assert_eq!(cache.len(), 2);
}

#[test]
fn cache_is_bounded() {
    install_provider();
    let ca = generate_ca().unwrap();
    let mut cache = LeafCache::new();
    for i in 0..(MAX_CACHED_LEAVES + 10) {
        cache.get_or_issue(&ca, &format!("h{i}.example.com")).unwrap();
    }
    assert!(cache.len() <= MAX_CACHED_LEAVES, "cache must not grow unbounded");
}
