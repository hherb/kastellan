//! Notebook entry payloads, and the pure fold of a task's entries into its
//! current notebook (#922).
//!
//! Every row in `task_notebook` has a `kind` and a JSON `payload`; the types
//! here are those payloads, one per kind. [`fold`] replays a task's rows in
//! `seq` order and returns what the notebook says *now*: the step list with
//! each step's status, the progress summary, the findings so far, the
//! operator's corrections, and the classification of every step's data.
//!
//! The fold is strict. A payload that does not decode, a tick for a step
//! that is not listed, or a step list that reuses a finished step's id is a
//! [`FoldError`] — the loop then fails the task in words rather than guess
//! what the notebook meant.

use kastellan_db::notebook::{Entry, EntryKind};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::cassandra::types::DataClass;

/// One step of the model's plan: what it is for, which tool it expects to
/// use, and what "done" means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepSpec {
    pub id: u32,
    pub purpose: String,
    pub tool: String,
    pub done_when: String,
}

/// A fact the answer needs, as the model wrote it, and where it came from
/// (a message id, an attachment, a URL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FindingSpec {
    pub text: String,
    pub source: String,
}

/// `steps`: the first step list, or a replan replacing the pending steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepsPayload {
    pub steps: Vec<StepSpec>,
}

/// `tick`: a step done. `classification` is set **by the loop** from the
/// step's dispatched call, never by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TickPayload {
    pub step: u32,
    pub note: String,
    pub findings: Vec<FindingSpec>,
    pub summary: String,
    pub classification: DataClass,
}

/// `failed_step`: the step's call was refused or failed. Its data still
/// counts towards the answer's ceiling (C1), so it carries a classification
/// like a tick does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailedStepPayload {
    pub step: u32,
    pub code: String,
    pub detail: String,
    pub classification: DataClass,
}

/// `correction`: text the operator added while the task runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CorrectionPayload {
    pub text: String,
}

/// `answer`: the final answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnswerPayload {
    pub body: String,
}

/// `infeasible`: why the task cannot be done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InfeasiblePayload {
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepStatus {
    Pending,
    Done,
    Failed,
}

/// A step in the current list, with its status and (once ticked) its note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub spec: StepSpec,
    pub status: StepStatus,
    pub note: Option<String>,
}

/// A finding, with the step it came from and that step's classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub text: String,
    pub source: String,
    pub step: u32,
    pub classification: DataClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Correction {
    pub seq: i64,
    pub text: String,
}

/// What a task's notebook says now. Built only by [`fold`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NotebookState {
    pub steps: Vec<Step>,
    /// The model's own progress summary, from the latest tick.
    pub summary: Option<String>,
    pub findings: Vec<Finding>,
    /// In `seq` order.
    pub corrections: Vec<Correction>,
    /// The classification of every ticked or failed step's data (C1).
    pub touched: Vec<DataClass>,
    /// An `answer`, `infeasible` or `cancelled` entry was written.
    pub terminal: bool,
    /// The highest `seq` folded; 0 for an empty notebook.
    pub last_seq: i64,
}

impl NotebookState {
    /// The step the task is working on: the first one still pending.
    pub(crate) fn current_step(&self) -> Option<&StepSpec> {
        self.steps.iter().find(|s| s.status == StepStatus::Pending).map(|s| &s.spec)
    }

    /// The corrections written after `seen_seq` — the ones the model has not
    /// been shown yet.
    pub(crate) fn unseen_corrections(&self, seen_seq: i64) -> &[Correction] {
        let first = self.corrections.partition_point(|c| c.seq <= seen_seq);
        &self.corrections[first..]
    }
}

/// Why a notebook could not be read. The loop fails the task with it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("task notebook entry {seq} ({kind}): {detail}")]
pub(crate) struct FoldError {
    pub seq: i64,
    pub kind: &'static str,
    pub detail: String,
}

/// Replay `entries` (in `seq` order) into the notebook they describe. Pure.
pub(crate) fn fold(entries: &[Entry]) -> Result<NotebookState, FoldError> {
    let mut state = NotebookState::default();
    for entry in entries {
        let fail = |detail: String| FoldError { seq: entry.seq, kind: entry.kind.as_str(), detail };
        // Only a correction may land after the end: the operator can type
        // one while the last turn is still writing its answer.
        if state.terminal && entry.kind != EntryKind::Correction {
            return Err(fail("an entry after the task ended".into()));
        }
        match entry.kind {
            EntryKind::Steps => {
                let p: StepsPayload = decode(entry).map_err(fail)?;
                // A replan keeps what is finished and replaces what is not (D4).
                state.steps.retain(|s| s.status != StepStatus::Pending);
                for spec in p.steps {
                    if state.steps.iter().any(|s| s.spec.id == spec.id) {
                        return Err(fail(format!("step id {} is already listed", spec.id)));
                    }
                    state.steps.push(Step { spec, status: StepStatus::Pending, note: None });
                }
            }
            EntryKind::Tick => {
                let p: TickPayload = decode(entry).map_err(fail)?;
                let step = step_mut(&mut state, p.step).map_err(fail)?;
                step.status = StepStatus::Done;
                step.note = Some(p.note);
                state.findings.extend(p.findings.into_iter().map(|f| Finding {
                    text: f.text,
                    source: f.source,
                    step: p.step,
                    classification: p.classification,
                }));
                state.summary = Some(p.summary);
                state.touched.push(p.classification);
            }
            EntryKind::FailedStep => {
                let p: FailedStepPayload = decode(entry).map_err(fail)?;
                step_mut(&mut state, p.step).map_err(fail)?.status = StepStatus::Failed;
                state.touched.push(p.classification);
            }
            EntryKind::Correction => {
                let p: CorrectionPayload = decode(entry).map_err(fail)?;
                state.corrections.push(Correction { seq: entry.seq, text: p.text });
            }
            EntryKind::Answer => {
                decode::<AnswerPayload>(entry).map_err(fail)?;
                state.terminal = true;
            }
            EntryKind::Infeasible => {
                decode::<InfeasiblePayload>(entry).map_err(fail)?;
                state.terminal = true;
            }
            EntryKind::Cancelled => state.terminal = true,
        }
        state.last_seq = entry.seq;
    }
    Ok(state)
}

fn decode<T: DeserializeOwned>(entry: &Entry) -> Result<T, String> {
    serde_json::from_value(entry.payload.clone()).map_err(|e| format!("malformed payload: {e}"))
}

/// The pending step `id` — a tick or a failure for a step already finished
/// is as wrong as one for a step never listed.
fn step_mut(state: &mut NotebookState, id: u32) -> Result<&mut Step, String> {
    let step = state.steps.iter_mut().find(|s| s.spec.id == id).ok_or_else(|| format!("no step {id} is listed"))?;
    if step.status != StepStatus::Pending {
        return Err(format!("step {id} is already {:?}", step.status));
    }
    Ok(step)
}

#[cfg(test)]
mod tests;
