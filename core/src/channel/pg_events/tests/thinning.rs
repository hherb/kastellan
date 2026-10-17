//! `pg_events`' tests of the lost-row count (#829, #859, #864, #865): how
//! failed inserts are thinned, by what they are counted, and what the
//! shutdown says of them. Split from `tests.rs` to keep both under the
//! 500-LOC soft cap; its fixtures are `super::`'s.

use super::*;

/// #864: an unknown outcome is counted apart from a refusal — its own burst,
/// so a flood of refusals cannot thin the line of a row that may be in the
/// table, and the shutdown can say each count apart. (Before #864 both were
/// one burst, and the shutdown's one number could not say which rows to
/// look for in `audit_log`.)
#[tokio::test]
async fn an_unknown_outcome_counts_apart_from_a_refusal() {
    static LOST: LostRows = LostRows::new();
    for _ in 0..40 {
        settle_or_report(async { Err::<(), _>("refused") }, received_row(record_unknown_burst, &LOST)).await;
    }
    settle_or_report(async { Err::<(), _>(Unknown("reset")) }, received_row(record_unknown_burst, &LOST)).await;
    let said = std::mem::take(&mut *UNKNOWN_BURST.lock().unwrap_or_else(|p| p.into_inner()));
    assert_eq!(said.len(), REPORT_EACH_UP_TO + 3, "rows 1..=17 and 33 of the refusals, and the reset: {said:#?}");
    assert_eq!(
        said.last().map(|(_, line)| line.as_str()),
        Some(r#"channel.received row for channel "matrix" may not have been written: reset; row 1 of this burst"#)
    );
    for _ in 0..19 {
        settle_or_report(async { Err::<(), _>(Unknown("reset")) }, received_row(record_unknown_burst, &LOST)).await;
    }
    UNKNOWN_BURST.lock().unwrap_or_else(|p| p.into_inner()).clear();
    report_unsaid(&LOST, record_unknown_burst);
    let said = std::mem::take(&mut *UNKNOWN_BURST.lock().unwrap_or_else(|p| p.into_inner()));
    assert_eq!(
        said.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>(),
        [
            r#"22 channel.received audit rows for channel "matrix" failed to insert (not written) and had no line of their own (thinned past a flood)"#,
            r#"3 channel.received audit rows for channel "matrix" failed to insert (may not have been written) and had no line of their own (thinned past a flood)"#,
        ]
    );
}

/// A test's failed insert known not written, of another cause than a plain
/// string's (#859): a REVOKE in the middle of a read-only flood.
struct Revoked;

impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("permission denied")
    }
}

impl InsertFailure for Revoked {
    fn outcome(&self) -> InsertOutcome {
        InsertOutcome::NotWritten
    }

    fn cause(&self) -> InsertCause {
        InsertCause::Client("revoked")
    }
}

/// #859, #865: a writer counts a row by its own action and its insert's own
/// cause. Mid-flood, a row of a new cause gets a line saying why, and a row
/// of another action is its own count's first.
#[tokio::test]
async fn a_new_cause_or_another_action_mid_flood_gets_its_line() {
    static LOST: LostRows = LostRows::new();
    for _ in 0..40 {
        settle_or_report(async { Err::<(), _>("read-only") }, received_row(record_new_cause, &LOST)).await;
    }
    NEW_CAUSE.lock().unwrap_or_else(|p| p.into_inner()).clear();
    settle_or_report(async { Err::<(), _>(Revoked) }, received_row(record_new_cause, &LOST)).await;
    let reply = RowReport { action: "channel.reply_undelivered", ..received_row(record_new_cause, &LOST) };
    settle_or_report(async { Err::<(), _>("read-only") }, reply).await;
    let said = std::mem::take(&mut *NEW_CAUSE.lock().unwrap_or_else(|p| p.into_inner()));
    assert_eq!(
        said.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>(),
        [
            r#"channel.received row for channel "matrix" not written: permission denied; row 41 of this burst, the first with its cause (22 before it had no line of their own)"#,
            r#"channel.reply_undelivered row for channel "matrix" not written: read-only; row 1 of this burst"#,
        ]
    );
}

static NEW_CAUSE: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_new_cause(writer: AuditLostWriter, line: &str) {
    NEW_CAUSE.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

static UNKNOWN_BURST: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_unknown_burst(writer: AuditLostWriter, line: &str) {
    UNKNOWN_BURST.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// What [`record_flood`] was told — its own static, as above.
static FLOOD: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_flood(writer: AuditLostWriter, line: &str) {
    FLOOD.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #829: against a Postgres that refuses every insert at once, a flood of
/// inbound messages is a flood of failed rows. Their lines are thinned like
/// the daemon's sinks' — the first [`REPORT_EACH_UP_TO`] each, then the
/// 2^k-th — each saying where it stands in the burst, and the shutdown says
/// how many got no line of their own. Written rows say nothing and count
/// for nothing.
#[tokio::test]
async fn a_flood_of_failed_inserts_is_thinned_and_its_tail_said_at_shutdown() {
    let lost = own_lost_rows();
    for _ in 0..40 {
        settle_or_report(async { Err::<(), _>("read-only") }, received_row(record_flood, lost)).await;
        settle_or_report(async { Ok::<(), &str>(()) }, received_row(record_flood, lost)).await;
    }
    let said = std::mem::take(&mut *FLOOD.lock().unwrap_or_else(|p| p.into_inner()));
    assert_eq!(said.len(), REPORT_EACH_UP_TO + 2, "rows 1..=16, then 17 and 33: {said:#?}");
    assert!(said.iter().all(|(w, _)| *w == AuditLostWriter::Bus));
    assert_eq!(
        said[REPORT_EACH_UP_TO + 1].1,
        r#"channel.received row for channel "matrix" not written: read-only; row 33 of this burst (15 before it had no line of their own)"#
    );

    report_unsaid(lost, record_flood);
    let said = std::mem::take(&mut *FLOOD.lock().unwrap_or_else(|p| p.into_inner()));
    assert_eq!(
        said,
        [(
            AuditLostWriter::Bus,
            r#"22 channel.received audit rows for channel "matrix" failed to insert (not written) and had no line of their own (thinned past a flood)"#
                .to_string()
        )],
        "40 failed, 18 said"
    );
}

/// What [`record_quiet`] was told — its own static, as above.
static QUIET: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_quiet(writer: AuditLostWriter, line: &str) {
    QUIET.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// A shutdown with nothing left unsaid says nothing.
#[test]
fn a_shutdown_with_every_loss_said_says_nothing_more() {
    let lost = own_lost_rows();
    let key = RowKey { writer: AuditLostWriter::Bus, channel: Some("matrix"), action: "a", outcome: InsertOutcome::NotWritten };
    lost.lose(key, InsertCause::RowCount);
    report_unsaid(lost, record_quiet);
    assert!(QUIET.lock().unwrap().is_empty());
}

/// #829: a row's loss is counted under its own `channel` field — the key
/// that keeps one channel's flood from thinning another's lines (#807).
#[test]
fn a_rows_loss_is_counted_under_its_own_channel() {
    assert_eq!(row_channel_name(&serde_json::json!({"channel": "email", "peer": "a"})), Some("email"));
    assert_eq!(row_channel_name(&serde_json::json!({"ask_id": 3, "task_id": 9})), None);
    assert_eq!(row_channel_name(&serde_json::json!({"channel": 7})), None, "only a string names one");
}

/// #829: the production writers count in the one count the shutdown reads —
/// a writer with a count of its own would leave its flood's tail unsaid. The
/// bus, the boot supervisor's sink and the shutdown all take theirs from
/// [`production_writer`], pinned here; the bus's is checked through it.
#[tokio::test]
async fn the_production_writer_counts_in_the_count_the_shutdown_reads() {
    assert!(std::ptr::eq(production_writer().1, &LOST_ROWS));
    let events = PgChannelEvents::new(test_support::refused_pool());
    assert!(std::ptr::eq(events.lost, &LOST_ROWS));
    fn say_nothing(_: AuditLostWriter, _: &str) {}
    let test_events = PgChannelEvents::with_reporter(test_support::refused_pool(), say_nothing);
    assert!(!std::ptr::eq(test_events.lost, &LOST_ROWS), "POSITIVE CONTROL: a test's is its own");
}

/// What [`record_routed`] was told — its own static, as above.
static ROUTED: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_routed(writer: AuditLostWriter, line: &str) {
    ROUTED.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #829, #807's lesson: [`audit_or_report`] counts a row's loss under its
/// writer and its own `channel` field. With 100 email rows of the bus's
/// already lost, the bus's next email row is thinned out — while its matrix
/// row and the boot supervisor's email row are each the first of their own
/// count. A writer or channel dropped on the way to the count would put all
/// three in one, and the latter two would go silent too.
#[tokio::test]
async fn a_rows_loss_is_counted_under_its_writer_and_channel() {
    let lost = own_lost_rows();
    let email = RowKey {
        writer: AuditLostWriter::Bus,
        channel: Some("email"),
        action: "channel.received",
        outcome: InsertOutcome::NotWritten,
    };
    for _ in 0..100 {
        // The cause a refused connection gives, so the 101st is not new (#859).
        lost.lose(email, InsertCause::Client("pool timed out"));
    }
    let pool = test_support::refused_pool();
    let row = |writer, channel: &str| {
        let payload = serde_json::json!({"channel": channel});
        audit_or_report(&pool, writer, "channel.received", payload, record_routed, lost)
    };
    tokio::join!(
        row(AuditLostWriter::Bus, "email"),
        row(AuditLostWriter::Bus, "matrix"),
        row(AuditLostWriter::ChannelSupervisor, "email"),
    );

    let mut said = std::mem::take(&mut *ROUTED.lock().unwrap_or_else(|p| p.into_inner()));
    said.sort_by_key(|(writer, _)| *writer == AuditLostWriter::ChannelSupervisor);
    assert_eq!(said.len(), 2, "the bus's 101st email row is thinned out: {said:#?}");
    assert_eq!(said[0].0, AuditLostWriter::Bus);
    assert!(said[0].1.starts_with(r#"channel.received row for channel "matrix" not written: "#), "{said:#?}");
    assert_eq!(said[1].0, AuditLostWriter::ChannelSupervisor);
    assert!(said[1].1.starts_with(r#"channel.received row for channel "email" not written: "#), "{said:#?}");
    assert!(said.iter().all(|(_, line)| line.ends_with("; row 1 of this burst")), "{said:#?}");
}
