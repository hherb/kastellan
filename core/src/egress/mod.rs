//! Host-side egress-proxy integration (slice #1).
//!
//! Two responsibilities, both reusable and **not yet wired into `tool_host`**
//! (that hookup lands in slice #2 with force-routing):
//!   - [`audit`]: map a proxy stdout decision line to an audit row (pure).
//!   - [`spawn`]: spawn the sandboxed sidecar proxy on a per-worker UDS.
//!
//! The proxy never touches Postgres (core-only-DB invariant); decisions flow
//! proxy → core stdout-ingest → PG.

pub mod audit;
pub mod spawn;
