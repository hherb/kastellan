//! The task notebook (#922): the planner's external working memory.
//!
//! Instead of re-reading a growing history every turn, the model writes its
//! own step list, works one step per turn, ticks each step off with a short
//! note and the facts it found, and keeps a short progress summary. Those
//! events are rows in `task_notebook` (`kastellan_db::notebook`); this module
//! turns them into the next turn's prompt.
//!
//! * [`state`] — the entry payloads and the pure fold of a task's entries
//!   into its current notebook.
//! * [`render`] — the user message of one turn, screened and stable-first.
//! * [`reply`] — parse and check a turn's reply; turn it into a `Plan`.
//! * [`formulator`] — send one turn to the model.
//! * [`repeat`] — a call this task already made is not made again.
//! * [`thinking`] — which turns think (planning, replanning, writing the answer).
//! * [`run`] — the loop: one step per turn, every call reviewed and dispatched
//!   by the classic loop's own code.
//!
//! Design: `docs/superpowers/specs/2026-11-01-task-notebook-design.md`.

pub mod formulator;
pub(crate) mod render;
pub(crate) mod repeat;
pub(crate) mod reply;
pub(crate) mod run;
pub(crate) mod state;
pub(crate) mod thinking;
