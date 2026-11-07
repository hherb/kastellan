//! `ask "<instruction>" [--fast|--long] [--classification-floor <DataClass>]`
//! — submit a task to the scheduler, follow it, then print the result.
//! Ctrl-C cancels the pending/running task.
//!
//! While it runs, each notebook entry but the answer and a cancel is printed
//! to stderr as one short line (#985, [`progress`]); stdout carries only the
//! answer. A classic-mode task has no notebook, so
//! it prints nothing new.
//!
//! A task can also stop short of any terminal state: CASSANDRA escalates
//! its plan, an operator ask is raised, and the task parks in
//! `awaiting_operator` until somebody answers (#564 slice 1b). `ask` then
//! tells the operator where their own question is waiting and exits 0.
//!
//! The waiting is the session's `task.follow` (`session/follow.rs`), not a
//! second copy of it.

mod progress;

use std::process::ExitCode;

use crate::common::{parse_classification_floor, resolve_connect_spec, with_runtime};

/// How often a follow (`session/follow.rs`, which `ask` uses) re-reads its
/// task, and so how soon it sees the task suspend itself on an
/// operator ask. Short enough that the operator is told within a few
/// seconds of the escalation; long enough that a task running for minutes
/// costs only a few reads per tick.
pub(crate) const SUSPEND_POLL_SECS: u64 = 3;

pub(crate) fn run_ask(args: &[String]) -> ExitCode {
    let mut lane = kastellan_db::tasks::Lane::Fast;
    let mut floor: Option<kastellan_core::cassandra::DataClass> = None;
    let mut instruction: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--long" => { lane = kastellan_db::tasks::Lane::Long; }
            "--fast" => { lane = kastellan_db::tasks::Lane::Fast; }
            "--classification-floor" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--classification-floor requires a value");
                    return ExitCode::from(2);
                };
                match parse_classification_floor(val) {
                    Ok(f) => floor = Some(f),
                    Err(msg) => {
                        eprintln!("{msg}");
                        return ExitCode::from(2);
                    }
                }
            }
            other if other.starts_with("--") => {
                eprintln!("ask: unknown flag {other}");
                return ExitCode::from(2);
            }
            other => {
                if instruction.is_some() {
                    eprintln!("ask: only one positional instruction allowed");
                    return ExitCode::from(2);
                }
                instruction = Some(other.to_string());
            }
        }
        i += 1;
    }
    let Some(instruction) = instruction else {
        eprintln!("usage: kastellan-cli ask \"<instruction>\" [--fast|--long] [--classification-floor <DataClass>]");
        return ExitCode::from(2);
    };

    // `with_runtime` builds a multi-thread runtime: the follow `ask` spawns
    // runs beside the loop that prints what it sends. Deferred until here
    // so an arg parse/validation error above never pays for a runtime it
    // won't use (Issue #97 posture).
    with_runtime("ask", ask_async(lane, instruction, floor))
}

/// Pure builder for the producer-side classification-floor decision.
///
/// Resolves `(floor, source, signals)` for an `ask` submission given:
///   - `instruction`: the user prompt (input to `infer_floor`).
///   - `operator_flag`: `Some(class)` iff `--classification-floor` was passed.
///   - `warn_on_suppress`: callback fired when the operator-explicit value
///     is LOWER than what inference would have produced (so the suppression
///     is operator-visible via `tracing::warn!` in the production caller).
///
/// Trust posture (producer-trusted, mirroring spec §2):
///   - Operator-explicit ALWAYS wins. The operator is committing to the
///     floor; inference results are visible via the warn callback but
///     never override.
///   - No operator flag + Public-with-no-signals → `Default` (no
///     elevation, no presentation noise).
///   - No operator flag + matched signals → `CliInferred` (carries the
///     matched tags for audit-row provenance).
///
/// Extracted as a pure helper so the wire shape is unit-testable
/// without spinning up a Postgres pool or the tokio runtime.
fn resolve_floor_for_submission(
    instruction: &str,
    operator_flag: Option<kastellan_core::cassandra::DataClass>,
    warn_on_suppress: &mut dyn FnMut(kastellan_core::cassandra::DataClass, &[&'static str]),
) -> (
    kastellan_core::cassandra::DataClass,
    kastellan_core::scheduler::inner_loop::ClassificationFloorSource,
    Vec<&'static str>,
) {
    use kastellan_core::cassandra::DataClass;
    use kastellan_core::classification_inference::infer_floor;
    use kastellan_core::scheduler::inner_loop::ClassificationFloorSource as Src;

    if let Some(op) = operator_flag {
        // Operator wins. Optionally warn if inference would have elevated.
        let inferred = infer_floor(instruction);
        if inferred.class.rank() > op.rank() {
            warn_on_suppress(inferred.class, &inferred.signals);
        }
        return (op, Src::Operator, vec![]);
    }
    let inferred = infer_floor(instruction);
    if inferred.class == DataClass::Public && inferred.signals.is_empty() {
        return (DataClass::Public, Src::Default, vec![]);
    }
    (inferred.class, Src::CliInferred, inferred.signals)
}

async fn ask_async(
    lane: kastellan_db::tasks::Lane,
    instruction: String,
    floor: Option<kastellan_core::cassandra::DataClass>,
) -> ExitCode {
    use std::time::Duration;

    use kastellan_core::cli_audit::{cancel_and_audit, submit_and_audit, CancelOutcome};
    use kastellan_db::pool::connect_runtime_pool;
    use serde_json::Value;
    use tokio::signal::unix::{signal, SignalKind};

    use crate::session::follow::{follow, Listener};

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("ask: {e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("ask: db connect failed: {e}"); return ExitCode::from(1); }
    };
    // Before the submit, so a database that refuses it creates no task.
    let listener = match Listener::connect(&pool).await {
        Ok(l) => l,
        Err(e) => { eprintln!("ask: {e}"); return ExitCode::from(1); }
    };

    let (payload, _suppressed) = submission_payload(&instruction, floor);
    // Installed before the submit, so a Ctrl-C from the moment the task
    // exists cancels it (`tokio::signal::ctrl_c()` registers only on its
    // first poll, and SIGINT's default action would kill `ask` and leave the
    // task running). Not before the connect: a hung connect stays killable.
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("ask: cannot catch Ctrl-C ({e}); Ctrl-C will not cancel the task, `kastellan-cli tasks cancel <id>` will");
            None
        }
    };
    // A lost audit row is a WARN line on stderr already (the CLI's tracing
    // subscriber), which is where an `ask` user looks.
    let id = match submit_and_audit(&pool, lane, payload).await {
        Ok(s) => s.id,
        Err(e) => { eprintln!("ask: insert failed: {e}"); return ExitCode::from(1); }
    };

    eprintln!("ask: submitted task {id} (lane={}); waiting for completion…", lane.as_sql());

    // The session's follow (`session/follow.rs`) does the waiting: it reads
    // the task's state and its notebook from the start, so starting it after
    // the INSERT misses nothing, and its poll tick sees a task parking on an
    // operator ask, which no NOTIFY announces. `ask` prints what it sends.
    // Zero cancel grace: `ask` prints no `cancelled` entry, so a task someone
    // else cancelled ends at once instead of waiting for the planner's.
    // Each follow gets its own channel, so one that panics closes `rx`.
    let start_follow = |after_seq: i64, delay: Duration| {
        let (tx, rx) = tokio::sync::mpsc::channel::<Value>(64);
        let pool = pool.clone();
        let wakes = listener.subscribe();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            follow(pool, wakes, id, after_seq, Duration::ZERO, tx).await;
        });
        rx
    };
    let mut rx = start_follow(0, Duration::ZERO);
    let mut failures = 0;
    loop {
        let message = tokio::select! {
            m = rx.recv() => m,
            // `None` is a signal driver shutting down, not a Ctrl-C.
            Some(()) = async { sigint.as_mut().expect("guarded").recv().await }, if sigint.is_some() => {
                // Exit 130 whatever the cancel did — the user wants out —
                // but say what it did. A cancel writes the producer-side
                // `actor='cli' action='task.cancelled'` row itself.
                match cancel_and_audit(&pool, id).await {
                    Ok(CancelOutcome::Cancelled { .. }) => eprintln!("ask: cancelled (task id {id})"),
                    Ok(CancelOutcome::NotCancellable) => {
                        eprintln!("ask: task {id} had already ended; `kastellan-cli tasks status {id}` shows how")
                    }
                    Err(e) => eprintln!(
                        "ask: cancelling task {id} failed ({e}); it may still be running: \
                         retry with `kastellan-cli tasks cancel {id}`"
                    ),
                }
                return ExitCode::from(130);  // standard SIGINT exit code
            }
        };
        // `follow` always ends with `task.finished` or `task.follow_failed`.
        let Some(message) = message else {
            eprintln!("ask: lost track of task {id}; {}", still_running(id));
            return ExitCode::from(1);
        };
        let params = &message["params"];
        match message["method"].as_str() {
            Some("task.follow_failed") => {
                // A blip (a Postgres restart, a pool timeout) must not end
                // `ask` while its task runs on: a retrying caller would
                // submit it twice. Resume from the last entry printed.
                let error = params["error"].as_str().unwrap_or("");
                failures += 1;
                if failures > FOLLOW_RETRIES {
                    eprintln!("ask: lost track of task {id}: {error}; {}", still_running(id));
                    return ExitCode::from(1);
                }
                eprintln!("ask: lost track of task {id} ({error}); retrying");
                let after_seq = params["last_seq"].as_i64().unwrap_or(0);
                rx = start_follow(after_seq, Duration::from_secs(SUSPEND_POLL_SECS));
                continue;
            }
            Some("notebook.entry") => {
                if let Some(line) = progress::entry_line(params["kind"].as_str().unwrap_or(""), &params["payload"]) {
                    eprintln!("{line}");
                }
            }
            Some("task.awaiting_operator") => {
                // NOT a failure, and deliberately exit 0: the task is
                // alive and suspended on a question the operator — the
                // very person at this terminal — has to answer. Exiting
                // non-zero would tell a script the submission failed,
                // which is untrue and would send it into a retry loop
                // against a task that is merely waiting for its author.
                // bottie (hherb/bottie) reads this line by its wording.
                eprintln!(
                    "ask: task {id} is waiting for an operator decision. \
                     Run `kastellan-cli inbox list` to see the question, then \
                     `kastellan-cli inbox resolve <id> approve|deny` to answer it. \
                     Re-run `kastellan-cli tasks status {id}` afterwards for the result."
                );
                return ExitCode::from(0);
            }
            Some("task.finished") => {
                let state = params["state"].as_str().unwrap_or("");
                let result = Some(&params["result"]).filter(|r| !r.is_null());
                return print_finished(state, result);
            }
            _ => {}
        }
        failures = 0; // a message from the follow: the database answers again
    }
}

/// How many follows in a row may fail before `ask` gives up on its task:
/// with [`SUSPEND_POLL_SECS`] between them, a database gone ~15 s or more.
const FOLLOW_RETRIES: u32 = 5;

/// The second half of a give-up line: the task was never cancelled.
fn still_running(id: i64) -> String {
    format!(
        "it may still be running: `kastellan-cli tasks status {id}` shows it, \
         `kastellan-cli tasks cancel {id}` stops it"
    )
}

/// Print how a task ended: a completed task's answer on stdout (a text
/// result's body, any other kind as JSON), anything else as the end line.
fn print_finished(state: &str, result: Option<&serde_json::Value>) -> ExitCode {
    match (state, result) {
        ("completed", Some(r)) => {
            if r.get("kind").and_then(|v| v.as_str()) == Some("text") {
                if let Some(b) = r.get("body").and_then(|v| v.as_str()) {
                    println!("{b}");
                    return ExitCode::from(0);
                }
            }
            // Unknown kind: dump JSON.
            println!("{}", serde_json::to_string_pretty(r).unwrap());
            ExitCode::from(0)
        }
        (state, result) => {
            eprintln!("{}", ended_line(state, result));
            ExitCode::from(1)
        }
    }
}

/// The `tasks.payload` an operator's instruction is stored as: the
/// instruction, `kind: "ask"`, and the classification floor with its source
/// (and matched signals, when inferred). Shared by `ask` and `session`'s
/// `task.submit`, so the two operator surfaces cannot drift on what the
/// scheduler reads.
///
/// The second value is `Some({inferred_class, inferred_signals})` when the
/// operator's pinned floor is lower than what the keyword classifier would
/// have set — logged at WARN here, and returned so `task.submit` can tell a
/// front end that only reads stdout (#988).
pub(crate) fn submission_payload(
    instruction: &str,
    floor: Option<kastellan_core::cassandra::DataClass>,
) -> (serde_json::Value, Option<serde_json::Value>) {
    let mut payload = serde_json::json!({"instruction": instruction, "kind": "ask"});

    // Resolve floor + source + signals via the pure helper. The closure
    // records what inference would have elevated to when the operator's
    // pinned floor overrode it, for the warn line below.
    let mut suppressed: Option<(kastellan_core::cassandra::DataClass, Vec<&'static str>)> = None;
    let (resolved_floor, resolved_source, resolved_signals) =
        resolve_floor_for_submission(instruction, floor, &mut |c, s| {
            suppressed = Some((c, s.to_vec()));
        });
    if let Some((inferred_class, inferred_sigs)) = &suppressed {
        tracing::warn!(
            inferred_class = inferred_class.as_pascal_str(),
            inferred_signals = ?inferred_sigs,
            operator_floor = floor.map(|f| f.as_pascal_str()).unwrap_or(""),
            "an operator classification floor (--classification-floor / classification_floor) suppressed an elevation the keyword classifier would have made"
        );
    }
    if let serde_json::Value::Object(ref mut m) = payload {
        // Floor as PascalCase string (matches `scheduler::runner`'s reader).
        m.insert(
            "classification_floor".into(),
            serde_json::to_value(resolved_floor).expect("DataClass serialises"),
        );
        // Source: always written, snake_case (matches the reader's
        // `from_str::<ClassificationFloorSource>` expectation).
        m.insert(
            "classification_floor_source".into(),
            serde_json::json!(resolved_source.as_snake_str()),
        );
        // Signals: only when present (omitted for Operator / Default /
        // empty CliInferred — though the helper never emits empty
        // CliInferred).
        if !resolved_signals.is_empty() {
            m.insert(
                "classification_floor_signals".into(),
                serde_json::json!(resolved_signals),
            );
        }
    }
    let suppressed = suppressed.map(|(class, signals)| {
        serde_json::json!({"inferred_class": class.as_pascal_str(), "inferred_signals": signals})
    });
    (payload, suppressed)
}

/// Why a task ended without an answer, in the scheduler's words: a failed
/// task's `detail`, a refusal's `body` (the text meant for the user), or a
/// blocked / denied / refused task's `reason`. Shared with `session`'s
/// `task.finished`.
pub(crate) fn ended_detail(result: Option<&serde_json::Value>) -> Option<&str> {
    let field = |key: &str| result.and_then(|r| r.get(key)).and_then(|v| v.as_str());
    field("detail").or_else(|| field("body")).or_else(|| field("reason"))
}

/// What `ask` prints for a task that did not complete: the state, then the
/// task's own `detail` when it left one, which is the plain-words reason the
/// scheduler wrote (#923) and the only part a user can act on.
fn ended_line(state: &str, result: Option<&serde_json::Value>) -> String {
    match ended_detail(result) {
        Some(detail) => format!("ask: task ended in state '{state}': {detail}"),
        None => format!("ask: task ended in state '{state}'"),
    }
}

#[cfg(test)]
mod ended_line_tests {
    use super::ended_line;

    #[test]
    fn a_failed_task_says_why_in_its_own_words() {
        let r = serde_json::json!({"kind": "error", "detail": "The language model answered with something that is not a plan."});
        assert_eq!(
            ended_line("failed", Some(&r)),
            "ask: task ended in state 'failed': The language model answered with something that is not a plan."
        );
    }

    #[test]
    fn a_blocked_task_says_its_reason() {
        let r = serde_json::json!({"kind": "blocked", "principle": "P1", "reason": "would mail a stranger"});
        assert_eq!(ended_line("blocked", Some(&r)), "ask: task ended in state 'blocked': would mail a stranger");
    }

    #[test]
    fn a_task_without_a_detail_says_only_its_state() {
        assert_eq!(ended_line("cancelled", None), "ask: task ended in state 'cancelled'");
        let r = serde_json::json!({"kind": "error"});
        assert_eq!(ended_line("failed", Some(&r)), "ask: task ended in state 'failed'");
    }
}

/// Tests for the producer-side floor resolution helper.
#[cfg(test)]
mod resolve_floor_for_submission_tests {
    use super::resolve_floor_for_submission;
    use kastellan_core::cassandra::DataClass;
    use kastellan_core::scheduler::inner_loop::ClassificationFloorSource as Src;

    #[test]
    fn no_operator_flag_no_signals_returns_default() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "How do I write a quicksort in Rust?",
            None,
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::Public);
        assert_eq!(src, Src::Default);
        assert!(sigs.is_empty());
        assert!(!suppressed, "no warn should fire for non-clinical prompt with no operator flag");
    }

    #[test]
    fn no_operator_flag_clinical_signals_returns_cli_inferred() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            None,
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::CliInferred);
        assert!(sigs.contains(&"patient"));
        assert!(sigs.contains(&"pathology"));
        assert!(!suppressed, "no warn — operator flag was absent, not suppressing");
    }

    #[test]
    fn operator_flag_wins_and_warns_when_inference_would_elevate() {
        let mut suppressed_class: Option<DataClass> = None;
        let mut suppressed_signals: Vec<&'static str> = vec![];
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            Some(DataClass::Public),  // operator pinned LOWER than inference
            &mut |c, s| {
                suppressed_class = Some(c);
                suppressed_signals = s.to_vec();
            },
        );
        assert_eq!(cls, DataClass::Public, "operator wins");
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty(), "Operator source carries no signals");
        assert_eq!(suppressed_class, Some(DataClass::ClinicalConfidential),
            "warn should fire because inference would have elevated to Clinical");
        assert!(suppressed_signals.contains(&"patient"));
    }

    #[test]
    fn operator_flag_wins_and_no_warn_when_inference_does_not_elevate() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "How do I write a quicksort in Rust?",
            Some(DataClass::ClinicalConfidential),
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty());
        assert!(!suppressed, "inference inferred Public (not elevating); no warn");
    }

    #[test]
    fn operator_flag_equal_to_inference_does_not_warn() {
        // Inference would return Clinical; operator pinned Clinical.
        // The suppression condition is strict inequality (inferred > op);
        // matching values are not a suppression.
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            Some(DataClass::ClinicalConfidential),
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty());
        assert!(!suppressed, "matching operator value is not a suppression");
    }
}

#[cfg(test)]
mod submission_payload_tests {
    use super::submission_payload;
    use kastellan_core::cassandra::DataClass;

    // The keys and casing the scheduler's runner reads back.
    #[test]
    fn an_operator_floor_is_stored_as_the_operators() {
        let (p, suppressed) = submission_payload("find the invoice", Some(DataClass::Personal));
        assert_eq!(suppressed, None, "nothing clinical to suppress");
        assert_eq!(p["instruction"], "find the invoice");
        assert_eq!(p["kind"], "ask");
        assert_eq!(p["classification_floor"], "Personal");
        assert_eq!(p["classification_floor_source"], "operator");
        assert!(p.get("classification_floor_signals").is_none(), "{p}");
    }

    #[test]
    fn an_unpinned_floor_is_inferred_with_its_signals() {
        let (p, suppressed) = submission_payload("Translate the patient's pathology report.", None);
        assert_eq!(suppressed, None, "an inferred floor suppresses nothing");
        assert_eq!(p["classification_floor_source"], "cli_inferred", "{p}");
        assert_ne!(p["classification_floor"], "Public", "{p}");
        assert!(p["classification_floor_signals"].as_array().is_some_and(|a| !a.is_empty()), "{p}");
    }

    #[test]
    fn a_pinned_floor_below_the_inferred_one_is_reported() {
        let (p, suppressed) = submission_payload("Translate the patient's pathology report.", Some(DataClass::Public));
        assert_eq!(p["classification_floor"], "Public", "the operator still wins");
        let s = suppressed.expect("the suppression is reported");
        assert_eq!(s["inferred_class"], "ClinicalConfidential", "{s}");
        assert!(s["inferred_signals"].as_array().is_some_and(|a| a.iter().any(|v| v == "patient")), "{s}");
    }
}
