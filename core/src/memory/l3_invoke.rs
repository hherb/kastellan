//! L3 skill invocation — the execution "DOOR". This module is a facade over
//! three seams, kept under the 500-LOC soft cap:
//!
//! - [`pure`] — argument parsing, template substitution, the trust gates,
//!   and the [`prepare_invocation`] decision. No I/O; deterministic and
//!   unit-testable.
//! - [`operator`] — the operator-CLI async orchestration ([`invoke_l3`]):
//!   dry-run by default, no CASSANDRA review (an operator running their own
//!   approved skill is authorised). Drives the existing sandboxed dispatcher.
//! - [`agent`] — the stricter pinned-only agent path ([`expand_for_agent`] +
//!   [`load_pinned_skill_by_name`]); the inner loop expands an agent-emitted
//!   `invoke_skill` directive through the unchanged review → dispatch → audit
//!   pipeline.
//!
//! Every public item is re-exported here, so existing `l3_invoke::<name>`
//! paths resolve unchanged.
//!
//! See `docs/superpowers/specs/2026-06-02-l3-skill-invocation-design.md`
//! and `docs/superpowers/specs/2026-06-04-l3-skill-autonomous-door-design.md`.

mod agent;
mod operator;
mod pure;

pub use agent::*;
pub use operator::*;
pub use pure::*;

#[cfg(test)]
mod tests;
