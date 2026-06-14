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

/// Transitive BFS over the dynamic-dependency graph seeded from `roots`
/// (the interpreter binary and its `libpython`). For each dependency: skip + do
/// not follow system libs; bind the **canonical parent dir** of any dep outside
/// `prefix`; follow every unvisited non-system dep — **including in-prefix libs**
/// (an out-of-prefix dep may be reachable only through one). Returns the dirs
/// sorted + deduped. Pure: `resolve_deps` and `canonicalize` are injected.
pub fn out_of_prefix_lib_dirs(
    roots: &[PathBuf],
    prefix: &Path,
    resolve_deps: &dyn Fn(&Path) -> Vec<PathBuf>,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    let canon = |p: &Path| canonicalize(p).unwrap_or_else(|| p.to_path_buf());
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<PathBuf> = Vec::new();

    // Seed the roots for scanning. They are in-prefix (interpreter / libpython),
    // so they are bound elsewhere — we enqueue them only to read their deps.
    for r in roots {
        let c = canon(r);
        if visited.insert(c.clone()) {
            queue.push(c);
        }
    }

    while let Some(obj) = queue.pop() {
        for dep in resolve_deps(&obj) {
            let c = canon(&dep);
            if is_system_lib_path(&c) {
                continue; // prune: do not bind, do not follow
            }
            if !c.starts_with(prefix) {
                if let Some(parent) = c.parent() {
                    dirs.insert(parent.to_path_buf());
                }
            }
            if visited.insert(c.clone()) {
                queue.push(c); // follow transitively (in-prefix libs too)
            }
        }
    }
    dirs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    /// Build a fake `resolve_deps` from an explicit adjacency map.
    fn graph(edges: &[(&str, &[&str])]) -> impl Fn(&Path) -> Vec<PathBuf> {
        let map: HashMap<PathBuf, Vec<PathBuf>> = edges
            .iter()
            .map(|(k, vs)| {
                (
                    PathBuf::from(k),
                    vs.iter().map(PathBuf::from).collect::<Vec<_>>(),
                )
            })
            .collect();
        move |p: &Path| map.get(p).cloned().unwrap_or_default()
    }

    /// Identity canonicalize (the fake paths are already "real").
    fn ident(p: &Path) -> Option<PathBuf> {
        Some(p.to_path_buf())
    }

    #[test]
    fn binds_out_of_prefix_dep_dir() {
        let g = graph(&[(
            "/px/bin/python3.12",
            &["/px/lib/libpython3.12.dylib", "/opt/hb/gettext/lib/libintl.8.dylib"],
        )]);
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert_eq!(dirs, vec![PathBuf::from("/opt/hb/gettext/lib")]);
    }

    #[test]
    fn follows_in_prefix_lib_to_reach_out_of_prefix_dep() {
        let g = graph(&[
            ("/px/bin/python3.12", &["/px/lib/libpython3.12.dylib"]),
            ("/px/lib/libpython3.12.dylib", &["/opt/hb/gettext/lib/libintl.8.dylib"]),
        ]);
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert_eq!(dirs, vec![PathBuf::from("/opt/hb/gettext/lib")]);
    }

    #[test]
    fn skips_and_does_not_follow_system_libs() {
        let g = graph(&[
            ("/px/bin/python3.12", &["/usr/lib/libSystem.B.dylib"]),
            ("/usr/lib/libSystem.B.dylib", &["/opt/hb/should-not-appear/lib/x.dylib"]),
        ]);
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert!(dirs.is_empty(), "system libs must be pruned, got {dirs:?}");
    }

    #[test]
    fn transitive_cross_dir_chain_binds_all_dirs() {
        let g = graph(&[
            ("/px/bin/python3.12", &["/opt/hb/a/lib/libA.dylib"]),
            ("/opt/hb/a/lib/libA.dylib", &["/opt/hb/b/lib/libB.dylib"]),
        ]);
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert_eq!(
            dirs,
            vec![PathBuf::from("/opt/hb/a/lib"), PathBuf::from("/opt/hb/b/lib")]
        );
    }

    #[test]
    fn cycle_is_safe() {
        let g = graph(&[
            ("/px/bin/python3.12", &["/opt/hb/a/lib/libA.dylib"]),
            ("/opt/hb/a/lib/libA.dylib", &["/opt/hb/b/lib/libB.dylib"]),
            ("/opt/hb/b/lib/libB.dylib", &["/opt/hb/a/lib/libA.dylib"]),
        ]);
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert_eq!(
            dirs,
            vec![PathBuf::from("/opt/hb/a/lib"), PathBuf::from("/opt/hb/b/lib")]
        );
    }

    #[test]
    fn canonicalizes_symlinked_dep_before_binding() {
        let g = graph(&[(
            "/px/bin/python3.12",
            &["/opt/hb/opt/gettext/lib/libintl.8.dylib"],
        )]);
        let canon = |p: &Path| -> Option<PathBuf> {
            if p == Path::new("/opt/hb/opt/gettext/lib/libintl.8.dylib") {
                Some(PathBuf::from("/opt/hb/Cellar/gettext/1.0/lib/libintl.8.dylib"))
            } else {
                Some(p.to_path_buf())
            }
        };
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &canon,
        );
        assert_eq!(dirs, vec![PathBuf::from("/opt/hb/Cellar/gettext/1.0/lib")]);
    }

    #[test]
    fn multi_root_seed_dedupes() {
        let g = graph(&[
            ("/px/bin/python3.12", &["/opt/hb/gettext/lib/libintl.8.dylib"]),
            ("/px/lib/libpython3.12.dylib", &["/opt/hb/gettext/lib/libintl.8.dylib"]),
        ]);
        let dirs = out_of_prefix_lib_dirs(
            &[
                PathBuf::from("/px/bin/python3.12"),
                PathBuf::from("/px/lib/libpython3.12.dylib"),
            ],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert_eq!(dirs, vec![PathBuf::from("/opt/hb/gettext/lib")]);
    }

    #[test]
    fn empty_graph_yields_no_dirs() {
        let g = graph(&[]);
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert!(dirs.is_empty());
    }
}
