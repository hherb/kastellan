//! The durable row for a guard tier that **refused to boot** (issue [#906]).
//!
//! Every [`GuardTierError`] stops the daemon (wiring-spec D6), and before
//! this module the refusal existed only in `kastellan-core.err`. On the DGX
//! that cost five days: the daemon refused 1500+ times between 2026-10-19
//! and 10-24, and `audit_log` held nothing at all for the whole stretch —
//! the only evidence was a growing `NRestarts` and a rotating stderr log.
//! D5's argument is that the row, not the log, is the forensic surface, so
//! "the tier refused to boot" is a row too.
//!
//! **Its own action, not a third `guard_tier.boot` shape.** Readers of
//! `guard_tier.boot` (operator queries, `guard_tier_boot_payload`'s
//! one-row-per-boot assertion) are written against the configured and
//! unconfigured payloads; a refusal is neither, and a boot that refused
//! never reached the point where those rows are written.
//!
//! **Closed tokens only, never the error text** — the same rule as
//! [#616]: a `/props` refusal carries the backend's own bytes in its
//! message, and none of them may reach a durable row. Every value this
//! payload can hold is a `&'static str` written here or in
//! [`GuardErrorKind`].
//!
//! [#616]: https://github.com/hherb/kastellan/issues/616
//! [#906]: https://github.com/hherb/kastellan/issues/906

use std::time::Duration;

use serde_json::{json, Value};

use super::super::tier::{error_kind, GuardErrorKind, GuardTierError};

/// The `audit_log.action` a refusal is recorded under (actor `policy`).
pub const REFUSED_ACTION: &str = "guard_tier.boot_refused";

/// How long one refusal row stands for every repeat of the same refusal.
///
/// A crash-looping daemon refuses once per restart; without thinning that
/// is one row per restart for as long as the misconfiguration lasts. One
/// row per distinct payload (`refusal` + `error_kind`) per hour still shows
/// when the loop started and that it went on; a *different* payload, or the
/// same one after a successful boot, is written at once.
pub const REFUSAL_ROW_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The closed token naming which refusal stopped the boot.
///
/// An exhaustive match, so a new [`GuardTierError`] variant is a compile
/// error here rather than a row with a guessed token.
pub fn refusal_kind(e: &GuardTierError) -> &'static str {
    match e {
        GuardTierError::Config(_) => "config",
        GuardTierError::Tau(_) => "tau",
        GuardTierError::PropsUnavailable(_) => "props_unavailable",
        GuardTierError::Context(_) => "context",
        GuardTierError::Timeout(_) => "timeout",
        GuardTierError::Required => "required",
    }
}

/// The `policy / guard_tier.boot_refused` payload.
///
/// `error_kind` says *why* `/props` could not be read (see
/// [`GuardErrorKind`]: a refused connection is a backend that is not
/// running; an HTTP status is usually the wrong endpoint, though a 503 that
/// outlasted the wait is a backend still starting), and is `null` on every
/// other refusal. Present on
/// every row, `null` when it does not apply, for the reason
/// [`super::super::GuardReport::audit_value`] gives: a key that appears only
/// sometimes cannot tell "not applicable" from "predates the field".
///
/// The whole payload doubles as the thinning key: two refusals with the
/// same payload are the same refusal (see `main/guard_refusal.rs`).
pub fn refusal_payload(e: &GuardTierError) -> Value {
    let error_kind: Option<GuardErrorKind> = match e {
        GuardTierError::PropsUnavailable(re) => Some(error_kind::classify(re)),
        _ => None,
    };
    json!({
        "refusal":    refusal_kind(e),
        "error_kind": error_kind.map(|k| k.as_str()),
    })
}

/// What [`record_refusal`] did; each is also its log line. The caller has
/// no use for it — it is here so every branch is testable.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusalRecord {
    /// The row was written (after a failed or slow lookup, too).
    Written,
    /// A standing row covers this refusal; nothing written.
    Thinned,
    /// The insert failed, leaving the row as its outcome says.
    InsertFailed(kastellan_db::audit::InsertOutcome),
    /// The insert was sent but did not answer within the budget: the row
    /// may or may not be in the table.
    TimedOut,
}

/// Write a refusal row unless the thinning lookup says one already stands,
/// all within `budget` (#906, #913). Never fails: the caller is about to
/// return the refusal itself, so every outcome here is a log line.
///
/// `lookup` is the thinning query (`kastellan_db::audit::exists_within`)
/// and `insert` the write (`insert_without_id`); they are passed in, not
/// called here, so the rules below are testable without Postgres
/// (`main/guard_refusal.rs` supplies the real ones):
///
/// - **A failed or slow lookup still writes the row** — a duplicate costs
///   one row, a skipped row costs the evidence. The lookup gets half the
///   budget, so a slow `audit_log` scan cannot use up the insert's time.
/// - **The whole thing is bounded by `budget`** — a wedged Postgres must
///   not turn the daemon's loud D6 stop into a silent hang.
///
/// `insert` must do nothing until awaited (both real ones are `async fn`s);
/// one that wrote on construction would bypass the thinning.
pub async fn record_refusal<E: std::fmt::Display>(
    payload: &Value,
    lookup: impl std::future::Future<Output = Result<bool, E>>,
    insert: impl std::future::Future<Output = Result<(), kastellan_db::audit::InsertError>>,
    budget: Duration,
) -> RefusalRecord {
    use kastellan_db::audit::InsertOutcome;
    let deadline = tokio::time::Instant::now() + budget;
    match tokio::time::timeout(budget / 2, lookup).await {
        Ok(Ok(true)) => {
            tracing::info!(
                payload = %payload,
                window_s = REFUSAL_ROW_INTERVAL.as_secs(),
                "guard_tier.boot_refused already recorded within the window, with no boot \
                 since; not writing another"
            );
            return RefusalRecord::Thinned;
        }
        Ok(Ok(false)) => {}
        Ok(Err(err)) => tracing::warn!(
            error = %err,
            "guard_tier.boot_refused thinning lookup failed; writing the row anyway"
        ),
        Err(_) => tracing::warn!(
            lookup_budget_ms = (budget / 2).as_millis() as u64,
            "guard_tier.boot_refused thinning lookup did not answer in time; writing the row \
             anyway"
        ),
    }
    match tokio::time::timeout_at(deadline, insert).await {
        Ok(Ok(())) => RefusalRecord::Written,
        Ok(Err(err)) => {
            let fate = match err.outcome() {
                InsertOutcome::NotWritten => "not written",
                InsertOutcome::Unknown => "may not have been written",
            };
            tracing::error!(
                error = %err,
                payload = %payload,
                "guard_tier.boot_refused audit row {fate}; this refusal is in this log"
            );
            RefusalRecord::InsertFailed(err.outcome())
        }
        Err(_) => {
            // The insert was sent and may have committed before the cut-off.
            tracing::error!(
                budget_ms = budget.as_millis() as u64,
                payload = %payload,
                "guard_tier.boot_refused insert did not answer in time; the row may not have \
                 been recorded, and this refusal is in this log"
            );
            RefusalRecord::TimedOut
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::guard_model::context_pin::GuardContextError;
    use crate::cassandra::guard_model::tier::TauError;
    use crate::cassandra::guard_model::timeout::TimeoutError;
    use kastellan_llm_router::RouterError;

    /// One of each variant, with the token its row must carry. Spelled out
    /// so a renamed token — a durable wire value — fails here.
    fn every_refusal() -> Vec<(GuardTierError, &'static str)> {
        vec![
            (GuardTierError::Config(RouterError::Config("x".into())), "config"),
            (GuardTierError::Tau(TauError::AboveOne(2.0)), "tau"),
            (
                GuardTierError::PropsUnavailable(RouterError::HttpStatus {
                    status: 404,
                    body: "not found".into(),
                }),
                "props_unavailable",
            ),
            (GuardTierError::Context(GuardContextError::NoContextSize), "context"),
            (GuardTierError::Timeout(TimeoutError::Zero), "timeout"),
            (GuardTierError::Required, "required"),
        ]
    }

    #[test]
    fn each_refusal_carries_its_own_token() {
        for (e, token) in every_refusal() {
            assert_eq!(refusal_payload(&e)["refusal"], json!(token), "{e:?}");
        }
    }

    #[test]
    fn a_props_refusal_names_why_props_failed() {
        let e = GuardTierError::PropsUnavailable(RouterError::HttpStatus {
            status: 404,
            body: "not found".into(),
        });
        assert_eq!(refusal_payload(&e)["error_kind"], json!("http_status"));
    }

    #[test]
    fn error_kind_is_present_and_null_on_every_other_refusal() {
        for (e, token) in every_refusal() {
            if token == "props_unavailable" {
                continue;
            }
            let payload = refusal_payload(&e);
            assert!(
                payload.get("error_kind").is_some_and(Value::is_null),
                "{token}: {payload}"
            );
        }
    }

    /// The backend's bytes must never reach the row (#616's rule): a
    /// `/props` body and a config message both stay out.
    #[test]
    fn no_error_text_reaches_the_payload() {
        let e = GuardTierError::PropsUnavailable(RouterError::HttpStatus {
            status: 500,
            body: "attacker-chosen body".into(),
        });
        let payload = refusal_payload(&e);
        assert_eq!(payload.as_object().map(|o| o.len()), Some(2), "{payload}");
        assert!(!payload.to_string().contains("attacker"), "{payload}");
    }

    // ---- record_refusal (#913) ----

    use std::sync::atomic::{AtomicBool, Ordering};

    /// An insert that only notes it ran.
    async fn noting_insert(ran: &AtomicBool) -> Result<(), kastellan_db::audit::InsertError> {
        ran.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Not the daemon's own budget, so a writer that ignored its `budget`
    /// argument and used a hard-coded one would fail here.
    const BUDGET: Duration = Duration::from_millis(1234);

    #[tokio::test]
    async fn a_failed_lookup_still_writes_the_row() {
        let ran = AtomicBool::new(false);
        let lookup = async { Err::<bool, _>("connection refused") };
        let did = record_refusal(&json!({}), lookup, noting_insert(&ran), BUDGET).await;
        assert_eq!(did, RefusalRecord::Written);
        assert!(ran.load(Ordering::SeqCst), "a failed lookup must not skip the row");
    }

    #[tokio::test]
    async fn a_standing_row_writes_nothing() {
        let ran = AtomicBool::new(false);
        let lookup = async { Ok::<_, String>(true) };
        let did = record_refusal(&json!({}), lookup, noting_insert(&ran), BUDGET).await;
        assert_eq!(did, RefusalRecord::Thinned);
        assert!(!ran.load(Ordering::SeqCst), "the standing row stands for this one");
    }

    #[tokio::test]
    async fn no_standing_row_writes_one() {
        let ran = AtomicBool::new(false);
        let lookup = async { Ok::<_, String>(false) };
        let did = record_refusal(&json!({}), lookup, noting_insert(&ran), BUDGET).await;
        assert_eq!(did, RefusalRecord::Written);
        assert!(ran.load(Ordering::SeqCst));
    }

    /// A failed insert is reported with what it says about the row.
    #[tokio::test]
    async fn a_failed_insert_reports_its_outcome() {
        use kastellan_db::audit::{InsertCause, InsertError, InsertOutcome};
        for outcome in [InsertOutcome::NotWritten, InsertOutcome::Unknown] {
            let insert = async move {
                Err(InsertError::assumed_for_test(
                    outcome,
                    InsertCause::RowCount,
                    kastellan_db::DbError::Query("x".into()),
                ))
            };
            let did = record_refusal(&json!({}), async { Ok::<_, String>(false) }, insert, BUDGET)
                .await;
            assert_eq!(did, RefusalRecord::InsertFailed(outcome));
        }
    }

    /// A slow lookup (a large `audit_log`, a held lock) must not cost the
    /// row: it gets half the budget, then the row is written anyway. The
    /// clock is paused, so this runs at once; the outer timeout turns a
    /// missing bound into a red test, not a hung one.
    #[tokio::test(start_paused = true)]
    async fn a_lookup_that_never_answers_still_writes_the_row() {
        let ran = AtomicBool::new(false);
        let started = tokio::time::Instant::now();
        let lookup = std::future::pending::<Result<bool, String>>();
        let did = tokio::time::timeout(
            BUDGET * 10,
            record_refusal(&json!({}), lookup, noting_insert(&ran), BUDGET),
        )
        .await
        .expect("record_refusal must return within its budget");
        assert_eq!(did, RefusalRecord::Written);
        assert!(ran.load(Ordering::SeqCst), "a slow lookup must not skip the row");
        assert_eq!(started.elapsed(), BUDGET / 2);
    }

    /// The budget covers the insert too, and counts from the start: a
    /// wedged Postgres (lookup and insert both silent) holds the exit for
    /// exactly `budget`, not more.
    #[tokio::test(start_paused = true)]
    async fn an_insert_that_never_answers_returns_at_the_budget() {
        for lookup_answers in [true, false] {
            let started = tokio::time::Instant::now();
            let insert = std::future::pending::<Result<(), kastellan_db::audit::InsertError>>();
            let lookup = async move {
                if !lookup_answers {
                    std::future::pending::<()>().await;
                }
                Ok::<_, String>(false)
            };
            let did = tokio::time::timeout(
                BUDGET * 10,
                record_refusal(&json!({}), lookup, insert, BUDGET),
            )
            .await
            .expect("record_refusal must return within its budget");
            assert_eq!(did, RefusalRecord::TimedOut, "lookup answers: {lookup_answers}");
            assert_eq!(started.elapsed(), BUDGET, "lookup answers: {lookup_answers}");
        }
    }
}
