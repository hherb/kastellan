//! Which notebook turns think (#922).
//!
//! Measured on the DGX (gemma4 26B-A4B, tasks 272–290): thinking on every
//! turn costs 45–55 s a turn; thinking off costs ~14 s but answers worse on
//! the hardest question. Most turns are routine — read the result, tick,
//! call the next step — and only a few need judgement: writing the step
//! list, replanning after a failure, and writing the answer. So those think
//! (`Allow`), routine turns follow the router's configured default
//! (`KASTELLAN_LLM_DISABLE_THINKING`), and the forced-answer turn never
//! thinks (it is the last chance, and a thinking turn there ran out of
//! budget, task 269).
//!
//! With thinking left on in the config, every turn thinks but the forced
//! answer and the #945 re-ask (`run::ask` always suppresses it there); the
//! selection only changes anything when the config turns thinking off.

use kastellan_llm_router::ThinkingPolicy;

/// Sent on the turn that rewrites an answer a non-thinking turn wrote: the
/// findings are in the notebook, so this turn only writes.
pub(crate) const WRITE_ANSWER_DIRECTIVE: &str = "You have gathered enough to answer. Write the final \
answer now, from the findings (and the current step's last result, if it has one): complete, clearly \
laid out, in the order and shape the instruction asks for, saying what is missing or uncertain. Reply \
with ONLY `answer` — no `tick`, no `steps`, no `call`.";

/// What a notebook turn is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnKind {
    /// The first turn: the step list is written.
    Plan,
    /// The turn after a failed, blocked or repeated call: the remaining steps are rethought.
    Replan,
    /// Read the last result, tick, call the next step.
    Routine,
    /// Rewrite the answer a routine turn wrote, from the findings.
    WriteAnswer,
    /// The turn budget or the ceiling is spent: answer now.
    ForcedAnswer,
}

/// The kind of the coming turn. Pure. The forced answer wins over
/// everything, then a pending answer rewrite, then the first turn, then a
/// replan.
pub(crate) fn turn_kind(forced_answer: bool, write_answer: bool, no_steps_yet: bool, replan_due: bool) -> TurnKind {
    if forced_answer {
        TurnKind::ForcedAnswer
    } else if write_answer {
        TurnKind::WriteAnswer
    } else if no_steps_yet {
        TurnKind::Plan
    } else if replan_due {
        TurnKind::Replan
    } else {
        TurnKind::Routine
    }
}

/// Whether a turn of `kind` thinks. Pure.
pub(crate) fn turn_thinking(kind: TurnKind) -> ThinkingPolicy {
    match kind {
        TurnKind::Plan | TurnKind::Replan | TurnKind::WriteAnswer => ThinkingPolicy::Allow,
        TurnKind::Routine => ThinkingPolicy::RouterDefault,
        TurnKind::ForcedAnswer => ThinkingPolicy::Suppress,
    }
}

/// Should an answer from a turn of `kind` be rewritten by a thinking turn
/// before it is used? Only when that turn did not think: a routine turn on a
/// backend configured not to think. Pure.
pub(crate) fn rewrite_answer(kind: TurnKind, thinks_by_default: bool) -> bool {
    kind == TurnKind::Routine && !thinks_by_default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_forced_answer_wins_then_the_rewrite_then_the_plan_then_a_replan() {
        assert_eq!(turn_kind(true, true, true, true), TurnKind::ForcedAnswer);
        assert_eq!(turn_kind(false, true, true, true), TurnKind::WriteAnswer);
        assert_eq!(turn_kind(false, false, true, true), TurnKind::Plan);
        assert_eq!(turn_kind(false, false, false, true), TurnKind::Replan);
        assert_eq!(turn_kind(false, false, false, false), TurnKind::Routine);
    }

    #[test]
    fn only_judgement_turns_think_and_the_forced_answer_never_does() {
        assert_eq!(turn_thinking(TurnKind::Plan), ThinkingPolicy::Allow);
        assert_eq!(turn_thinking(TurnKind::Replan), ThinkingPolicy::Allow);
        assert_eq!(turn_thinking(TurnKind::WriteAnswer), ThinkingPolicy::Allow);
        assert_eq!(turn_thinking(TurnKind::Routine), ThinkingPolicy::RouterDefault);
        assert_eq!(turn_thinking(TurnKind::ForcedAnswer), ThinkingPolicy::Suppress);
    }

    #[test]
    fn only_a_non_thinking_routine_answer_is_rewritten() {
        assert!(rewrite_answer(TurnKind::Routine, false));
        assert!(!rewrite_answer(TurnKind::Routine, true), "it already thought");
        for k in [TurnKind::Plan, TurnKind::Replan, TurnKind::WriteAnswer, TurnKind::ForcedAnswer] {
            assert!(!rewrite_answer(k, false), "{k:?}");
        }
    }
}
