//! Write surface of the entity review module: the three transactional
//! operator actions — `approve_entity`, `reject_entity`,
//! `merge_entities`. Each runs in a single transaction with
//! `SELECT … FOR UPDATE` row locks so concurrent operator runs
//! serialise instead of racing.
//!
//! Split out of the parent `entities.rs` (2026-07-05, Item 9b file-size
//! pass) — see the parent module doc for the split provenance. Function
//! bodies are verbatim moves; every symbol is re-exported from the
//! parent so `kastellan_db::entities::…` paths are unchanged.

use crate::DbError;
use sqlx::PgPool;

use super::{
    validate_merge_args, ApproveOutcome, EntitiesError, MergeOutcome, RejectOutcome,
};

/// Flip `quarantine` from TRUE to FALSE for one entity inside a
/// single transaction, distinguishing already-approved from not-found.
///
/// Transaction shape:
///   BEGIN;
///   SELECT id, kind, name, quarantine FROM entities WHERE id = $1 FOR UPDATE;
///   -- branch on observed state:
///   --   None                -> COMMIT, return NotFound
///   --   quarantine = FALSE  -> COMMIT, return AlreadyApproved
///   --   quarantine = TRUE   -> UPDATE … SET quarantine = FALSE, COMMIT,
///                               return Approved { kind, name }
pub async fn approve_entity(
    pool: &PgPool,
    id: i64,
) -> Result<ApproveOutcome, EntitiesError> {
    let mut tx = pool.begin().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("approve_entity begin: {e}")))
    })?;
    // FOR UPDATE: serialise concurrent operator runs and block approve+reject races.
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT kind, name, quarantine FROM entities WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("approve_entity select {id}: {e}")))
    })?;
    let (kind, name, quarantine) = match row {
        None => {
            tx.commit().await.map_err(|e| {
                EntitiesError::Db(DbError::Query(format!("approve_entity commit not-found {id}: {e}")))
            })?;
            return Ok(ApproveOutcome::NotFound);
        }
        Some(t) => t,
    };
    if !quarantine {
        tx.commit().await.map_err(|e| {
            EntitiesError::Db(DbError::Query(format!("approve_entity commit already-approved {id}: {e}")))
        })?;
        return Ok(ApproveOutcome::AlreadyApproved);
    }
    sqlx::query("UPDATE entities SET quarantine = FALSE, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!("approve_entity update {id}: {e}")))
        })?;
    tx.commit().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("approve_entity commit {id}: {e}")))
    })?;
    Ok(ApproveOutcome::Approved { kind, name })
}

/// Delete one entity inside a single transaction, capturing the cascade
/// row count from `memory_entities` for the audit-row payload.
///
/// Transaction shape:
///   BEGIN;
///   SELECT id, kind, name FROM entities WHERE id = $1 FOR UPDATE;
///   -- on None -> COMMIT, return NotFound
///   SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1;
///   DELETE FROM entities WHERE id = $1;   -- cascades memory_entities
///   COMMIT;
pub async fn reject_entity(
    pool: &PgPool,
    id: i64,
) -> Result<RejectOutcome, EntitiesError> {
    let mut tx = pool.begin().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity begin: {e}")))
    })?;
    // FOR UPDATE: serialise concurrent operator runs and block approve+reject races.
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT kind, name FROM entities WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity select {id}: {e}")))
    })?;
    let (kind, name) = match row {
        None => {
            tx.commit().await.map_err(|e| {
                EntitiesError::Db(DbError::Query(format!("reject_entity commit not-found {id}: {e}")))
            })?;
            return Ok(RejectOutcome::NotFound);
        }
        Some(t) => t,
    };
    // Note: a tiny TOCTOU window exists here. The auto-linker (which
    // INSERTs into memory_entities) can fire AFTER this COUNT but BEFORE
    // the DELETE — those rows will be cascaded away correctly but won't
    // appear in the mentions_dropped audit value. The entity row lock
    // does NOT block inserts into memory_entities (different table).
    // Acceptable: data integrity is preserved; only the audit counter
    // can undercount in a narrow race. Not worth advisory-lock complexity.
    let mentions_dropped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity count {id}: {e}")))
    })?;
    sqlx::query("DELETE FROM entities WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!("reject_entity delete {id}: {e}")))
        })?;
    tx.commit().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity commit {id}: {e}")))
    })?;
    Ok(RejectOutcome::Rejected { kind, name, mentions_dropped })
}

/// Single-transaction merge: validate -> SELECT FOR UPDATE on keep + all
/// drops -> precondition check (same kind for every drop) -> INSERT new
/// memory_entities rows pointing at keep, ON CONFLICT DO NOTHING (so a
/// memory already linked to both keep and a drop is consolidated, not
/// duplicated) -> count the consolidated-as-duplicate rows -> DELETE
/// drops (cascading the old memory_entities) -> COMMIT.
///
/// Returns the outcome; on precondition failure returns
/// EntitiesError::{NotFound, KindMismatch, NoDropIds, KeepInDropList}
/// with the transaction rolled back (no DB writes).
pub async fn merge_entities(
    pool: &PgPool,
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<MergeOutcome, EntitiesError> {
    validate_merge_args(keep_id, drop_ids)?;
    let mut tx = pool.begin().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities begin: {e}")))
    })?;

    // FOR UPDATE: serialise concurrent operator runs and block approve+reject races.
    let keep_row: Option<(String, String)> = sqlx::query_as(
        "SELECT kind, name FROM entities WHERE id = $1 FOR UPDATE",
    )
    .bind(keep_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!(
            "merge_entities select keep {keep_id}: {e}"
        )))
    })?;
    let (keep_kind, keep_name) = match keep_row {
        None => {
            tx.rollback().await.map_err(|e| {
                EntitiesError::Db(DbError::Query(format!("merge_entities rollback keep-missing {keep_id}: {e}")))
            })?;
            return Err(EntitiesError::NotFound(keep_id));
        }
        Some(t) => t,
    };

    // Lock each drop + verify kind. ORDER BY id is load-bearing: it
    // forces a consistent lock-acquisition order so two concurrent
    // merge_entities calls with overlapping drop sets cannot deadlock.
    // The kind-mismatch check loops over the result and compares to
    // keep_kind, surfacing the first offending id.
    let drop_rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, kind FROM entities WHERE id = ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(drop_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!(
            "merge_entities select drops: {e}"
        )))
    })?;
    let found_ids: std::collections::BTreeSet<i64> = drop_rows.iter().map(|(id, _)| *id).collect();
    for did in drop_ids {
        if !found_ids.contains(did) {
            tx.rollback().await.map_err(|e| {
                EntitiesError::Db(DbError::Query(format!("merge_entities rollback drop-missing {did}: {e}")))
            })?;
            return Err(EntitiesError::NotFound(*did));
        }
    }
    for (drop_id, drop_kind) in &drop_rows {
        if drop_kind != &keep_kind {
            tx.rollback().await.map_err(|e| {
                EntitiesError::Db(DbError::Query(format!("merge_entities rollback kind-mismatch keep={keep_id} drop={drop_id}: {e}")))
            })?;
            return Err(EntitiesError::KindMismatch {
                keep_id,
                keep_kind: keep_kind.clone(),
                drop_id: *drop_id,
                drop_kind: drop_kind.clone(),
            });
        }
    }

    // Count duplicate links — rows in memory_entities where entity_id is
    // a drop AND memory_id is also linked to keep. Counts ROWS, not
    // DISTINCT memories: if a memory is linked to two different drops
    // AND to keep, it adds 2 here. links_retargeted (the ON-CONFLICT
    // INSERT rows_affected) reports distinct retargets, so the two
    // counters report different facets: this one is "links absorbed by
    // dedup", the other is "unique memories newly visible from keep".
    let dup_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM memory_entities WHERE entity_id = ANY($1)
        AND memory_id IN (SELECT memory_id FROM memory_entities WHERE entity_id = $2)
        "#,
    )
    .bind(drop_ids)
    .bind(keep_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities dup_count: {e}")))
    })?;

    // Retarget. ON CONFLICT DO NOTHING absorbs the duplicates; the
    // rows_affected of the INSERT is the count of UNIQUE memory_ids
    // gained by keep — i.e. links_retargeted.
    let res = sqlx::query(
        r#"
        INSERT INTO memory_entities (memory_id, entity_id)
        SELECT DISTINCT memory_id, $1
        FROM memory_entities
        WHERE entity_id = ANY($2)
        ON CONFLICT (memory_id, entity_id) DO NOTHING
        "#,
    )
    .bind(keep_id)
    .bind(drop_ids)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities retarget: {e}")))
    })?;
    let links_retargeted = res.rows_affected() as i64;

    // Drop the source entities. Cascade removes the old links.
    sqlx::query("DELETE FROM entities WHERE id = ANY($1)")
        .bind(drop_ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!("merge_entities delete drops: {e}")))
        })?;

    tx.commit().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities commit: {e}")))
    })?;
    Ok(MergeOutcome {
        kept_id: keep_id,
        kept_kind: keep_kind,
        kept_name: keep_name,
        dropped_ids: drop_ids.to_vec(),
        links_retargeted,
        links_dropped_as_duplicate: dup_count,
    })
}
