//! `hhagent-cli` — operator-facing CLI tool.
//!
//! Subcommands:
//!
//! * `audit tail`  — stream the daemon's `audit-YYYY-MM-DD.jsonl`
//!   files from `~/.local/state/hhagent/`. Works without Postgres
//!   and survives a crashed daemon (the JSONL is the durable replica
//!   of `audit_log` written by the mirror task —
//!   see [`hhagent_core::audit_mirror`]).
//!
//! * `ask "<instruction>" [--fast|--long] [--classification-floor <DataClass>]` — submit a task to the
//!   scheduler, LISTEN for the completion NOTIFY, then print the
//!   result. Ctrl-C cancels the pending/running task.
//!
//! * `tasks list|status|cancel|fail|tail` — inspect and manage
//!   tasks in the scheduler DB.
//!
//! * `tools allowlist add|remove|list` — manage the per-tool argv0
//!   allowlist stored in `tool_allowlists`. Add/remove emit one
//!   `actor='cli' action='tools.allowlist.{add,remove}'` audit row
//!   on a real state change; idempotent no-ops and validation errors
//!   write no audit row.
//!
//! * `memory l1 add|list|remove` — operator-facing management of
//!   layer-1 (in-prompt insight) memories. Add/remove emit one
//!   `actor='cli' action='l1.{added,removed}'` audit row per
//!   operation. `add` is idempotent (duplicate body_sha256 returns
//!   `skipped_duplicate`); `list` prints the in-prompt slice by
//!   default, or every L1 row with `--all`.
//!
//! * `entities list|show|approve|reject|merge` — operator review CLI
//!   for the quarantine-by-default entities table populated by the
//!   GLiNER-Relex extractor.
//!
//! * `entities kinds add|remove|list` — manage the operator-curated
//!   entity-kind vocabulary stored in `entity_kinds`. Symmetric to
//!   `relations kinds`; shares the `connect_admin_pool` plumbing.
//!   Migration 0016 REVOKEs writes from the runtime role so the
//!   daemon cannot widen entity vocab on its own.
//!
//! * `relations kinds add|remove|list` — manage the operator-curated
//!   relation-label vocabulary stored in `relation_kinds`. Add/remove
//!   emit one `actor='cli' action='relation_kinds.{add,remove}'` audit
//!   row on a real state change; idempotent no-ops, validation errors,
//!   and the explicit `'undefined'` sentinel rejection write no audit
//!   row. Requires admin-pool privileges (peer auth as cluster
//!   superuser) — migration 0017 deliberately REVOKEs writes from the
//!   runtime role so the daemon cannot widen vocab on its own.
//!
//! * `relations show <entity-id> [--depth N] [--format plain|json]` —
//!   operator-facing graph-edge introspection. Walks outbound + inbound
//!   edges from the seed up to `--depth N` hops (default 1, hard cap
//!   `MAX_WALK_DEPTH` = 5). Read-only — uses the runtime pool, emits no
//!   audit row. Quarantined endpoints are tagged `[Q]`.
//!
//! * `observation replay [--captures-dir PATH] [--model SLUG]` — re-run
//!   captured plans through the production review chain for offline
//!   rule iteration.
//!
//! Usage:
//!
//! ```text
//! hhagent-cli ask "<instruction>" [--fast|--long] [--classification-floor <DataClass>]
//! hhagent-cli tasks list   [--lane fast|long] [--state <state>] [-n 20]
//! hhagent-cli tasks status <id>
//! hhagent-cli tasks cancel <id>
//! hhagent-cli tasks fail   <id>
//! hhagent-cli tasks tail   <id>
//! hhagent-cli tools allowlist add    <tool> <argv0>
//! hhagent-cli tools allowlist remove <tool> <argv0>
//! hhagent-cli tools allowlist list   [--tool <name>]
//! hhagent-cli memory l1 add    <body>
//! hhagent-cli memory l1 list   [--all]
//! hhagent-cli memory l1 remove <id>
//! hhagent-cli entities list      [--kind K] [--state quarantined|approved|any]
//!                                [--limit N] [--since RFC3339] [--min-mentions N]
//! hhagent-cli entities show      <id>
//! hhagent-cli entities approve   <id> [<id>...]
//! hhagent-cli entities reject    <id> [<id>...]
//! hhagent-cli entities merge     --keep <id> --drop <id>[,<id>...]
//! hhagent-cli entities kinds add    <kind> [--description "<text>"]
//! hhagent-cli entities kinds remove <kind>
//! hhagent-cli entities kinds list
//! hhagent-cli relations kinds add    <kind> [--description "<text>"]
//! hhagent-cli relations kinds remove <kind>
//! hhagent-cli relations kinds list
//! hhagent-cli relations show         <entity-id> [--depth N] [--format plain|json]
//! hhagent-cli observation replay     [--captures-dir PATH] [--model SLUG]
//! hhagent-cli audit tail   [--from-start] [--no-follow] [--state-dir PATH]
//! ```
//!
//! The CLI parser is hand-rolled (no `clap` dep) because the surface
//! is tiny and a parser dep would dominate the binary footprint. If
//! we ever grow to ~5+ subcommands or richer flag parsing, swapping
//! in `clap` is a strictly local change here.
//!
//! Module map (issue #66 split): every subcommand tree lives in its
//! own sibling file. `main.rs` is the thin top-level dispatcher.
//!
//! * [`common`] — helpers shared across modules (`resolve_connect_spec`,
//!   `parse_classification_floor`, `multi_thread_runtime`).
//! * [`audit_tail`] — `audit tail`.
//! * [`ask`] — `ask`.
//! * [`tasks`] — `tasks {list,status,cancel,fail,tail}`.
//! * [`tools_allowlist`] — `tools allowlist {add,remove,list}`.
//! * [`memory_l1`] — `memory l1 {add,list,remove}`.
//! * [`entities`] — `entities {list,show,approve,reject,merge}`. The
//!   `kinds` arm delegates to [`entities_kinds`].
//! * [`entities_kinds`] — `entities kinds {add,remove,list}`.
//! * [`relations`] — `relations {kinds,show}`.
//! * [`observation_replay`] — `observation replay`.

use std::process::ExitCode;

mod common;

mod ask;
mod audit_tail;
mod entities;
mod entities_kinds;
mod memory_l1;
mod observation_replay;
mod relations;
mod tasks;
mod tools_allowlist;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("{}", help_text());
        return ExitCode::from(2);
    }
    match args[1].as_str() {
        "audit" => match args.get(2).map(|s| s.as_str()) {
            Some("tail") => audit_tail::run_audit_tail(&args[3..]),
            _ => {
                eprintln!("usage: hhagent-cli audit tail [opts]");
                ExitCode::from(2)
            }
        },
        "ask"         => ask::run_ask(&args[2..]),
        "tasks"       => tasks::run_tasks(&args[2..]),
        "tools"       => tools_allowlist::run_tools(&args[2..]),
        "memory"      => memory_l1::run_memory(&args[2..]),
        "entities"    => entities::run_entities(&args[2..]),
        "relations"   => relations::run_relations(&args[2..]),
        "observation" => observation_replay::run_observation(&args[2..]),
        "--help" | "-h" | "help" => {
            println!("{}", help_text());
            ExitCode::from(0)
        }
        other => {
            eprintln!("unknown subcommand: {other}\n\n{}", help_text());
            ExitCode::from(2)
        }
    }
}

fn help_text() -> &'static str {
    "hhagent-cli — operator CLI for hhagent

usage:
    hhagent-cli ask \"<instruction>\" [--fast|--long] [--classification-floor <DataClass>]
    hhagent-cli tasks list   [--lane fast|long] [--state <state>] [-n 20]
    hhagent-cli tasks status <id>
    hhagent-cli tasks cancel <id>
    hhagent-cli tasks fail   <id>
    hhagent-cli tasks tail   <id>
    hhagent-cli tools allowlist add    <tool> <argv0>
    hhagent-cli tools allowlist remove <tool> <argv0>
    hhagent-cli tools allowlist list   [--tool <name>]
    hhagent-cli memory l1 add    <body>
    hhagent-cli memory l1 list   [--all]
    hhagent-cli memory l1 remove <id>
    hhagent-cli entities list      [--kind K] [--state quarantined|approved|any]
                                   [--limit N] [--since RFC3339] [--min-mentions N]
    hhagent-cli entities show      <id>
    hhagent-cli entities approve   <id> [<id>...]
    hhagent-cli entities reject    <id> [<id>...]
    hhagent-cli entities merge     --keep <id> --drop <id>[,<id>...]
    hhagent-cli entities kinds add    <kind> [--description \"<text>\"]
    hhagent-cli entities kinds remove <kind>
    hhagent-cli entities kinds list
    hhagent-cli relations kinds add    <kind> [--description \"<text>\"]
    hhagent-cli relations kinds remove <kind>
    hhagent-cli relations kinds list
    hhagent-cli relations show         <entity-id> [--depth N] [--format plain|json]
    hhagent-cli observation replay     [--captures-dir PATH] [--model SLUG]
    hhagent-cli audit tail   [--from-start] [--no-follow] [--state-dir PATH]

flags (ask):
    --fast | --long             Lane selection (default: --fast).
    --classification-floor V    Set the task-level data classification
                                floor. Valid values: Public (default),
                                Personal, ClinicalConfidential, Secret.
                                Pin a non-Public floor when the task
                                involves sensitive data so the Stage 0
                                reviewer can catch classification leaks
                                in the agent's plans.

flags (audit tail):
    --from-start    Replay every line in every existing audit file
                    before switching to follow mode.
    --no-follow     Exit after replaying existing content (use with
                    --from-start for a 'cat' of the JSONL files).
    --state-dir P   Override the state dir (default: $HHAGENT_STATE_DIR
                    or $HOME/.local/state/hhagent).

flags (observation replay):
    --captures-dir P  Override the captures directory (default:
                      tests/observation/captures relative to
                      CARGO_MANIFEST_DIR for cargo-run, or cwd for
                      installed binaries).
    --model SLUG      Filter to captures whose filename contains the
                      slug (e.g. gemma4-26b-a4b-it-q8-0). Without it,
                      every <fixture_id>/*.json is replayed.
"
}
