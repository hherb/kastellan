//! Pins the property the block order exists for: what changes between two
//! planner requests sits at the END, so everything before it is byte-identical
//! and a backend's prompt cache can serve it (Ollama/llama.cpp in memory, oMLX
//! on SSD). A reorder that puts a changing block early fails here, not as a
//! silent slowdown on the live hosts. The whole request (this system prompt,
//! then the user message with the clock last) is pinned in
//! `scheduler::agent::tests`.

use super::*;
use crate::prompt_assembly::AdvertisedTool;
use crate::recall_assembly::RecalledContext;
use crate::worker_manifest::ToolDoc;
use kastellan_db::memories::{Memory, MemoryLayer};

fn mem(id: i64, body: &str, layer: MemoryLayer) -> Memory {
    Memory {
        id,
        body: body.to_string(),
        metadata: serde_json::json!({}),
        layer,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

fn prompt(recalled: &[&str]) -> String {
    let l0 = vec![mem(1, "L0 RULE", MemoryLayer::Meta)];
    let l1 = vec![mem(2, "L1 INSIGHT", MemoryLayer::Index)];
    let skills = vec![SurfacedSkill { name: "foo".into(), description: "does foo.".into(), params: vec![], invocable: false }];
    let tools = [AdvertisedTool::without_allowlist(ToolDoc {
        name: "web-search",
        method: "web.search",
        summary: "Search the web.",
        params: &[],
    })];
    let recalled = RecalledContext::new(
        (0..recalled.len() as i64).collect(),
        recalled.iter().map(|s| s.to_string()).collect(),
        "f".repeat(64),
    );
    assemble_system_prompt(&l0, &l1, &skills, &recalled, "BASE BODY", &tools)
}

/// Everything before `marker`, which both prompts must share, at the same offset.
fn shared_through(a: &str, b: &str, marker: &str) -> usize {
    let cut = a.find(marker).unwrap_or_else(|| panic!("{marker} missing in:\n{a}"));
    assert_eq!(b.find(marker), Some(cut), "{marker} must sit at the same offset in both");
    assert_eq!(&a[..cut], &b[..cut], "the prompts must agree up to {marker}");
    cut
}

#[test]
fn a_new_recall_keeps_every_other_block_cached() {
    let a = prompt(&["fact one"]);
    let b = prompt(&["fact two"]);
    let cut = shared_through(&a, &b, "<recalled>");
    for block in ["</base>", "</handoff>", "</tools>", "</l0_meta_rules>", "</l1_insights>", "</skills>"] {
        assert!(a[..cut].contains(block), "{block} must be in the shared prefix:\n{a}");
    }
}
