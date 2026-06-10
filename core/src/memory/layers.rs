//! L1 insight-index loader.
//!
//! L1 is the "always-in-context" memory layer (GenericAgent's design
//! thesis): a small set of hand-curated or programmatically promoted
//! routing pointers that the prompt assembler concatenates into every
//! system prompt regardless of the user's query. The hard caps below
//! exist because L1's whole purpose is "fits in the prompt
//! unconditionally"; a soft cap that sometimes overshoots defeats the
//! design.
//!
//! ## Layering vs. recall
//!
//! L1 load is a separate call, not a fourth recall lane. The three
//! existing lanes (semantic / lexical / graph) all use Reciprocal Rank
//! Fusion over a query; L1 is unconditional and query-independent —
//! fusing it would either require synthesising a fake rank or drop it
//! when the query matches nothing. Both are wrong. The future prompt
//! assembler will call [`load_l1`] and [`crate::memory::recall`]
//! separately and concatenate the results.
//!
//! ## Two caps catch two failure modes
//!
//! `cap_rows` bounds the number of L1 rows. `cap_bytes` bounds the
//! cumulative body length. Either alone would miss a class of
//! overshoot (many tiny rows blows row count; one fat row blows byte
//! count). Both apply, in that order: the DB caps the row count first,
//! the in-Rust loop applies the byte cap second.

use kastellan_db::memories::{load_layer, Memory, MemoryLayer};
use kastellan_db::DbError;
use sqlx::PgPool;

/// Default upper bound on L1 row count.
///
/// Picked to keep the L1 block scannable by the model in a single
/// attention sweep — small enough that the routing pointers don't crowd
/// out the actual task. 32 rows matches GenericAgent's L1 sizing
/// guidance for sub-30 K-token target windows.
pub const L1_DEFAULT_CAP_ROWS: usize = 32;

/// Default upper bound on the byte sum of L1 row bodies.
///
/// 4 KiB ≈ 1 K tokens at typical English+code density; about 3% of a
/// 30 K target window. The L1 block is supposed to be the prompt's
/// routing-table chrome, not its content — overshooting starves the
/// task.
pub const L1_DEFAULT_CAP_BYTES: usize = 4096;

/// Load L1 rows for prompt pinning.
///
/// Returns at most `cap_rows`, truncating earlier if pushing the next
/// row would make the cumulative body byte length *strictly exceed*
/// `cap_bytes`. The boundary is inclusive — rows that fill `cap_bytes`
/// exactly still fit. Rows come back newest-first
/// (`(created_at DESC, id DESC)` from
/// [`kastellan_db::memories::load_layer`]); the caller concatenates them
/// into the system prompt verbatim.
///
/// Returns `Ok(vec![])` when no L1 rows exist — that is the expected
/// state until something explicitly writes one. Not an error.
///
/// A row whose body alone exceeds `cap_bytes` is dropped (the byte
/// loop breaks before pushing it) and a `tracing::warn!` is emitted
/// with the row id and the over-budget size so an operator can either
/// retire the row or raise the budget. The conservative choice — an
/// over-budget single row would blow the prompt — but the drop is no
/// longer silent.
///
/// `cap_rows = 0` or `cap_bytes = 0` returns `Ok(vec![])` immediately
/// (the caller asked for nothing). Most callers should not pass `0`
/// by accident — prefer [`load_l1_default`], which pins the published
/// defaults so a fat-fingered `0` can't silently empty the L1 block.
pub async fn load_l1(
    pool: &PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<Memory>, DbError> {
    if cap_rows == 0 || cap_bytes == 0 {
        return Ok(Vec::new());
    }

    let candidates = load_layer(pool, MemoryLayer::Index, cap_rows).await?;

    let mut acc: Vec<Memory> = Vec::with_capacity(candidates.len());
    let mut bytes_used: usize = 0;
    for row in candidates {
        let row_bytes = row.body.len();
        // saturating_add: defense-in-depth against a future caller
        // somehow supplying a row whose body length wraps usize on
        // accumulation. Pinned by `bytes_used.saturating_add(row_bytes)
        // > cap_bytes` — overflow becomes "definitely over the cap,"
        // which is the safe direction.
        if bytes_used.saturating_add(row_bytes) > cap_bytes {
            // Distinguish "one row is by itself over budget" (operator
            // signal: retire the row or raise the cap) from "the
            // budget is just full" (expected exit condition). The
            // former gets a `tracing::warn!` with the offending id so
            // it surfaces in logs; the latter stays silent.
            if acc.is_empty() && row_bytes > cap_bytes {
                tracing::warn!(
                    memory_id = row.id,
                    row_bytes,
                    cap_bytes,
                    "load_l1: dropping L1 row whose body alone exceeds cap_bytes; \
                     prompt pinning will skip it"
                );
            }
            break;
        }
        bytes_used += row_bytes;
        acc.push(row);
    }
    Ok(acc)
}

/// Convenience wrapper over [`load_l1`] that pins the published
/// defaults ([`L1_DEFAULT_CAP_ROWS`], [`L1_DEFAULT_CAP_BYTES`]).
///
/// Prefer this from the prompt assembler. It exists specifically so
/// a caller cannot accidentally pass `cap_rows = 0` or `cap_bytes = 0`
/// (which silently empty the L1 block) — overriding the caps requires
/// calling [`load_l1`] explicitly, which forces the override to be
/// deliberate at the call site.
pub async fn load_l1_default(pool: &PgPool) -> Result<Vec<Memory>, DbError> {
    load_l1(pool, L1_DEFAULT_CAP_ROWS, L1_DEFAULT_CAP_BYTES).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure pin on the published default values. A silent drift to a
    /// larger row cap could push the L1 block past the prompt budget;
    /// a drift to a smaller byte cap could drop legitimate L1 rows.
    /// Either change should be deliberate, not accidental.
    #[test]
    fn l1_default_caps_pin() {
        assert_eq!(L1_DEFAULT_CAP_ROWS, 32);
        assert_eq!(L1_DEFAULT_CAP_BYTES, 4096);
    }

    /// `MemoryLayer::Index.as_db()` must equal the SMALLINT 1 stored in
    /// `memories.layer` for L1 rows, and `from_db(1)` must round-trip
    /// back to `Index`. Anything else means future readers of L1 rows
    /// would either miss them (wrong filter) or mis-classify them
    /// (wrong decode).
    #[test]
    fn memory_layer_round_trip_db_value() {
        assert_eq!(MemoryLayer::Index.as_db(), 1);
        assert_eq!(MemoryLayer::from_db(1).expect("decode 1"), MemoryLayer::Index);
    }

    /// `from_db` rejects values outside 0..=4 with `DbError::Invariant`.
    /// The DB CHECK constraint forbids them, so hitting this path means
    /// a schema invariant broke (not a transient query failure). The
    /// error variant choice is part of the contract — callers can
    /// distinguish "retry the query" from "the schema is wrong."
    #[test]
    fn memory_layer_from_db_rejects_out_of_range() {
        let err = MemoryLayer::from_db(5).expect_err("layer 5 must be rejected");
        match err {
            DbError::Invariant(msg) => {
                assert!(msg.contains("5"), "msg should name the bad value: {msg}");
                assert!(msg.contains("layer"), "msg should mention layer: {msg}");
            }
            other => panic!("expected DbError::Invariant, got {other:?}"),
        }
    }
}
