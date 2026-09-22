//! The **value** a reporting caller ends up holding: a worker's retained
//! stderr lines together with what is known about whether they are all of
//! them.
//!
//! Split out of [`super`] because that module is the *capture* machinery — the
//! ring, the drain threads, the wait — while this is the small immutable thing
//! the renderers in `report/` actually match on.
//!
//! Re-exported by the parent, so `worker_stderr::CapturedTail` remains the one
//! public path and no call site names this module.
//!
//! # The three questions, and why they are one type
//!
//! A renderer wants to say one of a handful of things about a dead worker, and
//! every one of them is a *different claim*:
//!
//! * "here are its last words" — a claim about the worker,
//! * "it said nothing at all" — a **diagnosis**, and the strongest claim here,
//! * "here is what we got before we stopped listening" — a claim about *us*,
//! * "we could not read its pipe" — a claim about our own plumbing.
//!
//! Getting these confused is not cosmetic. [#732] shipped a report that told
//! an operator a worker "wrote NOTHING to stderr — suspect a kill
//! (wall-clock/OOM/seccomp)" when the truth was that we had waited 250 ms and
//! given up on a worker that explained itself at 260 ms. [#747] is the same
//! defect through a different door: a drain that died on a read error was
//! recorded as having *completed*, so a worker whose pipe we failed to read
//! earned the same confident diagnosis.
//!
//! [#732]: https://github.com/hherb/kastellan/issues/732
//! [#747]: https://github.com/hherb/kastellan/issues/747

/// How a stderr drain thread stopped reading a worker's pipe.
///
/// ⚠️ **The two variants are not interchangeable, and conflating them is
/// [#747](https://github.com/hherb/kastellan/issues/747).** Only [`Self::Eof`]
/// means "we have read everything the worker ever wrote". [`Self::ReadError`]
/// means the pipe broke *on our side*: whatever the worker wrote after that
/// point is gone, and we cannot tell how much that was — so an empty tail says
/// nothing whatsoever about the worker's own behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainEnd {
    /// `read(2)` returned 0: the worker closed its stderr (normally by
    /// exiting), and everything it ever wrote has been read.
    Eof,
    /// `read(2)` failed with something other than `EINTR` — a vsock- or
    /// pty-backed guest fd returning `EIO` on teardown is the motivating case.
    /// The drain stopped early and the remainder is unrecoverable.
    ReadError,
}

/// What a [`CapturedTail`] licenses its reader to *say* — the entire state
/// space, as one exhaustive enum.
///
/// # Why this is an enum and not a pair of predicates
///
/// It used to be `is_complete()` + `is_known_silent()` + `lines()`, and the
/// rule for using them correctly lived in a doc comment: *match
/// `is_known_silent()` first, so your later `lines().is_empty()` arm can only
/// be a partial*. That is a convention the compiler does not check, and
/// [#732](https://github.com/hherb/kastellan/issues/732) is precisely a
/// renderer that did not follow it. A third renderer
/// ([#746](https://github.com/hherb/kastellan/issues/746), in the egress
/// sidecar path) then did not follow it either — so the convention had failed
/// twice out of three.
///
/// Matching on this instead, a renderer that forgets a case **does not
/// compile**, and adding a state to the system forces every renderer to decide
/// what it now says. That is the forcing function the doc comment was standing
/// in for.
///
/// ⚠️ **Deliberately NOT `#[non_exhaustive]`.** A new variant breaking every
/// renderer in the tree is the entire point; letting out-of-crate matches fall
/// through a wildcard would hand back exactly the silent-collapse this type
/// exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailState<'a> {
    /// The drain reached EOF and the ring was empty: the worker really did
    /// write nothing.
    ///
    /// ⚠️ **This is the only state that licenses a *diagnosis*** — the "suspect
    /// a kill (wall-clock/OOM/seccomp)" sentence and its siblings. In every
    /// other empty state the silence is ours, not the worker's.
    KnownSilent,
    /// The drain reached EOF after these lines, so they genuinely are the last
    /// thing the worker said.
    LastWords(&'a [String]),
    /// We stopped waiting before the drain reached EOF and nothing had arrived
    /// yet. The worker may have been mid-sentence.
    NothingCapturedYet,
    /// We stopped waiting before the drain reached EOF, holding these lines.
    ///
    /// ⚠️ **These are the worker's FIRST words, not its last.** The ring evicts
    /// *oldest* first, so it only becomes a window on the end of the stream
    /// once the drain has finished. Calling them "last words" points a reader
    /// at startup noise.
    FirstWords(&'a [String]),
    /// The drain died on a read error before anything arrived.
    ///
    /// Renders as a statement about our plumbing. It must never borrow
    /// [`Self::KnownSilent`]'s sentence: the worker may have explained itself
    /// perfectly well into a pipe we then failed to read
    /// ([#747](https://github.com/hherb/kastellan/issues/747)).
    DrainFailedSilent,
    /// The drain died on a read error holding these lines. Like
    /// [`Self::FirstWords`], they are not known to be the last.
    DrainFailedWords(&'a [String]),
}

/// A worker's retained stderr **and what is known about whether it is all of
/// it** ([#732](https://github.com/hherb/kastellan/issues/732),
/// [#747](https://github.com/hherb/kastellan/issues/747)).
///
/// ## Why the drain outcome travels with the lines
///
/// [`collect_tail_after_drain`](super::collect_tail_after_drain) used to
/// discard [`StderrTail::wait_for_drain`](super::StderrTail::wait_for_drain)'s
/// return value, and the renderers then stated the opposite of what the system
/// knew. `StderrTail`'s own doc calls that ambiguity "precisely the ambiguity
/// #666 exists to remove — so the flag is load-bearing, not a convenience",
/// and the one call site dropped it on the floor.
///
/// Three separate lies came out of that, and they are why this is a struct
/// rather than a second parameter someone can forget to pass:
///
/// * **Empty and timed out read as "the worker said nothing."** The report
///   then printed a *diagnosis* — suspect a kill, a jail that refused the
///   spawn, seccomp — for a worker that explained itself at 260 ms and was
///   simply not waited for. The operator audits cgroup limits and seccomp
///   profiles for a fault that was in the guest's Python.
/// * **Non-empty and timed out read as "its last words."** The ring evicts
///   **oldest** first, so under a *complete* drain the tail really is the last
///   thing the worker said. Under an incomplete one it is the **first** thing —
///   the boot lines — labelled as the last.
/// * **A drain that died on a read error read as complete** (#747). The
///   `mark_drained` after a failed read is deliberate and right on its own
///   terms — a waiter must not burn the full 250 ms cap because the pipe broke
///   — but recording it as EOF handed an empty tail the one sentence only a
///   *known* silence earns.
///
/// ⚠️ **An incomplete tail is not "no data".** Whatever arrived before the cap
/// is still here and still worth printing; the tail is a bounded ring, not a
/// transaction. What changes is what may be *claimed* about it.
///
/// ⚠️ **The fields are PRIVATE, and that is the difference between this type
/// and a tuple with a good doc comment.** With a public flag, a caller could
/// forge the one claim the type exists to gate — which is exactly the door
/// [`StderrTail::mark_drained`](super::StderrTail::mark_drained) is
/// `pub(crate)` to keep shut one layer down. Read access goes through
/// [`CapturedTail::lines`] and [`CapturedTail::state`]; construction goes
/// through the three named constructors or
/// [`collect_tail_after_drain`](super::collect_tail_after_drain), all of which
/// state the claim being made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTail {
    /// The lines that had arrived when the snapshot was taken.
    lines: Vec<String>,
    /// How the drain ended, or `None` if it had not ended when we stopped
    /// waiting.
    end: Option<DrainEnd>,
}

impl CapturedTail {
    /// A tail known to be the whole of what the worker wrote: the drain
    /// reached EOF.
    ///
    /// Named rather than constructed field-by-field so a test reads as the
    /// *claim* it is making.
    pub fn complete(lines: Vec<String>) -> Self {
        Self { lines, end: Some(DrainEnd::Eof) }
    }

    /// A tail whose drain had not finished when we stopped waiting: possibly
    /// incomplete, possibly empty only because nothing had arrived yet.
    pub fn partial(lines: Vec<String>) -> Self {
        Self { lines, end: None }
    }

    /// A tail whose drain stopped on a **read error**
    /// ([#747](https://github.com/hherb/kastellan/issues/747)).
    ///
    /// Distinct from [`Self::partial`] even though both mean "possibly
    /// incomplete", because they point a reader at different places: `partial`
    /// says we ran out of patience and could have waited longer, this says the
    /// pipe itself failed and waiting would not have helped.
    pub fn drain_failed(lines: Vec<String>) -> Self {
        Self { lines, end: Some(DrainEnd::ReadError) }
    }

    /// Build from a drain outcome as observed by
    /// [`StderrTail::wait_for_drain`](super::StderrTail::wait_for_drain):
    /// `None` means the wait timed out.
    pub fn from_drain(lines: Vec<String>, end: Option<DrainEnd>) -> Self {
        Self { lines, end }
    }

    /// The lines that had arrived, whether or not that is all of them.
    ///
    /// ⚠️ **Prefer [`Self::state`] in a renderer.** Reading the lines alone
    /// cannot tell an empty ring that the worker was silent from an empty ring
    /// we simply failed to fill, which is the whole of #732 and #747.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The single question a renderer should ask: what may be *said* about
    /// this tail.
    ///
    /// Exhaustive by construction — see [`TailState`] for why this replaced
    /// the `is_complete()` / `is_known_silent()` predicate pair.
    pub fn state(&self) -> TailState<'_> {
        match (self.end, self.lines.is_empty()) {
            (Some(DrainEnd::Eof), true) => TailState::KnownSilent,
            (Some(DrainEnd::Eof), false) => TailState::LastWords(&self.lines),
            (None, true) => TailState::NothingCapturedYet,
            (None, false) => TailState::FirstWords(&self.lines),
            (Some(DrainEnd::ReadError), true) => TailState::DrainFailedSilent,
            (Some(DrainEnd::ReadError), false) => TailState::DrainFailedWords(&self.lines),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_tail_is_only_known_silent_when_the_drain_reached_eof() {
        // The distinction the whole of #732 and #747 rests on, at its
        // smallest. All three values have identical (empty) `lines`, and
        // exactly one of them licenses the "suspect a kill
        // (wall-clock/OOM/seccomp)" sentence.
        assert_eq!(
            CapturedTail::complete(vec![]).state(),
            TailState::KnownSilent,
            "an EMPTY tail whose drain reached EOF is the worker genuinely saying nothing — \
             the one case a report may diagnose"
        );
        assert_eq!(
            CapturedTail::partial(vec![]).state(),
            TailState::NothingCapturedYet,
            "an empty tail whose drain TIMED OUT is a fact about us, not about the worker. \
             Treating it as silence is what sends an operator to audit seccomp and cgroups \
             for a fault that was in the guest's Python (#732)"
        );
        assert_eq!(
            CapturedTail::drain_failed(vec![]).state(),
            TailState::DrainFailedSilent,
            "an empty tail whose drain died on a READ ERROR is a fact about our plumbing. The \
             worker may have explained itself into a pipe we could not read (#747)"
        );
    }

    #[test]
    fn lines_are_only_the_last_words_when_the_drain_reached_eof() {
        // The quieter half of #732, and its #747 sibling. The ring evicts
        // OLDEST first, so only a finished drain makes the tail a window on
        // the END of the stream.
        let l = vec!["boom".to_string()];
        assert_eq!(CapturedTail::complete(l.clone()).state(), TailState::LastWords(&l));
        assert_eq!(CapturedTail::partial(l.clone()).state(), TailState::FirstWords(&l));
        assert_eq!(
            CapturedTail::drain_failed(l.clone()).state(),
            TailState::DrainFailedWords(&l),
            "a read error leaves the same doubt a timeout does — there may have been more — \
             but points at a different cause, so it earns its own state"
        );
    }

    #[test]
    fn every_tail_state_is_reachable_from_a_constructor() {
        // A state no constructor can produce is a renderer arm no test can
        // reach, which is how an unreachable success path ships. Six states,
        // three constructors, two line-shapes each.
        let empty: Vec<String> = vec![];
        let full = vec!["x".to_string()];
        // Bound first: `state()` borrows the tail, so building these inline
        // would borrow temporaries that die at the end of the statement.
        let tails = [
            CapturedTail::complete(empty.clone()),
            CapturedTail::complete(full.clone()),
            CapturedTail::partial(empty.clone()),
            CapturedTail::partial(full.clone()),
            CapturedTail::drain_failed(empty),
            CapturedTail::drain_failed(full),
        ];
        let reached: Vec<TailState<'_>> = tails.iter().map(CapturedTail::state).collect();
        // Compared by variant NAME rather than by value: two `LastWords`
        // holding different slices are different values but the same state,
        // and it is the states we are counting.
        let mut kinds: Vec<String> = reached
            .iter()
            .map(|s| format!("{s:?}").split('(').next().unwrap().to_string())
            .collect();
        kinds.sort();
        kinds.dedup();
        assert_eq!(
            kinds.len(),
            6,
            "all six TailState variants must be constructible, or an arm exists that no test \
             can reach: {reached:#?}"
        );
    }

    #[test]
    fn from_drain_agrees_with_the_named_constructors() {
        // `from_drain` is what `collect_tail_after_drain` uses, so a
        // disagreement here would make every production tail differ from every
        // tested one.
        let l = vec!["a".to_string()];
        assert_eq!(CapturedTail::from_drain(l.clone(), Some(DrainEnd::Eof)), CapturedTail::complete(l.clone()));
        assert_eq!(CapturedTail::from_drain(l.clone(), None), CapturedTail::partial(l.clone()));
        assert_eq!(
            CapturedTail::from_drain(l.clone(), Some(DrainEnd::ReadError)),
            CapturedTail::drain_failed(l)
        );
    }
}
