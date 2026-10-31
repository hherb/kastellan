//! Pins the property the block order exists for: what changes between two
//! planner turns sits at the END, so everything before it is byte-identical
//! and a backend's prompt cache can serve it (Ollama/llama.cpp in memory, oMLX
//! on SSD). A reorder that puts a changing block early fails here, not as a
//! silent slowdown on the live hosts.

use super::*;
use crate::recall_assembly::RecalledContext;
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

fn prompt(recalled: &[&str], now: &str) -> String {
    let l0 = vec![mem(1, "L0 RULE", MemoryLayer::Meta)];
    let l1 = vec![mem(2, "L1 INSIGHT", MemoryLayer::Index)];
    let recalled = RecalledContext::new(
        (0..recalled.len() as i64).collect(),
        recalled.iter().map(|s| s.to_string()).collect(),
        "f".repeat(64),
    );
    assemble_system_prompt(&l0, &l1, &[], &recalled, "BASE BODY", &[], Some(now))
}

/// Everything before `marker` in `a`, which `b` must start with too.
fn shared_through(a: &str, b: &str, marker: &str) -> usize {
    let cut = a.find(marker).unwrap_or_else(|| panic!("{marker} missing in:\n{a}"));
    assert_eq!(&a[..cut], &b[..cut], "the prompts must agree up to {marker}");
    cut
}

#[test]
fn a_new_minute_changes_only_the_tail() {
    let a = prompt(&["fact"], "<now>\n14:05\n</now>\n");
    let b = prompt(&["fact"], "<now>\n14:06\n</now>\n");
    let cut = shared_through(&a, &b, "<now>");
    assert!(a[..cut].contains("</base>") && a[..cut].contains("</recalled>"), "{a}");
}

#[test]
fn a_new_recall_keeps_base_handoff_and_curated_layers_cached() {
    let a = prompt(&["fact one"], "<now>\n14:05\n</now>\n");
    let b = prompt(&["fact two"], "<now>\n14:05\n</now>\n");
    let cut = shared_through(&a, &b, "<recalled>");
    for block in ["</base>", "</handoff>", "</l0_meta_rules>", "</l1_insights>"] {
        assert!(a[..cut].contains(block), "{block} must be in the shared prefix:\n{a}");
    }
}
