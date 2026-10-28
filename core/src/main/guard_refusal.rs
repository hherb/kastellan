//! Record a guard tier that refused to boot as a `policy /
//! guard_tier.boot_refused` audit row before the daemon stops (#906).
//!
//! Postgres is up by then — the pool is built before the router config and
//! the guard tier — so the refusal can be a row and not only a line in a
//! rotating stderr log. The payload is closed tokens only
//! (`boot_report::refusal_payload`).
//!
//! **Thinned across restarts.** A crash-looping daemon refuses once per
//! restart, and no in-memory state survives one, so the thinning reads the
//! table: a refusal whose payload is already in a row within
//! `REFUSAL_ROW_INTERVAL` writes nothing — unless a `guard_tier.boot` row
//! (a boot that got past the guard tier) came after that row, which makes
//! this refusal a new outage. A failed lookup writes the row anyway — a
//! duplicate costs one row, a skipped row costs the evidence.
//!
//! **Bounded.** The daemon is about to exit with the refusal; a wedged
//! Postgres must not turn that loud stop into a silent hang, so the lookup
//! and the insert together get [`RECORD_BUDGET`].

use std::time::Duration;

use kastellan_core::cassandra::guard_model::boot_report::{
    record_refusal, refusal_payload, REFUSAL_ROW_INTERVAL, REFUSED_ACTION,
};
use kastellan_core::cassandra::guard_model::GuardTierError;

/// How long the refusal row may hold up the daemon's exit.
const RECORD_BUDGET: Duration = Duration::from_secs(5);

/// The action of a boot that got past the guard tier (`report_guard_tier`);
/// one after a refusal row ends that refusal's thinning.
pub(crate) const BOOT_ACTION: &str = "guard_tier.boot";

/// Write the refusal row (thinned, bounded); never fails — the caller is
/// already returning the refusal itself. The rules live in the lib's
/// `record_refusal`, where they are tested (#913); this binds them to the
/// real table.
pub(crate) async fn record_guard_refusal(pool: &sqlx::PgPool, e: &GuardTierError) {
    let payload = refusal_payload(e);
    let lookup = kastellan_db::audit::exists_within(
        pool,
        "policy",
        REFUSED_ACTION,
        &payload,
        REFUSAL_ROW_INTERVAL,
        BOOT_ACTION,
    );
    // No `RETURNING id` (#829): the id is unused, and its decode could fail
    // after the row is written.
    let insert =
        kastellan_db::audit::insert_without_id(pool, "policy", REFUSED_ACTION, payload.clone());
    record_refusal(&payload, lookup, insert, RECORD_BUDGET).await;
}
