//! `hhagent-cli` — operator-facing CLI tool.
//!
//! Today the only subcommand is `audit tail`, which streams the
//! daemon's `audit-YYYY-MM-DD.jsonl` files from
//! `~/.local/state/hhagent/`. The viewer needs no Postgres connection
//! and works against a daemon that has crashed (the JSONL files are
//! the durable replica of the `audit_log` DB table written by the
//! mirror task — see [`hhagent_core::audit_mirror`]).
//!
//! Future subcommands (status, memory dump, manual dispatch, …) will
//! plug in here as the daemon grows side-channels.
//!
//! Usage:
//!
//! ```text
//! hhagent-cli audit tail [--from-start] [--no-follow] [--state-dir PATH]
//! ```
//!
//! Options:
//!   --from-start   Replay every existing line before switching to
//!                  follow mode (default: anchor at end of latest
//!                  file, like `tail -f`).
//!   --no-follow    Exit after replaying existing content (like
//!                  `cat`); only meaningful with --from-start.
//!   --state-dir P  Override the state directory (default:
//!                  $HHAGENT_STATE_DIR or $HOME/.local/state/hhagent).
//!
//! The CLI parser is hand-rolled (no `clap` dep) because the surface
//! is tiny and a parser dep would dominate the binary footprint. If
//! we ever grow to ~5+ subcommands or richer flag parsing, swapping
//! in `clap` is a strictly local change here.

use std::path::PathBuf;
use std::process::ExitCode;

use hhagent_core::audit_mirror;
use hhagent_core::audit_tail::{tail_loop, TailConfig};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("{}", help_text());
        return ExitCode::from(2);
    }
    match args[1].as_str() {
        "audit" => match args.get(2).map(|s| s.as_str()) {
            Some("tail") => run_audit_tail(&args[3..]),
            _ => {
                eprintln!("usage: hhagent-cli audit tail [opts]");
                ExitCode::from(2)
            }
        },
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
    hhagent-cli audit tail [--from-start] [--no-follow] [--state-dir PATH]

flags:
    --from-start    Replay every line in every existing audit file
                    before switching to follow mode.
    --no-follow     Exit after replaying existing content (use with
                    --from-start for a 'cat' of the JSONL files).
    --state-dir P   Override the state dir (default: $HHAGENT_STATE_DIR
                    or $HOME/.local/state/hhagent).
"
}

fn run_audit_tail(args: &[String]) -> ExitCode {
    let mut from_start = false;
    let mut follow = true;
    let mut state_dir_arg: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from-start" => from_start = true,
            "--no-follow" => follow = false,
            "--state-dir" => {
                i += 1;
                match args.get(i) {
                    Some(p) => state_dir_arg = Some(p.clone()),
                    None => {
                        eprintln!("--state-dir requires a path argument");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("unknown audit-tail flag: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let state_dir: PathBuf = match state_dir_arg {
        Some(p) => PathBuf::from(p),
        None => match std::env::var_os(audit_mirror::ENV_STATE_DIR) {
            Some(p) => PathBuf::from(p),
            None => match audit_mirror::default_state_dir() {
                Some(p) => p,
                None => {
                    eprintln!(
                        "$HOME unset and no --state-dir given; cannot resolve audit dir"
                    );
                    return ExitCode::from(2);
                }
            },
        },
    };

    // The viewer does file I/O + a 250 ms sleep loop, no
    // `block_in_place` — a current-thread runtime is the right shape
    // (smallest footprint, no extra worker thread). Calling
    // `Builder::new_current_thread()` explicitly so the binary's
    // runtime choice is independent of which `tokio` feature flags
    // happen to be active in the workspace deps.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let result = rt.block_on(async {
        let stdout = tokio::io::stdout();
        tail_loop(
            TailConfig {
                state_dir,
                from_start,
                follow,
            },
            stdout,
        )
        .await
    });

    match result {
        Ok(()) => ExitCode::from(0),
        // BrokenPipe is the canonical "downstream `head` / `less`
        // closed early" exit; not an error from the operator's
        // perspective. Match BSD `tail`'s behaviour.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::from(0),
        Err(e) => {
            eprintln!("hhagent-cli audit tail: {e}");
            ExitCode::from(1)
        }
    }
}
