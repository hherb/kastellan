//! `entities {list, show, approve, reject, merge}` — operator review
//! CLI for the quarantine-by-default `entities` table populated by
//! the GLiNER-Relex extractor.
//!
//! The `kinds` substree (`entities kinds {add, remove, list}`) lives
//! in the sibling module [`crate::entities_kinds`]. The `"kinds"` arm
//! in [`run_entities`] just delegates to it; everything in this file
//! is about reviewing extracted entities, not managing the kind
//! vocabulary.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_entities(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli entities <list|show|approve|reject|merge|reembed|kinds> ...");
        return ExitCode::from(2);
    }
    // Per-action dispatch. `with_runtime` is called only from the known
    // arms — an invalid action exits 2 without spawning tokio worker
    // threads (Issue #97).
    match args[0].as_str() {
        "list"    => with_runtime("entities", entities_list(&args[1..])),
        "show"    => with_runtime("entities", entities_show(&args[1..])),
        "approve" => with_runtime("entities", entities_approve(&args[1..])),
        "reject"  => with_runtime("entities", entities_reject(&args[1..])),
        "merge"   => with_runtime("entities", entities_merge(&args[1..])),
        "reembed" => with_runtime("entities", entities_reembed(&args[1..])),
        "kinds"   => crate::entities_kinds::run(&args[1..]),
        other     => {
            eprintln!("entities: unknown action '{other}'; expected: list | show | approve | reject | merge | reembed | kinds");
            ExitCode::from(2)
        }
    }
}

/// Parse the `--state` flag value. Case-insensitive.
fn parse_entity_state(s: &str) -> Result<kastellan_db::entities::EntityState, String> {
    use kastellan_db::entities::EntityState;
    match s.trim().to_ascii_lowercase().as_str() {
        "quarantined" => Ok(EntityState::Quarantined),
        "approved"    => Ok(EntityState::Approved),
        "any"         => Ok(EntityState::Any),
        other         => Err(format!(
            "invalid --state '{other}'; expected: quarantined | approved | any"
        )),
    }
}

/// Parse the `--drop` flag value. Comma-separated i64s; whitespace
/// around commas is permitted; empty segments and non-numeric entries
/// are rejected. Negative ids parse successfully and are passed through
/// to `merge_entities`, which surfaces them as `EntitiesError::NotFound`
/// (BIGSERIAL ids are always positive in practice, so an operator typo
/// like `--drop -5` ends in a clear NotFound rather than a parse error).
fn parse_id_list(s: &str) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    for raw in s.split(',') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!("--drop list contains empty entry in '{s}'"));
        }
        let id: i64 = trimmed.parse().map_err(|e| {
            format!("--drop entry '{trimmed}' is not an integer: {e}")
        })?;
        out.push(id);
    }
    if out.is_empty() {
        return Err("--drop list is empty".into());
    }
    Ok(out)
}

async fn entities_list(args: &[String]) -> ExitCode {
    use kastellan_db::entities::{list_entities, ListFilter};
    use kastellan_db::pool::connect_runtime_pool;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let mut filter = ListFilter::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--kind requires a value"); return ExitCode::from(2); }
                };
                filter.kind = Some(v.clone());
                i += 2;
            }
            "--state" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--state requires a value"); return ExitCode::from(2); }
                };
                filter.state = match parse_entity_state(v) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("{e}"); return ExitCode::from(2); }
                };
                i += 2;
            }
            "--limit" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--limit requires a value"); return ExitCode::from(2); }
                };
                let n: i64 = match v.parse() {
                    Ok(n) => n,
                    Err(e) => { eprintln!("--limit '{v}' is not an integer: {e}"); return ExitCode::from(2); }
                };
                if !(1..=1000).contains(&n) {
                    eprintln!("--limit must be between 1 and 1000 (got {n})");
                    return ExitCode::from(2);
                }
                filter.limit = n;
                i += 2;
            }
            "--since" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--since requires a value"); return ExitCode::from(2); }
                };
                let dt = match OffsetDateTime::parse(v, &Rfc3339) {
                    Ok(dt) => dt,
                    Err(e) => { eprintln!("--since '{v}' is not RFC3339: {e}"); return ExitCode::from(2); }
                };
                filter.since = Some(dt);
                i += 2;
            }
            "--min-mentions" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--min-mentions requires a value"); return ExitCode::from(2); }
                };
                let n: i64 = match v.parse() {
                    Ok(n) => n,
                    Err(e) => { eprintln!("--min-mentions '{v}' is not an integer: {e}"); return ExitCode::from(2); }
                };
                if n < 0 {
                    eprintln!("--min-mentions must be >= 0 (got {n})");
                    return ExitCode::from(2);
                }
                filter.min_mentions = n;
                i += 2;
            }
            other => {
                eprintln!("entities list: unknown flag '{other}'");
                return ExitCode::from(2);
            }
        }
    }
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let rows = match list_entities(&pool, &filter).await {
        Ok(r) => r,
        Err(e) => { eprintln!("entities list: {e}"); return ExitCode::from(1); }
    };

    println!(
        "{:<8}  {:<12}  {:<30}  {:<10}  {:>8}  CREATED_AT",
        "ID", "KIND", "NAME", "QUARANTINE", "MENTIONS"
    );
    for r in rows {
        let name_display = if r.name.chars().count() > 30 {
            let mut s: String = r.name.chars().take(29).collect();
            s.push('…');
            s
        } else {
            r.name.clone()
        };
        println!(
            "{:<8}  {:<12}  {:<30}  {:<10}  {:>8}  {}",
            r.id,
            r.kind,
            name_display,
            if r.quarantine { "TRUE" } else { "FALSE" },
            r.mention_count,
            r.created_at,
        );
    }
    ExitCode::from(0)
}

async fn entities_show(args: &[String]) -> ExitCode {
    use kastellan_db::entities::get_entity_with_mentions;
    use kastellan_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: kastellan-cli entities show <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("entities show: invalid id '{id_str}': {e}");
            return ExitCode::from(2);
        }
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let (entity, mems) = match get_entity_with_mentions(&pool, id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            eprintln!("entities show: id={id} not found");
            return ExitCode::from(1);
        }
        Err(e) => { eprintln!("entities show: {e}"); return ExitCode::from(1); }
    };

    println!("id:            {}", entity.id);
    println!("kind:          {}", entity.kind);
    println!("name:          {}", entity.name);
    println!("name_norm:     {}", entity.name_norm);
    println!("quarantine:    {}", if entity.quarantine { "TRUE" } else { "FALSE" });
    println!("created_at:    {}", entity.created_at);
    println!("mentions:      {}", entity.mention_count);
    println!();
    println!("linked memories (showing first {} of {}):",
        mems.len(), entity.mention_count);
    // Layer is constrained to 0..=4 by the schema CHECK constraint, so
    // the `other` branch is operationally dead code. We still render
    // unknown layers as `L<n>` rather than aborting the whole listing
    // mid-iteration — a constraint violation is a DB-level alarm, not a
    // reason to truncate the operator's view of the entity's mentions.
    for m in mems {
        let layer_name: std::borrow::Cow<'_, str> = match m.layer {
            0 => "L0".into(),
            1 => "L1".into(),
            2 => "L2".into(),
            3 => "L3".into(),
            4 => "L4".into(),
            other => {
                eprintln!(
                    "entities show: unexpected layer {other} on memory id {} \
                     (schema CHECK violation; rendering as L{other})",
                    m.memory_id,
                );
                format!("L{other}").into()
            }
        };
        println!("  {layer_name}  id={:<6}  {}", m.memory_id, m.body_preview);
    }
    ExitCode::from(0)
}

async fn entities_approve(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::entities_approve_and_audit;
    use kastellan_db::entities::ApproveOutcome;
    use kastellan_db::pool::connect_runtime_pool;

    if args.is_empty() {
        eprintln!("usage: kastellan-cli entities approve <id> [<id>...]");
        return ExitCode::from(2);
    }
    let mut ids: Vec<i64> = Vec::with_capacity(args.len());
    for a in args {
        match a.parse::<i64>() {
            Ok(n) => ids.push(n),
            Err(e) => {
                eprintln!("entities approve: invalid id '{a}': {e}");
                return ExitCode::from(2);
            }
        }
    }
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let mut any_not_found = false;
    let total = ids.len();
    for (idx, id) in ids.iter().enumerate() {
        match entities_approve_and_audit(&pool, *id).await {
            Ok(ApproveOutcome::Approved { kind, name }) => {
                println!("id={id}: approved {kind} {name}");
            }
            Ok(ApproveOutcome::AlreadyApproved) => {
                println!("id={id}: already approved");
            }
            Ok(ApproveOutcome::NotFound) => {
                println!("id={id}: not found");
                any_not_found = true;
            }
            Err(e) => {
                eprintln!("entities approve: id={id}: {e}");
                let remaining = total - idx - 1;
                if remaining > 0 {
                    eprintln!(
                        "entities approve: stopped after error on id={id}; \
                         {remaining} remaining id(s) not attempted",
                    );
                }
                return ExitCode::from(1);
            }
        }
    }
    if any_not_found { ExitCode::from(1) } else { ExitCode::from(0) }
}

async fn entities_reject(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::entities_reject_and_audit;
    use kastellan_db::entities::RejectOutcome;
    use kastellan_db::pool::connect_runtime_pool;

    if args.is_empty() {
        eprintln!("usage: kastellan-cli entities reject <id> [<id>...]");
        return ExitCode::from(2);
    }
    let mut ids: Vec<i64> = Vec::with_capacity(args.len());
    for a in args {
        match a.parse::<i64>() {
            Ok(n) => ids.push(n),
            Err(e) => {
                eprintln!("entities reject: invalid id '{a}': {e}");
                return ExitCode::from(2);
            }
        }
    }
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let mut any_not_found = false;
    let total = ids.len();
    for (idx, id) in ids.iter().enumerate() {
        match entities_reject_and_audit(&pool, *id).await {
            Ok(RejectOutcome::Rejected { kind, name, mentions_dropped }) => {
                println!("id={id}: rejected {kind} {name} (mentions_dropped={mentions_dropped})");
            }
            Ok(RejectOutcome::NotFound) => {
                println!("id={id}: not found");
                any_not_found = true;
            }
            Err(e) => {
                eprintln!("entities reject: id={id}: {e}");
                let remaining = total - idx - 1;
                if remaining > 0 {
                    eprintln!(
                        "entities reject: stopped after error on id={id}; \
                         {remaining} remaining id(s) not attempted",
                    );
                }
                return ExitCode::from(1);
            }
        }
    }
    if any_not_found { ExitCode::from(1) } else { ExitCode::from(0) }
}

async fn entities_merge(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::entities_merge_and_audit;
    use kastellan_db::entities::EntitiesError;
    use kastellan_db::pool::connect_runtime_pool;

    let mut keep: Option<i64> = None;
    let mut drop_ids: Option<Vec<i64>> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--keep" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--keep requires a value"); return ExitCode::from(2); }
                };
                keep = Some(match v.parse() {
                    Ok(n) => n,
                    Err(e) => { eprintln!("--keep '{v}' is not an integer: {e}"); return ExitCode::from(2); }
                });
                i += 2;
            }
            "--drop" => {
                if drop_ids.is_some() {
                    eprintln!("--drop may only appear once; pass a comma-separated list");
                    return ExitCode::from(2);
                }
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--drop requires a value"); return ExitCode::from(2); }
                };
                drop_ids = Some(match parse_id_list(v) {
                    Ok(v) => v,
                    Err(e) => { eprintln!("{e}"); return ExitCode::from(2); }
                });
                i += 2;
            }
            other => {
                eprintln!("entities merge: unknown flag '{other}'");
                return ExitCode::from(2);
            }
        }
    }
    let keep = match keep {
        Some(k) => k,
        None => { eprintln!("entities merge requires --keep <id>"); return ExitCode::from(2); }
    };
    let drop_ids = match drop_ids {
        Some(d) => d,
        None => { eprintln!("entities merge requires --drop <id>[,<id>...]"); return ExitCode::from(2); }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match entities_merge_and_audit(&pool, keep, &drop_ids).await {
        Ok(outcome) => {
            println!(
                "merged: kept id={} ({} {}), dropped={:?}, retargeted={}, duplicates_dropped={}",
                outcome.kept_id, outcome.kept_kind, outcome.kept_name,
                outcome.dropped_ids,
                outcome.links_retargeted, outcome.links_dropped_as_duplicate,
            );
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("entities merge: {e}");
            match e {
                EntitiesError::KindMismatch { .. }
                | EntitiesError::NotFound(_)
                | EntitiesError::NoDropIds
                | EntitiesError::KeepInDropList(_) => ExitCode::from(2),
                EntitiesError::Db(_) => ExitCode::from(1),
            }
        }
    }
}

/// `entities reembed` — backfill `entities.embedding` for every entity whose
/// embedding is NULL, through the real `RouterEmbedder` (same config as the
/// daemon). Prints `scanned=/embedded=/skipped=`; exits non-zero when a batch
/// found rows but embedded none (e.g. an unreachable embed endpoint) so a
/// scripted `reembed && next-step` chain does not proceed. Takes no args.
async fn entities_reembed(args: &[String]) -> ExitCode {
    use std::sync::Arc;

    use kastellan_core::memory::{
        format_reembed_report, reembed_batch_failed, reembed_entities_null, RouterEmbedder,
    };
    use kastellan_db::pool::connect_runtime_pool;

    if !args.is_empty() {
        eprintln!("usage: kastellan-cli entities reembed");
        return ExitCode::from(2);
    }

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

    // Build the Router-backed embedder. `from_env` reads the host's
    // KASTELLAN_LLM_* config — run this with the same env the daemon uses so
    // backfilled vectors match on-insert ones.
    let router_cfg = match kastellan_llm_router::RouterConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("entities reembed: RouterConfig::from_env: {e}");
            return ExitCode::from(1);
        }
    };
    let router = match kastellan_llm_router::Router::new(router_cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("entities reembed: Router::new: {e}");
            return ExitCode::from(1);
        }
    };
    let embedder = RouterEmbedder::new(pool.clone(), router);

    match reembed_entities_null(&pool, &embedder).await {
        Ok(report) => {
            println!("{}", format_reembed_report(&report));
            // A batch that found rows but embedded none exits non-zero; the
            // idempotent no-op (scanned==0) exits 0.
            if reembed_batch_failed(&report) {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(e) => {
            eprintln!("entities reembed: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod entities_parser_tests {
    use super::{parse_entity_state, parse_id_list};

    #[test]
    fn parse_entity_state_accepts_canonical_lowercase_and_case_insensitive() {
        use kastellan_db::entities::EntityState;
        assert_eq!(parse_entity_state("quarantined").unwrap(), EntityState::Quarantined);
        assert_eq!(parse_entity_state("APPROVED").unwrap(),    EntityState::Approved);
        assert_eq!(parse_entity_state("Any").unwrap(),         EntityState::Any);
        assert_eq!(parse_entity_state("  approved  ").unwrap(), EntityState::Approved);
        assert!(parse_entity_state("OTHER").is_err());
        assert!(parse_entity_state("").is_err());
    }

    #[test]
    fn parse_id_list_accepts_comma_separated_and_rejects_empty_segments() {
        assert_eq!(parse_id_list("1,2,3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_id_list(" 4 , 5 ,6").unwrap(), vec![4, 5, 6]);
        assert_eq!(parse_id_list("7").unwrap(), vec![7]);
        assert!(parse_id_list("1,,2").is_err());
        assert!(parse_id_list(",").is_err());
        assert!(parse_id_list("").is_err());
        assert!(parse_id_list("foo").is_err());
        assert!(parse_id_list("1,foo,3").is_err());
    }

    // `parse_kinds_add_args` tests live in
    // `crate::entities_kinds::tests` post-split (Issue #112).
}
