//! `relations kinds {add, remove, list}` — operator-managed vocabulary
//! for the `relation_kinds` lookup table.
//!
//! Symmetric to [`crate::entities_kinds`]; both ride on
//! [`hhagent_db::pool::connect_admin_pool`] for `add` / `remove`
//! because migration 0017 REVOKEs INSERT/UPDATE/DELETE/TRUNCATE on
//! `relation_kinds` from the runtime role (the daemon must not widen
//! vocab on its own — a compromised extractor must not be able to add
//! relation labels its operator never approved).
//!
//! `list` is SELECT-only; runtime role has SELECT per migration 0017,
//! so the read path uses [`hhagent_db::pool::connect_runtime_pool`].
//! Mirror of the same choice in [`crate::entities_kinds::run`]'s
//! `list` arm.
//!
//! ## Audit posture
//!
//! `add` and `remove` emit exactly one `actor='cli'
//! action='relation_kinds.{add,remove}'` audit row per real state
//! change. Idempotent no-ops, validation errors, and the
//! `RemovalOfUndefinedRejected` sentinel-rejection write no audit row.
//! Mirror of [`hhagent_core::cli_audit::tools_allowlist_add_and_audit`].
//!
//! Lifted from `relations.rs` per Item 22 (HANDOVER) to keep the
//! per-substree files under the 500-LOC soft cap.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

/// Per-action dispatcher for `relations kinds <add|remove|list>`.
///
/// Per [Issue #97](https://github.com/hherb/hhagent/issues/97)
/// posture, `with_runtime` is called only from known-action arms;
/// unknown actions exit 2 *before* any tokio runtime construction.
pub(crate) fn run(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli relations kinds <add|remove|list> ...");
        return ExitCode::from(2);
    }
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

/// Parse `relations kinds add` args:
///   * `<kind>`                              → (kind, None)
///   * `<kind> --description "<text>"`       → (kind, Some(text))
///
/// Mirror of [`crate::entities_kinds::parse_kinds_add_args`]. Returns
/// an `Err` carrying a printable usage line on shape errors so the
/// caller can fail with exit-2 + the line on stderr.
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
    use hhagent_db::pool::connect_runtime_pool;
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
    // `list_all` is SELECT-only on `relation_kinds`. The runtime role
    // has SELECT (migration 0017), so use the runtime pool so this
    // action works for operators without cluster-superuser peer-auth
    // (Issue [#111](https://github.com/hherb/hhagent/issues/111) item
    // 1). `add` / `remove` continue to use `connect_admin_pool`
    // because 0017 REVOKEs writes from the runtime role.
    let pool = match connect_runtime_pool(&spec).await {
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

#[cfg(test)]
mod tests {
    use super::parse_add_args;

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
}
