//! The worker's continuous background sync, and stopping it for
//! `matrix.drain` (#835) while telling whether the stop cost an uncounted sync
//! response. Split out of `mod.rs` when #835 grew it past the 500-LOC soft cap
//! (the loop moved verbatim, as a function).
//!
//! ## Why the stop can lose events the count cannot see
//!
//! matrix-sdk 0.18 handles a sync response in this order (`Client::process_sync`):
//! it **saves the new sync token** to the state store, *then* runs the event
//! handlers, one of which (`room_gate::register_message_handler`) pushes each
//! accepted text event onto the inbound queue. Aborting the sync task between
//! those two steps leaves a token that says "handled" for events that never
//! reached the queue: the homeserver will not send them again, and they are
//! not in the queue to be counted.
//!
//! So the sync loop records the `next_batch` of every response whose handlers
//! all ran ([`record_handled`], from `sync_with_callback`'s callback, which runs
//! after them). After the stop, the token the store holds is compared with it
//! ([`sync_response_interrupted`]): if the store moved further, one response
//! was cut short and the drain says so. A stop that lands after the handlers
//! but before the callback reads the same way — an over-report, said as
//! "possibly none", which is the honest direction.
//!
//! ## Why the comparison needs a write first
//!
//! Aborting a task does not stop work it handed to a blocking thread: an
//! in-flight token save runs to commit regardless, and a store *read* goes
//! through a separate connection that would not wait for it. matrix-sdk-sqlite
//! runs every write on one connection whose blocking closures serialise on a
//! mutex (deadpool-sync's `interact`), so [`stop_sync`] first saves an empty
//! [`StateChanges`]: it commits nothing, and it cannot start until any save the
//! abort left running has committed. ⚠️ That is an implementation detail of
//! matrix-sdk-sqlite 0.18 — re-check it on an upgrade.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use matrix_sdk::config::SyncSettings;
use matrix_sdk::{Client, LoopCtrl, StateChanges};
use tokio::runtime::Runtime;

use crate::sync_retry;

/// A `sync()` that ran at least this long before returning was healthy and
/// serving; its return is a transient blip, so the consecutive-failure counter
/// resets (see [`crate::sync_retry`]).
const SYNC_HEALTHY_RUN: Duration = Duration::from_secs(60);
/// Base + cap for the continuous-sync retry backoff (mirrors the core-side
/// `MatrixChannel` respawn backoff).
const SYNC_BACKOFF_BASE: Duration = Duration::from_secs(1);
const SYNC_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Consecutive *fast* `sync()` failures the loop tolerates before giving up and
/// exiting for a fresh supervised respawn — a persistently-wedged client (bad
/// token, corrupt store) only a fresh `connect` recovers, so retrying in place
/// forever would never heal it.
const SYNC_MAX_CONSECUTIVE: u32 = 10;

/// Spawn the continuous sync on `runtime`, recording each fully-handled
/// response's `next_batch` into `handled`. Returns its task, which
/// [`stop_sync`] stops.
pub(super) fn spawn_sync_loop(
    runtime: &Runtime,
    client: Client,
    handled: Arc<HandledToken>,
) -> tokio::task::JoinHandle<()> {
    // Continuous background sync: keeps the buffer filled for `poll` and the
    // crypto state fresh for `send`. `client.sync` returns whenever the SDK
    // hits an interruption — a transient server 5xx, a network blip through the
    // egress tunnel, a long-poll hiccup — none of which is necessarily fatal.
    //
    // Treating *every* return as fatal (the original `process::exit(1)`) meant a
    // single transient blip killed the whole worker and forced a supervised
    // respawn — the ~20–90s churn of #348. Instead we **retry in place** with
    // capped exponential backoff and only fail loud (exit → fresh respawn) after
    // sustained consecutive fast failures, since a persistently-wedged client
    // (bad token, corrupt store) only a fresh `connect` recovers (see
    // [`crate::sync_retry`]). The task is aborted before its body re-runs when
    // the client is dropped on shutdown, so the `process::exit` give-up path is
    // reached only on a real failure run; it skips destructors deliberately —
    // the deadpool SQLite teardown that needs a runtime context (see `LiveSdk`'s `Drop`
    // impl) never runs off-runtime; the OS reclaims everything.
    //
    // `sync_with_callback` with `Continue` is exactly what `sync` runs; the
    // callback runs after each response's event handlers, so it records the
    // newest position nothing was lost before (#835, below).
    runtime.spawn(async move {
        let mut consecutive = 0u32;
        loop {
            let started = Instant::now();
            let handled = handled.clone();
            let result = client
                .sync_with_callback(SyncSettings::default(), move |response| {
                    record_handled(&handled, response.next_batch);
                    async { LoopCtrl::Continue }
                })
                .await;
            let ran_for = started.elapsed();
            match &result {
                Ok(()) => eprintln!(
                    "kastellan-worker-matrix: sync loop returned (no error) after {ran_for:?}; retrying"
                ),
                Err(e) => eprintln!(
                    "kastellan-worker-matrix: sync loop failed after {ran_for:?}: {e}; retrying"
                ),
            }
            consecutive =
                sync_retry::update_consecutive(consecutive, ran_for, SYNC_HEALTHY_RUN);
            match sync_retry::next_action(
                consecutive,
                SYNC_MAX_CONSECUTIVE,
                SYNC_BACKOFF_BASE,
                SYNC_BACKOFF_MAX,
            ) {
                sync_retry::SyncOutcome::Backoff(delay) => {
                    tokio::time::sleep(delay).await;
                }
                sync_retry::SyncOutcome::GiveUp => {
                    eprintln!(
                        "kastellan-worker-matrix: sync loop failed {consecutive} times \
                         consecutively; exiting for a fresh supervised respawn"
                    );
                    std::process::exit(1);
                }
            }
        }
    })
}

/// The `next_batch` of the last sync response whose event handlers all ran —
/// the newest position nothing was lost before. `None` until the first one.
pub(super) type HandledToken = Mutex<Option<String>>;

/// How long [`stop_sync`] waits for the stop to land and the store to answer.
/// An abort lands at the task's next await point, and the barrier write and
/// token read are local SQLite calls, so this is generous; it bounds the
/// driver's exit (and with it the daemon's shutdown) against a wedged store.
const STOP_BOUND: Duration = Duration::from_secs(2);

/// Record `next_batch` as handled. Called after a response's handlers ran.
pub(super) fn record_handled(handled: &HandledToken, next_batch: String) {
    *handled.lock().expect("handled-token lock not poisoned") = Some(next_batch);
}

/// Pure: did the stop cut a sync response short? True when the token the
/// store holds is not the last one whose handlers all ran — the store moved
/// past events the queue may never have received. A store that could not be
/// read (`stored` is `None` while something was handled) also reads as
/// interrupted: unknown is said, never assumed away.
pub(super) fn sync_response_interrupted(stored: Option<&str>, handled: Option<&str>) -> bool {
    stored != handled
}

/// Stop the background sync and say whether the stop cut a sync response
/// short (see the module docs). `task` is `None` when an earlier drain already
/// stopped it; the comparison is still made, and still holds.
///
/// Returns `true` — "possibly lost" — whenever the answer cannot be had within
/// [`STOP_BOUND`]: the task did not finish, or the store did not answer.
pub(super) async fn stop_sync(
    task: Option<tokio::task::JoinHandle<()>>,
    client: &Client,
    handled: &HandledToken,
) -> bool {
    let stop = async {
        if let Some(task) = task {
            task.abort();
            // `Err(JoinError::Cancelled)` is the expected result; either way
            // the task is no longer running when this returns.
            let _ = task.await;
        }
        // The barrier: see the module docs. A failed write proves nothing
        // about the store's token, so it reads as interrupted.
        if client.state_store().save_changes(&StateChanges::default()).await.is_err() {
            return true;
        }
        let stored = super::read_prior_sync_token(client).await;
        let handled = handled.lock().expect("handled-token lock not poisoned").clone();
        sync_response_interrupted(stored.as_deref(), handled.as_deref())
    };
    tokio::time::timeout(STOP_BOUND, stop).await.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_that_holds_the_last_handled_token_lost_nothing() {
        assert!(!sync_response_interrupted(Some("s2"), Some("s2")));
    }

    #[test]
    fn a_store_past_the_last_handled_token_cut_a_response_short() {
        // The SDK saved `s3`, but the handlers of that response never all ran.
        assert!(sync_response_interrupted(Some("s3"), Some("s2")));
    }

    #[test]
    fn an_unreadable_store_is_said_not_assumed_away() {
        assert!(sync_response_interrupted(None, Some("s2")));
    }

    #[test]
    fn record_handled_keeps_the_newest_token() {
        let handled = HandledToken::default();
        record_handled(&handled, "s1".into());
        record_handled(&handled, "s2".into());
        assert_eq!(handled.lock().unwrap().as_deref(), Some("s2"));
    }
}
