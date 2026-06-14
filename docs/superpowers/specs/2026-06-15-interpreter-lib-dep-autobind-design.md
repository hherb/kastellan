# Auto-bind out-of-prefix interpreter library dependencies (resolves #284)

**Date:** 2026-06-15
**Status:** design approved
**Scope:** `core` (worker manifests / sandbox policy construction)
**Resolves:** [#284](https://github.com/hherb/kastellan/issues/284)

## Problem

A worker that binds an **external** language interpreter into its sandbox grants
the interpreter's *prefix* (via `resolve_interpreter_root` →
`BrowserDriverEnv::interpreter_root`) but **not** the load-bearing shared
libraries that interpreter links from **outside** that prefix.

On the dev Mac the `browser-driver` venv's `python3` is a **pyenv** CPython
(`~/.pyenv/versions/3.12.3/bin/python3.12`) that links a **Homebrew** dylib:

```
/opt/homebrew/opt/gettext/lib/libintl.8.dylib
```

Under the worker's Seatbelt profile (which grants the interpreter prefix and the
venv, but not `/opt/homebrew`), dyld's `open()` of `libintl` is blocked, so
**python SIGABRTs at dyld load — before Chromium ever launches**:

```
dyld: Library not loaded: /opt/homebrew/opt/gettext/lib/libintl.8.dylib
  Referenced from: ~/.pyenv/versions/3.12.3/bin/python3.12
  Reason: '...libintl.8.dylib' (file system sandbox blocked open())  → Abort trap: 6 (exit 134)
```

This is the SIGABRT reported in #284. The issue's hypothesis ("Chromium 148
needs more Seatbelt grants") was a **misdiagnosis**: the empty stderr is dyld
aborting before the worker (and Chromium) run. Confirmed by reproduction: with
`/opt/homebrew` added to the read set, the **identical** worker profile + env
renders fine on chromium-1223 / macOS 26.5.1 (`OK title='spike'`, exit 0), under
both an inherited and a fully cleared (`env -i`) environment.

The existing escape hatch `KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ='["/opt/homebrew"]'`
resolves it, but it is a **silent footgun**: the operator must know to set it,
and the failure mode (a swallowed SIGABRT surfacing as `Protocol(EarlyExit)`)
points at the wrong layer. The same gap latently affects any worker binding an
external interpreter (`python-exec`, `gliner-relex`) on such a host.

## Goal

Complete what `resolve_interpreter_root` already does for the prefix: detect the
interpreter's load-bearing shared-library dependencies that live outside the
prefix, and bind their containing directories read-only into the sandbox —
automatically, cross-platform, fail-safe.

## Non-goals

- Wiring `python-exec` / `gliner-relex` (no reproduced bug there; build the
  helper reusable, adopt later — tracked as a follow-up).
- Changing any network / write / seccomp posture. Reads only.
- Replacing the manual `EXTRA_FS_READ` hatch (kept as a backstop).

## Design

### 1. New pure module `core/src/workers/interpreter_deps.rs`

Three items; only the last touches the OS.

- **`out_of_prefix_lib_dirs(roots, prefix, resolve_deps, canonicalize) -> Vec<PathBuf>`**
  (pure). BFS over the dynamic-dependency graph seeded from `roots` — the
  canonicalized interpreter binary **and** its `libpython` shared library
  (seeded explicitly; see §2). For each dependency:
  - resolve its **canonical** path;
  - if it is a **system lib**, skip it entirely (don't bind, don't follow — system
    libs only link other system libs);
  - if it is **outside `prefix`**, collect its **canonical parent directory**;
  - if it is **unvisited**, enqueue it to follow transitively — **including
    in-prefix libs** (e.g. `libpython`), because an out-of-prefix dependency may
    be pulled in *only* through an in-prefix library.

  A visited-set guards cycles and bounds work. Returns the parent dirs
  **sorted + deduped**. All I/O arrives via the injected
  `resolve_deps: Fn(&Path) -> Vec<PathBuf>` and `canonicalize: Fn(&Path) -> Option<PathBuf>`
  closures, so the BFS is unit-testable with a fake dependency graph.

  Grants are the **canonical** parent dir on purpose: Homebrew uses
  `opt/<formula> → Cellar/<formula>/<ver>` symlinks, and Seatbelt/bwrap match the
  **resolved real path** — binding the symlink path would not cover the file dyld
  actually opens.

- **`is_system_lib_path(p) -> bool`** (pure). True for roots the base sandbox
  profile already grants, so we never emit a redundant bind:
  - macOS: under `/usr/lib` or `/System/Library`
  - Linux: under `/lib`, `/lib64`, or `/usr/lib`

- **`resolve_deps_via_tool(obj) -> Vec<PathBuf>`** (impure; the only I/O). Runs
  the platform linker-introspection tool and parses resolved dependency paths:
  - macOS: `otool -L <obj>` → the `\t<path> (compatibility …)` lines (skip the
    first line, which is the object itself).
  - Linux: `ldd <obj>` → the `name => /resolved/path (0x…)` lines (take the path
    after `=>`; skip `linux-vdso`, `ld-linux`, and `not found` lines).
  - **Fail-safe:** tool missing, non-zero exit, or unparseable output ⇒ empty
    vec. The worker then behaves exactly as today (no extra binds); the manual
    `EXTRA_FS_READ` hatch remains the backstop. Never panics, never blocks
    resolution.

### 2. Wire into `browser-driver` (`core/src/workers/browser_driver.rs`)

- `BrowserDriverEnv` gains `interpreter_lib_dirs: Vec<PathBuf>`.
- `resolve_env` gains a `resolve_deps` closure parameter. After canonicalizing
  the venv `python3` to the real interpreter, it builds the BFS seed roots:
  - the real interpreter binary;
  - its **`libpython`**, located at `<prefix>/lib/libpython<X.Y>.{dylib,so}`
    where `<X.Y>` is derived from the real interpreter's filename
    (`python3.12` → `libpython3.12`); seeded only when the `exists` probe finds
    it. If the guess misses, the BFS still reaches `libpython` by following the
    interpreter binary's deps — so the explicit seed is belt-and-braces.

  Then `interpreter_lib_dirs = out_of_prefix_lib_dirs(&roots, prefix, resolve_deps, canonicalize)`.
  Computed whenever the real interpreter is found — independent of whether
  `interpreter_root` is `Some` (a self-contained venv interpreter can still link
  out-of-prefix libs).
- `browser_driver_entry` pushes `env.interpreter_lib_dirs` into `fs_read`
  (deduped against existing entries).
- `BrowserDriverManifest::resolve` injects the real `resolve_deps_via_tool` shim.

### 3. Cross-platform behavior

The fix is genuinely cross-platform: out-of-prefix interpreter deps need binding
under bwrap (`--ro-bind`) + Landlock-RO (`KASTELLAN_LANDLOCK_RO`, derived from
`fs_read`) just as under Seatbelt. On the DGX the system python's deps live in
`/usr/lib` (a system lib root) ⇒ the helper returns **empty** ⇒ **no behavior
change**, the 1790/0 Linux baseline stays byte-identical.

### 4. Error handling

- Resolution never fails on dep-scan problems: a missing/erroring tool yields no
  extra binds (degraded but functional, with the manual hatch available).
- The `resolve_deps` closure is the sole impurity; the BFS and classification are
  pure and total.

## Testing

- **Pure BFS unit tests** (fake `resolve_deps` graph, no `otool`/`ldd`):
  out-of-prefix detection; in-prefix dir **not bound** but **followed** —
  an in-prefix `libpython` that pulls an out-of-prefix dep ⇒ that dep's dir is
  bound; system-lib exclusion (skipped, not followed); a transitive
  cross-directory chain (`A(homebrew) → B(homebrew, other dir)` ⇒ both dirs
  bound); cycle safety; canonicalization-of-symlinked-parent; multi-root seed
  (interpreter + libpython) dedupes; empty graph ⇒ empty result.
- **`is_system_lib_path` table tests** per OS.
- **`resolve_env` test** with a fake dep-resolver: the computed lib dirs land in
  `fs_read` via `browser_driver_entry`; empty resolver ⇒ unchanged `fs_read`.
- **`#[ignore]` real-host check**: `resolve_deps_via_tool` over the real
  interpreter returns a non-empty, canonical, existing set on a host whose
  interpreter links out-of-prefix libs (documents the live behavior; skipped in
  CI).

## Verification

- `cargo test -p kastellan-core` green (Mac, skip-as-pass).
- `cargo clippy --workspace --all-targets -D warnings` clean.
- The reproduced repro (`/tmp/bd-repro`) confirms the mechanism; the real e2e
  `browser_driver_e2e::real_render_of_loopback_page` should render under Seatbelt
  on this Mac **without** a manual `EXTRA_FS_READ` once wired (operator-run,
  `#[ignore]`).
- DGX `cargo test --workspace` unchanged at 1790/0 (helper is a no-op there).

## Follow-ups

- Adopt the shared helper in `python-exec` and `gliner-relex` manifests
  (same external-interpreter footgun).
- Correct #284 + HANDOVER/ROADMAP: it is not a Chromium-148 Seatbelt regression.
