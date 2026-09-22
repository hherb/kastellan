//! The **value** a reporting caller ends up holding: a worker's retained
//! stderr lines together with what is known about whether they are all of
//! them.
//!
//! Split out of [`super`] in a movement-only change because that module is the
//! *capture* machinery — the ring, the drain threads, the wait — while this is
//! the small immutable thing the renderers in `report/` actually match on.
//! Keeping them together had `mod.rs` over this tree's 500-line cap before the
//! [#747](https://github.com/hherb/kastellan/issues/747) work grew it further.
//!
//! Re-exported by the parent, so `worker_stderr::CapturedTail` remains the one
//! public path and no call site names this module.

/// A worker's retained stderr **and whether it is all of it**
/// ([#732](https://github.com/hherb/kastellan/issues/732)).
///
/// ## Why the flag travels with the lines
///
/// [`collect_tail_after_drain`] used to discard [`StderrTail::wait_for_drain`]'s
/// return value, and the renderers then stated the opposite of what the system
/// knew. `StderrTail::drained`'s own doc calls that ambiguity "precisely the
/// ambiguity #666 exists to remove — so the flag is load-bearing, not a
/// convenience", and the one call site dropped it on the floor.
///
/// Two separate lies came out of that, and they are why this is a struct rather
/// than a second parameter someone can forget to pass:
///
/// * **Empty and incomplete read as "the worker said nothing."** The report
///   then printed a *diagnosis* — suspect a kill, a jail that refused the
///   spawn, seccomp — for a worker that explained itself at 260 ms and was
///   simply not waited for. The operator audits cgroup limits and seccomp
///   profiles for a fault that was in the guest's Python. That is #719
///   reproduced, except that this time the report actively points the wrong
///   way.
/// * **Non-empty and incomplete read as "its last words."** The ring evicts
///   **oldest** first, so under a *complete* drain the tail really is the last
///   thing the worker said. Under an incomplete one it is the **first** thing —
///   the boot lines — labelled as the last. An operator correlating "last
///   words" against the moment of death is reading startup noise.
///
/// ⚠️ **`complete: false` is not "no data".** Whatever arrived before the cap
/// is still here and still worth printing; the tail is a bounded ring, not a
/// transaction. What changes is what may be *claimed* about it.
/// ⚠️ **The fields are PRIVATE, and that is the difference between this type
/// and a tuple with a good doc comment.** With `pub complete`, a caller could
/// write `t.complete = true` and forge the one claim the type exists to gate —
/// which is exactly the door [`StderrTail::mark_drained`] is `pub(crate)` to
/// keep shut one layer down. Read access goes through [`CapturedTail::lines`],
/// [`CapturedTail::is_complete`] and [`CapturedTail::is_known_silent`];
/// construction goes through [`CapturedTail::complete`] /
/// [`CapturedTail::partial`] or [`collect_tail_after_drain`], all of which
/// state the claim being made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTail {
    /// The lines that had arrived when the snapshot was taken.
    lines: Vec<String>,
    /// `true` when the drain thread reached EOF within [`TAIL_DRAIN_WAIT`], so
    /// these lines are the worker's complete retained stderr.
    complete: bool,
}

impl CapturedTail {
    /// A tail known to be the whole of what the worker wrote.
    ///
    /// For tests and for callers that did not have to wait. Named rather than
    /// constructed field-by-field so a test reads as the *claim* it is making.
    pub fn complete(lines: Vec<String>) -> Self {
        Self { lines, complete: true }
    }

    /// A tail whose drain timed out: possibly incomplete, possibly empty only
    /// because nothing had arrived yet.
    pub fn partial(lines: Vec<String>) -> Self {
        Self { lines, complete: false }
    }

    /// The lines that had arrived, whether or not that is all of them.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// `true` when the drain reached EOF, so these lines are the worker's
    /// complete retained stderr and really are its most recent ones.
    ///
    /// ⚠️ **Asking this is not the same as asking [`Self::is_known_silent`].**
    /// It answers "may I call these the *last* words", not "may I say the
    /// worker was silent" — a renderer needs both, and needs them in that
    /// order. See [`crate::worker_stderr::format_worker_failure_report`] for
    /// the four-arm shape that gets it right.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// `true` when the worker is **known** to have written nothing.
    ///
    /// ⚠️ **Not the same as `lines().is_empty()`, and that is the whole
    /// point.** An empty *partial* tail is "we did not wait long enough to
    /// find out", which must never be rendered as the worker having stayed
    /// silent.
    ///
    /// ⚠️ **This is the only predicate that licenses a *diagnosis*** — the
    /// "suspect a kill (wall-clock/OOM/seccomp)" sentence and "no stderr
    /// captured". Both renderers match it **first**, so their later
    /// `lines().is_empty()` arm can only be a partial. That ordering is a
    /// convention the compiler does not check: a new renderer that tests the
    /// vector on its own will silently lose the distinction, which is what
    /// #732 was. Copy the existing arm order.
    pub fn is_known_silent(&self) -> bool {
        self.lines.is_empty() && self.complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_tail_is_only_known_silent_when_the_drain_completed() {
        // The distinction the whole of #732 rests on, at its smallest. These
        // two values have identical `lines`, and exactly one of them licenses
        // the "suspect a kill (wall-clock/OOM/seccomp)" sentence.
        assert!(
            CapturedTail::complete(vec![]).is_known_silent(),
            "an EMPTY tail whose drain reached EOF is the worker genuinely saying nothing — \
             the one case a report may diagnose"
        );
        assert!(
            !CapturedTail::partial(vec![]).is_known_silent(),
            "an empty tail whose drain TIMED OUT is a fact about us, not about the worker. \
             Treating it as silence is what sends an operator to audit seccomp and cgroups \
             for a fault that was in the guest's Python (#732)"
        );
    }
}
