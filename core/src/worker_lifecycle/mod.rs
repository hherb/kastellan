//! Worker lifecycle policy — slice 1 (single_use runtime + idle_timeout types).
//!
//! See `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` for the
//! design contract and `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-1.md`
//! for the implementation plan this module realises.
//!
//! Public surface:
//!   - `Lifecycle`, `IdleTimeoutCaps`, `Contract` — pure value types declarable on a
//!     `ToolEntry`.
//!   - `WorkerLifecycleManager` — async trait that lends out `WorkerHandle`s.
//!   - `SingleUseLifecycle` — production impl for slice 1; spawns one process per
//!     acquire and tears down on handle drop. Behaviour byte-equivalent to today's
//!     `scheduler::tool_dispatch::dispatch_step` spawn path.
//!   - `IdleTimeoutLifecycle` — stub impl; `acquire()` panics with `unimplemented!()`
//!     until slice 2 implements warm-keeping. Declarable at the type level today so
//!     downstream code can name it.
//!   - `WorkerHandle` — `&mut`-able holder of a live `SupervisedWorker`. Drop semantics
//!     for slice 1 just drops the inner worker (today's behaviour).

pub mod manager;
pub mod types;

pub use manager::{IdleTimeoutLifecycle, SingleUseLifecycle, WorkerHandle, WorkerLifecycleManager};
pub use types::{Contract, IdleTimeoutCaps, Lifecycle, LifecycleValidationError};
