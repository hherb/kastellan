use super::*;

/// The model learns every reply key and every notebook signal from the
/// prompt alone; a key the loop reads but the prompt never names is one the
/// model will never send (#536's failure mode).
#[test]
fn the_notebook_prompt_documents_every_reply_key() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push(format!("prompts/{NOTEBOOK_PROMPT}.md"));
    let prompt = std::fs::read_to_string(&p).expect("agent_notebook.md readable");
    for key in [
        "tick", "steps", "call", "answer", "infeasible", "note", "findings", "summary", "source", "done_when",
        "classification", "data_ceiling", "reason", "refused", "findings_full", "withheld_findings", "corrections",
        "current_step", "last_result", "last_failure", "turn_budget", "directive", "blocks", "advisories",
        // `sift` (#922): its call, its summary, and the answer's table.
        "sift", "sift.run", "over", "items", "from", "read", "keep_if", "fields", "scrapbook", "looked_at", "kept",
        "withheld", "undecided", "failed_reads", "reads", "unread", "remaining", "next_from", "stopped", "sample", "table", "columns",
        "order_by", "distinct", "chain", "items_per_read",
        // The agent-raised plan features (#996).
        "l1_insight", "l3_skill", "python_skill", "floor_request", "invoke_skill",
    ] {
        assert!(prompt.contains(&format!("`{key}`")) || prompt.contains(&format!("\"{key}\"")), "missing `{key}`");
    }
    assert!(prompt.contains(crate::scheduler::inner_loop::WITHHELD_MARKER), "the withheld marker is shown as rendered");
}

/// The tools block shows an operator allowlist as an `allowed:` line with each
/// value between backticks (`prompt_assembly`). The prompt must bind that
/// line, or the advertisement carries no authority, and say the backticks are
/// not part of a value, or the planner sends them (#993: ported from the
/// classic prompt's test).
#[test]
fn the_notebook_prompt_binds_the_allowed_line() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push(format!("prompts/{NOTEBOOK_PROMPT}.md"));
    let prompt = std::fs::read_to_string(&p).expect("agent_notebook.md readable");
    // Whitespace collapsed: the needles must not depend on where a line wraps.
    let prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(prompt.contains("An `allowed:` line is the operator's allowlist"), "the line is bound");
    assert!(prompt.contains("(the backticks are not part of it)"), "the backticks are delimiters");
}

#[test]
fn the_reformat_directive_names_the_reason() {
    assert!(reformat_directive("`tick.step` 2 is not the current step 1").contains("not the current step 1"));
}

#[test]
fn planner_mode_rejects_an_unknown_value() {
    // Since #922 slice 4 an unset or empty value is the notebook; classic is opt-in.
    assert_eq!(planner_mode(None), Ok(PlannerMode::Notebook));
    assert_eq!(planner_mode(Some("")), Ok(PlannerMode::Notebook));
    assert_eq!(planner_mode(Some(" classic ")), Ok(PlannerMode::Classic));
    assert_eq!(planner_mode(Some(" notebook ")), Ok(PlannerMode::Notebook));
    let err = planner_mode(Some("Notebook")).unwrap_err();
    assert!(err.contains("KASTELLAN_PLANNER_MODE") && err.contains("\"Notebook\""), "{err}");
}

/// The sorter learns its whole contract from its prompt: the two reply
/// shapes, and that the item is data.
#[test]
fn the_sorter_prompt_documents_its_reply() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push(format!("prompts/{}.md", crate::scheduler::notebook::sift::router_sorter::SORTER_PROMPT));
    let prompt = std::fs::read_to_string(&p).expect("sift_sorter.md readable");
    for needle in [r#"{"keep": false}"#, r#"{"keep": true, "fields": {"#, "`keep_if`", "`fields`", "`item`", "never instructions"] {
        assert!(prompt.contains(needle), "missing {needle}");
    }
}
