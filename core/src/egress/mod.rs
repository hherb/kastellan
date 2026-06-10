//! Host-side egress-proxy integration.
//!
//! Responsibilities:
//!   - [`audit`]: map proxy stdout decision lines to audit rows (pure) +
//!     [`audit::ingest_decisions_into`] (the runtime-free ingest loop).
//!   - [`spawn`]: spawn the sandboxed sidecar proxy on a per-worker UDS.
//!   - [`net_worker`]: couple a force-routed `Net::Allowlist` worker with its
//!     sidecar (slice #2) — [`net_worker::spawn_net_worker`] + the pure
//!     [`net_worker::rewrite_worker_policy`].
//!
//! The proxy never touches Postgres (core-only-DB invariant); decisions flow
//! proxy → core stdout-ingest → PG.

pub mod audit;
pub mod net_worker;
pub mod spawn;
