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

/// Resolve the real interpreter prefix a venv's `python3` symlinks to.
///
/// Locates `<venv>/bin/{python3,python}`, canonicalizes it to the real CPython,
/// and returns its **prefix** (`<bin>/..`) — the tree holding the interpreter
/// binary + `libpython` + the stdlib. Returns `None` when the interpreter can't
/// be found/canonicalized, or when it already lives **under** `venv_dir`
/// (self-contained — the venv `fs_read` already covers it, nothing extra to
/// bind). Pure: `exists` and `canonicalize` are injected.
///
/// Shared by every venv-backed worker (browser-driver, gliner-relex) so the
/// "where's the real interpreter" rule lives in exactly one place.
pub fn resolve_interpreter_root(
    venv_dir: &Path,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let bin = venv_dir.join("bin");
    let candidate = ["python3", "python"]
        .iter()
        .map(|n| bin.join(n))
        .find(|p| exists(p))?;
    let real = canonicalize(&candidate)?;
    let prefix = real.parent()?.parent()?; // <prefix>/bin/python → <prefix>
    // Self-contained: the real interpreter is already under venv_dir, so the
    // venv fs_read covers it — nothing extra to bind.
    if prefix.starts_with(venv_dir) {
        return None;
    }
    Some(prefix.to_path_buf())
}

/// Compute the out-of-prefix shared-lib dirs to bind for a venv's interpreter.
///
/// Locates `<venv>/bin/{python3,python}`, canonicalizes it to the real
/// interpreter, then delegates to [`interpreter_lib_dirs_for_binary`].
/// `interpreter_root` is the external interpreter prefix (when the venv's python
/// lives outside the venv), else `None`; the dep-walk prefix is that root or,
/// self-contained, the venv dir (a self-contained interpreter can still link
/// out-of-prefix libs). Returns empty when the interpreter can't be located.
///
/// Shared by the browser-driver / gliner-relex manifests and their e2e resolvers
/// so the seed logic cannot drift across the crate boundary (review M2). Pure:
/// `exists`, `canonicalize`, and `resolve_deps` are injected.
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
    interpreter_lib_dirs_for_binary(&real, prefix, exists, canonicalize, resolve_deps)
}

/// Compute the out-of-prefix shared-lib dirs for an **already-resolved**
/// interpreter binary (no venv lookup). `real_python` must be the canonical
/// interpreter path; `prefix` is the directory tree treated as already bound
/// in-jail (deps under it are skipped, deps outside it are bound).
///
/// Seeds [`out_of_prefix_lib_dirs`] with the binary AND its `libpython` (at
/// `<real-prefix>/lib/libpython<X.Y>.{dylib,so}`, version derived from the binary
/// stem like `python3.12` → `3.12`; seeded only when present — a miss is harmless
/// because the walk reaches `libpython` through the binary's own deps anyway).
///
/// Shared by [`interpreter_lib_dirs`] (venv-backed workers) and the bare-binary
/// `python-exec` worker so the libpython-seed logic lives in one place. Pure:
/// `exists`, `canonicalize`, and `resolve_deps` are injected.
pub fn interpreter_lib_dirs_for_binary(
    real_python: &Path,
    prefix: &Path,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
    resolve_deps: &dyn Fn(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut roots = vec![real_python.to_path_buf()];
    // Seed libpython explicitly (belt-and-braces). Derive `<X.Y>` from e.g.
    // "python3.12" → "3.12"; try `.dylib` then `.so` under `<real-prefix>/lib`.
    if let (Some(stem), Some(real_prefix)) = (
        real_python.file_name().and_then(|n| n.to_str()),
        real_python.parent().and_then(|b| b.parent()),
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
mod tests;
