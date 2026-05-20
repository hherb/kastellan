//! `ask "<instruction>" [--fast|--long] [--classification-floor <DataClass>]`
//! — submit a task to the scheduler, LISTEN for the completion NOTIFY,
//! then print the result. Ctrl-C cancels the pending/running task.

use std::process::ExitCode;

use crate::common::{multi_thread_runtime, parse_classification_floor, resolve_connect_spec};

pub(crate) fn run_ask(args: &[String]) -> ExitCode {
    let mut lane = hhagent_db::tasks::Lane::Fast;
    let mut floor: Option<hhagent_core::cassandra::DataClass> = None;
    let mut instruction: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--long" => { lane = hhagent_db::tasks::Lane::Long; }
            "--fast" => { lane = hhagent_db::tasks::Lane::Fast; }
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
        eprintln!("usage: hhagent-cli ask \"<instruction>\" [--fast|--long] [--classification-floor <DataClass>]");
        return ExitCode::from(2);
    };

    // Use a multi-thread runtime so `block_in_place` is available if
    // any sqlx internals need it; PgListener::recv does not require it
    // today but the shape is consistent with the rest of the DB code.
    let rt = match multi_thread_runtime("ask") {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    rt.block_on(ask_async(lane, instruction, floor))
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
    operator_flag: Option<hhagent_core::cassandra::DataClass>,
    warn_on_suppress: &mut dyn FnMut(hhagent_core::cassandra::DataClass, &[&'static str]),
) -> (
    hhagent_core::cassandra::DataClass,
    hhagent_core::scheduler::inner_loop::ClassificationFloorSource,
    Vec<&'static str>,
) {
    use hhagent_core::cassandra::DataClass;
    use hhagent_core::classification_inference::infer_floor;
    use hhagent_core::scheduler::inner_loop::ClassificationFloorSource as Src;

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
    lane: hhagent_db::tasks::Lane,
    instruction: String,
    floor: Option<hhagent_core::cassandra::DataClass>,
) -> ExitCode {
    use hhagent_core::cli_audit::{cancel_and_audit, submit_and_audit};
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tasks::get;
    use sqlx::postgres::PgListener;

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("ask: {e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("ask: db connect failed: {e}"); return ExitCode::from(1); }
    };

    // LISTEN BEFORE INSERT to avoid the race where the NOTIFY arrives
    // before we start listening.
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(e) => { eprintln!("ask: listener connect failed: {e}"); return ExitCode::from(1); }
    };
    if let Err(e) = listener.listen("tasks_completed").await {
        eprintln!("ask: listen failed: {e}");
        return ExitCode::from(1);
    }

    let mut payload = serde_json::json!({"instruction": instruction, "kind": "ask"});

    // Resolve floor + source + signals via the pure helper. The closure
    // captures `floor` so we can render its PascalCase string for the
    // warn line when inference would have elevated above the operator's
    // pinned value.
    let mut suppressed: Option<(hhagent_core::cassandra::DataClass, Vec<&'static str>)> = None;
    let (resolved_floor, resolved_source, resolved_signals) =
        resolve_floor_for_submission(&instruction, floor, &mut |c, s| {
            suppressed = Some((c, s.to_vec()));
        });
    if let Some((inferred_class, inferred_sigs)) = &suppressed {
        tracing::warn!(
            inferred_class = inferred_class.as_pascal_str(),
            inferred_signals = ?inferred_sigs,
            operator_floor = floor.map(|f| f.as_pascal_str()).unwrap_or(""),
            "--classification-floor explicitly suppressed an elevation the keyword classifier would have made"
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
    let id = match submit_and_audit(&pool, lane, payload).await {
        Ok(i) => i,
        Err(e) => { eprintln!("ask: insert failed: {e}"); return ExitCode::from(1); }
    };

    eprintln!("ask: submitted task {id} (lane={}); waiting for completion…", lane.as_sql());

    // Wait for a terminal-state NOTIFY for our id, OR ctrl-C.
    tokio::pin! {
        let sigint = tokio::signal::ctrl_c();
    }
    loop {
        tokio::select! {
            n = listener.recv() => match n {
                Ok(notif) => {
                    if notif.payload() == id.to_string() { break; }
                }
                Err(e) => { eprintln!("ask: listener.recv: {e}"); return ExitCode::from(1); }
            },
            result = &mut sigint => {
                if result.is_ok() {
                    // Best-effort: even if the UPDATE or audit insert
                    // hiccups, the SIGINT path still exits 130 — the
                    // user has signalled they want out. On success the
                    // helper emits the producer-side
                    // `actor='cli' action='task.cancelled'` row.
                    let _ = cancel_and_audit(&pool, id).await;
                    eprintln!("ask: cancelled (task id {id})");
                    return ExitCode::from(130);  // standard SIGINT exit code
                }
            }
        }
    }

    let task = match get(&pool, id).await {
        Ok(Some(t)) => t,
        Ok(None) => { eprintln!("ask: task {id} disappeared"); return ExitCode::from(1); }
        Err(e) => { eprintln!("ask: get failed: {e}"); return ExitCode::from(1); }
    };

    match (task.state.as_str(), task.result) {
        ("completed", Some(r)) => {
            if r.get("kind").and_then(|v| v.as_str()) == Some("text") {
                if let Some(b) = r.get("body").and_then(|v| v.as_str()) {
                    println!("{b}");
                    return ExitCode::from(0);
                }
            }
            // Unknown kind: dump JSON.
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
            ExitCode::from(0)
        }
        (state, _) => {
            eprintln!("ask: task ended in state '{state}'");
            ExitCode::from(1)
        }
    }
}

/// Tests for the producer-side floor resolution helper.
#[cfg(test)]
mod resolve_floor_for_submission_tests {
    use super::resolve_floor_for_submission;
    use hhagent_core::cassandra::DataClass;
    use hhagent_core::scheduler::inner_loop::ClassificationFloorSource as Src;

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
