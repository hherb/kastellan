//! Python-skill invocation — the execution "DOOR" for agent-authored Python
//! skills, mirroring [`crate::memory::l3_invoke`] one payload over. A Python
//! skill runs as exactly one `python.exec` step (verbatim code, no params),
//! SHA-256-bound so the approved bytes are the executed bytes.
//!
//! - [`pure`] — the [`prepare_python_invocation`] decision gate (trust →
//!   re-validate → re-hash vs `stored_sha256`) and the one-step builder.
//!   No I/O; deterministic and unit-testable.
//! - [`operator`] — operator-CLI async orchestration ([`invoke_python_skill`]):
//!   dry-run by default, no CASSANDRA review (an operator running their own
//!   approved skill is authorised). Reuses the templated dispatcher + report.
//! - [`agent`] — the stricter pinned-only agent path
//!   ([`expand_python_for_agent`] + [`load_pinned_python_skill_by_name`]).
//!
//! See `docs/superpowers/specs/2026-06-13-python-exec-skill-catalog-design.md`.

mod agent;
mod operator;
mod pure;

#[allow(unused_imports)]
pub use agent::*;
pub use operator::*;
pub use pure::*;

#[cfg(test)]
mod tests;
