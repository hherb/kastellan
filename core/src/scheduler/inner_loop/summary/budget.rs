//! The accumulated size budget of the planner's plan summary (#339).
//!
//! Lifted out of `summary.rs`, which sat over the 500-LOC cap, before #921
//! changed what the budget does.
//!
//! # Shrink evenly, then drop the oldest (#921)
//!
//! Until #921 an over-budget summary dropped the oldest outputs whole. Ten
//! `mail.get_message` results of up to 16 KiB each overrun the 96 KiB budget,
//! so the oldest messages lost their sender, date and subject; the planner
//! fetched them again and told the user they "could not be retrieved".
//!
//! Now the budget first gives every output an even share (water-filling:
//! an output smaller than the share stays whole, and what it does not use is
//! split among the larger ones), and re-renders each larger output at that
//! share — the same pruned view, trimmed further as [`render`](super::result_view::render)
//! trims (strings first, then list elements and fields), so its short labels
//! and identifiers outlast its long text. Only when a share would fall below
//! [`MIN_SHRUNK_OUTPUT_BYTES`] does it drop the oldest outputs whole, as many
//! as it takes for the rest to keep at least that much.

use super::RenderedStep;

/// The smallest share, in serialised bytes of the step outcome, that an output
/// is shrunk to before the budget drops the oldest outputs whole instead. 2 KiB
/// is meant to keep a message's headers (pinned at the floor by
/// `a_message_shrunk_to_the_floor_keeps_its_headers`).
///
/// It also bounds the cost: a shrunk output is re-rendered and re-screened on
/// every planner turn (the memoized render of #344 is the full-size one), and
/// at most `budget / MIN_SHRUNK_OUTPUT_BYTES` (48 at 96 KiB) can be shrunk.
pub(super) const MIN_SHRUNK_OUTPUT_BYTES: usize = 2 * 1024;

/// How the outputs are fitted to the room left for them, decided from their
/// sizes alone by [`plan_fit`].
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Fit {
    /// How many of the oldest outputs are dropped whole.
    pub(super) elide: usize,
    /// The most bytes any later output keeps; a larger one is re-rendered to
    /// it. `usize::MAX` when none needs shrinking.
    pub(super) share: usize,
}

/// What [`apply_summary_budget`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Fitted {
    /// Outputs replaced with the elided marker.
    pub(super) elided: usize,
    /// Outputs re-rendered at a smaller share.
    pub(super) shrunk: usize,
    /// Of the `elided`, those whose smaller view the sink screen blocked
    /// though the full one passed. Not audited (#928), so the caller warns.
    pub(super) blocked: usize,
}

/// Fit outputs of `sizes` bytes (oldest first) into `room` bytes. Pure.
///
/// Drops the fewest oldest outputs (each then costs `marker` bytes) for every
/// later one to keep `min(size, MIN_SHRUNK_OUTPUT_BYTES)`, then water-fills
/// the rest: the returned `share` is the largest per-output cap at which
/// `Σ min(size, share)` fits what is left. When even dropping all of them
/// does not fit, all are dropped — nothing else in the summary is the
/// budget's to cut.
pub(super) fn plan_fit(sizes: &[usize], room: usize, marker: usize) -> Fit {
    let sum = |s: &[usize]| s.iter().fold(0usize, |t, &b| t.saturating_add(b));
    if sum(sizes) <= room {
        return Fit { elide: 0, share: usize::MAX };
    }
    let floor_cost = |b: usize| b.min(MIN_SHRUNK_OUTPUT_BYTES);
    let mut rest = sizes.iter().fold(0usize, |t, &b| t.saturating_add(floor_cost(b)));
    let mut elide = 0;
    while elide < sizes.len() && marker.saturating_mul(elide).saturating_add(rest) > room {
        rest = rest.saturating_sub(floor_cost(sizes[elide]));
        elide += 1;
    }
    let kept = &sizes[elide..];
    let room = room.saturating_sub(marker.saturating_mul(elide));
    Fit { elide, share: water_level(kept, room) }
}

/// The largest cap at which `Σ min(size, cap)` is at most `room`, or
/// `usize::MAX` when every size already fits. Pure.
fn water_level(sizes: &[usize], mut room: usize) -> usize {
    let mut sorted = sizes.to_vec();
    sorted.sort_unstable();
    for (j, &size) in sorted.iter().enumerate() {
        let left = sorted.len() - j;
        if size > room / left {
            return room / left;
        }
        room -= size;
    }
    usize::MAX
}

/// Fit the serialised total of all step outcomes into `budget`: shrink the
/// successful-step outputs evenly, dropping the oldest whole only past
/// [`MIN_SHRUNK_OUTPUT_BYTES`] (see the module docs and [`plan_fit`]).
///
/// Walks `plans` oldest→newest (`plans[0]` is the oldest, pushed first by the
/// inner loop) and steps in order. Only an `elidable` step **larger than the
/// marker** is touched: errors and withheld outcomes are the signal, and
/// eliding a tiny output would grow it. `shrink(p, i, cap)` re-renders
/// `plans[p][i]` with its view at most `cap` bytes; a result that is not an
/// output (the sink screen blocked the smaller view) or that does not fit the
/// share becomes the elided marker, because the full view passed and only
/// this summary could not show it; a block is counted in [`Fitted::blocked`].
/// No-op when already within budget, and idempotent: a second pass finds the
/// summary within budget or nothing left to touch.
pub(super) fn apply_summary_budget(
    plans: &mut [Vec<RenderedStep>],
    budget: usize,
    mut shrink: impl FnMut(usize, usize, usize) -> RenderedStep,
) -> Fitted {
    // Saturating: `serialised_len` reports an unserialisable value as
    // `usize::MAX`, and a plain sum would panic in debug and wrap in release.
    let total = plans.iter().flatten().fold(0usize, |t, s| t.saturating_add(s.bytes));
    if total <= budget {
        return Fitted::default();
    }
    let marker = RenderedStep::elided();
    let touchable = |s: &RenderedStep| s.elidable && s.bytes > marker.bytes;
    let sizes: Vec<usize> = plans.iter().flatten().filter(|s| touchable(s)).map(|s| s.bytes).collect();
    let fixed = plans.iter().flatten().filter(|s| !touchable(s)).fold(0usize, |t, s| t.saturating_add(s.bytes));
    let fit = plan_fit(&sizes, budget.saturating_sub(fixed), marker.bytes);
    // What `{"output": …, "status": "ok"}` adds around a view.
    let wrapper = RenderedStep::output(serde_json::Value::Null).bytes - "null".len();

    let mut fitted = Fitted::default();
    let mut seen = 0;
    for (p, plan) in plans.iter_mut().enumerate() {
        for (i, step) in plan.iter_mut().enumerate() {
            if !touchable(step) {
                continue;
            }
            seen += 1;
            if seen <= fit.elide {
                *step = marker.clone();
                fitted.elided += 1;
            } else if step.bytes > fit.share {
                debug_assert!(fit.share >= MIN_SHRUNK_OUTPUT_BYTES, "share {} below the floor", fit.share);
                let shrunk = shrink(p, i, fit.share - wrapper);
                if shrunk.elidable && shrunk.bytes <= fit.share {
                    *step = shrunk;
                    fitted.shrunk += 1;
                } else {
                    fitted.blocked += usize::from(shrunk.sink_block.is_some());
                    *step = marker.clone();
                    fitted.elided += 1;
                }
            }
        }
    }
    fitted
}

#[cfg(test)]
mod tests;
