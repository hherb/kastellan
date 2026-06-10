//! Seed `tool_allowlists` rows for integration tests.
//!
//! Tests that bring up the `kastellan` daemon and want it to see a
//! populated argv allowlist can call this between PG cluster bring-up
//! and daemon start. Bypasses the CLI binary for setup speed.

use sqlx::PgPool;

/// Bulk-INSERT one entry per `argv0` for the given `tool`. Uses
/// `created_by = "test"` so the rows are visibly test fixtures. No-op
/// on an empty `argv0s` slice.
pub async fn seed_tool_allowlist(
    pool: &PgPool,
    tool: &str,
    argv0s: &[&str],
) -> Result<(), sqlx::Error> {
    for &argv0 in argv0s {
        sqlx::query(
            "INSERT INTO tool_allowlists (tool, argv0, created_by)
             VALUES ($1, $2, 'test')
             ON CONFLICT (tool, argv0) DO NOTHING",
        )
        .bind(tool)
        .bind(argv0)
        .execute(pool)
        .await?;
    }
    Ok(())
}
