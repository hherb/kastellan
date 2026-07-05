//! Read surface of the entity review module: `list_entities` and
//! `get_entity_with_mentions` plus their preview caps.
//!
//! Split out of the parent `entities.rs` (2026-07-05, Item 9b file-size
//! pass) — see the parent module doc for the split provenance. Function
//! bodies are verbatim moves; every symbol is re-exported from the
//! parent so `kastellan_db::entities::…` paths are unchanged.

use crate::DbError;
use sqlx::PgPool;
use time::OffsetDateTime;

use super::{body_preview, EntitiesError, EntityRow, EntityState, ListFilter, MemoryPreview};

/// List entities matching `filter`, joined with their `memory_entities`
/// count. Ordering is `created_at DESC, id DESC` so the operator sees
/// the most-recent entities first.
pub async fn list_entities(
    pool: &PgPool,
    filter: &ListFilter,
) -> Result<Vec<EntityRow>, EntitiesError> {
    let quarantine_filter: Option<bool> = match filter.state {
        EntityState::Quarantined => Some(true),
        EntityState::Approved => Some(false),
        EntityState::Any => None,
    };
    let rows: Vec<(
        i64,            // id
        String,         // kind
        String,         // name
        String,         // name_norm
        bool,           // quarantine
        OffsetDateTime, // created_at
        i64,            // mention_count
    )> = sqlx::query_as(
        r#"
        SELECT e.id, e.kind, e.name, e.name_norm, e.quarantine,
               e.created_at,
               COUNT(me.memory_id)::BIGINT AS mention_count
        FROM entities e
        LEFT JOIN memory_entities me ON me.entity_id = e.id
        WHERE
              ($1::TEXT        IS NULL OR e.kind        = $1)
          AND ($2::BOOL        IS NULL OR e.quarantine  = $2)
          AND ($3::TIMESTAMPTZ IS NULL OR e.created_at >= $3)
        GROUP BY e.id
        HAVING COUNT(me.memory_id) >= $4
        ORDER BY e.created_at DESC, e.id DESC
        LIMIT $5
        "#,
    )
    .bind(filter.kind.as_deref())
    .bind(quarantine_filter)
    .bind(filter.since)
    .bind(filter.min_mentions)
    .bind(filter.limit)
    .fetch_all(pool)
    .await
    .map_err(|e| EntitiesError::Db(DbError::Query(format!("list_entities: {e}"))))?;
    Ok(rows
        .into_iter()
        .map(
            |(id, kind, name, name_norm, quarantine, created_at, mention_count)| EntityRow {
                id,
                kind,
                name,
                name_norm,
                quarantine,
                created_at,
                mention_count,
            },
        )
        .collect())
}

/// Per-entity preview row cap for `get_entity_with_mentions`.
pub const SHOW_LINKED_MEMORIES_CAP: i64 = 10;
/// Per-memory body preview character cap.
pub const SHOW_BODY_PREVIEW_CHARS: usize = 80;

/// Fetch the entity row and up to `SHOW_LINKED_MEMORIES_CAP` linked
/// memory previews. Returns `Ok(None)` if no entity at that id.
pub async fn get_entity_with_mentions(
    pool: &PgPool,
    id: i64,
) -> Result<Option<(EntityRow, Vec<MemoryPreview>)>, EntitiesError> {
    let entity: Option<(i64, String, String, String, bool, OffsetDateTime, i64)> =
        sqlx::query_as(
            r#"
            SELECT e.id, e.kind, e.name, e.name_norm, e.quarantine, e.created_at,
                   (SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1)::BIGINT
                       AS mention_count
            FROM entities e
            WHERE e.id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!(
                "get_entity_with_mentions: select entity {id}: {e}"
            )))
        })?;
    let row = match entity {
        None => return Ok(None),
        Some(r) => r,
    };
    let entity_row = EntityRow {
        id: row.0,
        kind: row.1,
        name: row.2,
        name_norm: row.3,
        quarantine: row.4,
        created_at: row.5,
        mention_count: row.6,
    };

    // Linked memories — pull body + layer for the preview.
    let mems: Vec<(i64, i16, String)> = sqlx::query_as(
        r#"
        SELECT m.id, m.layer, m.body
        FROM memory_entities me
        JOIN memories m ON m.id = me.memory_id
        WHERE me.entity_id = $1
        ORDER BY m.id DESC
        LIMIT $2
        "#,
    )
    .bind(id)
    .bind(SHOW_LINKED_MEMORIES_CAP)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!(
            "get_entity_with_mentions: select memories for {id}: {e}"
        )))
    })?;
    let previews = mems
        .into_iter()
        .map(|(memory_id, layer, body)| MemoryPreview {
            memory_id,
            layer,
            body_preview: body_preview(&body, SHOW_BODY_PREVIEW_CHARS),
        })
        .collect();

    Ok(Some((entity_row, previews)))
}
