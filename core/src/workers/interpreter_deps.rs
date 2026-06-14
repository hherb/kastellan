//! Resolve the extra read-only directories an external interpreter needs to
//! dyld-load inside a sandbox.
//!
//! A worker that binds an external interpreter (e.g. a pyenv CPython) grants the
//! interpreter *prefix* but not shared libraries the interpreter links from
//! *outside* that prefix (e.g. a Homebrew `libintl`). Under Seatbelt/bwrap those
//! `open()`s are blocked and the interpreter SIGABRTs at dyld load — before the
//! worker runs (issue #284). This module finds those out-of-prefix libs and
//! returns the canonical parent dirs to bind read-only.
//!
//! Pure core: the dependency graph and path canonicalization arrive as injected
//! closures, so the transitive BFS is unit-testable without `otool`/`ldd`. The
//! only impurity is [`resolve_deps_via_tool`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// True when `p` lies under one of `roots` (path-prefix match).
fn is_system_lib_path_in(p: &Path, roots: &[&str]) -> bool {
    roots.iter().any(|r| p.starts_with(r))
}

/// True when the canonical path is a system library root already granted by the
/// base sandbox profile (so we never emit a redundant `fs_read` bind). The roots
/// mirror `macos_seatbelt::build_profile` (macOS) and bwrap's base binds (Linux).
pub fn is_system_lib_path(p: &Path) -> bool {
    #[cfg(target_os = "macos")]
    let roots: &[&str] = &["/usr/lib", "/System/Library"];
    #[cfg(not(target_os = "macos"))]
    let roots: &[&str] = &["/lib", "/lib64", "/usr/lib"];
    is_system_lib_path_in(p, roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_system_roots_by_prefix() {
        let roots = ["/usr/lib", "/System/Library"];
        assert!(is_system_lib_path_in(Path::new("/usr/lib/libSystem.B.dylib"), &roots));
        assert!(is_system_lib_path_in(
            Path::new("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation"),
            &roots
        ));
        assert!(!is_system_lib_path_in(
            Path::new("/opt/homebrew/Cellar/gettext/1.0/lib/libintl.8.dylib"),
            &roots
        ));
        // A path that merely *contains* a root segment but isn't under it.
        assert!(!is_system_lib_path_in(Path::new("/home/usr/lib/x"), &roots));
    }

    #[test]
    fn public_classifier_excludes_homebrew_includes_system() {
        // Homebrew is never a system root on any platform.
        assert!(!is_system_lib_path(Path::new(
            "/opt/homebrew/Cellar/gettext/1.0/lib/libintl.8.dylib"
        )));
        #[cfg(target_os = "macos")]
        assert!(is_system_lib_path(Path::new("/usr/lib/libSystem.B.dylib")));
        #[cfg(not(target_os = "macos"))]
        assert!(is_system_lib_path(Path::new("/usr/lib/x86_64-linux-gnu/libc.so.6")));
    }
}
