//! The scrapbook (#922 `sift`, migration 0029): the items one sift kept,
//! with their source and the fields extracted from them, plus the items the
//! safety screen withheld.
//!
//! One sift writes its rows once, all together ([`append_rows`]); they are
//! read back by code only ([`rows`]) — for the answer's table and for a
//! follow-up that sifts an earlier turn's scrapbook. The runtime role may add
//! and read rows but never rewrite or delete one (0029's grants).

use sqlx::PgPool;

use crate::DbError;

/// The closed set of row kinds — must match 0029's `CHECK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The sorter said the item matches; `fields` holds what it extracted.
    Kept,
    /// The safety screen blocked the item's text, so no sorter saw it.
    Withheld,
}

impl RowKind {
    /// The `kind` column's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RowKind::Kept => "kept",
            RowKind::Withheld => "withheld",
        }
    }

    fn parse(s: &str) -> Result<Self, DbError> {
        match s {
            "kept" => Ok(RowKind::Kept),
            "withheld" => Ok(RowKind::Withheld),
            other => Err(DbError::Invariant(format!("task_scrapbook: unknown kind {other:?}"))),
        }
    }
}

/// A row to write; `row` numbers are assigned by [`append_rows`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewRow {
    pub kind: RowKind,
    pub source: serde_json::Value,
    pub fields: serde_json::Value,
    /// The class of the call that read the item, in `DataClass`'s spelling.
    pub classification: String,
}

/// One stored row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub task_id: i64,
    pub sift: i32,
    pub row: i32,
    pub kind: RowKind,
    pub source: serde_json::Value,
    pub fields: serde_json::Value,
    pub classification: String,
}

/// Write `rows` as sift `sift` of `task_id`, numbered from 1, in one
/// transaction: a sift's rows land together or not at all. A second append
/// to the same sift collides on row 1 and is refused whole.
///
/// Rows are records, so a NUL in one is escaped rather than refused
/// (`crate::nul`), as in the notebook.
pub async fn append_rows(pool: &PgPool, task_id: i64, sift: i32, rows: &[NewRow]) -> Result<(), DbError> {
    let err = |e: sqlx::Error| DbError::Query(format!("task_scrapbook append: {e}"));
    let mut tx = pool.begin().await.map_err(err)?;
    for (i, r) in rows.iter().enumerate() {
        let (source, n1) = crate::nul::escape_json(r.source.clone());
        let (fields, n2) = crate::nul::escape_json(r.fields.clone());
        if n1 + n2 > 0 {
            tracing::warn!(task_id, sift, nuls_escaped = n1 + n2, "task_scrapbook append: NUL escaped to U+2400 (#818)");
        }
        sqlx::query(
            "INSERT INTO task_scrapbook (task_id, sift, row, kind, source, fields, classification) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(task_id)
        .bind(sift)
        .bind(i as i32 + 1)
        .bind(r.kind.as_str())
        .bind(&source)
        .bind(&fields)
        .bind(&r.classification)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    }
    tx.commit().await.map_err(err)
}

/// Sift `sift` of `task_id`, in `row` order; empty for a sift with no rows.
pub async fn rows(pool: &PgPool, task_id: i64, sift: i32) -> Result<Vec<Row>, DbError> {
    let got: Vec<(i64, i32, i32, String, serde_json::Value, serde_json::Value, String)> = sqlx::query_as(
        "SELECT task_id, sift, row, kind, source, fields, classification FROM task_scrapbook \
         WHERE task_id = $1 AND sift = $2 ORDER BY row",
    )
    .bind(task_id)
    .bind(sift)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("task_scrapbook rows: {e}")))?;
    got.into_iter()
        .map(|(task_id, sift, row, kind, source, fields, classification)| {
            Ok(Row { task_id, sift, row, kind: RowKind::parse(&kind)?, source, fields, classification })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Rust set and 0029's `CHECK` must list the same kinds.
    #[test]
    fn the_kinds_match_the_migrations_check() {
        let sql = include_str!("../migrations/0029_task_scrapbook.sql");
        let check = &sql[sql.find("CHECK (kind IN (").unwrap()..];
        let check = &check[..check.find("))").unwrap()];
        for k in [RowKind::Kept, RowKind::Withheld] {
            assert!(check.contains(&format!("'{}'", k.as_str())), "{} missing from {check}", k.as_str());
            assert_eq!(RowKind::parse(k.as_str()).unwrap(), k);
        }
        assert_eq!(check.matches('\'').count() / 2, 2, "{check}");
        assert!(RowKind::parse("kept ").is_err());
    }
}
