//! `entities kinds {add, remove, list}` — operator-managed vocabulary
//! for the `entity_kinds` lookup table.
//!
//! Symmetric to [`crate::relations_kinds`]; both ride on
//! [`hhagent_db::pool::connect_admin_pool`] for `add` / `remove`
//! because migration 0016 REVOKEs INSERT/UPDATE/DELETE/TRUNCATE on
//! `entity_kinds` from the runtime role (the daemon must not widen
//! vocab on its own — a compromised extractor must not be able to add
//! kinds its operator never approved).
//!
//! `list` is SELECT-only; runtime role has SELECT per migration 0015,
//! so the read path uses [`hhagent_db::pool::connect_runtime_pool`].
//! Mirror of the same choice in [`crate::relations_kinds::run`]'s
//! `list` arm — gives operators without admin-pool credentials a
//! working browse path.
//!
//! ## Audit posture
//!
//! `add` and `remove` emit exactly one `actor='cli'
//! action='entity_kinds.{add,remove}'` audit row per real state
//! change. Idempotent no-ops (re-adding an existing kind, removing an
//! absent kind) write no row. Validation errors
//! (`EntityKindError::InvalidKind` / `KindHasNul`) and the
//! `RemovalOfUndefinedRejected` sentinel-rejection write no row
//! either. Same posture as
//! [`hhagent_core::cli_audit::tools_allowlist_add_and_audit`].
//!
//! Lifted from `entities.rs` per Issue #112 to keep the file under
//! the 500-LOC soft cap; see also Issue #111 for the kinds-CLI
//! shared-lift tech-debt.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

/// Per-action dispatcher for `entities kinds <add|remove|list>`.
///
/// Per [Issue #97](https://github.com/hherb/hhagent/issues/97)
/// posture, `with_runtime` is called only from known-action arms;
/// unknown actions exit 2 *before* any tokio runtime construction.
pub(crate) fn run(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli entities kinds <add|remove|list> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "add" => with_runtime("entities kinds", entities_kinds_add(&args[1..])),
        "remove" => with_runtime("entities kinds", entities_kinds_remove(&args[1..])),
        "list" => with_runtime("entities kinds", entities_kinds_list(&args[1..])),
        other => {
            eprintln!("entities kinds: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

/// Parse `entities kinds add` args:
///   * `<kind>`                              → (kind, None)
///   * `<kind> --description "<text>"`       → (kind, Some(text))
///
/// Mirror of [`crate::relations_kinds::parse_add_args`]. Returns an
/// `Err` carrying a printable usage line on shape errors so the caller
/// can fail with exit-2 + the line on stderr.
fn parse_kinds_add_args(args: &[String]) -> Result<(String, Option<String>), String> {
    match args {
        [kind] => Ok((kind.clone(), None)),
        [kind, flag, value] if flag == "--description" => {
            Ok((kind.clone(), Some(value.clone())))
        }
        _ => Err(
            "usage: hhagent-cli entities kinds add <kind> [--description \"<text>\"]"
                .to_string(),
        ),
    }
}

async fn entities_kinds_add(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::entity_kinds_add_and_audit;
    use hhagent_db::entity_kinds::EntityKindError;
    use hhagent_db::pool::connect_admin_pool;

    let (kind, description) = match parse_kinds_add_args(args) {
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

    match entity_kinds_add_and_audit(&pool, &kind, description.as_deref()).await {
        Ok(true) => {
            println!("added {kind}");
            ExitCode::from(0)
        }
        Ok(false) => {
            println!("already present");
            ExitCode::from(0)
        }
        Err(e @ (EntityKindError::InvalidKind | EntityKindError::KindHasNul)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

async fn entities_kinds_remove(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::entity_kinds_remove_and_audit;
    use hhagent_db::entity_kinds::EntityKindError;
    use hhagent_db::pool::connect_admin_pool;

    let kind = match args {
        [k] => k.clone(),
        _ => {
            eprintln!("usage: hhagent-cli entities kinds remove <kind>");
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

    match entity_kinds_remove_and_audit(&pool, &kind).await {
        Ok(true) => {
            println!("removed {kind}");
            ExitCode::from(0)
        }
        Ok(false) => {
            println!("not present");
            ExitCode::from(0)
        }
        Err(e @ (EntityKindError::InvalidKind | EntityKindError::KindHasNul)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e @ EntityKindError::RemovalOfUndefinedRejected) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

async fn entities_kinds_list(args: &[String]) -> ExitCode {
    use hhagent_db::entity_kinds::list_all;
    use hhagent_db::pool::connect_admin_pool;

    if !args.is_empty() {
        eprintln!("usage: hhagent-cli entities kinds list");
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
    // Identical column shape to `relations kinds list` for symmetric
    // operator UX.
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

#[cfg(test)]
mod tests {
    use super::parse_kinds_add_args;

    // Mirror of `crate::relations_kinds::tests::parse_add_args_*` —
    // kept symmetric so a future refactor that unifies the two
    // parsers (or notices they're identical and lifts to a shared
    // `common::parse_kind_add_args` helper) can confirm both call
    // sites still satisfy the contract.

    #[test]
    fn parse_kinds_add_args_kind_only_returns_no_description() {
        let parsed = parse_kinds_add_args(&["person".to_string()]).unwrap();
        assert_eq!(parsed, ("person".to_string(), None));
    }

    #[test]
    fn parse_kinds_add_args_with_description_flag_returns_some() {
        let args = vec![
            "person".to_string(),
            "--description".to_string(),
            "a named individual".to_string(),
        ];
        let parsed = parse_kinds_add_args(&args).unwrap();
        assert_eq!(
            parsed,
            ("person".to_string(), Some("a named individual".to_string()))
        );
    }

    #[test]
    fn parse_kinds_add_args_rejects_empty() {
        let err = parse_kinds_add_args(&[]).unwrap_err();
        assert!(err.contains("usage"), "expected usage line: {err}");
    }

    #[test]
    fn parse_kinds_add_args_rejects_unknown_flag() {
        let args = vec![
            "person".to_string(),
            "--unknown".to_string(),
            "value".to_string(),
        ];
        let err = parse_kinds_add_args(&args).unwrap_err();
        assert!(err.contains("usage"), "expected usage line: {err}");
    }

    #[test]
    fn parse_kinds_add_args_rejects_dangling_description() {
        let args = vec!["person".to_string(), "--description".to_string()];
        let err = parse_kinds_add_args(&args).unwrap_err();
        assert!(err.contains("usage"), "expected usage line: {err}");
    }
}
