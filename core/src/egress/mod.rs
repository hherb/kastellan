//! Host-side egress-proxy integration.
//!
//! Responsibilities:
//!   - [`audit`]: map proxy stdout decision lines to audit rows (pure) +
//!     [`audit::ingest_decisions_into`] (the runtime-free ingest loop).
//!   - [`spawn`]: spawn the sandboxed sidecar proxy on a per-worker UDS.
//!   - [`net_worker`]: couple a force-routed `Net::Allowlist` worker with its
//!     sidecar (slice #2) — [`net_worker::spawn_net_worker`] + the pure
//!     [`net_worker::rewrite_worker_policy`].
//!   - [`cert_pins`]: parse the operator `KASTELLAN_EGRESS_CERT_PINS` config and
//!     select the per-worker pin subset handed to each sidecar (slice #4).
//!   - [`upstream_ca`]: parse the operator `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA`
//!     config and select the one extra trust anchor a force-routed worker's
//!     sidecar may use on its re-origination leg (#492) — fail-closed, and only
//!     for a single private origin.
//!
//! The proxy never touches Postgres (core-only-DB invariant); decisions flow
//! proxy → core stdout-ingest → PG.

pub mod audit;
pub mod cert_pins;
pub mod force_routing_notice;
pub mod leak_provision;
pub mod net_worker;
pub mod persistent_net;
pub mod scratch_sweep;
pub mod spawn;
pub mod upstream_ca;
