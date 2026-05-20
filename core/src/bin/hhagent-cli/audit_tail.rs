//! `audit tail` subcommand — streams the daemon's
//! `audit-YYYY-MM-DD.jsonl` mirror files under
//! `~/.local/state/hhagent/`. Needs no Postgres connection.

use std::path::PathBuf;
use std::process::ExitCode;

use hhagent_core::audit_mirror;
use hhagent_core::audit_tail::{tail_loop, TailConfig};

pub(crate) fn run_audit_tail(args: &[String]) -> ExitCode {
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
