//! What `audit_sink_report.rs` says at shutdown (#792, #796, #797, #798,
//! #802): the pure formatters and the reporters, against a recording
//! [`Reporter`]. Split out of `audit_sink_tests.rs` to keep it under the
//! 500-LOC soft cap; `#[path]`-included by `audit_sink_report.rs`, so
//! `super::` is the report module and `super::super::` is `audit_sink`.

use super::*;
use crate::audit_sink::{Drained, InFlight};
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
    assert_eq!(format_unreported_since(0), None);
    assert_eq!(format_pending_at_shutdown(&none, bound), None);
    assert_eq!(format_stuck_replies_at_shutdown(&none, bound), None);
    assert_eq!(format_live_at_shutdown(&none, bound), None);
    assert_eq!(format_unreported_at_shutdown(&none), None);

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
    report_drained(&Drained::default(), record_clean);
    report_drained(&drained(0, 1, 0), record_clean);
    report_drained(&Drained { in_flight: InFlight { starting: 1, ..InFlight::default() }, ..Drained::default() }, record_clean);
    report_unreported_since(0, record_clean);
    assert_eq!(*CLEAN_SAID.lock().unwrap(), Vec::<String>::new());
}

/// `report_drained` sends each loss to the reporter under the shutdown writer,
/// and nothing for a clean drain.
#[test]
fn report_drained_reports_each_loss_through_the_reporter() {
    report_drained(&Drained::default(), record);
    let d = Drained {
        named: vec!["REPORT-DRAINED-ROW".into()],
        unreported: 4,
        ..drained(1, 1, 1)
    };
    report_drained(&d, record);
    report_unreported_since(3, record);
    for needle in [
        "REPORT-DRAINED-ROW",
        "1 channel driver had not exited after",
        "4 channel audit rows were refused",
        "3 more channel audit rows were refused after the shutdown report",
    ] {
        let said = said_containing(needle);
        assert_eq!(said.len(), 1, "{needle}: {said:?}");
        assert_eq!(said[0].0, AuditLostWriter::Shutdown);
    }
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
        format_unreported_since(1).unwrap(),
        "1 more channel audit row was refused after the shutdown report without a line of its \
         own (a driver still running past the drain)"
    );
}
