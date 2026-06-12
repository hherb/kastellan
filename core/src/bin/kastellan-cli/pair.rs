//! `pair {issue, list, revoke}` — operator management of channel pairings
//! (comms slice #3).
//!
//! - `issue` mints a single-use, short-lived pairing code: a random secret is
//!   generated, only its SHA-256 is stored (`pairing_codes`), and the plaintext
//!   is **printed once** for the operator to hand to the new user out-of-band.
//!   The user sends it to the bot, which binds them (`pairings`).
//! - `list` shows active (or, with `--all`, all) pairings.
//! - `revoke` deactivates a pairing.
//!
//! `issue`/`revoke` use [`connect_admin_pool`]: migration 0018 REVOKEs INSERT on
//! `pairing_codes` and UPDATE on `pairings` from the runtime role (minting +
//! revoking are deliberate operator actions the daemon must not perform). `list`
//! is SELECT-only and uses the runtime pool.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

/// Number of random bytes in a pairing code (160 bits → infeasible to guess).
const CODE_BYTES: usize = 20;
/// Default code lifetime.
const DEFAULT_TTL_MINUTES: i64 = 10;

pub(crate) fn run(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli pair <issue|list|revoke> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "issue" => with_runtime("pair issue", pair_issue(&args[1..])),
        "list" => with_runtime("pair list", pair_list(&args[1..])),
        "revoke" => with_runtime("pair revoke", pair_revoke(&args[1..])),
        other => {
            eprintln!("pair: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

/// Parse `pair issue [--label <text>] [--ttl-mins <n>]`.
fn parse_issue_args(args: &[String]) -> Result<(Option<String>, i64), String> {
    let mut label: Option<String> = None;
    let mut ttl = DEFAULT_TTL_MINUTES;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--label" => {
                let v = args.get(i + 1).ok_or("--label requires a value")?;
                label = Some(v.clone());
                i += 2;
            }
            "--ttl-mins" => {
                let v = args.get(i + 1).ok_or("--ttl-mins requires a value")?;
                ttl = v.parse::<i64>().map_err(|_| "--ttl-mins must be an integer".to_string())?;
                if ttl <= 0 {
                    return Err("--ttl-mins must be positive".to_string());
                }
                i += 2;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    Ok((label, ttl))
}

/// Generate a random pairing code as a lowercase hex string (no ambiguous chars
/// to fuss over; copy-paste friendly).
fn generate_code() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; CODE_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn pair_issue(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_admin_pool;

    let (label, ttl) = match parse_issue_args(args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}\nusage: kastellan-cli pair issue [--label <text>] [--ttl-mins <n>]");
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

    let code = generate_code();
    let hash = kastellan_core::channel::ingest::sha256_hex(code.as_bytes());

    let id = match kastellan_db::pairings::insert_code(&pool, &hash, label.as_deref(), ttl).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("pair issue: {e}");
            return ExitCode::from(1);
        }
    };

    // Audit: hash + label + ttl only — NEVER the plaintext code.
    let _ = kastellan_db::audit::insert(
        &pool,
        "cli",
        "pairing.code_issued",
        serde_json::json!({"id": id, "code_sha256": hash, "label": label, "ttl_minutes": ttl}),
    )
    .await;

    println!("Pairing code (valid {ttl} min, single use):\n");
    println!("    {code}\n");
    println!("Give this to the new user out-of-band; they send it to the bot to pair.");
    ExitCode::from(0)
}

async fn pair_list(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;

    let include_revoked = match args {
        [] => false,
        [flag] if flag == "--all" => true,
        _ => {
            eprintln!("usage: kastellan-cli pair list [--all]");
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

    let rows = match kastellan_db::pairings::list_pairings(&pool, include_revoked).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    if rows.is_empty() {
        println!("(no pairings)");
        return ExitCode::from(0);
    }
    for p in rows {
        let status = match p.revoked_at {
            Some(at) => format!("revoked {at}"),
            None => "active".to_string(),
        };
        println!("{}  {}  [{}]  {}  ({})", p.channel, p.peer, p.method, status, p.paired_at);
    }
    ExitCode::from(0)
}

async fn pair_revoke(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_admin_pool;

    let (channel, peer) = match args {
        [c, p] => (c.clone(), p.clone()),
        _ => {
            eprintln!("usage: kastellan-cli pair revoke <channel> <peer>");
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

    match kastellan_db::pairings::revoke_pairing(&pool, &channel, &peer).await {
        Ok(true) => {
            let _ = kastellan_db::audit::insert(
                &pool,
                "cli",
                "pairing.revoked",
                serde_json::json!({"channel": channel, "peer": peer}),
            )
            .await;
            println!("revoked {channel}/{peer}");
            ExitCode::from(0)
        }
        Ok(false) => {
            println!("no active pairing for {channel}/{peer}");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("pair revoke: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_issue_defaults() {
        let (label, ttl) = parse_issue_args(&[]).unwrap();
        assert_eq!(label, None);
        assert_eq!(ttl, DEFAULT_TTL_MINUTES);
    }

    #[test]
    fn parse_issue_label_and_ttl() {
        let args = vec![
            "--label".to_string(),
            "for Alice".to_string(),
            "--ttl-mins".to_string(),
            "30".to_string(),
        ];
        let (label, ttl) = parse_issue_args(&args).unwrap();
        assert_eq!(label.as_deref(), Some("for Alice"));
        assert_eq!(ttl, 30);
    }

    #[test]
    fn parse_issue_rejects_bad_ttl_and_dangling_flags() {
        assert!(parse_issue_args(&["--ttl-mins".into(), "zero".into()]).is_err());
        assert!(parse_issue_args(&["--ttl-mins".into(), "0".into()]).is_err());
        assert!(parse_issue_args(&["--label".into()]).is_err());
        assert!(parse_issue_args(&["--bogus".into()]).is_err());
    }

    #[test]
    fn generated_codes_are_hex_and_unique() {
        let a = generate_code();
        let b = generate_code();
        assert_eq!(a.len(), CODE_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two codes must differ");
    }
}
