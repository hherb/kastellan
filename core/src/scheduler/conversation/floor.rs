//! What a follow-up inherits from the turns before it (#701).
//!
//! A follow-up is planned against data its earlier turns produced, so it is
//! bounded by the most sensitive class any of them touched. Pure.

use super::Turn;
use crate::cassandra::types::DataClass;
use crate::scheduler::inner_loop::ClassificationFloorSource;

/// The floor and provenance a task should run with, given its payload's own
/// floor and the turns loaded for it.
///
/// **Every turn loaded counts, including ones the budget later drops or the
/// screen withholds.** A turn absent from the prompt is still what the user is
/// referring to, and inheriting too high costs a re-planned step, while
/// inheriting too low would run the follow-up below the class of the
/// conversation it continues.
///
/// Never lowers a floor, and never relabels one that was already at least as
/// high: an operator-set or CLI-inferred floor keeps its own provenance, so
/// the audit row does not attribute an operator's decision to the conversation.
pub(crate) fn inherit_floor(
    payload_floor: DataClass,
    payload_source: ClassificationFloorSource,
    turns: &[Turn],
) -> (DataClass, ClassificationFloorSource) {
    let inherited = turns
        .iter()
        .filter_map(|t| t.record.as_ref().map(|r| r.data_class))
        .max_by_key(|c| c.rank());

    match inherited {
        Some(class) if class.rank() > payload_floor.rank() => {
            (class, ClassificationFloorSource::ConversationInherited)
        }
        _ => (payload_floor, payload_source),
    }
}

#[cfg(test)]
mod tests;
