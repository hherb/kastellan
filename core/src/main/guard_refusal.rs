//! Record a guard tier that refused to boot as a `policy /
//! guard_tier.boot_refused` audit row before the daemon stops (#906).
//!
//! Postgres is up by then — the pool is built before the guard tier — so the
//! refusal can be a row and not only a line in a rotating stderr log. The
//! payload is closed tokens only (`boot_report::refusal_payload`).
//!
//! **Thinned across restarts.** A crash-looping daemon refuses once per
//! restart, and no in-memory state survives one, so the thinning reads the
//! table: a refusal whose identical payload already has a row within
//! `REFUSAL_ROW_INTERVAL` writes nothing. A failed lookup writes the row
//! anyway — a duplicate costs one row, a skipped row costs the evidence.
//!
//! **Bounded.** The daemon is about to exit with the refusal; a wedged
//! Postgres must not turn that loud stop into a silent hang, so the lookup
//! and the insert together get [`RECORD_BUDGET`].

use std::time::Duration;

use kastellan_core::cassandra::guard_model::boot_report::{
    refusal_payload, REFUSAL_ROW_INTERVAL, REFUSED_ACTION,
};
use kastellan_core::cassandra::guard_model::GuardTierError;

/// How long the refusal row may hold up the daemon's exit.
const RECORD_BUDGET: Duration = Duration::from_secs(5);

/// Write the refusal row (thinned, bounded); never fails — the caller is
/// already returning the refusal itself.
pub(crate) async fn record_guard_refusal(pool: &sqlx::PgPool, e: &GuardTierError) {
    let payload = refusal_payload(e);
    let write = async {
        match kastellan_db::audit::exists_within(
            pool,
            "policy",
            REFUSED_ACTION,
            &payload,
            REFUSAL_ROW_INTERVAL,
        )
        .await
        {
            Ok(true) => {
                tracing::info!(
                    payload = %payload,
                    "guard_tier.boot_refused already recorded within the last hour; not writing another"
                );
                return;
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(
                error = %err,
                "guard_tier.boot_refused thinning lookup failed; writing the row anyway"
            ),
        }
        if let Err(err) =
            kastellan_db::audit::insert(pool, "policy", REFUSED_ACTION, payload.clone()).await
        {
            tracing::error!(
                error = %err,
                payload = %payload,
                "guard_tier.boot_refused audit insert failed; this refusal is in the log only"
            );
        }
    };
    if tokio::time::timeout(RECORD_BUDGET, write).await.is_err() {
        tracing::error!(
            budget_ms = RECORD_BUDGET.as_millis() as u64,
            "guard_tier.boot_refused row not recorded in time; this refusal is in the log only"
        );
    }
}
