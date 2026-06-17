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

use std::path::PathBuf;

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

#[cfg(test)]
mod tests {
    use super::workspace_target_binary;
    use crate::env::{env_lock, EnvVarGuard};
    use std::path::PathBuf;

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
