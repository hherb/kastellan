//! Operator-facing entity review surface.
//!
//! Migration `0015_entity_kinds_and_quarantine.sql` introduced
//! `entities.quarantine BOOLEAN NOT NULL DEFAULT TRUE` — every newly
//! extracted entity is invisible to production `graph_search` (which
//! passes `include_quarantined = false`) until an operator reviews and
//! either approves (flip quarantine = FALSE), rejects (DELETE,
//! cascading memory_entities), or merges (consolidate near-duplicates
//! from extractor variance into one canonical row).
//!
//! This module owns the SQL surface for those operations. The CLI
//! consumer lives in `core/src/bin/hhagent-cli.rs` under the
//! `entities` subcommand tree; the audit wrapper lives in
//! `core::cli_audit`. Layout mirrors `db::tool_allowlists`.
//!
//! ## Grants
//!
//! No new migration. The runtime role already has full CRUD on
//! `entities` (migration `0002` default GRANT, never revoked) and
//! `memory_entities` rows cascade via the FK from migration `0007`.
//! `entity_kinds` (migration `0016` REVOKE) is deliberately untouched.

use crate::DbError;
use sqlx::PgPool;
use time::OffsetDateTime;

/// One row in the `entities` table joined with its mention count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRow {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub name_norm: String,
    pub quarantine: bool,
    pub created_at: OffsetDateTime,
    pub mention_count: i64,
}

/// CLI-surface filter on `quarantine` state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityState {
    /// Default — surfaces rows the operator hasn't reviewed yet.
    Quarantined,
    /// Already-approved rows (visible in production graph_search).
    Approved,
    /// No filter — for export / sanity dumps.
    Any,
}

/// One row in `list_entities` filter.
///
/// Use `ListFilter::default()` for the operator-friendly default
/// (quarantined / limit 50 / no other filters).
#[derive(Clone, Debug)]
pub struct ListFilter {
    pub kind: Option<String>,
    pub state: EntityState,
    pub limit: i64,
    pub since: Option<OffsetDateTime>,
    pub min_mentions: i64,
}

impl Default for ListFilter {
    fn default() -> Self {
        Self {
            kind: None,
            state: EntityState::Quarantined,
            limit: 50,
            since: None,
            min_mentions: 0,
        }
    }
}

/// One linked-memory preview row used by `get_entity_with_mentions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPreview {
    pub memory_id: i64,
    pub layer: i16,
    pub body_preview: String,
}

/// Three-variant outcome of `approve_entity`. Carries enough info for
/// the CLI to distinguish operator messages without a second DB probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApproveOutcome {
    Approved { kind: String, name: String },
    AlreadyApproved,
    NotFound,
}

/// Two-variant outcome of `reject_entity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectOutcome {
    Rejected {
        kind: String,
        name: String,
        mentions_dropped: i64,
    },
    NotFound,
}

/// Outcome of a successful merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOutcome {
    pub kept_id: i64,
    pub kept_kind: String,
    pub kept_name: String,
    pub dropped_ids: Vec<i64>,
    pub links_retargeted: i64,
    pub links_dropped_as_duplicate: i64,
}

/// Errors specific to the entities review surface. `Db` wraps the
/// generic `DbError`; the structured variants carry enough context that
/// the operator sees the offending id in the error message.
#[derive(Debug, thiserror::Error)]
pub enum EntitiesError {
    #[error("entity {0} not found")]
    NotFound(i64),
    #[error(
        "merge: kind mismatch — keep id {keep_id} is kind '{keep_kind}', \
         drop id {drop_id} is kind '{drop_kind}'"
    )]
    KindMismatch {
        keep_id: i64,
        keep_kind: String,
        drop_id: i64,
        drop_kind: String,
    },
    #[error("merge requires at least one --drop id")]
    NoDropIds,
    #[error("merge: --drop list contains keep id ({0})")]
    KeepInDropList(i64),
    #[error("database: {0}")]
    Db(#[from] DbError),
}

// ─────────────────────── pure helpers ───────────────────────

/// Validate the `keep_id` / `drop_ids` shape *before* any DB call. Pure
/// CPU; testable without a pool.
#[allow(dead_code)]
pub(crate) fn validate_merge_args(
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<(), EntitiesError> {
    if drop_ids.is_empty() {
        return Err(EntitiesError::NoDropIds);
    }
    if drop_ids.contains(&keep_id) {
        return Err(EntitiesError::KeepInDropList(keep_id));
    }
    Ok(())
}

/// Build a body preview suitable for the `show` command's
/// `linked memories` block: collapse newlines + multi-space runs to a
/// single space, then truncate to `max_chars` characters with no
/// trailing ellipsis (operators see the cap by the row width). Pure
/// CPU; testable without a pool.
pub(crate) fn body_preview(body: &str, max_chars: usize) -> String {
    let collapsed: String = body
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    // Collapse runs of whitespace to a single space.
    let mut out = String::with_capacity(collapsed.len());
    let mut prev_space = false;
    for c in collapsed.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim().to_string();
    if out.chars().count() > max_chars {
        out.chars().take(max_chars).collect()
    } else {
        out
    }
}

// ─────────────────────── I/O layer ───────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_preview_collapses_newlines_and_multispace() {
        let body = "line one\nline two\t\twith   gaps";
        assert_eq!(body_preview(body, 80), "line one line two with gaps");
    }

    #[test]
    fn body_preview_truncates_at_max_chars() {
        let body = "abcdefghij".repeat(10); // 100 chars
        let out = body_preview(&body, 30);
        assert_eq!(out.chars().count(), 30);
        assert!(body.starts_with(&out));
    }

    #[test]
    fn body_preview_trims_leading_and_trailing_whitespace() {
        let body = "   leading\nand trailing   ";
        assert_eq!(body_preview(body, 80), "leading and trailing");
    }

    #[test]
    fn validate_merge_args_rejects_empty_drops() {
        assert!(matches!(
            validate_merge_args(1, &[]),
            Err(EntitiesError::NoDropIds)
        ));
    }

    #[test]
    fn validate_merge_args_rejects_keep_in_drop_list() {
        assert!(matches!(
            validate_merge_args(5, &[3, 5, 7]),
            Err(EntitiesError::KeepInDropList(5))
        ));
    }

    #[test]
    fn validate_merge_args_accepts_well_formed_args() {
        assert!(validate_merge_args(1, &[2]).is_ok());
        assert!(validate_merge_args(1, &[2, 3, 4]).is_ok());
    }

    #[test]
    fn list_filter_default_is_quarantined_limit_50() {
        let f = ListFilter::default();
        assert_eq!(f.state, EntityState::Quarantined);
        assert_eq!(f.limit, 50);
        assert!(f.kind.is_none());
        assert!(f.since.is_none());
        assert_eq!(f.min_mentions, 0);
    }
}
