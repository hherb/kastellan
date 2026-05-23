//! `relations {kinds, show}` — top-level dispatcher for graph-layer
//! operator commands. Both substrees live in sibling modules; this
//! file is just the router.
//!
//! - **`kinds {add, remove, list}`** in [`crate::relations_kinds`] —
//!   manage the operator-curated relation-label vocabulary stored in
//!   `relation_kinds`. Mirror of [`crate::entities_kinds`] /
//!   [`crate::tools_allowlist`].
//!
//! - **`show <entity-id> [--depth N] [--format plain|json]`** in
//!   [`crate::relations_show`] — operator-facing graph-edge
//!   introspection. Walks `relations` outbound and inbound from the
//!   given entity. Read-only.
//!
//! ## Why a separate top-level subcommand
//!
//! `relations` is a top-level namespace (alongside `entities`,
//! `tools`, `memory`, `tasks`, `observation`, `audit`). The vocabulary
//! and the graph-introspection commands cohabit cleanly under one
//! namespace — operators thinking about edges/relations look here.
//!
//! ## Why three files
//!
//! The original `relations.rs` grew to ~803 LOC after Item 21
//! (`relations show`) landed. Per the 500-LOC soft cap, the substrees
//! lift cleanly into their own files; this file stays minimal as the
//! routing root. See Item 22 in HANDOVER.

use std::process::ExitCode;

/// Top-level `relations` dispatcher. `kinds` substree drives vocab
/// management; `show` drives graph-edge introspection. New subcommands
/// can be added without restructuring.
pub(crate) fn run_relations(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli relations <kinds|show> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "kinds" => crate::relations_kinds::run(&args[1..]),
        "show" => crate::relations_show::run(&args[1..]),
        other => {
            eprintln!("relations: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}
