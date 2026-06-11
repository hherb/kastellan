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
//!   - `CompositeLifecycle` — production manager that dispatches by `entry.lifecycle`
//!     (added with the gliner-relex slice 2; lets a mixed registry hold both
//!     `Lifecycle::SingleUse` and `Lifecycle::IdleTimeout` entries side by side).
//!   - `WorkerHandle` — `&mut`-able holder of a live `SupervisedWorker`.
//!   - `RestartBackoff` — operator-tunable exponential backoff configuration.

pub mod composite;
pub mod force_route;
pub mod idle_timeout;
pub mod manager;
pub mod types;

pub use composite::CompositeLifecycle;
pub use force_route::{resolve_force_routing, ForceRoutingConfig, ProxyBinaryNotFound};
pub use idle_timeout::RestartBackoff;
pub use manager::{IdleTimeoutLifecycle, SingleUseLifecycle, WorkerHandle, WorkerLifecycleManager};
pub use types::{Contract, IdleTimeoutCaps, Lifecycle, LifecycleValidationError};
