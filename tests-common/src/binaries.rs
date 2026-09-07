//! Workspace target-dir-aware binary discovery for integration tests
//! that exec the workspace binaries.
//!
//! The compute is `CARGO_TARGET_DIR.unwrap_or(<workspace_root>/target)/debug/<name>`.
//! `env!("CARGO_MANIFEST_DIR")` resolves at *compile time* to the
//! manifest dir of this crate (`tests-common/`), and its parent is the
//! workspace root because `tests-common` lives at the same depth as
//! the runtime crates.
//!
//! All binaries are `cargo build --workspace` artifacts; callers
//! `[SKIP]` cleanly when `exists()` returns `false` (i.e. the binary
//! was not built yet — common on a freshly-cloned tree before the
//! first `cargo build`).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Returns the path to `<workspace_root>/target/debug/<name>`,
/// honouring `CARGO_TARGET_DIR` if set.
///
/// Existence is **not** checked — callers decide whether to skip,
/// panic, or build on the fly.
pub fn workspace_target_binary(name: &str) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            manifest
                .parent()
                .expect("CARGO_MANIFEST_DIR has no parent — broken workspace layout")
                .join("target")
        });
    target.join("debug").join(name)
}

/// Path to `kastellan-worker-shell-exec`.
pub fn shell_exec_worker_binary() -> PathBuf {
    workspace_target_binary("kastellan-worker-shell-exec")
}

/// Path to the agent core daemon (`kastellan`).
pub fn core_binary() -> PathBuf {
    workspace_target_binary("kastellan")
}

/// Path to the operator CLI (`kastellan-cli`).
pub fn cli_binary() -> PathBuf {
    workspace_target_binary("kastellan-cli")
}

/// Path to the egress-proxy sidecar binary, or `None` with a `[SKIP]` line when
/// it has not been built.
///
/// Deliberately **debug-only** (via [`workspace_target_binary`]): every forced-
/// egress e2e that spawns a real sidecar is itself a `cargo test` debug build, so
/// the debug artifact is the one guaranteed to match the tree under test. A
/// release fallback would reintroduce the stale-binary trap that has already cost
/// this repo a false leak finding — `locate_microvm_run` prefers `target/release`
/// and silently ran an old launcher.
pub fn egress_proxy_bin_or_skip() -> Option<PathBuf> {
    match egress_proxy_bin_or_reason() {
        Ok(p) => Some(p),
        Err(reason) => {
            eprint!("{}", crate::skip::skip_line(&reason));
            None
        }
    }
}

/// The egress-proxy sidecar binary, or the *reason* it is unavailable — no
/// `[SKIP]` prefix, so a caller that must not skip can render it as a
/// failure.
///
/// The `*_or_reason` half of [`egress_proxy_bin_or_skip`], added for #679:
/// four micro-VM suites had each grown their own byte-identical private copy
/// of the skip half, all of them invisible to
/// `KASTELLAN_MICROVM_REQUIRE_E2E`. See [`crate::microvm::dep_or_skip`].
pub fn egress_proxy_bin_or_reason() -> Result<PathBuf, String> {
    workspace_binary_or_reason("kastellan-worker-egress-proxy")
        .map_err(|_| {
            "egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`".to_string()
        })
}

/// A `cargo build --workspace` artifact by name, or the *reason* it is
/// missing — no `[SKIP]` prefix, for the same reason as its siblings.
///
/// Covers the host-side broker sidecars (`kastellan-worker-embed-broker`,
/// `kastellan-worker-search-broker`), which four micro-VM suites had been
/// checking with a hand-written `eprintln!("[SKIP] …")` — the shape
/// the source guard in `microvm::guard` now refuses, because a hand-written
/// line cannot be turned into a failure by any knob.
///
/// Debug-only via [`workspace_target_binary`], deliberately: every e2e that
/// spawns one of these is itself a `cargo test` debug build, so the debug
/// artifact is the one guaranteed to match the tree under test.
pub fn workspace_binary_or_reason(name: &str) -> Result<PathBuf, String> {
    let p = workspace_target_binary(name);
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!("{name} not built; run `cargo build --workspace`"))
    }
}

/// A [`Command`] for the operator CLI with the deliberately-minimal env every
/// CLI e2e test uses: `env_clear()` then exactly `PATH`, `LC_ALL`, `USER`, and
/// `KASTELLAN_DATA_DIR`.
///
/// The empty environment is load-bearing — these tests prove the daemon, not
/// the operator subprocess, owns the live tool registry (the #179 invariant),
/// so the CLI must NOT inherit `KASTELLAN_*_BIN`. Callers chain `.args(...)`
/// and any test-specific env (e.g. `KASTELLAN_L3_RUN_GRACE_SECS`).
pub fn cli_command(data_dir: &Path, user: &str) -> Command {
    let mut cmd = Command::new(cli_binary());
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", user)
        .env("KASTELLAN_DATA_DIR", data_dir.to_string_lossy().as_ref());
    cmd
}

#[cfg(test)]
mod tests {
    use super::{egress_proxy_bin_or_reason, workspace_binary_or_reason, workspace_target_binary};
    use crate::env::{env_lock, EnvVarGuard};
    use std::path::PathBuf;

    /// A missing artifact yields a reason that names BOTH the binary and the
    /// command that produces it. Under `KASTELLAN_MICROVM_REQUIRE_E2E` this
    /// string becomes a panic message and is the only thing the operator
    /// gets, so "not built" alone would leave them guessing which one.
    #[test]
    fn a_missing_workspace_binary_reason_names_the_binary_and_the_remedy() {
        let err = workspace_binary_or_reason("kastellan-worker-does-not-exist")
            .expect_err("a binary that cannot exist must not resolve");
        assert!(err.contains("kastellan-worker-does-not-exist"), "names the binary: {err}");
        assert!(err.contains("cargo build --workspace"), "names the remedy: {err}");
    }

    /// A *directory* at the artifact path is not an artifact, and a real file
    /// beside it is — both arms, one fixture.
    ///
    /// `is_file()` rather than `exists()` is the check: `target/debug/`
    /// routinely holds directories (`build/`, `deps/`, `incremental/`), so an
    /// `exists()` test would hand a caller a path it cannot exec.
    ///
    /// The `Ok` half is not decoration. Without it `workspace_binary_or_reason`
    /// returning `Err` unconditionally survives the suite — which under
    /// `KASTELLAN_MICROVM_REQUIRE_E2E` turns every broker and egress-proxy
    /// precondition into a permanent panic, and without it into a permanent
    /// skip. And the *path* is asserted, not just the `Ok`-ness, or the
    /// function could hand back any path at all.
    ///
    /// The `CARGO_TARGET_DIR` override is asserted before it is relied on: if
    /// it stopped being honoured, both lookups would miss under the real
    /// `target/debug` and the directory-vs-file distinction would silently
    /// stop being pinned.
    #[test]
    fn a_directory_is_not_an_artifact_but_a_file_beside_it_is() {
        const KEY: &str = "CARGO_TARGET_DIR";
        let _lock = env_lock();
        let tmp = std::env::temp_dir().join(format!("kastellan-binreason-{}", std::process::id()));
        let debug = tmp.join("debug");
        std::fs::create_dir_all(debug.join("adir")).expect("mkdir fixture");
        std::fs::write(debug.join("afile"), b"#!/bin/sh\n").expect("write fixture binary");
        let _restore = EnvVarGuard::set(KEY, tmp.to_str().expect("utf8 tmp"));

        let target_dir_is_honoured = workspace_target_binary("afile") == debug.join("afile");
        let dir = workspace_binary_or_reason("adir");
        let file = workspace_binary_or_reason("afile");
        std::fs::remove_dir_all(&tmp).ok();

        assert!(target_dir_is_honoured, "CARGO_TARGET_DIR must drive the lookup, or this is vacuous");
        assert!(dir.is_err(), "a directory must not pass as a built binary: {dir:?}");
        assert_eq!(
            file.expect("a real file at the artifact path resolves"),
            debug.join("afile"),
            "resolves to the artifact path itself, not merely to something"
        );
    }

    /// The egress-proxy resolver names the **right binary** and the remedy
    /// that builds it. The binary name is a bare literal in the source; swap it
    /// for any other worker and every micro-VM egress tier silently runs the
    /// wrong sidecar, which no other test would notice.
    #[test]
    fn the_egress_proxy_resolver_names_its_own_binary_and_remedy() {
        const KEY: &str = "CARGO_TARGET_DIR";
        let _lock = env_lock();
        let tmp = std::env::temp_dir().join(format!("kastellan-proxyreason-{}", std::process::id()));
        let debug = tmp.join("debug");
        std::fs::create_dir_all(&debug).expect("mkdir fixture");
        let _restore = EnvVarGuard::set(KEY, tmp.to_str().expect("utf8 tmp"));

        let absent = egress_proxy_bin_or_reason();
        std::fs::write(debug.join("kastellan-worker-egress-proxy"), b"#!/bin/sh\n")
            .expect("write fixture binary");
        let present = egress_proxy_bin_or_reason();
        std::fs::remove_dir_all(&tmp).ok();

        let reason = absent.expect_err("an empty target dir has no egress proxy");
        assert!(reason.contains("egress-proxy"), "names the sidecar: {reason}");
        assert!(
            reason.contains("cargo build -p kastellan-worker-egress-proxy"),
            "names the remedy that builds it: {reason}"
        );
        assert_eq!(
            present.expect("a built proxy resolves"),
            debug.join("kastellan-worker-egress-proxy"),
            "resolves the egress-proxy binary, not some other worker"
        );
    }

    /// `CARGO_TARGET_DIR` (when set) overrides the default
    /// `<workspace_root>/target`; otherwise the default applies. `env_lock()`
    /// serialises against any sibling test that reads the var, and the
    /// `EnvVarGuard` captures the real prior up front and restores it on drop
    /// — even on an unwinding assertion — so the intermediate mutation never
    /// leaks into another test.
    #[test]
    fn honours_cargo_target_dir_else_workspace_target() {
        const KEY: &str = "CARGO_TARGET_DIR";
        let _lock = env_lock();

        // `unset` records the true prior; its `Drop` restores it regardless of
        // the `set_var` below, so no manual save/restore is needed.
        let _restore = EnvVarGuard::unset(KEY);
        let got_default = workspace_target_binary("foo");

        std::env::set_var(KEY, "/custom/target");
        let got_override = workspace_target_binary("foo");

        let want_default = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join("debug")
            .join("foo");
        assert_eq!(got_default, want_default, "unset → workspace target/debug");
        assert_eq!(
            got_override,
            PathBuf::from("/custom/target/debug/foo"),
            "set → <CARGO_TARGET_DIR>/debug"
        );
    }
}
