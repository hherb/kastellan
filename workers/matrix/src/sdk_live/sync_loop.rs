//! The worker's continuous background sync, and stopping it for
//! `matrix.drain` (#835) while telling whether the stop cost an uncounted sync
//! response. Split out of `mod.rs` when #835 grew it past the 500-LOC soft cap.
//! The retry loop moved as a function; its one change is `sync` →
//! `sync_with_callback`, so each fully handled response's token is recorded.
//! The pure pieces (the handled token, the comparison, the stop-then-count
//! order) are in [`crate::drain_policy`], ungated, so their tests run in the
//! default build.
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
//! all ran ([`HandledToken::record`], from `sync_with_callback`'s callback,
//! which runs after them). After the stop, the token the store holds is
//! compared with it ([`sync_response_interrupted`]): if the store moved further, one response
//! was cut short and the drain says so. A stop that lands after the handlers
//! but before the callback reads the same way — and that gap includes
//! `sync_once`'s trailing `send_outgoing_requests`, a network call — an
//! over-report, said as "possibly none", which is the honest direction.
//!
//! ## Why the comparison needs a write first
//!
//! Aborting a task does not stop work it handed to a blocking thread: an
//! in-flight token save runs to commit regardless, and a store *read* goes
//! through a separate connection that would not wait for it. matrix-sdk-sqlite
//! runs every write on one connection whose blocking closures serialise on a
//! `std::sync::Mutex` (deadpool-sync's `interact`), so [`stop_sync`] first
//! saves an empty [`StateChanges`] — it commits nothing, and writes no token —
//! which cannot run while a save holds that lock.
//!
//! ⚠️ **Mutual exclusion, not a queue.** The abort drops the save's tokio-level
//! write guard, and a std mutex is not fair: a save closure submitted just
//! before the abort, but not yet holding the lock, could lose the race to the
//! barrier, which would then read the old token — a miss, the worst outcome.
//! So the barrier runs [`BARRIERS`] times, [`BARRIER_GAP`] apart: a save
//! submitted before the abort would have to stay unscheduled across all of
//! them. That narrows the window to a stalled blocking pool; it does not close
//! it, and the SDK offers no hook that would. ⚠️ All of this is
//! matrix-sdk-sqlite 0.18's implementation — re-check it on an upgrade.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kastellan_matrix_wire::DRAIN_STOP_BOUND_MS;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::store::StateStoreDataKey;
use matrix_sdk::{Client, LoopCtrl, StateChanges};
use tokio::runtime::Runtime;

use crate::drain_policy::{sync_response_interrupted, HandledToken};
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
    // newest position nothing was lost before (#835, module docs).
    runtime.spawn(async move {
        let mut consecutive = 0u32;
        loop {
            let started = Instant::now();
            let handled = handled.clone();
            let result = client
                .sync_with_callback(SyncSettings::default(), move |response| {
                    handled.record(response.next_batch);
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

/// How long [`stop_sync`] waits for the stop to land and the store to answer.
/// An abort lands at the task's next await point, and the barrier writes and
/// token read are local SQLite calls, so this is generous; it bounds the
/// driver's exit (and with it the daemon's shutdown, which the core asserts
/// against) against a wedged store.
const STOP_BOUND: Duration = Duration::from_millis(DRAIN_STOP_BOUND_MS);

/// How many empty barrier writes [`stop_sync`] makes, and the pause before
/// each after the first (module docs: the lock is not a queue).
const BARRIERS: usize = 2;
const BARRIER_GAP: Duration = Duration::from_millis(20);

/// Stop the background sync and say whether the stop cut a sync response
/// short (see the module docs). `task` is `None` when an earlier drain already
/// stopped it; the comparison is still made, and still holds.
///
/// Returns `true` — "possibly lost" — whenever the answer cannot be had: the
/// stop was not confirmed within [`STOP_BOUND`], a barrier write failed, or
/// the store's token could not be read. Each cause is said on stderr (no ids),
/// because the core's line can only say "may have cut short, or could not be
/// confirmed".
pub(super) async fn stop_sync(
    task: Option<tokio::task::JoinHandle<()>>,
    client: &Client,
    handled: &HandledToken,
) -> bool {
    let stop = async {
        if let Some(task) = task {
            task.abort();
            // `Err(JoinError::Cancelled)` is the expected result; any of them
            // means the task is no longer running. This is the first time
            // anyone looks at its handle, so a task that had already ended is
            // said: the worker was deaf from then on.
            match task.await {
                Err(e) if e.is_cancelled() => {}
                Err(_) => eprintln!(
                    "kastellan-worker-matrix: drain: the sync task had panicked before the \
                     drain; the worker was not receiving since"
                ),
                Ok(()) => eprintln!(
                    "kastellan-worker-matrix: drain: the sync task had ended before the drain; \
                     the worker was not receiving since"
                ),
            }
        }
        // The barrier: see the module docs. A failed write proves nothing
        // about the store's token, so it reads as interrupted.
        for i in 0..BARRIERS {
            if i > 0 {
                tokio::time::sleep(BARRIER_GAP).await;
            }
            if let Err(e) = client.state_store().save_changes(&StateChanges::default()).await {
                eprintln!(
                    "kastellan-worker-matrix: drain: barrier write failed ({e}); answering interrupted"
                );
                return true;
            }
        }
        // Read fail-CLOSED here — the opposite of `read_prior_sync_token`'s
        // fail-soft `None`, which is the safe direction for its caller only.
        let stored = match client.state_store().get_kv_data(StateStoreDataKey::SyncToken).await {
            Ok(value) => value.and_then(|value| value.into_sync_token()),
            Err(e) => {
                eprintln!(
                    "kastellan-worker-matrix: drain: the stored sync token could not be read \
                     ({e}); answering interrupted"
                );
                return true;
            }
        };
        sync_response_interrupted(stored.as_deref(), &handled.get())
    };
    match tokio::time::timeout(STOP_BOUND, stop).await {
        Ok(interrupted) => interrupted,
        Err(_) => {
            eprintln!(
                "kastellan-worker-matrix: drain: the stop was not confirmed within {STOP_BOUND:?}; \
                 answering interrupted"
            );
            true
        }
    }
}

/// `stop_sync` against a real matrix-sdk client and SQLite store, offline (no
/// homeserver is ever contacted). The pure comparison is tested in
/// `drain_policy`; these pin the wiring: the abort, the store read, the bound.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// A client over a fresh store whose saved sync token is `token`.
    async fn client_with_token(name: &str, token: &str) -> (Client, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kastellan-stop-sync-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let client = Client::builder()
            .homeserver_url("https://fake-homeserver.invalid")
            .sqlite_store(&dir, None)
            .build()
            .await
            .expect("client builds offline");
        let changes = StateChanges { sync_token: Some(token.into()), ..Default::default() };
        client.state_store().save_changes(&changes).await.expect("token saved");
        (client, dir)
    }

    /// A running sync is stopped (it makes no progress after), and a store
    /// that holds the last handled token lost nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_clean_stop_of_a_running_sync_lost_nothing() {
        let (client, dir) = client_with_token("clean", "s2").await;
        let ticks = Arc::new(AtomicUsize::new(0));
        let t = ticks.clone();
        let task = tokio::spawn(async move {
            loop {
                t.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let interrupted = stop_sync(Some(task), &client, &HandledToken::new("s2".into())).await;
        assert!(!interrupted);
        let after = ticks.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), after, "the sync task still runs");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The store saved a token past the last handled one: interrupted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_store_past_the_handled_token_reads_as_interrupted() {
        let (client, dir) = client_with_token("past", "s3").await;
        assert!(stop_sync(None, &client, &HandledToken::new("s2".into())).await);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A task with no await point cannot be aborted: the stop is bounded by
    /// `STOP_BOUND` and answers interrupted, never "nothing lost".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_that_cannot_land_in_time_reads_as_interrupted() {
        let (client, dir) = client_with_token("wedged", "s2").await;
        let started = Arc::new(AtomicBool::new(false));
        let s = started.clone();
        let task = tokio::spawn(async move {
            s.store(true, Ordering::SeqCst);
            std::thread::sleep(STOP_BOUND * 2);
        });
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let begun = Instant::now();
        let interrupted = stop_sync(Some(task), &client, &HandledToken::new("s2".into())).await;
        let took = begun.elapsed();
        assert!(interrupted, "a stop that could not be confirmed is never 'nothing lost'");
        assert!(took >= STOP_BOUND, "{took:?}");
        assert!(took < STOP_BOUND + Duration::from_millis(400), "unbounded: {took:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
