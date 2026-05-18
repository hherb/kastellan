//! Worker lifecycle policy — slice 1 (single_use runtime + idle_timeout types) plus
//! slice 2 (idle_timeout runtime).
//!
//! See `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` for the
//! design contract.
//!
//! Public surface:
//!   - `Lifecycle`, `IdleTimeoutCaps`, `Contract` — pure value types declarable on a
//!     `ToolEntry`.
//!   - `WorkerLifecycleManager` — async trait that lends out `WorkerHandle`s.
//!   - `SingleUseLifecycle` — spawn-per-request impl.
//!   - `IdleTimeoutLifecycle` — warm-keeping impl (slice 2 runtime).
//!   - `WorkerHandle` — `&mut`-able holder of a live `SupervisedWorker`.
//!   - `RestartBackoff` — operator-tunable exponential backoff configuration.

pub mod idle_timeout;
pub mod manager;
pub mod types;

pub use idle_timeout::RestartBackoff;
pub use manager::{IdleTimeoutLifecycle, SingleUseLifecycle, WorkerHandle, WorkerLifecycleManager};
pub use types::{Contract, IdleTimeoutCaps, Lifecycle, LifecycleValidationError};
