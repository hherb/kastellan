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

// ---------------------------------------------------------------------------
// resolve_interpreter_root — venv → external interpreter prefix
// ---------------------------------------------------------------------------

#[test]
fn interpreter_root_none_for_self_contained_venv() {
    // python3 canonicalizes to a path *under* venv_dir ⇒ nothing extra to bind.
    let exists = |p: &Path| p == Path::new("/v/bin/python3");
    let canon = |p: &Path| {
        (p == Path::new("/v/bin/python3")).then(|| PathBuf::from("/v/bin/python3.12"))
    };
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon),
        None
    );
}

#[test]
fn interpreter_root_resolved_for_external_venv() {
    // Pyenv-style: venv python3 symlinks to an interpreter outside the venv.
    let exists = |p: &Path| p == Path::new("/v/bin/python3");
    let canon = |p: &Path| {
        (p == Path::new("/v/bin/python3"))
            .then(|| PathBuf::from("/home/u/.pyenv/versions/3.12.3/bin/python3.12"))
    };
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon),
        Some(PathBuf::from("/home/u/.pyenv/versions/3.12.3"))
    );
}

#[test]
fn interpreter_root_none_when_no_python_in_venv() {
    let exists = |_p: &Path| false;
    let canon = |_p: &Path| None;
    assert_eq!(
        resolve_interpreter_root(Path::new("/v"), &exists, &canon),
        None
    );
}

// ---------------------------------------------------------------------------
// interpreter_lib_dirs_for_binary — shared seed for an already-resolved binary
// ---------------------------------------------------------------------------

#[test]
fn lib_dirs_for_binary_seeds_binary_and_finds_out_of_prefix() {
    // The interpreter binary links a Homebrew libintl outside its prefix.
    let exists = |_p: &Path| false; // no libpython on disk; reached via the binary's deps
    let g = graph(&[(
        "/px/bin/python3.12",
        &["/opt/hb/gettext/lib/libintl.8.dylib"],
    )]);
    let dirs = interpreter_lib_dirs_for_binary(
        Path::new("/px/bin/python3.12"),
        Path::new("/px"),
        &exists,
        &ident,
        &g,
    );
    assert_eq!(dirs, vec![PathBuf::from("/opt/hb/gettext/lib")]);
}

#[test]
fn lib_dirs_for_binary_seeds_libpython_when_present() {
    // The binary lists no deps, but libpython (seeded explicitly) does — so the
    // out-of-prefix dir is still found through the libpython seed.
    let exists = |p: &Path| p == Path::new("/px/lib/libpython3.12.dylib");
    let g = graph(&[(
        "/px/lib/libpython3.12.dylib",
        &["/opt/hb/gettext/lib/libintl.8.dylib"],
    )]);
    let dirs = interpreter_lib_dirs_for_binary(
        Path::new("/px/bin/python3.12"),
        Path::new("/px"),
        &exists,
        &ident,
        &g,
    );
    assert_eq!(dirs, vec![PathBuf::from("/opt/hb/gettext/lib")]);
}

#[test]
fn lib_dirs_for_binary_empty_when_all_deps_in_prefix() {
    let exists = |_p: &Path| false;
    let g = graph(&[(
        "/px/bin/python3.12",
        &["/px/lib/libpython3.12.dylib"],
    )]);
    let dirs = interpreter_lib_dirs_for_binary(
        Path::new("/px/bin/python3.12"),
        Path::new("/px"),
        &exists,
        &ident,
        &g,
    );
    assert!(dirs.is_empty(), "all deps in-prefix ⇒ no extra dirs, got {dirs:?}");
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
