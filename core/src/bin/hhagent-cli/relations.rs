//! `relations {kinds, show}` — operator CLI for the graph layer.
//!
//! Two subtrees, each documented inline below:
//!
//! - **`kinds {add, remove, list}`** — manage the operator-curated
//!   relation-label vocabulary stored in `relation_kinds`. Add/remove
//!   emit one `actor='cli' action='relation_kinds.{add,remove}'` audit
//!   row on a real state change; idempotent no-ops, validation errors,
//!   and the explicit "cannot remove undefined" rejection write no
//!   audit row. Mirror of [`super::tools_allowlist`].
//!
//! - **`show <entity-id> [--depth N] [--format plain|json]`** —
//!   operator-facing graph-edge introspection. Walks `relations`
//!   outbound and inbound from the given entity up to `--depth N`
//!   hops (default 1, hard-capped at [`hhagent_db::graph::MAX_WALK_DEPTH`]).
//!   Renders one row per traversed edge in canonical
//!   `(src_kind, "src_name") --[edge_kind]--> (dst_kind, "dst_name")`
//!   shape regardless of which walk surfaced it; quarantined entities
//!   are tagged `[Q]`. Read-only; emits no audit row.
//!
//! ## Why a separate top-level subcommand
//!
//! `relations` is a top-level namespace (alongside `entities`,
//! `tools`, `memory`, `tasks`, `observation`, `audit`). The vocabulary
//! and the graph-introspection commands cohabit cleanly under one
//! namespace — operators thinking about edges/relations look here.
//!
//! ## Connection shape
//!
//! `kinds {add, remove}` mutate a REVOKE-protected table (migration
//! 0017 carves SELECT-only for the runtime role), so both connect via
//! [`hhagent_db::pool::connect_admin_pool`] (peer auth as OS user =
//! cluster bootstrap superuser, no `SET ROLE`). `kinds list` uses the
//! same admin pool for consistency. `show` is purely a SELECT path
//! against `entities` + `relations` and connects via the runtime pool
//! — runtime role has SELECT on both tables and `show` is the first
//! caller to exercise SELECT-only on this side, so it uses the right
//! pool from the start rather than inheriting the kinds-CLI's
//! deliberately broader admin-pool choice.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

/// Top-level `relations` dispatcher. `kinds` substree drives vocab
/// management; `show` drives graph-edge introspection. New subcommands
/// can be added without restructuring.
pub(crate) fn run_relations(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli relations <kinds|show> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "kinds" => run_relations_kinds(&args[1..]),
        "show" => with_runtime("relations", relations_show(&args[1..])),
        other => {
            eprintln!("relations: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

fn run_relations_kinds(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli relations kinds <add|remove|list> ...");
        return ExitCode::from(2);
    }
    // Per-action dispatch. `with_runtime` is called only from the known
    // arms — an invalid action exits 2 without spawning tokio worker
    // threads (Issue #97 posture).
    match args[0].as_str() {
        "add" => with_runtime("relations kinds", relations_kinds_add(&args[1..])),
        "remove" => with_runtime("relations kinds", relations_kinds_remove(&args[1..])),
        "list" => with_runtime("relations kinds", relations_kinds_list(&args[1..])),
        other => {
            eprintln!("relations kinds: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

/// Parse `add` args:
///   * `<kind>`                              → (kind, None)
///   * `<kind> --description "<text>"`       → (kind, Some(text))
///
/// Returns an `Err` carrying a printable usage line on shape errors so
/// the caller can fail with exit-2 + the line on stderr.
fn parse_add_args(args: &[String]) -> Result<(String, Option<String>), String> {
    match args {
        [kind] => Ok((kind.clone(), None)),
        [kind, flag, value] if flag == "--description" => {
            Ok((kind.clone(), Some(value.clone())))
        }
        _ => Err(
            "usage: hhagent-cli relations kinds add <kind> [--description \"<text>\"]"
                .to_string(),
        ),
    }
}

async fn relations_kinds_add(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::relation_kinds_add_and_audit;
    use hhagent_db::pool::connect_admin_pool;
    use hhagent_db::relation_kinds::RelationKindError;

    let (kind, description) = match parse_add_args(args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_admin_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match relation_kinds_add_and_audit(&pool, &kind, description.as_deref()).await {
        Ok(true) => {
            println!("added {kind}");
            ExitCode::from(0)
        }
        Ok(false) => {
            println!("already present");
            ExitCode::from(0)
        }
        // Validation errors exit 2 (operator-correctable input fault),
        // matching the tools-allowlist posture.
        Err(e @ (RelationKindError::InvalidKind | RelationKindError::KindHasNul)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        // `RemovalOfUndefinedRejected` is only producible by `remove`;
        // listing it here as an explicit no-op match arm would be
        // misleading. Default arm handles DB / permission errors.
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

async fn relations_kinds_remove(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::relation_kinds_remove_and_audit;
    use hhagent_db::pool::connect_admin_pool;
    use hhagent_db::relation_kinds::RelationKindError;

    let kind = match args {
        [k] => k.clone(),
        _ => {
            eprintln!("usage: hhagent-cli relations kinds remove <kind>");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_admin_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match relation_kinds_remove_and_audit(&pool, &kind).await {
        Ok(true) => {
            println!("removed {kind}");
            ExitCode::from(0)
        }
        Ok(false) => {
            println!("not present");
            ExitCode::from(0)
        }
        Err(e @ (RelationKindError::InvalidKind | RelationKindError::KindHasNul)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        // The sentinel-rejection has its own typed error so the operator
        // sees a precise diagnostic. Exit 2 (input fault — operator
        // tried to remove a row they're not allowed to remove).
        Err(e @ RelationKindError::RemovalOfUndefinedRejected) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

async fn relations_kinds_list(args: &[String]) -> ExitCode {
    use hhagent_db::pool::connect_admin_pool;
    use hhagent_db::relation_kinds::list_all;

    if !args.is_empty() {
        eprintln!("usage: hhagent-cli relations kinds list");
        return ExitCode::from(2);
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_admin_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let entries = match list_all(&pool).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    // Match the tools-allowlist `list`'s column-format posture: header
    // line + one row per entry. Description column is wide to fit the
    // longest seed description without truncation; over-long operator-
    // added descriptions wrap visually rather than being silently cut.
    println!("{:<24}  {:<24}  {}", "KIND", "CREATED_AT", "DESCRIPTION");
    for e in entries {
        println!(
            "{:<24}  {:<24}  {}",
            e.kind,
            e.created_at,
            e.description.unwrap_or_default(),
        );
    }
    ExitCode::from(0)
}

// ─── `relations show <entity-id> [--depth N] [--format plain|json]` ───

/// Per-direction OUTPUT row cap applied SQL-side in
/// [`hhagent_db::graph::Graph::walk_outbound_edges`] /
/// [`hhagent_db::graph::Graph::walk_inbound_edges`]. 10_000 is generous
/// enough that an operator inspecting a hub entity sees the full
/// neighbourhood even at depth 3-5.
///
/// **What this cap does NOT do:** the recursive CTE is enumerated to
/// completion *before* `ORDER BY (depth ASC, edge_id ASC) LIMIT N` clips
/// the output, so this constant bounds the row count we render, not the
/// row count Postgres traverses. The actual walk-cost bound is
/// [`hhagent_db::graph::MAX_WALK_DEPTH`] — at depth 5 on a 10-fan-out
/// graph the CTE can still touch ~100_000 rows before LIMIT applies.
/// `MAX_WALK_DEPTH` is the safety budget; `SHOW_PER_DIRECTION_LIMIT` is
/// purely an operator-output ergonomic.
const SHOW_PER_DIRECTION_LIMIT: i64 = 10_000;

/// Default `--depth N` value. Matches `entities show`'s implicit
/// "show me the first layer" mental model — operators who want more
/// pass `--depth 2` or higher.
const DEFAULT_SHOW_DEPTH: u8 = 1;

/// Output format selector for `relations show`. Plain is the default
/// human-scannable rendering with dynamic column widths; Json emits one
/// JSON object per line (NDJSON) so downstream tooling can jq it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShowFormat {
    Plain,
    Json,
}

/// Parse `relations show` arguments.
///
/// Accepted shapes (`--depth` and `--format` are both optional and may
/// appear in either order):
///
/// * `<id>`
/// * `<id> --depth N`
/// * `<id> --format plain|json`
/// * `<id> --depth N --format plain|json`
/// * `<id> --format plain|json --depth N`
///
/// Returns `(entity_id, depth, format)` on success or a printable usage
/// line on shape errors so the caller can fail with exit-2 + the line on
/// stderr (same posture as [`parse_add_args`]).
///
/// **Depth validation:** `--depth 0` is rejected (a depth-0 walk has no
/// edges by construction — almost certainly an operator mistake).
/// Depths greater than [`hhagent_db::graph::MAX_WALK_DEPTH`] are
/// rejected at parse time too rather than silently clamped — the
/// operator should see the cap, not get a surprising truncated output.
/// The DB layer also clamps as a defense-in-depth measure.
fn parse_show_args(args: &[String]) -> Result<(i64, u8, ShowFormat), String> {
    const USAGE: &str = "usage: hhagent-cli relations show <entity-id> \
        [--depth N] [--format plain|json]";

    if args.is_empty() {
        return Err(USAGE.to_string());
    }
    let id: i64 = args[0]
        .parse()
        .map_err(|e| format!("relations show: invalid entity-id '{}': {e}\n{USAGE}", args[0]))?;

    let mut depth: u8 = DEFAULT_SHOW_DEPTH;
    let mut format: ShowFormat = ShowFormat::Plain;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--depth" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("relations show: --depth requires a value\n{USAGE}"))?;
                let n: u8 = value.parse().map_err(|e| {
                    format!("relations show: --depth value '{value}' is not 0..=255: {e}\n{USAGE}")
                })?;
                if n == 0 {
                    return Err(format!(
                        "relations show: --depth 0 has no edges to walk; pass --depth 1 or more\n{USAGE}"
                    ));
                }
                if n > hhagent_db::graph::MAX_WALK_DEPTH {
                    return Err(format!(
                        "relations show: --depth {n} exceeds cap {cap}; pick a smaller value\n{USAGE}",
                        cap = hhagent_db::graph::MAX_WALK_DEPTH,
                    ));
                }
                depth = n;
                i += 2;
            }
            "--format" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("relations show: --format requires a value\n{USAGE}"))?;
                format = match value.as_str() {
                    "plain" => ShowFormat::Plain,
                    "json" => ShowFormat::Json,
                    other => {
                        return Err(format!(
                            "relations show: --format '{other}' not recognised; expected 'plain' or 'json'\n{USAGE}"
                        ))
                    }
                };
                i += 2;
            }
            other => {
                return Err(format!(
                    "relations show: unrecognised argument '{other}'\n{USAGE}"
                ));
            }
        }
    }
    Ok((id, depth, format))
}

async fn relations_show(args: &[String]) -> ExitCode {
    use hhagent_db::graph::{Graph, PgGraph};
    use hhagent_db::pool::connect_runtime_pool;

    let (id, depth, format) = match parse_show_args(args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // Resolve the seed entity first; missing-id is a load-bearing
    // distinction from "exists but has no edges" (the latter is a
    // valid result, just an empty walk).
    let seed = match fetch_entity_summary(&pool, id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("relations show: id={id} not found");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("relations show: {e}");
            return ExitCode::from(1);
        }
    };

    let g = PgGraph::new(&pool);
    let outbound = match g
        .walk_outbound_edges(id, depth, SHOW_PER_DIRECTION_LIMIT)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("relations show: walk_outbound_edges: {e}");
            return ExitCode::from(1);
        }
    };
    let inbound = match g
        .walk_inbound_edges(id, depth, SHOW_PER_DIRECTION_LIMIT)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("relations show: walk_inbound_edges: {e}");
            return ExitCode::from(1);
        }
    };

    match format {
        ShowFormat::Plain => render_show_plain(&seed, depth, &outbound, &inbound),
        ShowFormat::Json => render_show_json(&seed, depth, &outbound, &inbound),
    };
    ExitCode::from(0)
}

/// Minimal subset of the seed entity's columns needed for the
/// `relations show` header line. Kept private — the only consumer is
/// the renderer below, and the projection avoids paying for `attrs`
/// JSONB decoding on a code path that never displays it.
#[derive(Clone, Debug)]
struct SeedSummary {
    id: i64,
    kind: String,
    name: String,
    quarantine: bool,
}

async fn fetch_entity_summary(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<Option<SeedSummary>, sqlx::Error> {
    let row: Option<(i64, String, String, bool)> = sqlx::query_as(
        "SELECT id, kind, name, quarantine FROM entities WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, kind, name, quarantine)| SeedSummary {
        id,
        kind,
        name,
        quarantine,
    }))
}

/// Render the seed entity + outbound + inbound walks as plain text with
/// dynamically-sized columns. Deliberately avoids the fixed-width
/// `{:<24}` formatter that issue #111 flags as a truncation footgun on
/// long kind names.
fn render_show_plain(
    seed: &SeedSummary,
    depth: u8,
    outbound: &[hhagent_db::graph::WalkedEdge],
    inbound: &[hhagent_db::graph::WalkedEdge],
) {
    let q_tag = |q: bool| if q { " [Q]" } else { "" };
    println!(
        "entity: id={} kind={} name=\"{}\"{}",
        seed.id,
        seed.kind,
        seed.name,
        q_tag(seed.quarantine),
    );
    println!("depth: {depth}");
    println!();

    render_direction("outbound", outbound);
    println!();
    render_direction("inbound", inbound);
}

fn render_direction(label: &str, edges: &[hhagent_db::graph::WalkedEdge]) {
    println!("{label} ({}):", edges.len());
    if edges.is_empty() {
        return;
    }
    // Compute per-column max widths so the longest endpoint formats
    // cleanly without crowding shorter rows.
    let src_w = edges
        .iter()
        .map(|e| endpoint_str(&e.src_kind, &e.src_name, e.src_quarantine).len())
        .max()
        .unwrap_or(0);
    let kind_w = edges.iter().map(|e| e.kind.len()).max().unwrap_or(0);
    for e in edges {
        let src = endpoint_str(&e.src_kind, &e.src_name, e.src_quarantine);
        let dst = endpoint_str(&e.dst_kind, &e.dst_name, e.dst_quarantine);
        println!(
            "  depth={depth}  {src:<src_w$}  --[{kind:<kind_w$}]-->  {dst}",
            depth = e.depth,
            src = src,
            src_w = src_w,
            kind = e.kind,
            kind_w = kind_w,
            dst = dst,
        );
    }
}

/// One endpoint rendered as `(kind, "name") [Q]?`. The `[Q]` suffix is
/// applied iff `quarantine == true` so the operator sees at a glance
/// whether the row would be invisible to production `graph_search`.
///
/// `name` may contain `"` (entity names are arbitrary TEXT — no CHECK
/// constraint on character set), so we escape `\` then `"` inside the
/// rendered name. This keeps naive downstream regex-parsers of plain
/// output from miscounting the closing quote. The JSON path uses
/// `serde_json::json!` and handles escaping itself.
fn endpoint_str(kind: &str, name: &str, quarantine: bool) -> String {
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    if quarantine {
        format!("({kind}, \"{escaped}\") [Q]")
    } else {
        format!("({kind}, \"{escaped}\")")
    }
}

/// Render NDJSON: one `{"type": "header", "seed": ...}` header line
/// followed by one `{"type": "edge", "direction": "outbound" | "inbound", ...}`
/// line per edge. Suitable for piping to `jq`. Fields are deliberately
/// stable and flat so downstream tooling doesn't have to crawl nested
/// objects.
///
/// The `"type"` discriminant lets a consumer filter cleanly without
/// having to special-case "first line is the header":
/// `jq -c 'select(.type == "edge")'` keeps the edge stream;
/// `jq -c 'select(.type == "header") | .outbound_count'` reads counts.
fn render_show_json(
    seed: &SeedSummary,
    depth: u8,
    outbound: &[hhagent_db::graph::WalkedEdge],
    inbound: &[hhagent_db::graph::WalkedEdge],
) {
    println!(
        "{}",
        serde_json::json!({
            "type": "header",
            "seed": {
                "id": seed.id,
                "kind": seed.kind,
                "name": seed.name,
                "quarantine": seed.quarantine,
            },
            "depth": depth,
            "outbound_count": outbound.len(),
            "inbound_count": inbound.len(),
        })
    );
    for e in outbound {
        println!("{}", edge_to_json("outbound", e));
    }
    for e in inbound {
        println!("{}", edge_to_json("inbound", e));
    }
}

fn edge_to_json(direction: &str, e: &hhagent_db::graph::WalkedEdge) -> String {
    serde_json::json!({
        "type": "edge",
        "direction": direction,
        "depth": e.depth,
        "edge_id": e.edge_id,
        "src": {
            "id": e.src_id,
            "kind": e.src_kind,
            "name": e.src_name,
            "quarantine": e.src_quarantine,
        },
        "kind": e.kind,
        "dst": {
            "id": e.dst_id,
            "kind": e.dst_kind,
            "name": e.dst_name,
            "quarantine": e.dst_quarantine,
        },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_add_args ------------------------------------------------

    #[test]
    fn parse_add_args_kind_only_returns_no_description() {
        let parsed = parse_add_args(&["supervises".to_string()]).unwrap();
        assert_eq!(parsed, ("supervises".to_string(), None));
    }

    #[test]
    fn parse_add_args_with_description_flag_returns_some() {
        let args = vec![
            "supervises".to_string(),
            "--description".to_string(),
            "management relation".to_string(),
        ];
        let parsed = parse_add_args(&args).unwrap();
        assert_eq!(
            parsed,
            ("supervises".to_string(), Some("management relation".to_string()))
        );
    }

    #[test]
    fn parse_add_args_rejects_empty() {
        let err = parse_add_args(&[]).unwrap_err();
        assert!(err.contains("usage"), "expected usage line: {err}");
    }

    #[test]
    fn parse_add_args_rejects_unknown_flag() {
        let args = vec![
            "supervises".to_string(),
            "--unknown".to_string(),
            "value".to_string(),
        ];
        let err = parse_add_args(&args).unwrap_err();
        assert!(err.contains("usage"), "expected usage line: {err}");
    }

    #[test]
    fn parse_add_args_rejects_dangling_description() {
        // `--description` without a value: 2 args is interpreted as
        // (kind, --description) which doesn't match the 3-arg shape.
        let args = vec!["supervises".to_string(), "--description".to_string()];
        let err = parse_add_args(&args).unwrap_err();
        assert!(err.contains("usage"), "expected usage line: {err}");
    }

    // --- parse_show_args ----------------------------------------------

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_show_args_id_only_uses_defaults() {
        let parsed = parse_show_args(&args(&["42"])).unwrap();
        assert_eq!(parsed, (42, DEFAULT_SHOW_DEPTH, ShowFormat::Plain));
    }

    #[test]
    fn parse_show_args_accepts_negative_id_as_i64() {
        // BIGSERIAL is i64; negative ids are syntactically valid even
        // though no production row has one. The parser delegates the
        // existence check to the DB layer (which returns "not found").
        let parsed = parse_show_args(&args(&["-1"])).unwrap();
        assert_eq!(parsed.0, -1);
    }

    #[test]
    fn parse_show_args_accepts_depth() {
        let parsed = parse_show_args(&args(&["42", "--depth", "3"])).unwrap();
        assert_eq!(parsed, (42, 3, ShowFormat::Plain));
    }

    #[test]
    fn parse_show_args_accepts_format_json() {
        let parsed = parse_show_args(&args(&["42", "--format", "json"])).unwrap();
        assert_eq!(parsed, (42, DEFAULT_SHOW_DEPTH, ShowFormat::Json));
    }

    #[test]
    fn parse_show_args_accepts_format_plain_explicit() {
        let parsed = parse_show_args(&args(&["42", "--format", "plain"])).unwrap();
        assert_eq!(parsed, (42, DEFAULT_SHOW_DEPTH, ShowFormat::Plain));
    }

    #[test]
    fn parse_show_args_accepts_depth_and_format_in_either_order() {
        let a = parse_show_args(&args(&["42", "--depth", "2", "--format", "json"])).unwrap();
        let b = parse_show_args(&args(&["42", "--format", "json", "--depth", "2"])).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, (42, 2, ShowFormat::Json));
    }

    #[test]
    fn parse_show_args_rejects_empty() {
        let err = parse_show_args(&[]).unwrap_err();
        assert!(err.contains("usage"), "expected usage line: {err}");
    }

    #[test]
    fn parse_show_args_rejects_non_integer_id() {
        let err = parse_show_args(&args(&["not-a-number"])).unwrap_err();
        assert!(err.contains("invalid entity-id"), "got: {err}");
    }

    #[test]
    fn parse_show_args_rejects_depth_zero() {
        let err = parse_show_args(&args(&["42", "--depth", "0"])).unwrap_err();
        assert!(
            err.contains("--depth 0"),
            "expected explicit depth=0 diagnostic: {err}",
        );
    }

    #[test]
    fn parse_show_args_rejects_depth_above_cap() {
        let too_deep = hhagent_db::graph::MAX_WALK_DEPTH + 1;
        let err = parse_show_args(&args(&["42", "--depth", &too_deep.to_string()])).unwrap_err();
        assert!(
            err.contains("exceeds cap"),
            "expected cap-exceeded diagnostic: {err}",
        );
    }

    #[test]
    fn parse_show_args_rejects_dangling_depth() {
        let err = parse_show_args(&args(&["42", "--depth"])).unwrap_err();
        assert!(
            err.contains("--depth requires a value"),
            "expected dangling-depth diagnostic: {err}",
        );
    }

    #[test]
    fn parse_show_args_rejects_unknown_format() {
        let err = parse_show_args(&args(&["42", "--format", "xml"])).unwrap_err();
        assert!(
            err.contains("not recognised"),
            "expected unknown-format diagnostic: {err}",
        );
    }

    #[test]
    fn parse_show_args_rejects_dangling_format() {
        let err = parse_show_args(&args(&["42", "--format"])).unwrap_err();
        assert!(
            err.contains("--format requires a value"),
            "expected dangling-format diagnostic: {err}",
        );
    }

    #[test]
    fn parse_show_args_rejects_unknown_flag() {
        let err = parse_show_args(&args(&["42", "--bogus", "x"])).unwrap_err();
        assert!(
            err.contains("unrecognised argument"),
            "expected unknown-flag diagnostic: {err}",
        );
    }

    // --- endpoint_str (renderer helper) -------------------------------

    #[test]
    fn endpoint_str_strips_quarantine_tag_when_approved() {
        assert_eq!(
            endpoint_str("person", "Dr Smith", false),
            r#"(person, "Dr Smith")"#,
        );
    }

    #[test]
    fn endpoint_str_adds_quarantine_tag_when_quarantined() {
        assert_eq!(
            endpoint_str("disease", "asthma", true),
            r#"(disease, "asthma") [Q]"#,
        );
    }

    #[test]
    fn endpoint_str_escapes_embedded_double_quote() {
        // Entity names allow arbitrary TEXT (no character-set CHECK), so
        // a name like `Dr "Bob" Smith` is legal. The plain rendering must
        // escape the inner quotes so naive regex parsers of the output
        // don't miscount the closing quote.
        assert_eq!(
            endpoint_str("person", r#"Dr "Bob" Smith"#, false),
            r#"(person, "Dr \"Bob\" Smith")"#,
        );
    }

    #[test]
    fn endpoint_str_escapes_backslash_before_quote() {
        // Backslashes must be escaped first; otherwise `name\"` would
        // produce ambiguous-to-parse `name\\"` (escaped backslash + raw
        // quote vs raw backslash + escaped quote). The two-pass replace
        // gives the unambiguous result.
        assert_eq!(
            endpoint_str("k", r#"a\b"c"#, false),
            r#"(k, "a\\b\"c")"#,
        );
    }

    // --- edge_to_json (JSON shape pin) --------------------------------

    #[test]
    fn edge_to_json_emits_canonical_fields() {
        use hhagent_db::graph::WalkedEdge;
        let e = WalkedEdge {
            depth: 2,
            edge_id: 17,
            src_id: 10,
            src_kind: "person".into(),
            src_name: "Dr Smith".into(),
            src_quarantine: false,
            dst_id: 20,
            dst_kind: "disease".into(),
            dst_name: "asthma".into(),
            dst_quarantine: true,
            kind: "treats".into(),
        };
        let line = edge_to_json("outbound", &e);
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        // Field-by-field pin so a future renderer change that drops or
        // renames a field trips this test rather than silently breaking
        // downstream `jq` consumers.
        assert_eq!(v["type"], "edge");
        assert_eq!(v["direction"], "outbound");
        assert_eq!(v["depth"], 2);
        assert_eq!(v["edge_id"], 17);
        assert_eq!(v["kind"], "treats");
        assert_eq!(v["src"]["id"], 10);
        assert_eq!(v["src"]["kind"], "person");
        assert_eq!(v["src"]["name"], "Dr Smith");
        assert_eq!(v["src"]["quarantine"], false);
        assert_eq!(v["dst"]["id"], 20);
        assert_eq!(v["dst"]["kind"], "disease");
        assert_eq!(v["dst"]["name"], "asthma");
        assert_eq!(v["dst"]["quarantine"], true);
    }
}
