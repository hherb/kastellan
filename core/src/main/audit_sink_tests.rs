//! `audit_sink`'s own tests. Split out of `audit_sink.rs` to keep it under the
//! 500-LOC soft cap; `#[path]`-included there, so `super::` is `audit_sink`.

use super::test_support::{
    assert_insert_attempted, assert_returns_at_once, connected_within, stalled_pool,
    ACQUIRE_TIMEOUT,
};
use super::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type OnFailure = Box<dyn FnOnce(Unwritten<'_>) + Send>;

/// Records what `on_failure` was told.
fn recorder() -> (Arc<Mutex<Option<String>>>, impl FnOnce(Unwritten<'_>) + Send + 'static) {
    let told: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = told.clone();
    (told, move |u: Unwritten<'_>| *seen.lock().unwrap() = Some(u.to_string()))
}

/// #789: the insert does not hold the caller. The positive control first:
/// the same insert, **waited for** from the same kind of thread (as the
/// email sink did with `block_on`), takes the whole acquire timeout — so
/// the fixture really stalls, and the fast return below is the helper's
/// doing. Then the helper's failure callback does run, once the pool
/// gives up.
#[test]
fn the_insert_does_not_hold_the_calling_thread() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    // Made inside the runtime, but this thread stays outside it — like the
    // driver thread that calls the real sinks.
    let (pool, _listener) = rt.block_on(async { stalled_pool() });

    let t = Instant::now();
    let blocked = rt.block_on(kastellan_db::audit::insert(&pool, "a", "b", serde_json::json!({})));
    assert!(blocked.is_err(), "POSITIVE CONTROL: the stalled pool must fail the insert");
    assert!(
        t.elapsed() >= ACQUIRE_TIMEOUT,
        "POSITIVE CONTROL: waiting for the insert must take the acquire timeout, or this \
         fixture does not stall and the assertion below proves nothing: {:?}",
        t.elapsed()
    );

    let (told, on_failure) = recorder();
    let mut task = None;
    assert_returns_at_once("spawn_audit_insert", || {
        task = spawn_audit_insert(rt.handle(), &pool, "a", "b", serde_json::json!({}), on_failure);
    });
    rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
    assert!(told.lock().unwrap().is_some(), "a failed insert reaches `on_failure`");
}

/// The positive control for [`assert_insert_attempted`]: the stalled pool
/// opens no connection of its own accord, so a connection means an insert
/// was tried.
#[test]
fn a_stalled_pool_connects_only_for_an_insert() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, listener) = rt.block_on(async { stalled_pool() });
    assert!(
        !connected_within(&listener, Duration::from_millis(300)),
        "POSITIVE CONTROL: the lazy pool must not connect before it is used"
    );
    let task = spawn_audit_insert(rt.handle(), &pool, "a", "b", serde_json::json!({}), |_| {});
    assert_insert_attempted("spawn_audit_insert", &listener);
    rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
}

/// Past the queue bound a row is shed — at once, on the caller's thread,
/// and said so through `on_failure` — and the slot comes back once the
/// queued insert gives up.
#[test]
fn past_the_queue_bound_a_row_is_shed_and_said_so() {
    static ONE_SLOT: Limits = Limits::new(1, 1);
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { stalled_pool() });
    let spawn = |on_failure: OnFailure| {
        spawn_limited(&ONE_SLOT, rt.handle(), &pool, "a", "b", serde_json::json!({}), on_failure)
    };

    let queued = spawn(Box::new(|_: Unwritten<'_>| {}))
        .expect("POSITIVE CONTROL: the first row fits the queue");
    let (told, on_failure) = recorder();
    assert_returns_at_once("a shed row", || {
        assert!(spawn(Box::new(on_failure)).is_none(), "the second row must be shed");
    });
    let told = told.lock().unwrap().clone();
    assert!(
        told.as_deref().is_some_and(|t| t.starts_with("shed:")),
        "a shed row must reach `on_failure` at once, as shed: {told:?}"
    );

    rt.block_on(queued).expect("the queued insert ran to completion");
    let again = spawn(Box::new(|_: Unwritten<'_>| {})).expect("the slot comes back");
    rt.block_on(again).expect("the insert task ran to completion");
}

/// At most `connections` inserts use the pool at once, the rest wait: two
/// stalled inserts through one connection take two acquire timeouts, one
/// after the other. The control: through two, they take one.
#[test]
fn inserts_share_a_bounded_number_of_connections() {
    static ONE_CONNECTION: Limits = Limits::new(2, 1);
    static TWO_CONNECTIONS: Limits = Limits::new(2, 2);
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { stalled_pool() });
    let both = |limits: &'static Limits| {
        let t = Instant::now();
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                spawn_limited(limits, rt.handle(), &pool, "a", "b", serde_json::json!({}), |_| {})
                    .expect("not shed")
            })
            .collect();
        for task in tasks {
            rt.block_on(task).expect("the insert task ran to completion");
        }
        t.elapsed()
    };
    let parallel = both(&TWO_CONNECTIONS);
    assert!(
        parallel < ACQUIRE_TIMEOUT * 2,
        "POSITIVE CONTROL: two inserts through two connections wait side by side: {parallel:?}"
    );
    let serial = both(&ONE_CONNECTION);
    assert!(
        serial >= ACQUIRE_TIMEOUT * 2,
        "two inserts through one connection must wait one after the other: {serial:?}"
    );
}
