//! `audit_sink_thinning.rs`'s tests: which lost rows get a line, and what is
//! counted for the shutdown — per channel (#807), action (#837, #865, #867),
//! kind (#861) and outcome (#864). Split out of `audit_sink_thinning.rs` to
//! keep it under the 500-LOC soft cap; `#[path]`-included there, so `super::`
//! is the thinning module.

use super::*;
use kastellan_core::audit_thinning::unreported_of;
use kastellan_db::audit::SqlState;

/// Postgres's 25006, read_only_sql_transaction.
const READ_ONLY: Loss = Loss::Failed {
    outcome: InsertOutcome::NotWritten,
    cause: InsertCause::Answered(SqlState::parse("25006").unwrap()),
};
const RESET: Loss = Loss::Failed {
    outcome: InsertOutcome::Unknown,
    cause: InsertCause::Io { kind: std::io::ErrorKind::ConnectionReset, os: None },
};

/// Lose `n` of `channel`'s rows of `action` as `loss`.
fn lose(refusals: &Refusals, channel: SinkChannel, action: &'static str, loss: Loss, n: usize) {
    for _ in 0..n {
        refusals.refuse(channel, action, loss);
    }
}

/// One action's entry, for a comparison.
fn unsaid(action: &'static str, counts: UnsaidCounts) -> ActionUnsaid {
    ActionUnsaid { action, counts }
}

/// #807: one channel's flood leaves the other's rows reported, and the
/// counts stay apart by channel and by kind.
#[test]
fn one_channels_flood_does_not_thin_the_others_lines() {
    let refusals = Refusals::new();
    lose(&refusals, SinkChannel::Email, "a", Loss::Shed, 100);
    let matrix = refusals.refuse(SinkChannel::Matrix, "a", Loss::Shed);
    assert!(matrix.report(), "Matrix's first shed row is its own burst's first");
    assert_eq!(matrix.tally.nth, 1);
    refusals.refuse(SinkChannel::Email, "a", Loss::Late);
    let unreported = refusals.unreported();
    assert_eq!(unreported.email, [unsaid("a", UnsaidCounts { shed: unreported_of(100), ..Default::default() })]);
    assert!(unreported.matrix.is_empty(), "Matrix's one row had its line, so it has no entry: {unreported:?}");
    assert_eq!(unreported.total(), unreported_of(100));
}

/// #837, #865: one action's flood leaves the same channel's other actions
/// their lines. #867: and each action's count is its own — never summed
/// into the channel's — in the order each first lost a row.
#[test]
fn one_actions_flood_does_not_thin_anothers_lines() {
    let refusals = Refusals::new();
    lose(&refusals, SinkChannel::Matrix, "channel.inbound_dropped", READ_ONLY, 100);
    let reply = refusals.refuse(SinkChannel::Matrix, "channel.reply_undelivered", READ_ONLY);
    assert!(reply.report() && reply.tally.nth == 1, "another action's first row: {reply:?}");
    // Unequal counts per action, so a count that took one side for the
    // other could not pass.
    lose(&refusals, SinkChannel::Matrix, "channel.reply_undelivered", Loss::Shed, 20);
    lose(&refusals, SinkChannel::Matrix, "channel.inbound_dropped", Loss::Shed, 40);
    assert_eq!(
        refusals.unreported().matrix,
        [
            unsaid(
                "channel.inbound_dropped",
                UnsaidCounts { shed: unreported_of(40), failed_not_written: unreported_of(100), ..Default::default() }
            ),
            unsaid("channel.reply_undelivered", UnsaidCounts { shed: unreported_of(20), ..Default::default() }),
        ]
    );
}

/// Every kind is in its action's count — late rows and the unknown outcome
/// included — for each of two actions with unequal counts.
#[test]
fn each_actions_count_holds_every_kind() {
    let refusals = Refusals::new();
    lose(&refusals, SinkChannel::Email, "a", Loss::Late, 20);
    lose(&refusals, SinkChannel::Email, "b", Loss::Late, 40);
    lose(&refusals, SinkChannel::Email, "a", RESET, 40);
    lose(&refusals, SinkChannel::Email, "b", RESET, 20);
    let unreported = refusals.unreported();
    assert_eq!(
        unreported.counts(SinkChannel::Email, "a"),
        UnsaidCounts { late: unreported_of(20), failed_unknown: unreported_of(40), ..Default::default() }
    );
    assert_eq!(
        unreported.counts(SinkChannel::Email, "b"),
        UnsaidCounts { late: unreported_of(40), failed_unknown: unreported_of(20), ..Default::default() }
    );
}

/// #867: `since` subtracts action by action — an action new since `base`
/// whole, one counted in both by the difference — and leaves out an action
/// with nothing new, so a residual line names only the actions it counts.
#[test]
fn since_subtracts_action_by_action() {
    let refusals = Refusals::new();
    lose(&refusals, SinkChannel::Email, "a", Loss::Late, 40);
    lose(&refusals, SinkChannel::Email, "b", Loss::Late, 20);
    let base = refusals.unreported();
    // `b` gains rows, `c` appears, `a` stays where it was.
    lose(&refusals, SinkChannel::Email, "b", Loss::Late, 44);
    lose(&refusals, SinkChannel::Email, "c", Loss::Shed, 20);
    lose(&refusals, SinkChannel::Matrix, "a", Loss::Shed, 20);
    let since = refusals.unreported().since(&base);
    assert_eq!(
        since.email,
        [
            unsaid("b", UnsaidCounts { late: unreported_of(64) - unreported_of(20), ..Default::default() }),
            unsaid("c", UnsaidCounts { shed: unreported_of(20), ..Default::default() }),
        ]
    );
    assert_eq!(since.matrix, [unsaid("a", UnsaidCounts { shed: unreported_of(20), ..Default::default() })]);
    assert_eq!(refusals.unreported().since(&refusals.unreported()), Unreported::default(), "nothing new");
}

/// `since` on the counts subtracts kind by kind, each its own.
#[test]
fn since_subtracts_kind_by_kind() {
    let now = UnsaidCounts { shed: 11, late: 22, failed_not_written: 33, failed_unknown: 44 };
    let base = UnsaidCounts { shed: 1, late: 2, failed_not_written: 3, failed_unknown: 4 };
    assert_eq!(now.since(base), UnsaidCounts { shed: 10, late: 20, failed_not_written: 30, failed_unknown: 40 });
}

/// #861: failed inserts are a kind of their own — a flood of them leaves
/// the same channel's first shed row its burst's first — and they are in
/// their action's total, so the shutdown line counts them.
#[test]
fn failed_inserts_thin_apart_from_shed_rows() {
    let refusals = Refusals::new();
    lose(&refusals, SinkChannel::Email, "a", READ_ONLY, 100);
    let shed = refusals.refuse(SinkChannel::Email, "a", Loss::Shed);
    assert!(shed.report(), "the first shed row is its own burst's first");
    assert_eq!(shed.tally.nth, 1);
    let email = refusals.unreported().counts(SinkChannel::Email, "a");
    assert_eq!(email, UnsaidCounts { failed_not_written: unreported_of(100), ..Default::default() });
    assert_eq!(email.total(), unreported_of(100));
    let base = UnsaidCounts { failed_not_written: 10, ..Default::default() };
    assert_eq!(email.since(base).failed_not_written, unreported_of(100) - 10, "`since` counts it too");
}

/// #864: a failed insert's outcome is its count's, so the shutdown can say
/// how many rows are certainly missing and how many may be there — and a
/// flood of one outcome leaves the other's lines.
#[test]
fn failed_inserts_are_counted_by_outcome() {
    let refusals = Refusals::new();
    lose(&refusals, SinkChannel::Email, "a", READ_ONLY, 100);
    let reset = refusals.refuse(SinkChannel::Email, "a", RESET);
    assert!(reset.report() && reset.tally.nth == 1, "the other outcome's first row: {reset:?}");
    lose(&refusals, SinkChannel::Email, "a", RESET, 39);
    let email = refusals.unreported().counts(SinkChannel::Email, "a");
    assert_eq!(
        email,
        UnsaidCounts { failed_not_written: unreported_of(100), failed_unknown: unreported_of(40), ..Default::default() }
    );
    let base = UnsaidCounts { failed_unknown: 1, ..Default::default() };
    assert_eq!(email.since(base).failed_unknown, unreported_of(40) - 1, "`since` counts it too");
}

/// A failed insert's loss is its own outcome and cause.
#[test]
fn a_failed_inserts_loss_is_its_outcome_and_cause() {
    let cause = InsertCause::Io { kind: std::io::ErrorKind::BrokenPipe, os: None };
    let error = InsertError::assumed_for_test(InsertOutcome::Unknown, cause, kastellan_db::DbError::Query("x".into()));
    assert!(matches!(
        Loss::failed(&error),
        Loss::Failed { outcome: InsertOutcome::Unknown, cause: c } if c == cause
    ));
}

/// #859, reached through the sinks' count: a new cause of the same outcome
/// gets its line mid-flood.
#[test]
fn a_new_cause_mid_flood_gets_its_line() {
    let refusals = Refusals::new();
    lose(&refusals, SinkChannel::Email, "a", READ_ONLY, 100);
    let revoked = Loss::Failed {
        outcome: InsertOutcome::NotWritten,
        cause: InsertCause::Answered(SqlState::parse("42501").unwrap()),
    };
    let new = refusals.refuse(SinkChannel::Email, "a", revoked);
    assert_eq!(new.tally.new_cause_since, Some(65), "{new:?}");
    assert!(!refusals.refuse(SinkChannel::Email, "a", revoked).report(), "then thinned again");
}
