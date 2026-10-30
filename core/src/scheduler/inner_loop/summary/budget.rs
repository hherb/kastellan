//! The accumulated size budget of the planner's plan summary (#339).
//!
//! Lifted out of `summary.rs`, which sat over the 500-LOC cap, before #921
//! changed what the budget does.

use super::RenderedStep;

/// Elide the oldest successful-step outputs until the serialised total of all
/// step outcomes is within `budget`. Walks `plans` oldest→newest (`plans[0]` is
/// the oldest, pushed first by the inner loop) and steps in order, replacing
/// each `elidable` step **larger than the marker** with [`super::elided_outcome`],
/// decrementing a running total, and stopping the instant it is within budget.
/// No-op when already within budget. The `bytes > marker.bytes` guard means
/// eliding can never *grow* a tiny output and makes the pass idempotent (an
/// elided step is exactly the marker's size, so it is never re-elided).
/// Returns the number of steps elided.
pub(super) fn apply_summary_budget(plans: &mut [Vec<RenderedStep>], budget: usize) -> usize {
    // Saturating: `serialised_len` reports an unserialisable value as
    // `usize::MAX`, and a plain sum would panic in debug and wrap in release.
    let mut total = plans.iter().flatten().fold(0usize, |t, s| t.saturating_add(s.bytes));
    if total <= budget {
        return 0;
    }
    let marker = RenderedStep::elided();
    let mut elided = 0;
    for plan in plans.iter_mut() {
        for step in plan.iter_mut() {
            if total <= budget {
                return elided;
            }
            if step.elidable && step.bytes > marker.bytes {
                // Saturating for the same reason as the sum: once it has
                // saturated, the running total can be below a step's own size.
                total = total.saturating_sub(step.bytes - marker.bytes);
                *step = marker.clone(); // now minimal and non-elidable
                elided += 1;
            }
        }
    }
    elided
}

#[cfg(test)]
mod tests;
