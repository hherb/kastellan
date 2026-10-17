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
//! The classifier is [`outcome_of`], a pure function over `sqlx::Error`; a
//! statement that ran but wrote other than one row is classified by
//! `super::one_row_written`.

use super::insert_cause::InsertCause;
use crate::DbError;

/// Whether the row of a failed insert may still be in the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The row is not in the table: Postgres refused the statement, or the
    /// statement never reached Postgres.
    NotWritten,
    /// The row may be in the table: the connection failed while the
    /// statement may have been sent, or after it was committed; Postgres
    /// crashed on it (a `PANIC`) or ended the session (`FATAL`); or the
    /// statement wrote more than the one row. "Not provably absent" — only a
    /// query can tell.
    Unknown,
}

/// A failed [`super::insert_without_id`]: the error, what it says about
/// the row ([`InsertOutcome`]), and what kind of failure it was
/// ([`InsertCause`], #859).
///
/// It reads (`Display`) exactly as the [`DbError`] it wraps, so a line that
/// prints it is unchanged; the outcome is for the caller's own wording.
///
/// Its outcome comes only from this crate's classifiers ([`outcome_of`],
/// `one_row_written`), so no caller can claim "not written" without the
/// evidence; and there is no `From` into `DbError`, so a `?` cannot drop the
/// outcome unseen ([`Self::into_db_error`] does it by name).
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct InsertError {
    outcome: InsertOutcome,
    cause: InsertCause,
    error: DbError,
}

impl InsertError {
    /// `error`, of class `cause`, which leaves the row `outcome` — for this
    /// crate's classifiers only.
    pub(crate) fn new(outcome: InsertOutcome, cause: InsertCause, error: DbError) -> Self {
        Self { outcome, cause, error }
    }

    /// Test fixtures outside this crate only: an `InsertError` whose outcome
    /// and cause are the caller's say-so, not a classifier's. Production code
    /// never builds one; it gets it from [`super::insert_without_id`].
    #[doc(hidden)]
    pub fn assumed_for_test(outcome: InsertOutcome, cause: InsertCause, error: DbError) -> Self {
        Self::new(outcome, cause, error)
    }

    /// Whether the row may still be in the table.
    pub fn outcome(&self) -> InsertOutcome {
        self.outcome
    }

    /// What kind of failure it was: the same for the same fault on any row
    /// (#859), so a writer thinning a flood can tell a new fault from the
    /// same one again.
    pub fn cause(&self) -> InsertCause {
        self.cause
    }

    /// The error itself.
    pub fn error(&self) -> &DbError {
        &self.error
    }

    /// The error, without its outcome: for a caller that does not report the
    /// row. One that does must word it from [`Self::outcome`] first — "not
    /// written" of an [`InsertOutcome::Unknown`] row is the claim #858 removed.
    pub fn into_db_error(self) -> DbError {
        self.error
    }
}

/// Pure: what an `INSERT` (run on its own, so Postgres commits it as it
/// finishes) that failed with `e` says about its row.
///
/// **Not written** — the error came before the statement could run, or is
/// Postgres's own answer to it:
/// - `Database` at severity `ERROR`: Postgres answered with an error. An
///   autocommitted statement that fails is rolled back, so its row is not
///   there. (A connection Postgres ends *after* a commit — e.g. a shutdown
///   while waiting on a synchronous standby — does not send an error; the
///   client sees the connection close, an `Io` error below.)
/// - `PoolTimedOut`, `PoolClosed`: no connection was ever handed out. A
///   connection refused while connecting is retried until the timeout and
///   then reported as `PoolTimedOut`.
/// - `Configuration`, `InvalidArgument`, `Encode`: refused before anything
///   was sent.
///
/// **Unknown** — a `Database` error at `PANIC` or `FATAL` (`answered_with`),
/// and everything else, first of all `Io`, `Tls`, `Protocol` and
/// `WorkerCrashed`: the connection failed somewhere, possibly after the
/// statement was sent, possibly after it was committed. An `Io` error while
/// *connecting* (other than a refused connection) lands here too, although
/// no statement had been sent: `sqlx::Error` does not say which phase it
/// came from, and "may not have been written" over-hedges, which is the
/// safe direction. So does any variant this was not written for
/// (`sqlx::Error` is `#[non_exhaustive]`).
pub fn outcome_of(e: &sqlx::Error) -> InsertOutcome {
    match e {
        // Not a Postgres error (no other driver is linked): the safe side.
        sqlx::Error::Database(db) => db
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .map_or(InsertOutcome::Unknown, |pg| answered_with(pg.severity())),
        sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::Configuration(_)
        | sqlx::Error::InvalidArgument(_)
        | sqlx::Error::Encode(_) => InsertOutcome::NotWritten,
        _ => InsertOutcome::Unknown,
    }
}

/// Pure: what an error answer at `severity` says about the row of an
/// autocommitted INSERT.
///
/// Only `ERROR` means rolled back. A `PANIC` is sent to the client before the
/// server aborts, and can come *after* the commit record is durable: inside
/// the commit's critical section every error is promoted to `PANIC` (a WAL
/// fsync that fails after its write, a commit-status read that fails after
/// the flush), and crash recovery replays the record — the row is there. A
/// `FATAL` ends the session; none is known to follow a commit, but "may not
/// have been written" over-hedges, the safe direction. The other severities
/// are never an error answer; were one, the same.
fn answered_with(severity: sqlx::postgres::PgSeverity) -> InsertOutcome {
    match severity {
        sqlx::postgres::PgSeverity::Error => InsertOutcome::NotWritten,
        _ => InsertOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #858: an error that came before the statement could run means the
    /// row is not in the table. (Postgres's own `ERROR` answer is
    /// [`answered_with`]'s; `postgres_e2e` gets a real one end to end, from
    /// a read-only transaction.)
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
    /// that only wants a `DbError` takes it by name.
    #[test]
    fn an_insert_error_reads_as_its_db_error() {
        let failed = InsertError::new(InsertOutcome::Unknown, InsertCause::RowCount, DbError::Query("boom".into()));
        assert_eq!(failed.outcome(), InsertOutcome::Unknown);
        assert_eq!(failed.cause(), InsertCause::RowCount);
        assert_eq!(failed.to_string(), "postgres query failed: boom");
        assert!(matches!(failed.into_db_error(), DbError::Query(m) if m == "boom"));
    }

    /// Only Postgres's `ERROR` answer means the statement was rolled back. A
    /// `PANIC` can follow a durable commit record, and a `FATAL` is hedged
    /// with it.
    #[test]
    fn only_an_error_answer_means_rolled_back() {
        use sqlx::postgres::PgSeverity as S;
        assert_eq!(answered_with(S::Error), InsertOutcome::NotWritten);
        for severity in [S::Panic, S::Fatal, S::Warning, S::Notice, S::Debug, S::Info, S::Log] {
            assert_eq!(answered_with(severity), InsertOutcome::Unknown, "{severity:?}");
        }
    }

    /// A `Database` error that is not Postgres's cannot be read for its
    /// severity, so it falls to the safe side — and a `Database` error is
    /// not classified by its variant alone.
    #[test]
    fn a_database_error_that_is_not_postgres_s_is_unknown() {
        #[derive(Debug)]
        struct NotPostgres;
        impl std::fmt::Display for NotPostgres {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("not postgres")
            }
        }
        impl std::error::Error for NotPostgres {}
        impl sqlx::error::DatabaseError for NotPostgres {
            fn message(&self) -> &str {
                "not postgres"
            }
            fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
                self
            }
            fn kind(&self) -> sqlx::error::ErrorKind {
                sqlx::error::ErrorKind::Other
            }
        }
        let e = sqlx::Error::Database(Box::new(NotPostgres));
        assert_eq!(outcome_of(&e), InsertOutcome::Unknown);
        // Nor for its SQLSTATE (#859): one class for every such error.
        assert_eq!(super::super::insert_cause::cause_of(&e), InsertCause::Client(super::super::insert_cause::ClientFault::Database));
    }
}
