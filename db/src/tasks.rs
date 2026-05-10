//! Typed CRUD against the `tasks` table.
//!
//! All writes go through this module; the scheduler never builds raw
//! SQL. Reads are typed too (no `serde_json::Value` leaking out where
//! a `Task` would do).

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row;
use time::OffsetDateTime;

use crate::DbError;

/// The two concurrency lanes. `fast` is the default; `long` is opt-in
/// via the producer (CLI flag, channel adapter default, etc.).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Fast,
    Long,
}

impl Lane {
    pub fn as_sql(self) -> &'static str {
        match self {
            Lane::Fast => "fast",
            Lane::Long => "long",
        }
    }

    pub fn from_sql(s: &str) -> Result<Self, DbError> {
        match s {
            "fast" => Ok(Lane::Fast),
            "long" => Ok(Lane::Long),
            other => Err(DbError::Other(format!("unknown lane: {other}"))),
        }
    }
}

/// Default deadlines per lane. Used at claim time when the producer
/// does not pin `payload.deadline_seconds` itself.
pub const DEFAULT_DEADLINE_FAST_S: i64 = 60;
pub const DEFAULT_DEADLINE_LONG_S: i64 = 30 * 60;

/// Default plan-iteration caps per lane. Mirror values in
/// `core::scheduler` so a producer omitting the cap gets the same
/// behaviour as the runner enforces.
pub const DEFAULT_MAX_PLANS_FAST: u32 = 3;
pub const DEFAULT_MAX_PLANS_LONG: u32 = 12;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_round_trips_through_sql_string() {
        assert_eq!(Lane::Fast.as_sql(), "fast");
        assert_eq!(Lane::Long.as_sql(), "long");
        assert_eq!(Lane::from_sql("fast").unwrap(), Lane::Fast);
        assert_eq!(Lane::from_sql("long").unwrap(), Lane::Long);
        assert!(Lane::from_sql("medium").is_err());
    }
}
