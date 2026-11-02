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
        "current_step", "last_result", "turn_budget", "directive", "blocks", "advisories",
    ] {
        assert!(prompt.contains(&format!("`{key}`")) || prompt.contains(&format!("\"{key}\"")), "missing `{key}`");
    }
    assert!(prompt.contains(crate::scheduler::inner_loop::WITHHELD_MARKER), "the withheld marker is shown as rendered");
}

#[test]
fn the_reformat_directive_names_the_reason() {
    assert!(reformat_directive("`tick.step` 2 is not the current step 1").contains("not the current step 1"));
}
