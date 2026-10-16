//! What a failed [`super::insert_without_id`] says about its row (#858):
//! **not written**, or **unknown** — the row may be in the table.
//!
//! Why the difference matters: the channel bus's writers and the daemon's
//! audit sinks report a failed insert on the `[audit-lost]` marker, and an
//! operator reads "not written" as "this row is missing from `audit_log`".
//! That is true when Postgres answered the statement with an error, or when
//! the statement never reached Postgres. It is NOT known when the connection
//! broke while the statement may have been on its way, or after Postgres had
//! committed it but before its answer came back: the row may be there, and
//! only a query can say. Before #858 both cases became the same
//! `DbError::Query` string and were both called "not written".
//!
//! The classifier is [`outcome_of`], a pure function over `sqlx::Error`.

use crate::DbError;

/// Whether the row of a failed insert may still be in the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The row is not in the table: Postgres refused the statement, or the
    /// statement never reached Postgres.
    NotWritten,
    /// The row may be in the table: the connection failed while the
    /// statement may have been sent, or after it was committed. Only a
    /// query can tell.
    Unknown,
}

/// A failed [`super::insert_without_id`]: the error, and what it says about
/// the row ([`InsertOutcome`]).
///
/// It reads (`Display`) exactly as the [`DbError`] it wraps, so a line that
/// prints it is unchanged; the outcome is for the caller's own wording. It
/// converts into that `DbError` for a caller that has no use for the
/// outcome (`?` works).
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct InsertError {
    outcome: InsertOutcome,
    error: DbError,
}

impl InsertError {
    /// `error`, which leaves the row `outcome`.
    pub fn new(outcome: InsertOutcome, error: DbError) -> Self {
        Self { outcome, error }
    }

    /// Whether the row may still be in the table.
    pub fn outcome(&self) -> InsertOutcome {
        self.outcome
    }

    /// The error itself.
    pub fn error(&self) -> &DbError {
        &self.error
    }
}

impl From<InsertError> for DbError {
    fn from(value: InsertError) -> Self {
        value.error
    }
}

/// Pure: what an `INSERT` (run on its own, so Postgres commits it as it
/// finishes) that failed with `e` says about its row.
///
/// **Not written** — the error came before the statement could run, or is
/// Postgres's own answer to it:
/// - `Database`: Postgres answered with an error. An autocommitted statement
///   that fails is rolled back, so its row is not there. (A connection
///   Postgres ends *after* a commit — e.g. a shutdown while waiting on a
///   synchronous standby — does not send an error; the client sees the
///   connection close, an `Io` error below.)
/// - `PoolTimedOut`, `PoolClosed`: no connection was ever handed out. A
///   connection refused while connecting is retried until the timeout and
///   then reported as `PoolTimedOut`.
/// - `Configuration`, `InvalidArgument`, `Encode`: refused before anything
///   was sent.
///
/// **Unknown** — everything else, first of all `Io`, `Tls`, `Protocol` and
/// `WorkerCrashed`: the connection failed somewhere, possibly after the
/// statement was sent, possibly after it was committed. An `Io` error while
/// *connecting* (other than a refused connection) lands here too, although
/// no statement had been sent: `sqlx::Error` does not say which phase it
/// came from, and "may not have been written" over-hedges, which is the
/// safe direction. So does any variant this was not written for
/// (`sqlx::Error` is `#[non_exhaustive]`).
pub fn outcome_of(e: &sqlx::Error) -> InsertOutcome {
    match e {
        sqlx::Error::Database(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::Configuration(_)
        | sqlx::Error::InvalidArgument(_)
        | sqlx::Error::Encode(_) => InsertOutcome::NotWritten,
        _ => InsertOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #858: an error that came before the statement could run, or that is
    /// Postgres's own answer to it, means the row is not in the table. (A
    /// `Database` error cannot be built outside sqlx; `postgres_e2e` gets a
    /// real one from a read-only transaction.)
    #[test]
    fn an_error_before_or_from_the_statement_means_not_written() {
        let not_written = [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
            sqlx::Error::Configuration("bad url".into()),
            sqlx::Error::InvalidArgument("x".into()),
            sqlx::Error::Encode("x".into()),
        ];
        for e in not_written {
            assert_eq!(outcome_of(&e), InsertOutcome::NotWritten, "{e}");
        }
    }

    /// #858: a connection that failed while the statement may have been on
    /// its way, or after Postgres committed it, leaves the outcome unknown.
    #[test]
    fn a_broken_connection_leaves_the_outcome_unknown() {
        let unknown = [
            sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")),
            sqlx::Error::Io(std::io::ErrorKind::UnexpectedEof.into()),
            sqlx::Error::Protocol("unexpected message".into()),
            sqlx::Error::Tls("bad record".into()),
            sqlx::Error::WorkerCrashed,
            // Anything this classifier was not written for: the safe side.
            sqlx::Error::Decode("x".into()),
        ];
        for e in unknown {
            assert_eq!(outcome_of(&e), InsertOutcome::Unknown, "{e}");
        }
    }

    /// The failed insert reads as the `DbError` it wraps — the line the
    /// writers print is unchanged by the outcome riding along — and a caller
    /// that only wants a `DbError` can still `?` it into one.
    #[test]
    fn an_insert_error_reads_as_its_db_error() {
        let failed = InsertError::new(InsertOutcome::Unknown, DbError::Query("boom".into()));
        assert_eq!(failed.outcome(), InsertOutcome::Unknown);
        assert_eq!(failed.to_string(), "postgres query failed: boom");
        let db: DbError = failed.into();
        assert!(matches!(db, DbError::Query(m) if m == "boom"));
    }
}
