//! The per-tool doc and the catalogue the notebook loop uses (#922).

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
fn render_tool_doc_matches_the_classic_block() {
    let tools = tools();
    let joined: String = tools.iter().map(render_tool_doc).collect();
    assert_eq!(render_tools_block(&tools), format!("<tools>\n{joined}</tools>\n\n"));
    assert!(joined.contains("  allowed: "), "the allowlist line is part of the doc: {joined}");
}

#[test]
fn the_catalogue_lists_every_tool_without_params() {
    let out = render_tool_catalogue(&tools());
    assert_eq!(
        out,
        "<tools>\n- shell-exec (method: shell.exec): Run one allowlisted command.\n\
         - web-search (method: web.search): Search the web.\n</tools>\n\n"
    );
}
