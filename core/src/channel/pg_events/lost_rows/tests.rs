//! `lost_rows`' tests. Split out of `lost_rows.rs` to keep it under the 500-LOC
//! soft cap.

use super::*;
use crate::audit_thinning::{BURST_QUIET_GAP, REPORT_EACH_UP_TO};
use kastellan_db::audit::SqlState;

/// Postgres's 25006, read_only_sql_transaction.
const READ_ONLY: InsertCause = InsertCause::Answered(SqlState::parse("25006").unwrap());

/// A `channel.received` row of `writer` on `channel` — written for that
/// channel too, or for Matrix if the row names none — known not written.
fn received(writer: AuditLostWriter, channel: Option<&str>) -> RowKey<'_> {
    let writer_channel = channel.unwrap_or("matrix");
    RowKey { writer, writer_channel, channel, action: "channel.received", outcome: InsertOutcome::NotWritten }
}

/// An `ask.resolved` row — which names no channel — of the bus written
/// for `writer_channel`, known not written.
fn ask_resolved(writer_channel: &str) -> RowKey<'_> {
    RowKey {
        writer: AuditLostWriter::Bus,
        writer_channel,
        channel: None,
        action: "ask.resolved",
        outcome: InsertOutcome::NotWritten,
    }
}

/// Lose `n` rows counted by `key` at `at`, returning how many got a line.
fn lose_n(rows: &LostRows, key: RowKey<'_>, n: usize, at: Instant) -> usize {
    (0..n).filter(|_| rows.lose_at(key, READ_ONLY, at).report()).count()
}

fn unsaid(writer: AuditLostWriter, channel: RowChannel, action: &'static str, outcome: InsertOutcome, rows: usize) -> Unsaid {
    Unsaid { key: CountKey { writer, channel, action, outcome }, rows }
}

/// A flood is thinned like the daemon's: the first rows each, then the
/// 2^k-th — and what was left out is counted.
#[test]
fn a_flood_of_lost_rows_is_thinned_and_counted() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    let said = lose_n(&rows, received(AuditLostWriter::Bus, Some("matrix")), 100, t0);
    assert_eq!(said, REPORT_EACH_UP_TO + 3, "0..16, then 16, 32 and 64");
    let matrix = RowChannel::Named("matrix".into());
    assert_eq!(
        rows.unsaid(),
        [unsaid(AuditLostWriter::Bus, matrix, "channel.received", InsertOutcome::NotWritten, 100 - said)]
    );
}

/// #807's lesson, for the bus: one channel's flood leaves the other
/// channel's lines, and the other writer's, unthinned.
#[test]
fn each_writer_and_channel_thins_on_its_own() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    lose_n(&rows, received(AuditLostWriter::Bus, Some("email")), 100, t0);
    let matrix = rows.lose_at(received(AuditLostWriter::Bus, Some("matrix")), READ_ONLY, t0);
    assert!(matrix.report() && matrix.tally.nth == 1, "another channel's first row: {matrix:?}");
    let supervisor_email = received(AuditLostWriter::ChannelSupervisor, Some("email"));
    let supervisor = rows.lose_at(supervisor_email, READ_ONLY, t0);
    assert!(supervisor.report() && supervisor.tally.nth == 1, "another writer's first row: {supervisor:?}");
    let ask = rows.lose_at(received(AuditLostWriter::Bus, None), READ_ONLY, t0);
    assert!(ask.report() && ask.tally.nth == 1, "a row with no channel counts apart: {ask:?}");
    // The shutdown's count is said as each writer's own, in the order
    // each was first lost.
    let supervisor_said = 1 + lose_n(&rows, supervisor_email, 99, t0);
    let bus_said = REPORT_EACH_UP_TO + 3;
    let email = || RowChannel::Named("email".into());
    let nw = InsertOutcome::NotWritten;
    assert_eq!(
        rows.unsaid(),
        [
            unsaid(AuditLostWriter::Bus, email(), "channel.received", nw, 100 - bus_said),
            unsaid(AuditLostWriter::ChannelSupervisor, email(), "channel.received", nw, 100 - supervisor_said),
        ]
    );
}

/// #865, for the bus: one action's flood leaves another action's first
/// row on the same channel its line.
#[test]
fn each_action_thins_on_its_own() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    lose_n(&rows, received(AuditLostWriter::Bus, Some("matrix")), 100, t0);
    let reply = RowKey { action: "channel.reply_undelivered", ..received(AuditLostWriter::Bus, Some("matrix")) };
    let first = rows.lose_at(reply, READ_ONLY, t0);
    assert!(first.report() && first.tally.nth == 1, "another action's first row: {first:?}");
}

/// #860: a row naming no channel is counted under its bus's channel, so
/// one bus's flood of them leaves another bus's first such row its line
/// — and the shutdown says which bus's they were.
#[test]
fn a_row_naming_no_channel_counts_under_its_buss_channel() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    let email_said = lose_n(&rows, ask_resolved("email"), 100, t0);
    let matrix = rows.lose_at(ask_resolved("matrix"), READ_ONLY, t0);
    assert!(matrix.report() && matrix.tally.nth == 1, "the other bus's first such row: {matrix:?}");
    // Apart from the bus's rows that name the channel themselves, too.
    let named = rows.lose_at(RowKey { action: "ask.resolved", ..received(AuditLostWriter::Bus, Some("email")) }, READ_ONLY, t0);
    assert!(named.report() && named.tally.nth == 1, "a row naming its channel counts apart: {named:?}");
    let nw = InsertOutcome::NotWritten;
    assert_eq!(
        rows.unsaid(),
        [unsaid(AuditLostWriter::Bus, RowChannel::Unnamed("email".into()), "ask.resolved", nw, 100 - email_said)]
    );
}

/// #864: the rows that may have been written are counted apart from
/// those that certainly were not, and said apart at shutdown.
#[test]
fn each_outcome_is_counted_on_its_own() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    let refused = received(AuditLostWriter::Bus, Some("matrix"));
    let reset = RowKey { outcome: InsertOutcome::Unknown, ..refused };
    let refused_said = lose_n(&rows, refused, 100, t0);
    let first = rows.lose_at(reset, READ_ONLY, t0);
    assert!(first.report() && first.tally.nth == 1, "the other outcome's first row: {first:?}");
    let reset_said = 1 + lose_n(&rows, reset, 39, t0);
    let matrix = || RowChannel::Named("matrix".into());
    assert_eq!(
        rows.unsaid(),
        [
            unsaid(AuditLostWriter::Bus, matrix(), "channel.received", InsertOutcome::NotWritten, 100 - refused_said),
            unsaid(AuditLostWriter::Bus, matrix(), "channel.received", InsertOutcome::Unknown, 40 - reset_said),
        ]
    );
}

/// #859, reached through this count: a new cause mid-flood gets its line.
#[test]
fn a_new_cause_mid_flood_gets_its_line() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    let key = received(AuditLostWriter::Bus, Some("matrix"));
    lose_n(&rows, key, 100, t0);
    let revoked = rows.lose_at(key, InsertCause::Answered(SqlState::parse("42501").unwrap()), t0);
    assert!(revoked.report() && revoked.tally.new_cause_since.is_some(), "{revoked:?}");
    assert!(!rows.lose_at(key, READ_ONLY, t0).report(), "the flood's own cause is still thinned");
}

/// A quiet gap starts a new burst, whose first line says how the last
/// one ended — the shared core's behaviour, reached through this count.
#[test]
fn a_quiet_gap_starts_a_new_burst_of_lines() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    let key = received(AuditLostWriter::Bus, Some("matrix"));
    lose_n(&rows, key, 20, t0);
    let next = rows.lose_at(key, READ_ONLY, t0 + BURST_QUIET_GAP);
    assert!(next.report(), "{next:?}");
    assert_eq!(next.tally.nth, 1);
    assert!(next.tally.ended.is_some(), "it says how the 20-row burst ended: {next:?}");
}

/// Past [`MAX_NAMED_CHANNELS`] a writer's further channels share one
/// count, so the count's memory stays bounded — while a channel it
/// already knows keeps its own, and another writer is unaffected. The
/// cap counts channels, not entries: a channel's other actions and
/// outcomes are not further channels.
#[test]
fn channels_past_the_cap_share_one_count() {
    let rows = LostRows::new();
    let t0 = Instant::now();
    let names: Vec<String> = (0..MAX_NAMED_CHANNELS).map(|i| format!("c{i}")).collect();
    for name in &names[..MAX_NAMED_CHANNELS - 1] {
        rows.lose_at(received(AuditLostWriter::Bus, Some(name)), READ_ONLY, t0);
    }
    // c0's other outcome is a further entry, not a further channel: the
    // last channel under the cap is still named.
    let c0_unknown = RowKey { outcome: InsertOutcome::Unknown, ..received(AuditLostWriter::Bus, Some("c0")) };
    rows.lose_at(c0_unknown, READ_ONLY, t0);
    let last = &names[MAX_NAMED_CHANNELS - 1];
    rows.lose_at(received(AuditLostWriter::Bus, Some(last)), READ_ONLY, t0);
    let entries = |rows: &LostRows| rows.entries.lock().unwrap().len();
    assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 1, "POSITIVE CONTROL: each channel named apart");
    assert!(
        rows.entries.lock().unwrap().iter().all(|e| e.key.channel != RowChannel::Other),
        "no channel past the cap yet"
    );
    rows.lose_at(received(AuditLostWriter::Bus, Some("one-more")), READ_ONLY, t0);
    rows.lose_at(received(AuditLostWriter::Bus, Some("and-another")), READ_ONLY, t0);
    assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 2, "both counted under one Other entry");
    let c0 = rows.lose_at(received(AuditLostWriter::Bus, Some("c0")), READ_ONLY, t0);
    assert_eq!(c0.tally.nth, 2, "c0 keeps its own count");
    rows.lose_at(received(AuditLostWriter::ChannelSupervisor, Some("new")), READ_ONLY, t0);
    assert_eq!(entries(&rows), MAX_NAMED_CHANNELS + 3, "another writer names its own");
    let other = rows.entries.lock().unwrap().iter().filter(|e| e.key.channel == RowChannel::Other).count();
    assert_eq!(other, 1);
}

/// #862: the second line says only the rows thinned since the first —
/// a count in both by the difference, one new since whole — and none
/// for a count with nothing new.
#[test]
fn unsaid_since_subtracts_count_by_count() {
    let matrix = || RowChannel::Named("matrix".into());
    let nw = InsertOutcome::NotWritten;
    let base = [
        unsaid(AuditLostWriter::Bus, matrix(), "channel.received", nw, 40),
        unsaid(AuditLostWriter::Bus, matrix(), "ask.resolved", nw, 7),
    ];
    let now = vec![
        unsaid(AuditLostWriter::Bus, matrix(), "channel.received", nw, 43),
        unsaid(AuditLostWriter::Bus, matrix(), "ask.resolved", nw, 7),
        unsaid(AuditLostWriter::ChannelSupervisor, matrix(), "channel.received", nw, 2),
    ];
    assert_eq!(
        unsaid_since(now, &base),
        [
            unsaid(AuditLostWriter::Bus, matrix(), "channel.received", nw, 3),
            unsaid(AuditLostWriter::ChannelSupervisor, matrix(), "channel.received", nw, 2),
        ]
    );
    assert_eq!(unsaid_since(base.to_vec(), &base), [], "nothing new");
}

/// #862: the second line says its rows are more, thinned during the stop.
#[test]
fn the_line_after_the_stop_says_its_rows_are_more() {
    let key = unsaid(AuditLostWriter::Bus, RowChannel::Named("matrix".into()), "channel.received", InsertOutcome::NotWritten, 3);
    assert_eq!(
        format_unsaid(&key, UnsaidPart::DuringStop),
        r#"3 more channel.received audit rows for channel "matrix" failed to insert (not written) and had no line of their own (thinned past a flood, while the channels stopped)"#
    );
}

/// The shutdown's line names the action and the channel quoted, says
/// what is known of the rows (#864), and agrees in number.
#[test]
fn the_unsaid_line_names_its_count_and_what_is_known() {
    let nw = InsertOutcome::NotWritten;
    let line = |channel, outcome, rows| {
        format_unsaid(&unsaid(AuditLostWriter::Bus, channel, "channel.received", outcome, rows), UnsaidPart::BeforeStop)
    };
    assert_eq!(
        line(RowChannel::Named("matrix".into()), nw, 83),
        r#"83 channel.received audit rows for channel "matrix" failed to insert (not written) and had no line of their own (thinned past a flood)"#
    );
    assert_eq!(
        line(RowChannel::Unnamed("email".into()), InsertOutcome::Unknown, 1),
        "1 channel.received audit row naming no channel (written for channel \"email\") failed to insert (may not \
         have been written) and had no line of its own (thinned past a flood)"
    );
    assert_eq!(
        line(RowChannel::Other, nw, 2),
        "2 channel.received audit rows for channels past the first 16 failed to insert (not written) and had no \
         line of their own (thinned past a flood)"
    );
    let hostile = line(RowChannel::Named("x\"\n[SKIP]".into()), nw, 2);
    assert!(!hostile.contains('\n'), "a channel name is quoted: {hostile}");
}
