//! `install` / `uninstall` — operator bring-up of a per-user supervised
//! Kastellan. Thin wrapper over `kastellan_core::install`.

use std::io::{self, Write};
use std::process::ExitCode;

use kastellan_core::install::plan::parse_install_args;
use kastellan_core::install::run::{run_install, run_uninstall};

pub(crate) fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(&args[1..]),
        _ => {
            eprintln!("usage: kastellan-cli install [--llm-model <m>] [--llm-url <u>] [--embedding-model <m>] [--pg-bin-dir <d>] [--from <d>] [--no-start]");
            eprintln!("       kastellan-cli uninstall [--purge]");
            ExitCode::from(2)
        }
    }
}

fn install(args: &[String]) -> ExitCode {
    let parsed = match parse_install_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\nusage: kastellan-cli install [--llm-model <name>] [--llm-url <url>] [--embedding-model <name>] [--pg-bin-dir <dir>] [--from <dir>] [--no-start]");
            return ExitCode::from(2);
        }
    };
    match run_install(parsed) {
        Ok(()) => ExitCode::from(0),
        Err(e) => { eprintln!("install failed: {e}"); ExitCode::from(1) }
    }
}

fn uninstall(args: &[String]) -> ExitCode {
    let purge = match args {
        [] => false,
        [flag] if flag == "--purge" => true,
        _ => { eprintln!("usage: kastellan-cli uninstall [--purge]"); return ExitCode::from(2); }
    };
    if purge {
        eprint!("--purge DELETES the Postgres cluster + stored secrets. Type 'purge' to confirm: ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() || line.trim() != "purge" {
            eprintln!("aborted.");
            return ExitCode::from(1);
        }
    }
    match run_uninstall(purge) {
        Ok(()) => ExitCode::from(0),
        Err(e) => { eprintln!("uninstall failed: {e}"); ExitCode::from(1) }
    }
}
