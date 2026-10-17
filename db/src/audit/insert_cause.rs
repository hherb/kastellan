//! What *kind* of failure a failed [`super::insert_without_id`] was (#859):
//! a comparable class — Postgres's SQLSTATE, the I/O error's kind and OS
//! code, or the client error's variant ([`ClientFault`]) — never the error's
//! text.
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
//! codes; an [`std::io::ErrorKind`], an OS error number and a
//! [`ClientFault`] are fixed too. Fixed, not few: what bounds the causes a
//! burst tells apart is the thinning's own per-stretch cap.

/// A five-character Postgres SQLSTATE, e.g. `25006` (read-only transaction).
///
/// Only built from five ASCII digits or upper-case letters
/// ([`SqlState::parse`]), so [`SqlState::as_str`] cannot fail.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlState([u8; 5]);

impl SqlState {
    /// Pure: `code` as a SQLSTATE, or `None` unless it is exactly five ASCII
    /// digits or upper-case letters — the form every Postgres error code
    /// has. `const`, so a test can name a real code as a constant.
    pub const fn parse(code: &str) -> Option<Self> {
        let b = code.as_bytes();
        if b.len() != 5 {
            return None;
        }
        let mut i = 0;
        while i < 5 {
            if !(b[i].is_ascii_digit() || b[i].is_ascii_uppercase()) {
                return None;
            }
            i += 1;
        }
        Some(Self([b[0], b[1], b[2], b[3], b[4]]))
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
    /// The connection failed with an I/O error: its kind, and the OS error
    /// number when it has one. The kind alone would merge faults the
    /// standard library has no kind for — `EMFILE`, `ENFILE` and `ENOBUFS`
    /// are each `Uncategorized` — so a second one mid-flood would read as
    /// the first again.
    Io { kind: std::io::ErrorKind, os: Option<i32> },
    /// Any other client-side error, by its `sqlx::Error` variant.
    Client(ClientFault),
    /// The statement ran but wrote other than the one row.
    RowCount,
}

/// A client-side `sqlx::Error` with no SQLSTATE or I/O error to tell it by:
/// its variant. Closed, so two variants cannot share a class by a typo, and
/// a test cannot invent a cause production never builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientFault {
    PoolTimedOut,
    PoolClosed,
    Configuration,
    InvalidArgument,
    Encode,
    Decode,
    Tls,
    Protocol,
    WorkerCrashed,
    /// A database error with no SQLSTATE: not Postgres's (no other driver is
    /// linked), or a Postgres code that is not five digits or upper-case
    /// letters. Neither is known to happen.
    Database,
    /// A variant this was not written for. `sqlx::Error` is
    /// `#[non_exhaustive]`, and the variants not named above (`RowNotFound`,
    /// `ColumnDecode`, `BeginFailed`, …) come from reading rows, columns or
    /// transactions — nothing an autocommitted `INSERT` without `RETURNING`
    /// does.
    Other,
}

/// Pure: the class of an `INSERT` that failed with `e`.
pub fn cause_of(e: &sqlx::Error) -> InsertCause {
    match e {
        sqlx::Error::Database(db) => db
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .and_then(|pg| SqlState::parse(pg.code()))
            .map_or(InsertCause::Client(ClientFault::Database), InsertCause::Answered),
        sqlx::Error::Io(io) => InsertCause::Io { kind: io.kind(), os: io.raw_os_error() },
        sqlx::Error::PoolTimedOut => InsertCause::Client(ClientFault::PoolTimedOut),
        sqlx::Error::PoolClosed => InsertCause::Client(ClientFault::PoolClosed),
        sqlx::Error::Configuration(_) => InsertCause::Client(ClientFault::Configuration),
        sqlx::Error::InvalidArgument(_) => InsertCause::Client(ClientFault::InvalidArgument),
        sqlx::Error::Encode(_) => InsertCause::Client(ClientFault::Encode),
        sqlx::Error::Decode(_) => InsertCause::Client(ClientFault::Decode),
        sqlx::Error::Tls(_) => InsertCause::Client(ClientFault::Tls),
        sqlx::Error::Protocol(_) => InsertCause::Client(ClientFault::Protocol),
        sqlx::Error::WorkerCrashed => InsertCause::Client(ClientFault::WorkerCrashed),
        _ => InsertCause::Client(ClientFault::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sqlstate_is_exactly_five_ascii_alphanumerics() {
        assert_eq!(SqlState::parse("25006").map(|s| s.as_str().to_string()), Some("25006".into()));
        assert_eq!(SqlState::parse("XX000").map(|s| s.as_str().to_string()), Some("XX000".into()));
        for bad in ["", "2500", "250060", "25 06", "2500é", "25-06", "xx000"] {
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
        assert_eq!(cause_of(&reset("x")), InsertCause::Io { kind: std::io::ErrorKind::ConnectionReset, os: None });
        let os = |code: i32| sqlx::Error::Io(std::io::Error::from_raw_os_error(code));
        assert_eq!(cause_of(&os(24)), cause_of(&os(24)), "one OS error is one cause");
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
            // Two OS errors: a different number is a different cause even
            // where the standard library gives both one kind.
            cause_of(&sqlx::Error::Io(std::io::Error::from_raw_os_error(23))),
            cause_of(&sqlx::Error::Io(std::io::Error::from_raw_os_error(24))),
            InsertCause::RowCount,
        ];
        for (i, a) in causes.iter().enumerate() {
            for b in &causes[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(cause_of(&sqlx::Error::RowNotFound), InsertCause::Client(ClientFault::Other));
    }
}
