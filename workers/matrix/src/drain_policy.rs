//! Pure policy for `matrix.drain` (#835): what a stop of the background sync
//! cost, and the order the drain stops and counts in. Not feature-gated, so
//! its unit tests run in the default build; the `live-matrix` SDK glue that
//! uses it (`sdk_live::sync_loop`, `LiveSdk::drain`) is gated — cf.
//! [`crate::sync_retry`].

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

use kastellan_matrix_wire::{DrainResult, Event};

/// The `next_batch` of the last sync response whose event handlers all ran —
/// the newest position nothing was lost before. Seeded with the initial
/// sync's `next_batch`, so there always is one: a store that cannot be read
/// can never compare equal to "nothing handled yet" and pass as nothing lost.
#[derive(Debug)]
pub struct HandledToken(Mutex<String>);

impl HandledToken {
    /// Seed with the initial sync's `next_batch`.
    pub fn new(initial: String) -> Self {
        Self(Mutex::new(initial))
    }

    /// Record `next_batch` as handled. Called after a response's handlers ran.
    pub fn record(&self, next_batch: String) {
        *self.0.lock().expect("handled-token lock not poisoned") = next_batch;
    }

    /// The last token recorded.
    pub fn get(&self) -> String {
        self.0.lock().expect("handled-token lock not poisoned").clone()
    }
}

/// Did the stop cut a sync response short? True when the token the store
/// holds is not the last one whose handlers all ran — the store moved past
/// events the queue may never have received. A store that could not be read
/// (`stored` is `None`) also reads as interrupted: unknown is said, never
/// assumed away.
pub fn sync_response_interrupted(stored: Option<&str>, handled: &str) -> bool {
    stored != Some(handled)
}

/// Stop, **then** count and discard what `buffer` still holds. `stop`
/// resolves to whether the stop may have cut a response short (or could not
/// be confirmed); once it has resolved, no handler can push another event
/// behind the count — and if it could not be confirmed, the answer says
/// `interrupted`, which covers a late push.
pub async fn stop_then_count(
    stop: impl Future<Output = bool>,
    buffer: &Mutex<VecDeque<Event>>,
) -> DrainResult {
    let interrupted = stop.await;
    let discarded = buffer.lock().expect("inbound buffer not poisoned").drain(..).count();
    DrainResult { discarded, interrupted }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> Event {
        Event { conversation: "!room:srv".into(), peer: "@me:srv".into(), body: "hi".into() }
    }

    #[test]
    fn a_store_that_holds_the_last_handled_token_lost_nothing() {
        assert!(!sync_response_interrupted(Some("s2"), "s2"));
    }

    #[test]
    fn a_store_past_the_last_handled_token_cut_a_response_short() {
        // The SDK saved `s3`, but the handlers of that response never all ran.
        assert!(sync_response_interrupted(Some("s3"), "s2"));
    }

    #[test]
    fn an_unreadable_store_is_said_not_assumed_away() {
        assert!(sync_response_interrupted(None, "s2"));
    }

    #[test]
    fn the_handled_token_keeps_the_newest() {
        let handled = HandledToken::new("s0".into());
        assert_eq!(handled.get(), "s0");
        handled.record("s1".into());
        handled.record("s2".into());
        assert_eq!(handled.get(), "s2");
    }

    /// The order: an event a handler pushes while the stop is still landing
    /// is counted, because the count is taken after the stop resolves.
    #[test]
    fn the_count_is_taken_after_the_stop() {
        let buffer = Mutex::new(VecDeque::from([ev()]));
        let stop = async {
            buffer.lock().unwrap().push_back(ev());
            true
        };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let held = rt.block_on(stop_then_count(stop, &buffer));
        assert_eq!(held, DrainResult { discarded: 2, interrupted: true });
        assert!(buffer.lock().unwrap().is_empty(), "discarded, not just counted");
    }
}
