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
//! closures, so the transitive graph walk is unit-testable without `otool`/`ldd`. The
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
pub(crate) fn is_system_lib_path(p: &Path) -> bool {
    #[cfg(target_os = "macos")]
    let roots: &[&str] = &["/usr/lib", "/System/Library"];
    #[cfg(not(target_os = "macos"))]
    let roots: &[&str] = &["/lib", "/lib64", "/usr/lib"];
    is_system_lib_path_in(p, roots)
}

/// Transitive depth-first walk over the dynamic-dependency graph seeded from `roots`
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
    let mut stack: Vec<PathBuf> = Vec::new();

    // Seed the walk; roots are starting points for dep-scanning only — we never
    // emit a bound dir for a root itself, only for the deps reachable from it.
    for r in roots {
        let c = canon(r);
        if visited.insert(c.clone()) {
            stack.push(c);
        }
    }

    while let Some(obj) = stack.pop() {
        for dep in resolve_deps(&obj) {
            let c = canon(&dep);
            if is_system_lib_path(&c) {
                continue; // prune: do not bind, do not follow
            }
            if !c.starts_with(prefix) {
                if let Some(parent) = c.parent() {
                    // Never bind the filesystem root: a (pathological) dep
                    // directly under `/` would otherwise grant a read of `/`.
                    // Real otool/ldd output never yields this, but the guard is
                    // free defense-in-depth.
                    if parent != Path::new("/") {
                        dirs.insert(parent.to_path_buf());
                    }
                }
            }
            if visited.insert(c.clone()) {
                stack.push(c); // follow transitively (in-prefix libs too)
            }
        }
    }
    dirs.into_iter().collect()
}

/// Compute the out-of-prefix shared-lib dirs to bind for a venv's interpreter.
///
/// Locates `<venv>/bin/{python3,python}`, canonicalizes it to the real
/// interpreter, then seeds [`out_of_prefix_lib_dirs`] with that binary AND its
/// `libpython` (at `<real-prefix>/lib/libpython<X.Y>.{dylib,so}`, version derived
/// from the binary stem like `python3.12` → `3.12`; seeded only when present — a
/// miss is harmless because the walk reaches `libpython` through the binary's own
/// deps anyway). `interpreter_root` is the external interpreter prefix (when the
/// venv's python lives outside the venv), else `None`; the dep-walk prefix is that
/// root or, self-contained, the venv dir (a self-contained interpreter can still
/// link out-of-prefix libs). Returns empty when the interpreter can't be located.
///
/// Shared by the browser-driver manifest and its e2e resolver so the seed logic
/// cannot drift across the crate boundary (review M2). Pure: `exists`,
/// `canonicalize`, and `resolve_deps` are injected.
pub fn interpreter_lib_dirs(
    venv_dir: &Path,
    interpreter_root: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
    resolve_deps: &dyn Fn(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    let bin = venv_dir.join("bin");
    let candidate = match ["python3", "python"]
        .iter()
        .map(|n| bin.join(n))
        .find(|p| exists(p))
    {
        Some(c) => c,
        None => return Vec::new(),
    };
    let real = match canonicalize(&candidate) {
        Some(r) => r,
        None => return Vec::new(),
    };
    // The prefix to treat as "already bound in-jail": the external interpreter
    // root, or (self-contained) the venv dir.
    let prefix = interpreter_root.unwrap_or(venv_dir);

    let mut roots = vec![real.clone()];
    // Seed libpython explicitly (belt-and-braces). Derive `<X.Y>` from e.g.
    // "python3.12" → "3.12"; try `.dylib` then `.so` under `<real-prefix>/lib`.
    if let (Some(stem), Some(real_prefix)) = (
        real.file_name().and_then(|n| n.to_str()),
        real.parent().and_then(|b| b.parent()),
    ) {
        let ver = stem.trim_start_matches("python");
        if !ver.is_empty() {
            for ext in ["dylib", "so"] {
                let lib = real_prefix.join("lib").join(format!("libpython{ver}.{ext}"));
                if exists(&lib) {
                    roots.push(lib);
                }
            }
        }
    }
    out_of_prefix_lib_dirs(&roots, prefix, resolve_deps, canonicalize)
}

/// Parse `otool -L` output into resolved dependency paths. The first line is the
/// object's own header (`<path>:`); dependency lines are tab-indented as
/// `\t<abs-path> (compatibility version …)`. We take the path before ` (`.
#[cfg_attr(all(not(test), not(target_os = "macos")), allow(dead_code))]
fn parse_otool_output(out: &str) -> Vec<PathBuf> {
    out.lines()
        .filter_map(|line| line.strip_prefix('\t'))
        .filter_map(|rest| rest.rsplit_once(" (").map(|(path, _)| path))
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

/// Parse `ldd` output into resolved dependency paths. Dependency lines look like
/// `\t<soname> => /resolved/path (0x…)`; we take the path after `=> ` and before
/// ` (`. Lines without a `=>` (vdso, the loader) and `=> not found` are skipped.
#[cfg_attr(all(not(test), target_os = "macos"), allow(dead_code))]
fn parse_ldd_output(out: &str) -> Vec<PathBuf> {
    out.lines()
        .filter_map(|line| line.split_once(" => "))
        .map(|(_, rhs)| rhs.trim())
        // Fallback `Some(rhs)`: a bare resolved path with no ` (load-address)`
        // suffix (non-standard ldd output) — keep the whole right-hand side.
        .filter_map(|rhs| rhs.rsplit_once(" (").map(|(path, _)| path).or(Some(rhs)))
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

/// Run the platform's linker-introspection tool on `obj` and return its resolved
/// dependency paths. macOS: `otool -L`; Linux: `ldd`. Fail-safe: a missing tool,
/// a non-zero exit, or unparseable output yields an empty vec (the caller then
/// binds nothing extra and the manual `*_EXTRA_FS_READ` hatch remains the
/// backstop). Never panics. This is the only impure item in the module.
pub fn resolve_deps_via_tool(obj: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let (tool, args): (&str, &[&str]) = ("otool", &["-L"]);
    #[cfg(not(target_os = "macos"))]
    let (tool, args): (&str, &[&str]) = ("ldd", &[]);

    let output = match std::process::Command::new(tool)
        .args(args)
        .arg(obj)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    #[cfg(target_os = "macos")]
    {
        parse_otool_output(&text)
    }
    #[cfg(not(target_os = "macos"))]
    {
        parse_ldd_output(&text)
    }
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

    #[test]
    fn empty_roots_yields_no_dirs() {
        let g = graph(&[(
            "/px/bin/python3.12",
            &["/opt/hb/gettext/lib/libintl.8.dylib"],
        )]);
        let dirs = out_of_prefix_lib_dirs(&[], Path::new("/px"), &g, &ident);
        assert!(dirs.is_empty(), "empty roots ⇒ no dirs, got {dirs:?}");
    }

    #[test]
    fn never_binds_filesystem_root() {
        // A (pathological) dep directly under `/` must NOT grant a read of `/`.
        let g = graph(&[("/px/bin/python3.12", &["/libfoo.so"])]);
        let dirs = out_of_prefix_lib_dirs(
            &[PathBuf::from("/px/bin/python3.12")],
            Path::new("/px"),
            &g,
            &ident,
        );
        assert!(dirs.is_empty(), "root `/` must never be bound, got {dirs:?}");
    }

    #[test]
    fn parses_otool_output() {
        let out = "\
/px/bin/python3.12:
\t/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation (compatibility version 150.0.0, current version 4201.0.0)
\t/px/lib/libpython3.12.dylib (compatibility version 3.12.0, current version 3.12.0)
\t/opt/hb/opt/gettext/lib/libintl.8.dylib (compatibility version 13.0.0, current version 13.5.0)
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1356.0.0)
";
        let deps = parse_otool_output(out);
        assert_eq!(
            deps,
            vec![
                PathBuf::from("/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation"),
                PathBuf::from("/px/lib/libpython3.12.dylib"),
                PathBuf::from("/opt/hb/opt/gettext/lib/libintl.8.dylib"),
                PathBuf::from("/usr/lib/libSystem.B.dylib"),
            ]
        );
    }

    #[test]
    fn parses_ldd_output() {
        let out = "\
\tlinux-vdso.so.1 (0x0000ffff)
\tlibpython3.12.so.1.0 => /px/lib/libpython3.12.so.1.0 (0x0000ffff)
\tlibc.so.6 => /usr/lib/aarch64-linux-gnu/libc.so.6 (0x0000ffff)
\tlibmissing.so => not found
\t/lib/ld-linux-aarch64.so.1 (0x0000ffff)
";
        let deps = parse_ldd_output(out);
        assert_eq!(
            deps,
            vec![
                PathBuf::from("/px/lib/libpython3.12.so.1.0"),
                PathBuf::from("/usr/lib/aarch64-linux-gnu/libc.so.6"),
            ]
        );
    }

    /// Live check (operator-run): on a host whose interpreter links out-of-prefix
    /// libs, the real tool returns a non-empty, absolute dep set. Skipped in CI.
    #[test]
    #[ignore = "runs the real otool/ldd against the current interpreter"]
    fn real_tool_returns_absolute_deps() {
        let me = std::env::current_exe().expect("current_exe");
        let deps = resolve_deps_via_tool(&me);
        assert!(!deps.is_empty(), "expected some linked deps for {me:?}");
        assert!(deps.iter().all(|p| p.is_absolute()), "deps: {deps:?}");
    }
}
