//! What `audit_sink_report.rs` says at shutdown (#792, #796, #797, #798,
//! #802): the pure formatters and the reporters, against a recording
//! [`Reporter`]. Split out of `audit_sink_tests.rs` to keep it under the
//! 500-LOC soft cap; `#[path]`-included by `audit_sink_report.rs`, so
//! `super::` is the report module and `super::super::` is `audit_sink`.

use super::*;
use crate::audit_sink::{ActionUnsaid, Drained, InFlight, UnsaidCounts, Unreported};
use kastellan_core::worker_stderr::AuditLostWriter;
use std::sync::Mutex;
use std::time::Duration;

/// A final report with these counts.
fn drained(rows_pending: usize, sinks_live: usize, auditing_live: usize) -> Drained {
    let in_flight = InFlight { rows_pending, sinks_live, auditing_live, ..InFlight::default() };
    Drained { in_flight, ..Drained::default() }
}

/// A [`Reporter`] that records, for the tests that read what `report_drained`
/// says. One static, shared: each test asserts on lines only it can produce.
static SAID: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record(writer: AuditLostWriter, line: &str) {
    SAID.lock().unwrap().push((writer, line.to_string()));
}

fn said_containing(needle: &str) -> Vec<(AuditLostWriter, String)> {
    SAID.lock().unwrap().iter().filter(|(_, l)| l.contains(needle)).cloned().collect()
}

/// `counts` of the `channel.inbound_dropped` action, for a line's tests.
fn dropped(counts: UnsaidCounts) -> ActionUnsaid {
    ActionUnsaid { action: "channel.inbound_dropped", counts }
}

#[test]
fn the_counting_words_agree_with_their_number() {
    assert_eq!(counted(1, "row", "was", "were"), "1 row was");
    assert_eq!(counted(2, "row", "was", "were"), "2 rows were");
    assert_eq!(counted(0, "row", "was", "were"), "0 rows were");
}

#[test]
fn the_shutdown_lines_count_rows_and_drivers_and_are_silent_at_zero() {
    let bound = Duration::from_secs(3);
    let none = Drained::default();
    assert_eq!(format_starting_at_shutdown(&none), None);
    assert_eq!(format_unreported_since(dropped(UnsaidCounts::default())), None);
    assert_eq!(format_pending_at_shutdown(&none, bound), None);
    assert_eq!(format_stuck_replies_at_shutdown(&none, bound), None);
    assert_eq!(format_live_at_shutdown(&none, bound), None);
    assert_eq!(format_unreported_at_shutdown(dropped(UnsaidCounts::default())), None);

    let one = drained(1, 1, 0);
    assert_eq!(
        format_pending_at_shutdown(&one, bound).unwrap(),
        "1 channel audit row was still unwritten after waiting 3 s at shutdown; the database \
         pool closes next, so it is lost unless already mid-write"
    );
    assert_eq!(
        format_live_at_shutdown(&one, bound).unwrap(),
        "1 channel driver had not exited after 3 s at shutdown; an audit row one writes from \
         now on is refused, and reported on the [audit-lost] marker unless thinned out"
    );
    let many = drained(2, 2, 0);
    assert!(format_pending_at_shutdown(&many, bound).unwrap().starts_with("2 channel audit rows were"));
    assert!(format_pending_at_shutdown(&many, bound).unwrap().contains("so they are lost"));
    assert!(format_live_at_shutdown(&many, bound).unwrap().starts_with("2 channel drivers had"));
}

/// #797: the line names the rows it can, and counts the ones it cannot.
#[test]
fn the_pending_line_names_the_rows_it_knows_and_counts_the_rest() {
    let bound = Duration::from_secs(3);
    let d = Drained {
        named: vec!["email skipped message <a@h>".into(), "matrix reply to !r (gave_up)".into()],
        ..drained(7, 0, 0)
    };
    let line = format_pending_at_shutdown(&d, bound).unwrap();
    assert!(
        line.contains("(email skipped message <a@h>; matrix reply to !r (gave_up); and 5 more)"),
        "{line}"
    );
}

/// #796: a driver that audits on exit (Matrix's) still running is a possible
/// loss, said as one; one silent on exit (email's) is not, and stays at INFO.
#[test]
fn a_stuck_matrix_driver_is_a_loss_and_a_stuck_email_driver_is_not() {
    let bound = Duration::from_secs(3);
    let matrix = drained(0, 1, 1);
    let line = format_stuck_replies_at_shutdown(&matrix, bound).unwrap();
    assert!(line.contains("may not be audited as `channel.reply_undelivered`"), "{line}");
    assert_eq!(format_live_at_shutdown(&matrix, bound), None, "not also an INFO line");

    let email = drained(0, 1, 0);
    assert_eq!(format_stuck_replies_at_shutdown(&email, bound), None);
    assert!(format_live_at_shutdown(&email, bound).is_some());

    let both = drained(0, 3, 1);
    assert!(format_live_at_shutdown(&both, bound).unwrap().starts_with("2 channel drivers had"));
}

static CLEAN_SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_clean(_: AuditLostWriter, line: &str) {
    CLEAN_SAID.lock().unwrap().push(line.to_string());
}

/// `report_drained` says nothing at ERROR for a clean drain, nor for an email
/// driver still in its long-poll (INFO only) — #796 — nor for an abandoned
/// bring-up (#802); and no residual line is said for zero rows.
#[test]
fn a_clean_drain_and_a_stuck_email_driver_report_no_loss() {
    report_drained(&Drained::default(), SaidUnreported::default(), record_clean);
    report_drained(&drained(0, 1, 0), SaidUnreported::default(), record_clean);
    let starting = Drained { in_flight: InFlight { starting: 1, ..InFlight::default() }, ..Drained::default() };
    report_drained(&starting, SaidUnreported::default(), record_clean);
    report_unreported_since(&Unreported::default(), record_clean);
    assert_eq!(*CLEAN_SAID.lock().unwrap(), Vec::<String>::new());
}

/// `report_drained` sends each loss to the reporter — what is no one
/// channel's under the shutdown writer, each channel's rows thinned since the
/// line before the stop under its own, one line per action (#867) — and
/// nothing for a clean drain.
#[test]
fn report_drained_reports_each_loss_through_the_reporter() {
    report_drained(&Drained::default(), SaidUnreported::default(), record);
    let shed = |n| UnsaidCounts { shed: n, ..UnsaidCounts::default() };
    let late = |n| UnsaidCounts { late: n, ..UnsaidCounts::default() };
    // Said before the channels stopped: two of Matrix's undelivered replies.
    let before_stop = SaidUnreported(Unreported {
        matrix: vec![ActionUnsaid { action: "channel.reply_undelivered", counts: shed(2) }],
        email: vec![],
    });
    let d = Drained {
        named: vec!["REPORT-DRAINED-ROW".into()],
        unreported: Unreported {
            matrix: vec![
                ActionUnsaid { action: "channel.reply_undelivered", counts: shed(3) },
                ActionUnsaid { action: "channel.inbound_dropped", counts: shed(5) },
            ],
            email: vec![ActionUnsaid { action: "channel.skipped_ack_only", counts: shed(4) }],
        },
        ..drained(1, 1, 1)
    };
    report_drained(&d, before_stop, record);
    report_unreported_since(
        &Unreported {
            matrix: vec![ActionUnsaid { action: "channel.reply_undelivered", counts: late(3) }],
            email: vec![ActionUnsaid { action: "channel.skipped_ack_only", counts: late(6) }],
        },
        record,
    );
    let stopping = "had no line of its own (thinned past a flood, while the channels stopped";
    let stopping_many = "had no line of their own (thinned past a flood, while the channels stopped";
    for (needle, writer) in [
        ("REPORT-DRAINED-ROW", AuditLostWriter::Shutdown),
        ("1 channel driver had not exited after", AuditLostWriter::Shutdown),
        // #807: a channel's thinned rows are said under its own writer —
        // the drain's, and the residual after it. #867: one line per
        // action, never a sum of the channel's. #862: the drain's counts
        // only the rows since the line before the stop (3 - 2).
        (&format!("1 more channel.reply_undelivered audit row {stopping}"), AuditLostWriter::Matrix),
        (&format!("5 more channel.inbound_dropped audit rows {stopping_many}"), AuditLostWriter::Matrix),
        (&format!("4 more channel.skipped_ack_only audit rows {stopping_many}"), AuditLostWriter::Email),
        (
            "3 more channel.reply_undelivered audit rows had no line of their own after the shutdown report",
            AuditLostWriter::Matrix,
        ),
        (
            "6 more channel.skipped_ack_only audit rows had no line of their own after the shutdown report",
            AuditLostWriter::Email,
        ),
    ] {
        let said = said_containing(needle);
        assert_eq!(said.len(), 1, "{needle}: {said:?}");
        assert_eq!(said[0].0, writer, "{needle}");
    }
    // #867: and no line has the pre-#867 shape, which summed a channel's
    // actions ("7 channel audit rows had no line"), singular or plural.
    let summed = SAID.lock().unwrap().iter().filter(|(_, l)| l.contains(" channel audit row") && l.contains("had no line")).count();
    assert_eq!(summed, 0, "{:?}", SAID.lock().unwrap());
}

/// What [`record_stop`] was told, with whose line it was.
static STOP_SAID: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_stop(writer: AuditLostWriter, line: &str) {
    STOP_SAID.lock().unwrap().push((writer, line.to_string()));
}

/// #862's lesson, for the daemon's sinks: their thinned rows are said before
/// the channels stop — so a stop the SIGKILL ends cannot take the count with
/// it — and the drain's line says only the rows thinned since, never the
/// first line's again. Through a ledger, as `main` calls it.
#[test]
fn the_sinks_thinned_rows_are_said_before_the_stop_and_only_the_rest_after_the_drain() {
    use crate::audit_sink::{Bounds, Ledger, Loss, SinkChannel};
    use kastellan_core::audit_thinning::unreported_of;
    static LEDGER: Ledger = Ledger::new(Bounds { queued: 1, connections: 1 });
    let lose = |n| {
        for _ in 0..n {
            LEDGER.refusals.refuse(SinkChannel::Matrix, "channel.reply_undelivered", Loss::Shed);
        }
    };
    lose(20);
    let said = report_unreported_on(&LEDGER, record_stop);
    assert_eq!(
        std::mem::take(&mut *STOP_SAID.lock().unwrap()),
        [(
            AuditLostWriter::Matrix,
            format!(
                "{} channel.reply_undelivered audit rows had no line of their own (thinned past a flood): {} shed \
                 over the daemon's life (too many inserts already waiting for the pool)",
                unreported_of(20),
                unreported_of(20)
            )
        )]
    );

    lose(5);
    report_drained(&LEDGER.final_snapshot(), said, record_stop);
    let more = unreported_of(25) - unreported_of(20);
    assert!(more > 0, "POSITIVE CONTROL: rows 21-25 are thinned");
    assert_eq!(
        std::mem::take(&mut *STOP_SAID.lock().unwrap()),
        [(
            AuditLostWriter::Matrix,
            format!(
                "{more} more channel.reply_undelivered audit rows had no line of their own (thinned past a flood, \
                 while the channels stopped and their drivers drained): {more} shed (too many inserts already \
                 waiting for the pool)"
            )
        )]
    );
}

/// Whether `inner` (a quoted string's inside) holds a `"` that no backslash
/// escapes. Walks escapes as pairs, so `\\"` — an escaped backslash, then a
/// bare quote — is caught, which a "preceded by `\`" check misses.
fn has_bare_quote(inner: &str) -> bool {
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return true,
            _ => {}
        }
    }
    false
}

/// #802: an id is quoted, so one that contains the shutdown line's `"; "`
/// separator — or a `"` to end its own quotes early, with or without a `\`
/// before it — cannot read as a second named row.
#[test]
fn a_quoted_id_cannot_forge_a_second_named_row() {
    assert_eq!(quoted_id("<a@h>"), r#""<a@h>""#);
    assert!(has_bare_quote(r#"x\\"; y"#), "POSITIVE CONTROL: the check sees `\\\\\"`");
    for hostile in [
        r#"x"; matrix reply to conversation "!forged" (gave_up"#,
        r#"x\"; matrix reply to conversation "!forged" (gave_up"#,
    ] {
        let quoted = quoted_id(hostile);
        assert!(quoted.starts_with('"') && quoted.ends_with('"'), "{quoted}");
        let inner = &quoted[1..quoted.len() - 1];
        assert!(!has_bare_quote(inner), "the entry cannot end early: {quoted}");
    }
    assert!(quoted_id("a\nb").contains("\\n"), "a control character is escaped too");
}

/// #802: a row label quotes its id, and its own words stay as they are.
#[test]
fn a_row_label_quotes_its_id() {
    let label = RowLabel::new("matrix reply to conversation", "!r\"; x", Some("gave_up"));
    assert_eq!(label.into_string(), r#"matrix reply to conversation "!r\"; x" (gave_up)"#);
    let label = RowLabel::new("email skipped message", "<id@h>", None);
    assert_eq!(label.into_string(), r#"email skipped message "<id@h>""#);
}

/// #826: a row about a number of things is named by the count.
#[test]
fn a_counted_row_label_names_its_count() {
    let label = RowLabel::with_count("matrix inbound messages dropped", 3);
    assert_eq!(label.into_string(), "matrix inbound messages dropped (3)");
}

/// #798: the cap still bites inside the quotes.
#[test]
fn a_quoted_id_is_capped() {
    let quoted = quoted_id(&"x".repeat(10_000));
    assert!(quoted.len() < 300, "{} bytes", quoted.len());
}

/// #802: a bring-up that never finished is said at INFO, with no claim of a
/// loss; the residual line counts what was thinned after the shutdown report.
#[test]
fn the_starting_and_residual_lines_say_what_they_count() {
    let starting = Drained { in_flight: InFlight { starting: 2, ..InFlight::default() }, ..Drained::default() };
    assert_eq!(
        format_starting_at_shutdown(&starting).unwrap(),
        "2 channel drivers had not finished starting at shutdown (an abandoned bring-up); none \
         had queued anything to audit, and an audit row one writes from now on is refused, and \
         reported unless thinned out"
    );
    assert_eq!(format_stuck_replies_at_shutdown(&starting, Duration::from_secs(3)), None);
    assert_eq!(
        format_unreported_since(dropped(UnsaidCounts { shed: 0, late: 1, failed_not_written: 0, failed_unknown: 0 })).unwrap(),
        "1 more channel.inbound_dropped audit row had no line of its own after the shutdown report (a driver \
         still running past the drain, or an insert still pending at it): 1 tried after \
         shutdown began"
    );
    // The last lines keep the kinds apart too: a row never tried is not a
    // failed insert, which may have been committed.
    assert_eq!(
        format_unreported_since(dropped(UnsaidCounts { shed: 0, late: 2, failed_not_written: 3, failed_unknown: 1 }))
            .unwrap(),
        format!(
            "6 more channel.inbound_dropped audit rows had no line of their own after the shutdown report (a \
             driver still running past the drain, or an insert still pending at it): 2 tried \
             after shutdown began; 3 failed to insert, {NOT_WRITTEN}; 1 failed to insert, \
             {MAY_BE_WRITTEN}"
        )
    );
}

/// What the shutdown says of each failed-insert outcome: the per-row lines'
/// verdict, then what it covers — and for the unknown one, no single cause
/// (it is a lost connection, a `PANIC`, a `FATAL` such as a rotated password,
/// …), since naming one would send the operator after the wrong fault.
const NOT_WRITTEN: &str = "not written (Postgres answered with an error or wrote no row, or it was never sent)";
const MAY_BE_WRITTEN: &str = "may not have been written (the insert failed in a way that does not prove the row absent — a lost connection, a server crash, an ended session: look for them in audit_log)";

/// #807: shed and late are said apart, so last week's flood reads apart from
/// this shutdown's refusals; one kind alone names only itself. #861: failed
/// inserts are a third kind, said apart from both — by outcome since #864.
#[test]
fn the_unreported_line_says_each_kind_apart() {
    assert_eq!(
        format_unreported_at_shutdown(dropped(UnsaidCounts { shed: 3, late: 2, failed_not_written: 0, failed_unknown: 0 })).unwrap(),
        "5 channel.inbound_dropped audit rows had no line of their own (thinned past a flood): 3 shed over the \
         daemon's life (too many inserts already waiting for the pool); 2 tried after shutdown \
         began"
    );
    assert_eq!(
        format_unreported_at_shutdown(dropped(UnsaidCounts { shed: 0, late: 1, failed_not_written: 0, failed_unknown: 0 })).unwrap(),
        "1 channel.inbound_dropped audit row had no line of its own (thinned past a flood): 1 tried after \
         shutdown began"
    );
    assert_eq!(
        format_unreported_at_shutdown(dropped(UnsaidCounts { shed: 3, late: 0, failed_not_written: 0, failed_unknown: 0 })).unwrap(),
        "3 channel.inbound_dropped audit rows had no line of their own (thinned past a flood): 3 shed over the \
         daemon's life (too many inserts already waiting for the pool)"
    );
    assert_eq!(
        format_unreported_at_shutdown(dropped(UnsaidCounts { failed_not_written: 7, ..Default::default() })).unwrap(),
        format!(
            "7 channel.inbound_dropped audit rows had no line of their own (thinned past a flood): 7 failed to \
             insert over the daemon's life, {NOT_WRITTEN}"
        )
    );
    // #864: the rows that may be in audit_log are said apart from those that
    // are certainly not.
    assert_eq!(
        format_unreported_at_shutdown(dropped(UnsaidCounts { failed_not_written: 2, failed_unknown: 5, ..Default::default() }))
            .unwrap(),
        format!(
            "7 channel.inbound_dropped audit rows had no line of their own (thinned past a flood): 2 failed to \
             insert over the daemon's life, {NOT_WRITTEN}; 5 failed to insert over the daemon's \
             life, {MAY_BE_WRITTEN}"
        )
    );
    assert_eq!(format_unreported_at_shutdown(dropped(UnsaidCounts::default())), None);
}

/// #858: a failed insert's line says "not written" only when the row is
/// known not to be in the table; a shed or late row was never tried. #861:
/// it carries its tally, like a refused row's.
#[test]
fn a_failed_inserts_line_says_what_is_known_and_its_tally() {
    use kastellan_db::audit::{InsertError, InsertOutcome};
    use kastellan_db::audit::InsertCause::RowCount;
    let tally = crate::audit_sink::BurstTally { nth: 17, new_cause_since: None, unsaid_before: 0, ended: None };
    let refused = InsertError::assumed_for_test(InsertOutcome::NotWritten, RowCount, kastellan_db::DbError::Query("ro".into()));
    let unknown = InsertError::assumed_for_test(InsertOutcome::Unknown, RowCount, kastellan_db::DbError::Query("reset".into()));
    let refused = Unwritten::Insert { failed: &refused, tally };
    let unknown = Unwritten::Insert { failed: &unknown, tally };
    assert_eq!(refused.verdict(), "not written");
    assert_eq!(unknown.verdict(), "may not have been written");
    assert_eq!(unknown.said(), "may not have been written: postgres query failed: reset; row 17 of this burst");
    assert_eq!(refused.to_string(), "postgres query failed: ro; row 17 of this burst");
    assert_eq!(Unwritten::Shed(tally).verdict(), "not written");
    assert_eq!(Unwritten::AfterShutdown(tally).verdict(), "not written");
}

/// #807: a shed or late row's own line says where it stands in its burst.
#[test]
fn a_refused_rows_line_carries_its_tally() {
    let tally = crate::audit_sink::BurstTally { nth: 33, new_cause_since: None, unsaid_before: 15, ended: None };
    let shed = Unwritten::Shed(tally).to_string();
    assert!(shed.starts_with("shed: "), "{shed}");
    assert!(shed.ends_with("; row 33 of this burst (15 before it had no line of their own)"), "{shed}");
    let late = Unwritten::AfterShutdown(tally).to_string();
    assert!(late.starts_with("not tried: the daemon was shutting down"), "{late}");
    assert!(late.contains("row 33 of this burst"), "{late}");
}
