# Interpreter Library Dependency Auto-Bind Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Auto-bind the read-only directories of an external interpreter's out-of-prefix shared-library dependencies into the worker sandbox, so a pyenv/Homebrew-linked interpreter no longer SIGABRTs at dyld load (resolves #284).

**Architecture:** A new pure module `core/src/workers/interpreter_deps.rs` does a transitive BFS over the dynamic-dependency graph (deps + canonicalization injected as closures) and returns the canonical parent dirs of out-of-prefix, non-system libs. The only impurity is `resolve_deps_via_tool`, which shells out to `otool -L` (macOS) / `ldd` (Linux) and parses the output via pure parsers. `browser-driver`'s manifest wires it into `fs_read`.

**Tech Stack:** Rust (workspace crate `kastellan-core`), `std::process::Command`, `std::collections::BTreeSet`. No new dependencies.

---

## File Structure

- **Create** `core/src/workers/interpreter_deps.rs` — pure BFS + classification + tool parsers + the impure tool shim. One responsibility: "what extra dirs must be bound for an interpreter to dyld-load in a jail."
- **Modify** `core/src/workers/mod.rs` — add `pub mod interpreter_deps;`.
- **Modify** `core/src/workers/browser_driver.rs` — `BrowserDriverEnv.interpreter_lib_dirs`, `resolve_env` gains a `resolve_deps` closure, `browser_driver_entry` binds the dirs, manifest injects the real shim, fix all call sites.
- **Modify** `core/tests/browser_driver_e2e.rs` — `resolve_browser_env` computes `interpreter_lib_dirs` so the real render works without a manual `EXTRA_FS_READ`.

Build/test commands (always source cargo first):
```sh
source "$HOME/.cargo/env"
```

---

### Task 1: Module skeleton + `is_system_lib_path`

**Files:**
- Create: `core/src/workers/interpreter_deps.rs`
- Modify: `core/src/workers/mod.rs` (add `pub mod interpreter_deps;` in alphabetical position, after `pub mod gliner_relex;`)

- [ ] **Step 1: Register the module**

In `core/src/workers/mod.rs`, add after the `pub mod gliner_relex;` line:
```rust
pub mod interpreter_deps;
```

- [ ] **Step 2: Write the failing test for system-path classification**

Create `core/src/workers/interpreter_deps.rs` with only the doc header, a `is_system_lib_path_in` stub, and tests:
```rust
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
}
```

- [ ] **Step 3: Run the test to verify it passes (compiles + logic)**

Run: `cargo test -p kastellan-core interpreter_deps::tests::classifies_system_roots -- --nocapture`
Expected: PASS (this step verifies the pure helper; the public wrapper comes next).

- [ ] **Step 4: Add the per-OS public wrapper**

Append to `interpreter_deps.rs` (above the `#[cfg(test)]` module):
```rust
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
```

- [ ] **Step 5: Add a current-OS test for the public wrapper**

Add inside `mod tests`:
```rust
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
```

- [ ] **Step 6: Run tests + clippy**

Run: `cargo test -p kastellan-core interpreter_deps:: -- --nocapture`
Expected: PASS (2 tests)
Run: `cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: clean

- [ ] **Step 7: Commit**

```bash
git add core/src/workers/mod.rs core/src/workers/interpreter_deps.rs
git commit -m "feat(#284): interpreter_deps module + is_system_lib_path classifier"
```

---

### Task 2: `out_of_prefix_lib_dirs` transitive BFS

**Files:**
- Modify: `core/src/workers/interpreter_deps.rs`

- [ ] **Step 1: Write the failing tests for the BFS**

Add inside `mod tests` (uses a fake dep graph + identity canonicalize):
```rust
    use std::collections::HashMap;

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
        // interpreter -> [in-prefix libpython, out-of-prefix libintl]
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
        // interpreter -> libpython (in-prefix); libpython -> libintl (out-of-prefix).
        // The out-of-prefix dep is reachable ONLY through the in-prefix lib.
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
        // A system lib that (hypothetically) links an out-of-prefix lib must NOT
        // be followed — system libs are pruned entirely.
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
        // libA(homebrew dirA) -> libB(homebrew dirB): both dirs bound.
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
        // dep is the opt/ symlink path; canonicalize maps it to the Cellar real path.
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
        // interpreter + libpython both seeded; both link the same out-of-prefix lib.
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
```

- [ ] **Step 2: Run the tests to verify they fail (function not defined)**

Run: `cargo test -p kastellan-core interpreter_deps::tests::binds_out_of_prefix -- --nocapture`
Expected: compile error — `out_of_prefix_lib_dirs` not found.

- [ ] **Step 3: Implement the BFS**

Add to `interpreter_deps.rs` (above `#[cfg(test)]`):
```rust
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
```

- [ ] **Step 4: Run all BFS tests**

Run: `cargo test -p kastellan-core interpreter_deps:: -- --nocapture`
Expected: PASS (all tests, ~10)

- [ ] **Step 5: clippy + commit**

Run: `cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: clean
```bash
git add core/src/workers/interpreter_deps.rs
git commit -m "feat(#284): transitive BFS out_of_prefix_lib_dirs"
```

---

### Task 3: Pure `otool` / `ldd` output parsers

**Files:**
- Modify: `core/src/workers/interpreter_deps.rs`

- [ ] **Step 1: Write the failing parser tests**

Add inside `mod tests`:
```rust
    #[test]
    fn parses_otool_output() {
        // First line is the object header (ends ':'); deps are tab-indented.
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-core interpreter_deps::tests::parses_ -- --nocapture`
Expected: compile error — parsers not found.

- [ ] **Step 3: Implement the parsers**

Add to `interpreter_deps.rs` (above `#[cfg(test)]`):
```rust
/// Parse `otool -L` output into resolved dependency paths. The first line is the
/// object's own header (`<path>:`); dependency lines are tab-indented as
/// `\t<abs-path> (compatibility version …)`. We take the path before ` (`.
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
fn parse_ldd_output(out: &str) -> Vec<PathBuf> {
    out.lines()
        .filter_map(|line| line.split_once(" => "))
        .map(|(_, rhs)| rhs.trim())
        .filter_map(|rhs| rhs.rsplit_once(" (").map(|(path, _)| path).or(Some(rhs)))
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}
```

- [ ] **Step 4: Run parser tests**

Run: `cargo test -p kastellan-core interpreter_deps::tests::parses_ -- --nocapture`
Expected: PASS (2)

- [ ] **Step 5: clippy + commit**

Run: `cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: clean
```bash
git add core/src/workers/interpreter_deps.rs
git commit -m "feat(#284): otool/ldd dependency parsers"
```

---

### Task 4: Impure `resolve_deps_via_tool` shim + real-host check

**Files:**
- Modify: `core/src/workers/interpreter_deps.rs`

- [ ] **Step 1: Implement the impure shim**

Add to `interpreter_deps.rs` (above `#[cfg(test)]`):
```rust
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
```

- [ ] **Step 2: Write a `#[ignore]` real-host check**

Add inside `mod tests`:
```rust
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
```

- [ ] **Step 3: Run unit tests (the ignored one is skipped) + the live one explicitly**

Run: `cargo test -p kastellan-core interpreter_deps:: -- --nocapture`
Expected: PASS, 1 ignored.
Run: `cargo test -p kastellan-core interpreter_deps::tests::real_tool_returns_absolute_deps -- --ignored --nocapture`
Expected: PASS (the kastellan test binary has linked deps).

- [ ] **Step 4: clippy + commit**

Run: `cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: clean
```bash
git add core/src/workers/interpreter_deps.rs
git commit -m "feat(#284): resolve_deps_via_tool otool/ldd shim + live check"
```

---

### Task 5: Wire into `browser-driver`

**Files:**
- Modify: `core/src/workers/browser_driver.rs`

- [ ] **Step 1: Add the field + a failing entry test**

In `core/src/workers/browser_driver.rs`, add to `BrowserDriverEnv` (after `interpreter_root`):
```rust
    /// Read-only directories of the interpreter's out-of-prefix shared-library
    /// dependencies (e.g. a Homebrew `libintl` dir a pyenv CPython links). Bound
    /// so the interpreter can dyld-load inside the jail — without them it
    /// SIGABRTs before the worker runs (issue #284). Empty when the interpreter
    /// is self-contained or the dep tool is unavailable.
    pub interpreter_lib_dirs: Vec<PathBuf>,
```

Add this test inside `mod tests` (it will fail to compile until the field is wired):
```rust
    #[test]
    fn interpreter_lib_dirs_are_bound_in_fs_read() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
            interpreter_root: Some(PathBuf::from("/px")),
            interpreter_lib_dirs: vec![PathBuf::from("/opt/hb/gettext/lib")],
            extra_fs_read: vec![],
        };
        let entry = browser_driver_entry(&env, &[]);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/opt/hb/gettext/lib")));
    }
```

- [ ] **Step 2: Bind the dirs in `browser_driver_entry`**

In `browser_driver_entry`, after the `fs_read.extend(env.extra_fs_read...)` block (around line 203), add:
```rust
    // Bind the interpreter's out-of-prefix shared-lib dirs (issue #284) so a
    // pyenv/Homebrew-linked interpreter can dyld-load in the jail.
    fs_read.extend(env.interpreter_lib_dirs.iter().cloned());
```

- [ ] **Step 3: Add `resolve_deps` to `resolve_env` and compute the dirs**

Change the `resolve_env` signature to add a `resolve_deps` closure (a new generic param `R`):
```rust
pub fn resolve_env<E, D, X, C, R>(
    env_lookup: E,
    _is_dir: D,
    exists: X,
    canonicalize: C,
    resolve_deps: R,
) -> Result<BrowserDriverEnv, ResolveSkipReason>
where
    E: Fn(&str) -> Option<String>,
    D: Fn(&Path) -> bool,
    X: Fn(&Path) -> bool,
    C: Fn(&Path) -> Option<PathBuf>,
    R: Fn(&Path) -> Vec<PathBuf>,
```

Replace the `interpreter_root` computation block (currently `let interpreter_root = resolve_interpreter_root(&venv_dir, &exists, &canonicalize);`) with:
```rust
    let interpreter_root = resolve_interpreter_root(&venv_dir, &exists, &canonicalize);
    let interpreter_lib_dirs = resolve_interpreter_lib_dirs(
        &venv_dir,
        interpreter_root.as_deref(),
        &exists,
        &canonicalize,
        &resolve_deps,
    );
```

And add `interpreter_lib_dirs` to the returned `BrowserDriverEnv { … }` literal.

- [ ] **Step 4: Add the `resolve_interpreter_lib_dirs` helper**

Add after `resolve_interpreter_root` in `browser_driver.rs`:
```rust
/// Compute the out-of-prefix shared-lib dirs to bind for the venv interpreter.
///
/// Seeds the BFS with the real interpreter binary AND its `libpython` (located at
/// `<prefix>/lib/libpython<X.Y>.{dylib,so}`, version derived from the real
/// interpreter filename; seeded only when present — a miss is harmless because
/// the BFS reaches libpython through the binary's deps anyway). `prefix` is the
/// interpreter prefix when external, else the venv dir (a self-contained
/// interpreter can still link out-of-prefix libs). Returns empty when the
/// interpreter can't be located.
fn resolve_interpreter_lib_dirs(
    venv_dir: &Path,
    interpreter_root: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
    resolve_deps: &dyn Fn(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    let bin = venv_dir.join("bin");
    let candidate = match ["python3", "python"].iter().map(|n| bin.join(n)).find(|p| exists(p)) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let real = match canonicalize(&candidate) {
        Some(r) => r,
        None => return Vec::new(),
    };
    // The prefix to treat as "in-jail already bound": the external interpreter
    // root, or (self-contained) the venv dir.
    let prefix = interpreter_root.unwrap_or(venv_dir);

    let mut roots = vec![real.clone()];
    // Seed libpython explicitly (belt-and-braces). Derive <X.Y> from e.g.
    // "python3.12" -> "3.12"; try .dylib then .so under <real-prefix>/lib.
    if let (Some(stem), Some(real_prefix)) =
        (real.file_name().and_then(|n| n.to_str()), real.parent().and_then(|b| b.parent()))
    {
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
    crate::workers::interpreter_deps::out_of_prefix_lib_dirs(
        &roots, prefix, resolve_deps, canonicalize,
    )
}
```

- [ ] **Step 5: Fix the manifest `resolve()` call site**

In `BrowserDriverManifest::resolve`, change the `resolve_env(...)` call to pass the real shim as the 5th arg:
```rust
        match resolve_env(
            |k| (ctx.get_env)(k),
            |p| (ctx.is_dir)(p),
            |p| (ctx.exists)(p),
            |p| (ctx.canonicalize)(p),
            |p| crate::workers::interpreter_deps::resolve_deps_via_tool(p),
        ) {
```

- [ ] **Step 6: Fix all unit-test call sites in `browser_driver.rs`**

Every `resolve_env(env, is_dir, exists, no_canon)` and `resolve_env(env, is_dir, exists, canon)` (8 sites) gains a 5th arg. Add a `no_deps` helper near `no_canon`:
```rust
    /// No interpreter deps in most tests.
    fn no_deps(_p: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
```
Then update each call to pass `no_deps` as the final argument, e.g.:
```rust
            resolve_env(env, is_dir, exists, no_canon, no_deps),
```
And the direct-construction unit test at line ~469 (`entry_has_browser_client_policy_and_operator_allowlist`) and any other `BrowserDriverEnv { … }` literal in tests must add `interpreter_lib_dirs: vec![],`.

- [ ] **Step 7: Add a `resolve_env` test that exercises dep binding**

Add inside `mod tests`:
```rust
    #[test]
    fn resolve_env_binds_out_of_prefix_interpreter_deps() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        // External pyenv interpreter at /px linking a Homebrew libintl.
        let canon = |p: &Path| match p.to_str() {
            Some("/v/bin/python3") => Some(PathBuf::from("/px/bin/python3.12")),
            _ => Some(p.to_path_buf()),
        };
        let deps = |p: &Path| {
            if p == Path::new("/px/bin/python3.12") {
                vec![PathBuf::from("/opt/hb/gettext/lib/libintl.8.dylib")]
            } else {
                vec![]
            }
        };
        let out = resolve_env(env, |_p| true, |_p| true, canon, deps).expect("resolves");
        assert_eq!(
            out.interpreter_lib_dirs,
            vec![PathBuf::from("/opt/hb/gettext/lib")]
        );
        let entry = browser_driver_entry(&out, &[]);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/opt/hb/gettext/lib")));
    }
```

- [ ] **Step 8: Run the browser_driver tests + clippy**

Run: `cargo test -p kastellan-core workers::browser_driver:: -- --nocapture`
Expected: PASS (all existing + 2 new)
Run: `cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: clean

- [ ] **Step 9: Commit**

```bash
git add core/src/workers/browser_driver.rs
git commit -m "feat(#284): browser-driver binds out-of-prefix interpreter lib deps"
```

---

### Task 6: Make the real e2e render without a manual hatch

**Files:**
- Modify: `core/tests/browser_driver_e2e.rs` (`resolve_browser_env`, lines ~38-83)

- [ ] **Step 1: Compute `interpreter_lib_dirs` in the test resolver**

In `resolve_browser_env`, after the `extra_fs_read` block and before `Some(BrowserDriverEnv { … })`, add:
```rust
    // Mirror the manifest: bind the interpreter's out-of-prefix shared-lib dirs
    // (issue #284) so a pyenv/Homebrew-linked interpreter dyld-loads in the jail.
    let interpreter_lib_dirs = {
        let real = ["python3", "python"]
            .iter()
            .map(|n| venv_dir.join("bin").join(n))
            .find(|p| p.exists())
            .and_then(|p| std::fs::canonicalize(&p).ok());
        match real {
            Some(real) => {
                let prefix = interpreter_root
                    .clone()
                    .unwrap_or_else(|| venv_dir.clone());
                let mut roots = vec![real.clone()];
                if let (Some(stem), Some(real_prefix)) = (
                    real.file_name().and_then(|n| n.to_str()),
                    real.parent().and_then(|b| b.parent()),
                ) {
                    let ver = stem.trim_start_matches("python");
                    if !ver.is_empty() {
                        for ext in ["dylib", "so"] {
                            let lib = real_prefix
                                .join("lib")
                                .join(format!("libpython{ver}.{ext}"));
                            if lib.exists() {
                                roots.push(lib);
                            }
                        }
                    }
                }
                kastellan_core::workers::interpreter_deps::out_of_prefix_lib_dirs(
                    &roots,
                    &prefix,
                    &|p| kastellan_core::workers::interpreter_deps::resolve_deps_via_tool(p),
                    &|p| std::fs::canonicalize(p).ok(),
                )
            }
            None => Vec::new(),
        }
    };
```

Then add `interpreter_lib_dirs,` to the `BrowserDriverEnv { … }` literal.

- [ ] **Step 2: Verify it compiles**

Run: `cargo test -p kastellan-core --test browser_driver_e2e --no-run`
Expected: builds.

- [ ] **Step 3: Operator render check (this Mac)**

Stage the worker (`scripts/workers/browser-driver/install.sh` already run) and run the real render WITHOUT a manual `EXTRA_FS_READ`:
```bash
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test browser_driver_e2e real_render_of_loopback_page -- --ignored --nocapture
```
Expected: PASS (renders, `status:200`, `js-ran`) — proving #284 is fixed end to end on a host that previously SIGABRTed. If PG bring-up flakes, re-run; the render itself is the signal.

- [ ] **Step 4: Commit**

```bash
git add core/tests/browser_driver_e2e.rs
git commit -m "test(#284): e2e resolver binds interpreter lib deps (renders w/o manual hatch)"
```

---

### Task 7: Whole-workspace verification

- [ ] **Step 1: Full core test run (Mac skip-as-pass posture)**

Run: `cargo test -p kastellan-core -- --nocapture 2>&1 | tail -25`
Expected: all PASS (skip-as-pass for PG-gated suites without `KASTELLAN_PG_BIN_DIR`).

- [ ] **Step 2: Workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Cross-clippy the Linux path is unaffected (pure-Rust nature)**

`core` can't cross-compile on the Mac (ring C dep). Note in the PR that the Linux branch (`ldd` + `is_system_lib_path` Linux roots) is CI-verified; the helper returns empty for system-python deps so the DGX 1790/0 baseline is unchanged. (Optional: verify on the DGX via `ssh dgx 'cd ~/src/kastellan && cargo test -p kastellan-core interpreter_deps::'`.)

---

## Notes for session wrap (not code tasks)

- Comment on #284 with the real root cause (pyenv→Homebrew `libintl`, not Chromium) and close it via the PR.
- Update `HANDOVER.md` + `ROADMAP.md`: #284 resolved; it was a misdiagnosis, not a Chromium-148 Seatbelt regression. Note the reusable `interpreter_deps` helper + the python-exec/gliner-relex adoption follow-up.
