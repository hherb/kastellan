//! The per-tool doc the notebook loop shows for the current step (#922).

use super::*;
use crate::prompt_assembly::AdvertisedTool;
use crate::worker_manifest::{ToolDoc, ToolParam};
use kastellan_db::tool_allowlists::EntryKind;

fn tools() -> [AdvertisedTool; 2] {
    [
        AdvertisedTool::with_allowlist(
            ToolDoc {
                name: "shell-exec",
                method: "shell.exec",
                summary: "Run one allowlisted command.",
                params: &[ToolParam { name: "argv", description: "command and arguments", required: true }],
            },
            EntryKind::Argv0,
            &["/usr/bin/ls".to_string()],
        ),
        AdvertisedTool::without_allowlist(ToolDoc {
            name: "web-search",
            method: "web.search",
            summary: "Search the web.",
            params: &[],
        }),
    ]
}

#[test]
fn render_tool_doc_matches_the_tools_block() {
    let tools = tools();
    let joined: String = tools.iter().map(render_tool_doc).collect();
    assert_eq!(render_tools_block(&tools), format!("<tools>\n{joined}</tools>\n\n"));
    assert!(joined.contains("  allowed: "), "the allowlist line is part of the doc: {joined}");
}
