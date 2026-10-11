//! "Did any pump stop?" — one signal, rung by RAII (#517).
//!
//! [`ChannelBus`](crate::channel::ChannelBus) runs its work in detached tasks:
//! one per channel, plus the completed-task pump. Each of them can *end* —
//! `PgCompletedTasks::next_completed` returning `None`, a per-channel task
//! seeing `recv()` closed, or either one panicking — and before this module
//! nothing noticed. The supervisor above them had already parked on the
//! shutdown oneshot, so a bus whose pumps had all quietly returned still looked
//! exactly like a healthy one: every unit `active`, Postgres fine, and the log
//! silent because there was nothing left to log (#514's signature, reached
//! after boot rather than during it).
//!
//! This is the smallest thing that closes that: a bell every pump holds a guard
//! on, and one waiter that wakes when *any* guard drops.
//!
//! Two decisions carry the weight:
//!
//! 1. **The signal is a [`Drop`], not a line of code after the pump body.**
//!    A guard dropped while the stack unwinds covers a **panicking** pump,
//!    which trailing code cannot; it also covers `JoinHandle::abort`, which is
//!    what shutdown does. "The task is gone" is exactly the condition we want
//!    to hear about, and only `Drop` observes all three ways of getting there.
//! 2. **[`Notify::notify_one`], not `notify_waiters`.** `notify_one` stores a
//!    permit when nobody is waiting yet, so a pump that dies *before* the
//!    supervisor starts awaiting still wakes it. `notify_waiters` drops that
//!    wakeup on the floor — and the resulting bug (deaf only when the death is
//!    fast, silent when it is slow) is about the hardest shape there is to
//!    reproduce deliberately.
//!
//! Nothing here knows what a pump *is*. It owns no clock, spawns nothing, and
//! touches no database, so the whole module is testable with a guard and a
//! `drop`.

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::Notify;

/// The listening half: hands out one [`PumpLife`] per pump and answers "has any
/// of them ended?".
///
/// Cloning is deliberately not offered — a bus has exactly one bell, and
/// [`signal`](Self::signal) already hands out `'static` waiters, which is the
/// only sharing anyone needs.
#[derive(Debug, Default)]
pub struct DeathBell {
    notify: Arc<Notify>,
}

impl DeathBell {
    /// A bell nobody has rung yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a guard for one pump. The pump must hold it for as long as it runs
    /// (`let _life = bell.guard();` at the top of the task body is the idiom):
    /// dropping it — by returning, by panicking, or by being aborted — rings.
    pub fn guard(&self) -> PumpLife {
        PumpLife { notify: Arc::clone(&self.notify) }
    }

    /// A future that completes once any guard has been dropped.
    ///
    /// `'static`, because the supervisor holds it across awaits while also
    /// owning the thing it is going to stop; borrowing the bell would tie those
    /// two lifetimes together for no reason.
    ///
    /// Completing once is all that is asked of it: the first death takes the
    /// whole channel down for a restart, so there is no second question to ask.
    pub fn signal(&self) -> BoxFuture<'static, ()> {
        let notify = Arc::clone(&self.notify);
        Box::pin(async move { notify.notified().await })
    }
}

/// A pump's liveness guard. Rings its [`DeathBell`] on drop.
///
/// Held, never called — the whole point is that no pump has to remember to
/// report its own death on every exit path, including the ones it does not
/// know it has.
#[derive(Debug)]
pub struct PumpLife {
    notify: Arc<Notify>,
}

impl Drop for PumpLife {
    fn drop(&mut self) {
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// How long a "did it ring?" assertion waits before calling it silence.
    /// Only ever reached on the failing path, so it costs nothing when green.
    const RANG: Duration = Duration::from_secs(5);

    /// The ordinary case: a pump returns, and the bell rings.
    #[tokio::test]
    async fn a_dropped_guard_rings_the_bell() {
        let bell = DeathBell::new();
        let life = bell.guard();

        drop(life);

        tokio::time::timeout(RANG, bell.signal()).await.expect("bell should have rung");
    }

    /// The lost-wakeup case, and the reason this uses `notify_one` rather than
    /// `notify_waiters`: the pump dies *before* anyone asks. A bell that only
    /// wakes existing waiters would go silent here — and a channel whose pumps
    /// die fast is precisely the one that must not be missed.
    #[tokio::test]
    async fn a_death_before_anyone_waits_is_still_heard() {
        let bell = DeathBell::new();
        let life = bell.guard();
        drop(life);

        // The waiter is only *created* now, well after the death.
        let signal = bell.signal();

        tokio::time::timeout(RANG, signal).await.expect("bell should have rung");
    }

    /// The other order: someone is already waiting when the pump dies.
    #[tokio::test]
    async fn a_waiter_already_waiting_is_woken() {
        let bell = DeathBell::new();
        let life = bell.guard();
        let waiter = tokio::spawn(bell.signal());

        // Give the waiter a chance to park on the notify before the death.
        tokio::task::yield_now().await;
        drop(life);

        tokio::time::timeout(RANG, waiter).await.expect("bell should have rung").expect("waiter");
    }

    /// A bus has several pumps and does not care *which* one stopped: one dead
    /// pump is a degraded channel even while its siblings are healthy.
    #[tokio::test]
    async fn one_death_among_healthy_siblings_still_rings() {
        let bell = DeathBell::new();
        let alive_one = bell.guard();
        let alive_two = bell.guard();
        let doomed = bell.guard();

        drop(doomed);

        tokio::time::timeout(RANG, bell.signal()).await.expect("bell should have rung");
        drop((alive_one, alive_two));
    }

    /// How long a "nothing rang" assertion listens before accepting silence.
    ///
    /// Short on purpose: an *absence* can only ever be sampled, so the choice
    /// is between a slow test and a weak one. This is the weak-but-fast end,
    /// deliberately — a bell that rings spuriously does so from a `Drop` that
    /// already happened, i.e. immediately, not after a delay. (Tokio's
    /// `start_paused` would let this be both fast and exhaustive; core's tests
    /// have had the `test-util` feature it needs since #825, but this test
    /// predates that and has not been moved onto it.)
    const SILENCE: Duration = Duration::from_millis(100);

    /// While every pump runs, the bell stays silent — otherwise the supervisor
    /// would tear a healthy channel down and rebuild it in a loop.
    #[tokio::test]
    async fn a_bell_with_every_guard_alive_stays_silent() {
        let bell = DeathBell::new();
        let _life = bell.guard();

        let waited = tokio::time::timeout(SILENCE, bell.signal()).await;

        assert!(waited.is_err(), "no pump ended, so nothing should have rung");
    }

    /// A bell nobody ever took a guard on is a bus with no pumps — still
    /// silent, rather than "instantly dead".
    #[tokio::test]
    async fn a_bell_with_no_guards_at_all_stays_silent() {
        let bell = DeathBell::new();

        let waited = tokio::time::timeout(SILENCE, bell.signal()).await;

        assert!(waited.is_err(), "a bell with no guards has had no deaths");
    }

    /// The case trailing code cannot cover, and the reason the signal is a
    /// `Drop`: a pump that panics is just as gone as one that returned, and the
    /// channel is just as deaf. Replacing the guard with a `notify_one()` after
    /// the pump body would pass every other test here and fail this one.
    ///
    /// The panic message is captured by the test harness on a passing run.
    #[tokio::test]
    async fn a_panicking_pump_rings_the_bell() {
        let bell = DeathBell::new();
        let life = bell.guard();

        // Synchronous: nothing is awaited inside, so the unwind stays entirely
        // within this closure and the guard's `Drop` runs on the way out.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _life = life;
            panic!("pump exploded");
        }));
        assert!(panicked.is_err(), "the closure was supposed to panic");

        tokio::time::timeout(RANG, bell.signal()).await.expect("bell should have rung");
    }

    /// The same property for the third way a task ends: `abort`. Shutdown
    /// aborts every pump, so this is also the path that must not be mistaken
    /// for a death worth restarting — the supervisor's `biased` select is what
    /// makes that call, and it can only do so if the ring is reliable here.
    #[tokio::test]
    async fn an_aborted_pump_rings_the_bell() {
        let bell = DeathBell::new();
        let life = bell.guard();

        let pump = tokio::spawn(async move {
            let _life = life;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        pump.abort();

        tokio::time::timeout(RANG, bell.signal()).await.expect("bell should have rung");
    }
}
