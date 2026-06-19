//! `secret {put, list, delete}` — operator management of `db::secrets`
//! (Matrix Phase D Task 5 slice 0). Thin wrapper over
//! `kastellan_core::secrets::admin`; see
//! docs/superpowers/specs/2026-06-19-kastellan-cli-secret-command-design.md.

use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use kastellan_core::secrets::admin::{remove_secret, store_secret, Outcome};
use kastellan_db::pool::{connect_admin_pool, connect_runtime_pool};

use crate::common::{resolve_connect_spec, with_runtime};

/// Turn raw stdin bytes into the secret value. Unless `keep_raw`, strips
/// exactly one trailing `\n` (and a preceding `\r`) so `echo pw |` and
/// `printf %s pw |` both store the same bytes. Empty result is rejected.
pub(crate) fn read_secret_value(raw: &[u8], keep_raw: bool) -> Result<Vec<u8>, String> {
    let mut bytes = raw.to_vec();
    if !keep_raw && bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.is_empty() {
        return Err("empty secret value (nothing on stdin)".to_string());
    }
    Ok(bytes)
}

/// Parse `secret put <name> [--raw]` → `(name, keep_raw)`.
pub(crate) fn parse_put_args(args: &[String]) -> Result<(String, bool), String> {
    let mut name: Option<String> = None;
    let mut keep_raw = false;
    for a in args {
        match a.as_str() {
            "--raw" => {
                if keep_raw {
                    return Err("--raw given twice".to_string());
                }
                keep_raw = true;
            }
            s if s.starts_with("--") => return Err(format!("unknown flag {s}")),
            s => {
                if name.is_some() {
                    return Err(format!("unexpected argument {s}"));
                }
                name = Some(s.to_string());
            }
        }
    }
    let name = name.ok_or("put requires <name>")?;
    Ok((name, keep_raw))
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("put") => with_runtime("secret put", secret_put(&args[1..])),
        Some("list") => with_runtime("secret list", secret_list(&args[1..])),
        Some("delete") => with_runtime("secret delete", secret_delete(&args[1..])),
        _ => {
            eprintln!("usage: kastellan-cli secret <put|list|delete> ...");
            ExitCode::from(2)
        }
    }
}

async fn secret_put(args: &[String]) -> ExitCode {
    let (name, keep_raw) = match parse_put_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}\nusage: kastellan-cli secret put <name> [--raw]");
            return ExitCode::from(2);
        }
    };

    // Read the value: silent prompt on a TTY, raw stdin when piped.
    let value: Vec<u8> = if std::io::stdin().is_terminal() {
        match rpassword::prompt_password(format!("Value for secret {name:?}: ")) {
            Ok(s) => s.into_bytes(),
            Err(e) => {
                eprintln!("read secret: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        let mut raw = Vec::new();
        if let Err(e) = std::io::stdin().read_to_end(&mut raw) {
            eprintln!("read stdin: {e}");
            return ExitCode::from(1);
        }
        match read_secret_value(&raw, keep_raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
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
    let kp = match kastellan_db::secrets::OsKeyringProvider::ensure_initialized() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("keyring: {e}");
            return ExitCode::from(1);
        }
    };

    match store_secret(&pool, &kp, &name, &value).await {
        Ok(Outcome::Created) => {
            println!("stored {name} (created)");
            ExitCode::from(0)
        }
        Ok(Outcome::Updated) => {
            println!("stored {name} (updated)");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("secret put: {e}");
            ExitCode::from(1)
        }
    }
}

async fn secret_list(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("usage: kastellan-cli secret list");
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
    match kastellan_db::secrets::list(&pool).await {
        Ok(rows) => {
            if rows.is_empty() {
                println!("(no secrets)");
                return ExitCode::from(0);
            }
            for s in rows {
                println!("{}\t{}\t{}\t{}", s.name, s.key_id, s.created_at, s.updated_at);
            }
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("secret list: {e}");
            ExitCode::from(1)
        }
    }
}

async fn secret_delete(args: &[String]) -> ExitCode {
    let name = match args {
        [n] => n.clone(),
        _ => {
            eprintln!("usage: kastellan-cli secret delete <name>");
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
    match remove_secret(&pool, &name).await {
        Ok(true) => {
            println!("deleted {name}");
            ExitCode::from(0)
        }
        Ok(false) => {
            println!("no such secret {name}");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("secret delete: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_value_strips_one_trailing_newline() {
        assert_eq!(read_secret_value(b"hunter2\n", false).unwrap(), b"hunter2");
        assert_eq!(read_secret_value(b"hunter2\r\n", false).unwrap(), b"hunter2");
        assert_eq!(read_secret_value(b"hunter2", false).unwrap(), b"hunter2");
        // only ONE newline stripped
        assert_eq!(read_secret_value(b"hunter2\n\n", false).unwrap(), b"hunter2\n");
    }

    #[test]
    fn read_value_raw_keeps_exact_bytes() {
        assert_eq!(read_secret_value(b"hunter2\n", true).unwrap(), b"hunter2\n");
        assert_eq!(read_secret_value(b"a\nb\n", true).unwrap(), b"a\nb\n");
    }

    #[test]
    fn read_value_rejects_empty() {
        assert!(read_secret_value(b"", false).is_err());
        assert!(read_secret_value(b"\n", false).is_err()); // strips to empty
        assert!(read_secret_value(b"", true).is_err());
    }

    #[test]
    fn parse_put_name_and_raw() {
        assert_eq!(parse_put_args(&["s".into()]).unwrap(), ("s".to_string(), false));
        assert_eq!(
            parse_put_args(&["s".into(), "--raw".into()]).unwrap(),
            ("s".to_string(), true)
        );
        assert_eq!(
            parse_put_args(&["--raw".into(), "s".into()]).unwrap(),
            ("s".to_string(), true)
        );
    }

    #[test]
    fn parse_put_rejects_bad_args() {
        assert!(parse_put_args(&[]).is_err()); // missing name
        assert!(parse_put_args(&["--bogus".into()]).is_err());
        assert!(parse_put_args(&["a".into(), "b".into()]).is_err()); // two names
        assert!(parse_put_args(&["a".into(), "--raw".into(), "--raw".into()]).is_err());
    }
}
