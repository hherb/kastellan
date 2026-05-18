//! Worker lifecycle policy — slice 1 (single_use runtime + idle_timeout types).
//!
//! See `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` for the
//! design contract and `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-1.md`
//! for the implementation plan this module realises.
//!
//! Public surface (filled in across the slice's TDD commits):
//!   - `Lifecycle`, `IdleTimeoutCaps`, `Contract` — pure value types declarable on a
//!     `ToolEntry`. (Task 1.)
//!   - `WorkerLifecycleManager`, `WorkerHandle`, `SingleUseLifecycle`,
//!     `IdleTimeoutLifecycle` — runtime layer. (Task 2.)

pub mod types;

pub use types::{Contract, IdleTimeoutCaps, Lifecycle, LifecycleValidationError};
