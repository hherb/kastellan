//! [`PolicyGate`] — the seam Phase 5 will plug into.
//!
//! Every chat-completion request is asked the same question by the
//! router: *which backend should serve this?* Today that question has
//! a trivial answer — [`DefaultLocalPolicy`] always returns
//! [`Backend::Local`]. The point of the trait is **not** to ship
//! useful policy logic now; it is to make the eventual Phase-5 slice
//! a pure-Rust addition (drop in a new `impl PolicyGate`, hand it to
//! [`crate::Router::with_policy`]) rather than a refactor that
//! retro-threads a decision into already-wired call sites.
//!
//! ## Why a trait and not a closure
//! A `Box<dyn Fn(&ChatRequest) -> Backend>` would carry the same
//! information today, but Phase 5's gate will need to read state
//! (recent escalation count, secrets-keyring availability, the
//! agent's current task) and probably emit traces / audit-log
//! payloads. Trait objects compose with that future state better
//! than a single function pointer; the cost today is one extra
//! `impl PolicyGate for DefaultLocalPolicy` block.
//!
//! ## Why `pick` is sync
//! Today's decision is local computation. A future async policy gate
//! (e.g. one that consults the keyring for a frontier API key) can
//! do its async work upfront in `Router::with_policy`'s constructor
//! and cache the result, or wrap the trait in an `async-trait`-style
//! shim then. Forcing every consumer to `.await` the policy lookup
//! today would buy nothing.

use crate::backend::Backend;
use crate::messages::ChatRequest;

/// Decide which backend serves a request.
///
/// Implementations should be cheap-to-evaluate and **must not** block:
/// the router calls `pick` synchronously on the dispatch path.
pub trait PolicyGate: Send + Sync + std::fmt::Debug {
    fn pick(&self, request: &ChatRequest) -> Backend;
}

/// Phase 0 default: always pick [`Backend::Local`].
///
/// Phase 5 will replace this with a real gate. The struct is
/// deliberately empty so a future `Default::default()` consumer
/// stays valid even after the placeholder is gone — Phase 5's
/// implementation may be `Default`-able too, in which case the
/// rotation is a one-line type swap.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultLocalPolicy;

impl PolicyGate for DefaultLocalPolicy {
    fn pick(&self, _request: &ChatRequest) -> Backend {
        Backend::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{ChatMessage, ChatRequest};

    #[test]
    fn default_local_policy_always_picks_local() {
        let p = DefaultLocalPolicy;
        let req = ChatRequest::new("any-model", vec![ChatMessage::user("hi")]);
        assert_eq!(p.pick(&req), Backend::Local);

        // Variation: long messages, multiple roles. The pin is "no
        // matter what the request looks like, Phase 0's policy stays
        // local". Phase 5's first commit removes this test.
        let req2 = ChatRequest {
            model: "frontier-only-model".into(),
            messages: vec![
                ChatMessage::system("be terse"),
                ChatMessage::user("write me an essay about Goedel"),
            ],
            max_tokens: Some(8192),
            temperature: Some(0.9),
        };
        assert_eq!(p.pick(&req2), Backend::Local);
    }

    #[test]
    fn default_local_policy_is_send_and_sync() {
        // Compile-time pin: `Router::with_policy` will store the gate
        // behind a trait object that crosses tokio task boundaries.
        // If a future implementor accidentally captures a `Rc<_>` this
        // assertion will refuse to compile.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DefaultLocalPolicy>();
    }
}
