//! What *kind* of failure a failed [`super::insert_without_id`] was (#859):
//! a small, comparable class — Postgres's SQLSTATE, the I/O error's kind,
//! or the client error's variant — never the error's text.
//!
//! Why it exists: the writers that report lost audit rows thin their lines
//! past a flood (`kastellan_core::audit_thinning`). Past the first few rows
//! of a burst only every 2^k-th row gets a line, so a row that failed for a
//! *different reason* in the middle of a flood (read-only, then a REVOKE,
//! then disk full) would be counted and never described. The thinning gives
//! the first row of each new cause its own line — and to tell "new" from
//! "the same again" it needs a key that is the same for the same fault.
//!
//! **Not the error's text.** A Postgres message can carry the row's own
//! values (`Key (id)=(5) already exists`), so every row of one fault would
//! read as a new cause and the flood would get a line per row — the very
//! thing the thinning is for. A SQLSTATE is one of a few hundred fixed
//! codes; an [`std::io::ErrorKind`] and a variant name are fixed too.

/// A five-character Postgres SQLSTATE, e.g. `25006` (read-only transaction).
///
/// Only built from five ASCII alphanumerics ([`SqlState::parse`]), so
/// [`SqlState::as_str`] cannot fail.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlState([u8; 5]);

impl SqlState {
    /// Pure: `code` as a SQLSTATE, or `None` unless it is exactly five ASCII
    /// letters or digits — the form every Postgres error code has.
    pub fn parse(code: &str) -> Option<Self> {
        let bytes: [u8; 5] = code.as_bytes().try_into().ok()?;
        bytes.iter().all(u8::is_ascii_alphanumeric).then_some(Self(bytes))
    }

    /// The code, e.g. `"25006"`.
    pub fn as_str(&self) -> &str {
        // ASCII by construction (`parse`), so always valid UTF-8.
        std::str::from_utf8(&self.0).unwrap_or("?????")
    }
}

impl std::fmt::Debug for SqlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SqlState({})", self.as_str())
    }
}

/// The class of a failed insert: equal for the same fault, whatever row it
/// hit. See the module docs for why it is never the error's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InsertCause {
    /// Postgres answered the statement with an error: its SQLSTATE.
    Answered(SqlState),
    /// The connection failed with an I/O error of this kind.
    Io(std::io::ErrorKind),
    /// Any other client-side error, by its `sqlx::Error` variant (e.g.
    /// `"pool timed out"`) — or `"database"` for a database error that is
    /// not Postgres's, `"other"` for a variant this was not written for.
    Client(&'static str),
    /// The statement ran but wrote other than the one row.
    RowCount,
}

/// Pure: the class of an `INSERT` that failed with `e`.
pub fn cause_of(e: &sqlx::Error) -> InsertCause {
    match e {
        sqlx::Error::Database(db) => db
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .and_then(|pg| SqlState::parse(pg.code()))
            .map_or(InsertCause::Client("database"), InsertCause::Answered),
        sqlx::Error::Io(io) => InsertCause::Io(io.kind()),
        sqlx::Error::PoolTimedOut => InsertCause::Client("pool timed out"),
        sqlx::Error::PoolClosed => InsertCause::Client("pool closed"),
        sqlx::Error::Configuration(_) => InsertCause::Client("configuration"),
        sqlx::Error::InvalidArgument(_) => InsertCause::Client("invalid argument"),
        sqlx::Error::Encode(_) => InsertCause::Client("encode"),
        sqlx::Error::Decode(_) => InsertCause::Client("decode"),
        sqlx::Error::Tls(_) => InsertCause::Client("tls"),
        sqlx::Error::Protocol(_) => InsertCause::Client("protocol"),
        sqlx::Error::WorkerCrashed => InsertCause::Client("worker crashed"),
        // `sqlx::Error` is `#[non_exhaustive]`: one class for the rest.
        _ => InsertCause::Client("other"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sqlstate_is_exactly_five_ascii_alphanumerics() {
        assert_eq!(SqlState::parse("25006").map(|s| s.as_str().to_string()), Some("25006".into()));
        assert_eq!(SqlState::parse("XX000").map(|s| s.as_str().to_string()), Some("XX000".into()));
        for bad in ["", "2500", "250060", "25 06", "2500é", "25-06"] {
            assert_eq!(SqlState::parse(bad), None, "{bad:?}");
        }
        assert_eq!(format!("{:?}", SqlState::parse("42501").unwrap()), "SqlState(42501)");
    }

    /// The same fault is the same cause whatever its text says — the text
    /// can carry a row's own values.
    #[test]
    fn the_cause_ignores_the_error_text() {
        let a = sqlx::Error::Protocol("unexpected message for row 1".into());
        let b = sqlx::Error::Protocol("unexpected message for row 2".into());
        assert_eq!(cause_of(&a), cause_of(&b));
        let reset = |msg: &str| sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, msg.to_string()));
        assert_eq!(cause_of(&reset("x")), cause_of(&reset("y")));
        assert_eq!(cause_of(&reset("x")), InsertCause::Io(std::io::ErrorKind::ConnectionReset));
    }

    /// Different faults are different causes: what lets the thinning see a
    /// new one mid-flood.
    #[test]
    fn different_faults_are_different_causes() {
        let causes = [
            cause_of(&sqlx::Error::PoolTimedOut),
            cause_of(&sqlx::Error::PoolClosed),
            cause_of(&sqlx::Error::Io(std::io::ErrorKind::ConnectionReset.into())),
            cause_of(&sqlx::Error::Io(std::io::ErrorKind::UnexpectedEof.into())),
            cause_of(&sqlx::Error::Protocol("x".into())),
            cause_of(&sqlx::Error::Tls("x".into())),
            cause_of(&sqlx::Error::WorkerCrashed),
            cause_of(&sqlx::Error::Configuration("x".into())),
            cause_of(&sqlx::Error::InvalidArgument("x".into())),
            cause_of(&sqlx::Error::Encode("x".into())),
            cause_of(&sqlx::Error::Decode("x".into())),
            cause_of(&sqlx::Error::RowNotFound),
            InsertCause::RowCount,
        ];
        for (i, a) in causes.iter().enumerate() {
            for b in &causes[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(cause_of(&sqlx::Error::RowNotFound), InsertCause::Client("other"));
    }
}
