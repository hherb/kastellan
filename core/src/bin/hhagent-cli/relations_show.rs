//! `relations show <entity-id> [--depth N] [--format plain|json]` —
//! operator-facing graph-edge introspection.
//!
//! Walks `relations` outbound and inbound from the given entity up to
//! `--depth N` hops (default 1, hard-capped at
//! [`hhagent_db::graph::MAX_WALK_DEPTH`]). Renders one row per
//! traversed edge in canonical
//! `(src_kind, "src_name") --[edge_kind]--> (dst_kind, "dst_name")`
//! shape regardless of which walk surfaced it; quarantined entities
//! are tagged `[Q]`. Read-only — uses the runtime pool, emits no
//! audit row.
//!
//! Lifted from `relations.rs` per Item 22 (HANDOVER) so each substree
//! lives under the 500-LOC soft cap.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

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

/// Public entry point delegated to from
/// [`crate::relations::run_relations`]'s `"show"` arm.
///
/// Synchronous wrapper that spawns the runtime via
/// [`crate::common::with_runtime`] only after parse-time validation
/// has succeeded (Issue #97 posture).
pub(crate) fn run(args: &[String]) -> ExitCode {
    with_runtime("relations", relations_show(args))
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
/// stderr (same posture as
/// [`crate::relations_kinds::parse_add_args`]).
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

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // --- parse_show_args ----------------------------------------------

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
