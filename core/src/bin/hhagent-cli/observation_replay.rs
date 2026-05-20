//! `observation replay [--captures-dir PATH] [--model SLUG]` — load
//! observation-phase captures from disk and re-run them through the
//! production review chain so the operator can iterate on
//! `ConstitutionalGuard` / `DeterministicPolicy` rule bodies offline.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::common::multi_thread_runtime;

pub(crate) fn run_observation(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli observation replay [opts]");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "replay" => run_observation_replay(&args[1..]),
        other => {
            eprintln!("observation: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

fn run_observation_replay(args: &[String]) -> ExitCode {
    let mut captures_dir: Option<PathBuf> = None;
    let mut model_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--captures-dir" => {
                i += 1;
                match args.get(i) {
                    Some(p) => captures_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--captures-dir requires a PATH argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--model" => {
                i += 1;
                match args.get(i) {
                    Some(s) => model_filter = Some(s.clone()),
                    None => {
                        eprintln!("--model requires a SLUG argument");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("observation replay: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let dir = captures_dir.unwrap_or_else(default_captures_dir);

    let rt = match multi_thread_runtime("observation replay") {
        Ok(rt) => rt,
        Err(code) => return code,
    };

    rt.block_on(observation_replay_async(&dir, model_filter.as_deref()))
}

/// Default captures dir. For `cargo run` invocations
/// `CARGO_MANIFEST_DIR` points at `core/`; the workspace root is one
/// level up. For installed binaries neither env var is set; fall back
/// to CWD-relative `tests/observation/captures`. Operator can always
/// override via `--captures-dir`.
///
/// Invariant: this binary lives in the `core/` crate. If it ever
/// moves, the `pop()`-to-workspace-root assumption breaks and the
/// default path resolves to the wrong place. The `debug_assert`
/// below catches the relocation during local dev (release builds
/// will silently produce the wrong default — `--captures-dir`
/// remains the escape hatch).
fn default_captures_dir() -> PathBuf {
    if let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let mut p = PathBuf::from(manifest);
        debug_assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("core"),
            "default_captures_dir assumes hhagent-cli lives in core/ \
             (CARGO_MANIFEST_DIR = {p:?})"
        );
        p.pop(); // strip `/core` to reach workspace root
        p.push("tests/observation/captures");
        return p;
    }
    PathBuf::from("tests/observation/captures")
}

async fn observation_replay_async(
    dir: &std::path::Path,
    model_filter: Option<&str>,
) -> ExitCode {
    use std::sync::Arc;
    use hhagent_core::cassandra::review::{
        ChainReviewStage, ConstitutionalGuard, DeterministicPolicy,
    };
    use hhagent_core::observation::replay::{
        format_report_table, load_captures_from_dir, replay_capture, ReplayResult,
    };

    let loaded = match load_captures_from_dir(dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("observation replay: cannot open {dir:?}: {e}");
            return ExitCode::from(1);
        }
    };

    if loaded.is_empty() {
        println!("(no captures found in {})", dir.display());
        return ExitCode::from(0);
    }

    // Production chain composition. Operator iterates by editing the
    // ConstitutionalGuard / DeterministicPolicy bodies in
    // core/src/cassandra/review.rs and re-running this subcommand.
    let chain = ChainReviewStage::new(vec![
        Arc::new(ConstitutionalGuard),
        Arc::new(DeterministicPolicy),
    ]);

    let mut results: Vec<ReplayResult> = Vec::new();
    let mut filtered_out: u32 = 0;
    for entry in loaded {
        if let Some(filter) = model_filter {
            let fname = entry.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !fname.contains(filter) {
                filtered_out = filtered_out.saturating_add(1);
                continue;
            }
        }
        let r = replay_capture(&entry.capture, &chain).await;
        results.push(r);
    }

    if results.is_empty() {
        eprintln!(
            "observation replay: no captures matched filter (--model {} filtered out {})",
            model_filter.unwrap_or("<none>"),
            filtered_out,
        );
        return ExitCode::from(0);
    }

    print!("{}", format_report_table(&results));
    ExitCode::from(0)
}
