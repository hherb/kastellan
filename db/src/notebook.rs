//! The task notebook (#922, migration 0028): an append-only, per-task log of
//! the planner's working memory.
//!
//! Each row is one event — a step list, a step ticked off, a failed step, an
//! operator correction, or how the task ended. This module only stores and
//! reads rows; what they *mean* (the current step, the findings, …) is the
//! pure fold in `core::scheduler::notebook::state`.
//!
//! The runtime role may [`append`] and read ([`entries`]) but never rewrite
//! or delete a row (0028's grants), so a past notebook is a faithful record.
//! Every insert fires a `NOTIFY` on [`NOTIFY_CHANNEL`] with payload
//! `"<task_id>:<seq>"`, so a watcher can follow a task without polling.

use std::fmt;
use std::str::FromStr;

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// The `LISTEN` channel 0028's trigger notifies on. Payload: `"<task_id>:<seq>"`.
pub const NOTIFY_CHANNEL: &str = "task_notebook";

/// The closed set of entry kinds — must match 0028's `CHECK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A step list: the first one, or a replan replacing the pending steps.
    Steps,
    /// A step done: its note, the facts found, the new progress summary.
    Tick,
    /// A step whose call was refused or failed.
    FailedStep,
    /// An operator correction, added while the task runs.
    Correction,
    /// The final answer.
    Answer,
    /// The model's reason the task cannot be done (a terminal answer).
    Infeasible,
    /// The task was cancelled.
    Cancelled,
}

impl EntryKind {
    /// Every kind, in declaration order (for tests that walk the set).
    pub const ALL: [EntryKind; 7] = [
        EntryKind::Steps,
        EntryKind::Tick,
        EntryKind::FailedStep,
        EntryKind::Correction,
        EntryKind::Answer,
        EntryKind::Infeasible,
        EntryKind::Cancelled,
    ];

    /// The `kind` column's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Steps => "steps",
            EntryKind::Tick => "tick",
            EntryKind::FailedStep => "failed_step",
            EntryKind::Correction => "correction",
            EntryKind::Answer => "answer",
            EntryKind::Infeasible => "infeasible",
            EntryKind::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EntryKind {
    type Err = DbError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        EntryKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| DbError::Invariant(format!("task_notebook: unknown kind {s:?}")))
    }
}

/// One notebook row.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub task_id: i64,
    pub seq: i64,
    pub kind: EntryKind,
    pub payload: serde_json::Value,
    pub created_at: OffsetDateTime,
}

/// Append one entry to `task_id`'s notebook and return its `seq`.
///
/// `seq` is the task's highest plus one. Two writers exist (the planner loop
/// and an operator correction), so the read of the highest and the insert
/// run under a transaction-scoped advisory lock keyed on the task id:
/// concurrent appends to one task queue up instead of racing for the same
/// number, and appends to different tasks never wait on each other. (A
/// "retry on duplicate key" loop can lose the race several times in a row
/// under two tight writers; the lock cannot.)
///
/// The payload is a record, so a NUL in it is escaped rather than refused
/// (`crate::nul`): a refused row would lose a step the task already took.
pub async fn append(
    pool: &PgPool,
    task_id: i64,
    kind: EntryKind,
    payload: serde_json::Value,
) -> Result<i64, DbError> {
    let (payload, nuls) = crate::nul::escape_json(payload);
    if nuls > 0 {
        tracing::warn!(task_id, kind = %kind, nuls_escaped = nuls, "task_notebook append: NUL escaped to U+2400 (#818)");
    }
    let err = |e: sqlx::Error| DbError::Query(format!("task_notebook append: {e}"));
    let mut tx = pool.begin().await.map_err(err)?;
    // Released at COMMIT/ROLLBACK. No other code in the tree takes advisory
    // locks, so the bare task id is an unambiguous key.
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(task_id).execute(&mut *tx).await.map_err(err)?;
    let seq = sqlx::query_scalar::<_, i64>(
        "INSERT INTO task_notebook (task_id, seq, kind, payload) \
         SELECT $1, COALESCE(MAX(seq), 0) + 1, $2, $3 FROM task_notebook WHERE task_id = $1 \
         RETURNING seq",
    )
    .bind(task_id)
    .bind(kind.as_str())
    .bind(&payload)
    .fetch_one(&mut *tx)
    .await
    .map_err(err)?;
    tx.commit().await.map_err(err)?;
    Ok(seq)
}

/// `task_id`'s entries with `seq > after_seq`, in `seq` order. `after_seq = 0`
/// reads the whole notebook.
pub async fn entries(pool: &PgPool, task_id: i64, after_seq: i64) -> Result<Vec<Entry>, DbError> {
    let rows: Vec<(i64, i64, String, serde_json::Value, OffsetDateTime)> = sqlx::query_as(
        "SELECT task_id, seq, kind, payload, created_at FROM task_notebook \
         WHERE task_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(task_id)
    .bind(after_seq)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("task_notebook entries: {e}")))?;
    rows.into_iter()
        .map(|(task_id, seq, kind, payload, created_at)| {
            Ok(Entry { task_id, seq, kind: kind.parse()?, payload, created_at })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_its_spelling() {
        for k in EntryKind::ALL {
            assert_eq!(k.as_str().parse::<EntryKind>().unwrap(), k);
        }
        assert!("tick ".parse::<EntryKind>().is_err());
    }

    /// The Rust set and 0028's `CHECK` must list the same kinds, or a kind
    /// the loop writes is refused by the table (or one the table allows is
    /// refused by the fold).
    #[test]
    fn the_kinds_match_the_migrations_check() {
        let sql = include_str!("../migrations/0028_task_notebook.sql");
        let check = &sql[sql.find("CHECK (kind IN (").unwrap()..];
        let check = &check[..check.find("))").unwrap()];
        let listed = check.matches('\'').count() / 2;
        assert_eq!(listed, EntryKind::ALL.len(), "{check}");
        for k in EntryKind::ALL {
            assert!(check.contains(&format!("'{}'", k.as_str())), "{k} missing from {check}");
        }
    }
}
