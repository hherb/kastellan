//! `memory l3 {list,approve,pin,revoke,remove,run}` — operator-facing
//! inspection, trust management, pruning, and execution of layer-3
//! (crystallised skill) memories. Skills are agent-crystallised, never
//! operator-authored, so there is no `add`. `approve`/`pin`/`revoke`/`remove`/`run`
//! each emit their own `actor='cli'` audit row(s); `run` (operator-triggered
//! invocation) is dry-run by default and only dispatches under `--execute`.
//!
//! This module is a thin dispatcher; each subcommand lives in its own sibling
//! file, and `approve`/`pin` share the [`shared`] prologue:
//!
//! - [`list`] — inspect skills
//! - [`show`] — print full skill payload (read source before approving)
//! - [`trust`] — `approve` + `pin` (trust-ladder transitions)
//! - [`revoke`] — downgrade trust to `untrusted`
//! - [`remove`] — prune a skill row
//! - [`run`] — operator-triggered invocation (dry-run by default)
//! - [`shared`] — registry-snapshot reader + the approve/pin prologue

use std::process::ExitCode;

use crate::common::with_runtime;

mod list;
mod remove;
mod revoke;
mod run;
mod shared;
mod show;
mod trust;

use list::memory_l3_list;
use remove::memory_l3_remove;
use revoke::memory_l3_revoke;
use run::memory_l3_run;
use show::memory_l3_show;
use trust::{memory_l3_approve, memory_l3_pin};

pub(crate) fn run_memory_l3(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli memory l3 <list|show|approve|pin|revoke|remove|run> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"    => with_runtime("memory l3", memory_l3_list(&args[1..])),
        "show"    => with_runtime("memory l3", memory_l3_show(&args[1..])),
        "approve" => with_runtime("memory l3", memory_l3_approve(&args[1..])),
        "pin"     => with_runtime("memory l3", memory_l3_pin(&args[1..])),
        "revoke"  => with_runtime("memory l3", memory_l3_revoke(&args[1..])),
        "remove"  => with_runtime("memory l3", memory_l3_remove(&args[1..])),
        "run"     => with_runtime("memory l3", memory_l3_run(&args[1..])),
        other     => {
            eprintln!("memory l3: unknown action '{other}'; expected: list | show | approve | pin | revoke | remove | run");
            ExitCode::from(2)
        }
    }
}
