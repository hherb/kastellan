//! End-to-end integration tests for the gliner-relex worker.
//!
//! These tests spawn the real Python worker
//! (`workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex`) on
//! the real model weights staged under
//! `$HHAGENT_DATA_DIR/workers/gliner-relex/weights/multi-v1.0/`. They
//! exercise the full Slice 2 wiring chain: `gliner_relex_entry` →
//! `IdleTimeoutLifecycle::acquire` → `tool_host::dispatch` → JSON-RPC
//! over stdio → Python `extract` dispatch → response decode through
//! [`hhagent_core::workers::gliner_relex::ExtractResponse`].
//!
//! Without the venv + weights (and without a running Postgres, and
//! without bwrap/Seatbelt), every test in this file `[SKIP]`s cleanly.
//! That matches the default deployment posture: gliner-relex is opt-in
//! via `HHAGENT_GLINER_RELEX_ENABLE=1` and operators run
//! `scripts/workers/gliner-relex/install.sh` before flipping the flag.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! and the Slice 2 section of
//! `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md` for the
//! design.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

/// Resolve the venv shim path relative to the workspace root.
///
/// Returns `None` (with a `[SKIP]` print on stderr) when the path
/// doesn't exist. Mirrors the resolution `core/src/main.rs::
/// build_gliner_relex_entry` does in production except that this
/// helper never honours the daemon's `HHAGENT_GLINER_RELEX_VENV_DIR`
/// override — tests always run against the in-tree
/// `workers/gliner-relex/.venv/`.
fn resolve_worker_script() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent — broken workspace layout")
        .to_path_buf();
    let script = workspace_root
        .join("workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex");
    if !script.exists() {
        eprintln!(
            "\n[SKIP] gliner-relex venv shim not built at {} — run scripts/workers/gliner-relex/install.sh\n",
            script.display()
        );
        return None;
    }
    Some(script)
}

/// Resolve the weights snapshot dir for `multi-v1.0`.
///
/// Honours `HHAGENT_DATA_DIR` first, falls back to
/// `$HOME/.local/share/hhagent` (mirrors `build_gliner_relex_entry`'s
/// resolution). Skip-as-pass when the dir is missing on disk.
fn resolve_weights_dir() -> Option<PathBuf> {
    let data_dir = std::env::var("HHAGENT_DATA_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share/hhagent"))
        })?;
    let weights = data_dir.join("workers/gliner-relex/weights/multi-v1.0");
    if !weights.is_dir() {
        eprintln!(
            "\n[SKIP] gliner-relex weights dir missing at {} — run scripts/workers/gliner-relex/install.sh\n",
            weights.display()
        );
        return None;
    }
    Some(weights)
}

/// Skip-helper smoke test: confirms the resolution helpers compile +
/// run without panicking on hosts where the venv/weights are absent.
/// The real assertions land in Tasks 2.6-2.8.
#[test]
fn skip_helpers_compile_and_return_cleanly_on_unstaged_hosts() {
    // We don't assert .is_some() / .is_none() here — the helper's
    // contract is "either return Some(existing) or print a [SKIP] line
    // and return None". Calling them is the smoke test; a panic would
    // be the failure mode.
    let _ = resolve_worker_script();
    let _ = resolve_weights_dir();
}
