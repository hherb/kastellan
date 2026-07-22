# Linux Bind Symlink Canonicalization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the Linux bwrap + Firecracker sandbox backends the same symlink-resolution guarantee the macOS Seatbelt backend already has, from one shared primitive — closing the symlink half of audit finding #7 ([#387](https://github.com/hherb/kastellan/issues/387)).

**Architecture:** Lift `canonicalize_one` into a shared `lib.rs` primitive. bwrap canonicalizes each host-source path and binds `canonical-src → original-dest`. Firecracker canonicalizes the `fs_read` root before its `non_anchor_top_level` anchor check, so a symlinked root that escapes the anchor set is rejected. The lexical `..`/absolute guard (`validate_linux_bind_path`) is unchanged and still runs first.

**Tech Stack:** Rust (`kastellan-sandbox`, a pure-Rust crate — no `ring`, so Mac cross-clippy works). `std::fs::canonicalize`, `std::os::unix::fs::symlink`. No new dependencies (house style is std-only for FS ops).

## Global Constraints

- **Posture: resolve, not reject** — canonicalize and proceed with the resolved path (macOS-Seatbelt parity). A Firecracker anchor escape is rejected by the *existing* allowlist on the resolved top-level.
- **Only host-source paths are canonicalized.** Guest-side paths (`persistent_store.guest_mount`; Firecracker `fs_write` mountpoints) are NOT — resolving a guest path against the host FS is meaningless. See the field table in the spec §3.4.
- **Fail closed on canonicalize errors** — a `PermissionDenied` (or any non-`NotFound`) error propagates as `SandboxError::Backend`; never bind/stage an unresolved path. `NotFound` on a not-yet-created leaf falls back to canonicalizing the parent, then to the lexical path.
- **Host split (this is the `cfg-linux-e2e-deadcode-dgx-clippy` trap):** `lib.rs` compiles on macOS + Linux; `linux_bwrap` and `linux_firecracker` are `#[cfg(target_os = "linux")]` (DGX-authoritative); `macos_seatbelt` is `#[cfg(target_os = "macos")]`. Task 1's tests run on both hosts. Tasks 2–3's tests run only on the DGX; use Mac cross-clippy (`cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu`) as a pre-DGX compile/lint check.
- **DGX access:** run native Linux checks as exactly `ssh dgx '<cmd>'` (the allow-rule is a prefix match; flags before the hostname get denied). Write run logs to `~` not `/tmp` (`/tmp` is scrubbed mid-run).
- **Negative-case discipline (the #479 house rule):** prove each new security assertion **fails** against the un-hardened path before restoring the fix.
- **Starting DGX baseline:** `cargo test --workspace` = **2629 / 0 / 50** at `4c03929f` (main). Record the new-test delta at the end.

---

### Task 1: Shared `canonicalize_one` primitive in `lib.rs`

Lift the symlink-resolving primitive out of the macOS backend into the crate root so all three backends share one copy.

**Files:**
- Modify: `sandbox/src/lib.rs` (add `canonicalize_one` after `validate_linux_bind_path`, ~line 249; add a `#[cfg(test)]` module)
- Modify: `sandbox/src/macos_seatbelt.rs` (delete the local `canonicalize_one` at ~lines 261-286; import the shared one)

**Interfaces:**
- Produces: `pub(crate) fn canonicalize_one(p: &std::path::Path) -> Result<std::path::PathBuf, SandboxError>` — resolves symlinks; `NotFound` on the leaf → canonicalize parent + reattach file name; parent also `NotFound` → original path; any other error propagates.

- [ ] **Step 1: Write the failing tests** — append to `sandbox/src/lib.rs`:

```rust
#[cfg(test)]
mod canonicalize_one_tests {
    use super::canonicalize_one;
    use std::path::PathBuf;

    // Unique per-test scratch dir under the OS temp dir (std-only; no tempfile dep).
    // Cleaned up best-effort at the end of each test.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kastellan-canon-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolves_a_symlink_to_its_target() {
        let dir = scratch("resolve");
        let target = dir.join("real");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Resolution equivalence, NOT a literal path: on macOS a temp dir resolves
        // through /private, so compare against an independent canonicalize of the
        // target rather than the lexical target path.
        let got = canonicalize_one(&link).unwrap();
        assert_eq!(got, std::fs::canonicalize(&target).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notfound_leaf_resolves_symlinks_in_the_parent() {
        let dir = scratch("notfound");
        let real_parent = dir.join("real_parent");
        std::fs::create_dir_all(&real_parent).unwrap();
        let link_parent = dir.join("link_parent");
        std::os::unix::fs::symlink(&real_parent, &link_parent).unwrap();

        // The leaf socket does not exist yet — the parent symlink must still resolve.
        let not_yet = link_parent.join("worker.sock");
        let got = canonicalize_one(&not_yet).unwrap();
        assert_eq!(got, std::fs::canonicalize(&real_parent).unwrap().join("worker.sock"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fully_absent_path_falls_back_to_the_original() {
        // Neither the leaf nor its parent exists → nothing to resolve, return as-is.
        let p = std::env::temp_dir().join(format!("kastellan-absent-{}/nope/leaf", std::process::id()));
        assert_eq!(canonicalize_one(&p).unwrap(), p);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-sandbox canonicalize_one -- --nocapture`
Expected: FAIL to compile — `cannot find function canonicalize_one in this scope`.

- [ ] **Step 3: Add the shared primitive** — insert into `sandbox/src/lib.rs` immediately after `validate_linux_bind_path`'s closing brace (~line 249):

```rust
/// Canonicalize a single host path, resolving symlinks (and `..`). For a path
/// whose final component does not exist yet (a not-yet-created socket or scratch
/// file), the parent directory is canonicalized and the file name reattached —
/// so symlinks *above* the leaf are still resolved before the leaf exists. If the
/// parent is also absent, the original path is returned unchanged (nothing to
/// resolve). Any other `io::Error` (e.g. `PermissionDenied` on a parent)
/// propagates, so a caller never binds/stages an unresolved path.
///
/// This is the symlink half of audit finding #7 (issue #387): the lexical guard
/// [`validate_linux_bind_path`] rejects `..`; this resolves symlinks so a path
/// that *names* one location cannot bind/stage what it *resolves* to. Shared by
/// the macOS Seatbelt backend and both Linux backends, so the guarantee comes
/// from one place on every platform.
pub(crate) fn canonicalize_one(
    p: &std::path::Path,
) -> Result<std::path::PathBuf, SandboxError> {
    match std::fs::canonicalize(p) {
        Ok(resolved) => Ok(resolved),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match p.parent().zip(p.file_name()) {
                Some((parent, name)) => match std::fs::canonicalize(parent) {
                    Ok(canon_parent) => Ok(canon_parent.join(name)),
                    Err(pe) if pe.kind() == std::io::ErrorKind::NotFound => Ok(p.to_path_buf()),
                    Err(pe) => Err(SandboxError::Backend(format!(
                        "could not canonicalize policy path {p:?}: {pe}"
                    ))),
                },
                None => Ok(p.to_path_buf()),
            }
        }
        Err(e) => Err(SandboxError::Backend(format!(
            "could not canonicalize policy path {p:?}: {e}"
        ))),
    }
}
```

- [ ] **Step 4: Point the macOS backend at the shared primitive** — in `sandbox/src/macos_seatbelt.rs`, delete the local `fn canonicalize_one` (~lines 261-286) and add near the top imports:

```rust
use crate::canonicalize_one;
```

(The five call sites inside `canonicalize_policy_paths` keep the bare name `canonicalize_one(...)` — now resolved via the import. Its own doc comment stays.)

- [ ] **Step 5: Run the tests to verify they pass + macOS backend still compiles**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-sandbox canonicalize_one -- --nocapture`
Expected: PASS (3 tests). No `canonicalize_one` redefinition/unused-import warnings.

- [ ] **Step 6: Run the full sandbox suite on the Mac (macOS blast-radius check)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-sandbox && cargo clippy -p kastellan-sandbox --all-targets -- -D warnings`
Expected: all pass, clippy clean (this exercises the macOS Seatbelt `canonicalize_policy_paths` now calling the shared fn).

- [ ] **Step 7: Commit**

```bash
git add sandbox/src/lib.rs sandbox/src/macos_seatbelt.rs
git commit -m "sandbox: lift canonicalize_one into a shared lib.rs primitive (#387)

Single source of the symlink-resolution guarantee for all three backends.
macOS Seatbelt now calls crate::canonicalize_one; the Linux backends adopt
it in the following tasks.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: bwrap — bind `canonical-src → original-dest`

Resolve each host-source path's symlinks and bind the resolved literal as the source while keeping the worker's original in-jail path as the destination.

**Files:**
- Modify: `sandbox/src/linux_bwrap.rs` — `push_bind` (line 268), `build_argv` (line 188), `spawn_under_policy` (line 164 call site), test module.

**Interfaces:**
- Consumes: `crate::canonicalize_one` (Task 1).
- Produces: `build_argv(policy, program, args, resolve: &dyn Fn(&std::path::Path) -> std::path::PathBuf) -> Vec<String>`; `push_bind(argv, flag, src: &Path, dest: &Path)`.

- [ ] **Step 1: Write the failing tests** — in `sandbox/src/linux_bwrap.rs`'s `#[cfg(test)]` module, add a test-local identity helper and two tests:

```rust
    // Identity resolver: for tests that assert the pre-#387 argv shape (canonical
    // == original), so binds stay `flag path path`.
    fn identity(p: &std::path::Path) -> std::path::PathBuf {
        p.to_path_buf()
    }

    #[test]
    fn symlinked_fs_read_binds_canonical_src_to_original_dest() {
        let mut p = strict_policy();
        p.fs_read = vec![PathBuf::from("/opt/link")];
        // Fake resolver standing in for a symlink /opt/link -> /etc.
        let resolve = |path: &std::path::Path| {
            if path == std::path::Path::new("/opt/link") {
                PathBuf::from("/etc")
            } else {
                path.to_path_buf()
            }
        };
        let argv = build_argv(&p, "/bin/true", &[], &resolve);
        assert!(
            argv.join(" ").contains("--ro-bind-try /etc /opt/link"),
            "symlinked fs_read must bind canonical src to original dest; got: {argv:?}"
        );
    }

    #[test]
    fn non_symlink_fs_read_is_byte_identical_under_identity() {
        let mut p = strict_policy();
        p.fs_read = vec![PathBuf::from("/etc/ssl")];
        let argv = build_argv(&p, "/bin/true", &[], &identity);
        assert!(argv.join(" ").contains("--ro-bind-try /etc/ssl /etc/ssl"));
    }
```

Then update **every existing** `build_argv(...)` call in the test module to pass `&identity` as the 4th argument (call sites at ~lines 286, 296, 304, 314, 322, 331, 344, 372 and any others — grep `build_argv(` in the file). Their assertions are unchanged (identity ⇒ `src == dest`).

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets 2>&1 | head -30`
Expected: FAIL to compile — `build_argv` takes 3 args but 4 supplied (the signature change is not in yet). This confirms the tests reference the new signature.

- [ ] **Step 3: Change `push_bind` to take separate src/dest** — replace `push_bind` (lines 268-273):

```rust
fn push_bind(argv: &mut Vec<String>, flag: &str, src: &Path, dest: &Path) {
    argv.push(flag.into());
    argv.push(src.display().to_string());
    argv.push(dest.display().to_string());
}
```

- [ ] **Step 4: Add the resolver param to `build_argv` and apply it to host-source binds** — change the signature (line 188) to:

```rust
pub fn build_argv(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
    resolve: &dyn Fn(&Path) -> PathBuf,
) -> Vec<String> {
```

Update the host-source binds (lines 228-258) to pass `resolve(path)` as src, original as dest:

```rust
    for path in &policy.fs_read {
        push_bind(&mut argv, "--ro-bind-try", &resolve(path), path);
    }
    for path in &policy.fs_write {
        push_bind(&mut argv, "--bind-try", &resolve(path), path);
    }
    if let Some(uds) = &policy.proxy_uds {
        push_bind(&mut argv, "--bind", &resolve(uds), uds);
    }
    if let Some(uds) = &policy.broker_uds {
        push_bind(&mut argv, "--bind", &resolve(uds), uds);
    }

    if let Some(ps) = &policy.persistent_store {
        argv.push("--bind".into());
        argv.push(resolve(&ps.host_backing).display().to_string());
        argv.push(ps.guest_mount.display().to_string());
    }
```

Add `use std::path::PathBuf;` to the imports if not already present (`Path` is already imported).

- [ ] **Step 5: Build the resolved map in `spawn_under_policy` and pass it** — in `spawn_under_policy`, after the persistent-store `create_dir_all` block (after line 162) and before `let bwrap_argv = build_argv(...)` (line 164), insert:

```rust
        // #387 symlink half: resolve each HOST-SOURCE path's symlinks up front —
        // the lexical `..`/absolute guard already ran above. We bind
        // `canonical-src → original-dest`, so the worker still opens the path it
        // was granted while bwrap receives the resolved literal (a symlink can't be
        // swapped between our check and bwrap's bind — TOCTOU-safe — and the argv/
        // audit shows the real target). A canonicalize error (e.g. PermissionDenied
        // on a parent) fails the spawn closed rather than binding an unresolved path.
        // Guest-side paths (persistent_store.guest_mount) are NOT resolved.
        let mut resolved: std::collections::HashMap<PathBuf, PathBuf> = std::collections::HashMap::new();
        for p in policy.fs_read.iter().chain(policy.fs_write.iter()) {
            resolved.insert(p.clone(), crate::canonicalize_one(p)?);
        }
        if let Some(uds) = &policy.proxy_uds {
            resolved.insert(uds.clone(), crate::canonicalize_one(uds)?);
        }
        if let Some(uds) = &policy.broker_uds {
            resolved.insert(uds.clone(), crate::canonicalize_one(uds)?);
        }
        if let Some(ps) = &policy.persistent_store {
            resolved.insert(ps.host_backing.clone(), crate::canonicalize_one(&ps.host_backing)?);
        }
        let resolve = |p: &Path| resolved.get(p).cloned().unwrap_or_else(|| p.to_path_buf());
```

Then change the call (line 164) to:

```rust
        let bwrap_argv = build_argv(policy, program, args, &resolve);
```

- [ ] **Step 6: Pre-DGX compile/lint check on the Mac (cross-clippy)**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean (compiles the `cfg(linux)` bwrap module + its tests without a linker; sandbox is pure Rust).

- [ ] **Step 7: Prove the negative case, then run the tests on the DGX**

First prove the new assertion is load-bearing: temporarily revert Step 4's `fs_read` bind to `push_bind(&mut argv, "--ro-bind-try", path, path)` (src == dest) and confirm `symlinked_fs_read_binds_canonical_src_to_original_dest` FAILS on the DGX; then restore.

Run (after restore): `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox 2>&1 | tail -20'`
Expected: all sandbox tests pass, including the two new bwrap tests.

- [ ] **Step 8: Commit**

```bash
git add sandbox/src/linux_bwrap.rs
git commit -m "sandbox(bwrap): resolve symlinks, bind canonical-src -> original-dest (#387)

Host-source paths (fs_read/fs_write/proxy_uds/broker_uds/host_backing) are
canonicalized in spawn_under_policy and bound as canonical source with the
worker's original in-jail path as destination. TOCTOU-safe (bwrap gets the
resolved literal); byte-identical argv for non-symlink paths. build_argv
takes an injected resolver so it stays pure + deterministically tested.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Firecracker — canonicalize the `fs_read` root before the anchor check

Resolve a symlinked `fs_read` root so a root that escapes the anchor set (e.g. `/opt/link → /etc/shadow`) is rejected by the existing allowlist on its resolved top-level.

**Files:**
- Modify: `sandbox/src/linux_firecracker/plan.rs` — the `fs_read` anchor loop (lines 301-309); test module.

**Interfaces:**
- Consumes: `crate::canonicalize_one` (Task 1), `non_anchor_top_level` (existing).

- [ ] **Step 1: Write the failing test** — in `sandbox/src/linux_firecracker/plan.rs`'s `#[cfg(test)]` module, add (uses the existing `img()` helper):

```rust
    #[test]
    fn symlinked_fs_read_root_escaping_anchor_is_rejected() {
        // Real symlink under /tmp (an anchor) pointing OUT to /etc (not an anchor).
        // Without canonicalization non_anchor_top_level sees "tmp" and accepts it.
        let dir = std::env::temp_dir().join(format!("kastellan-fc387-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("escape");
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        let policy = SandboxPolicy { fs_read: vec![link.clone()], ..Default::default() };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(
            format!("{err}").contains("share anchor") && format!("{err}").contains("/etc"),
            "escaping symlinked fs_read root must be rejected on its resolved top-level: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlinked_fs_read_root_within_anchor_is_accepted() {
        // /tmp/<x>/link -> /tmp/<x>/real: resolves within the /tmp anchor → accepted.
        let dir = std::env::temp_dir().join(format!("kastellan-fc387ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let policy = SandboxPolicy { fs_read: vec![link.clone()], ..Default::default() };
        // Must not error on the anchor check (the resolved top-level is /tmp).
        build_launch_plan(&policy, &img(), "/w", &[]).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run to verify failure (DGX — the module is `cfg(linux)`)**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox symlinked_fs_read_root 2>&1 | tail -20'`
Expected: `symlinked_fs_read_root_escaping_anchor_is_rejected` FAILS (`build_launch_plan` returns `Ok`, so `.unwrap_err()` panics) — the un-canonicalized check accepts `/tmp/escape`.

- [ ] **Step 3: Canonicalize the root in the anchor check** — replace the `fs_read` loop (lines 301-309):

```rust
    for p in &policy.fs_read {
        // #387 symlink half: resolve the root before the anchor check so a
        // symlinked fs_read root can't smuggle an out-of-anchor target past the
        // first-component allowlist (e.g. `/opt/link -> /etc/shadow`). The lexical
        // `..`/absolute guard already ran above; `canonicalize_one` falls back to
        // the lexical path for a not-yet-existing path, so a synthetic path is
        // checked exactly as before. RoShare.sources keeps the ORIGINAL path —
        // copy_tree resolves within-tree links itself; only this check resolves.
        let resolved = crate::canonicalize_one(p)?;
        if let Some(top) = non_anchor_top_level(&resolved) {
            return Err(SandboxError::Backend(format!(
                "fs_read path {p:?} (resolves to {resolved:?}) has top-level /{top}, which is not \
                 a micro-VM share anchor ({ANCHOR_HINT}): the guest cannot mount it on the \
                 read-only rootfs — place the shared dir under one of those anchors"
            )));
        }
    }
```

Update the `build_launch_plan` doc comment (line 217) from "Pure + fallible" to note it now resolves `fs_read` symlinks for the anchor check (touches the FS).

- [ ] **Step 4: Run to verify pass (DGX)**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox 2>&1 | tail -20'`
Expected: all sandbox tests pass, including the two new Firecracker tests, and the existing `fs_read_under_*` tests (synthetic non-existent paths canonicalize to themselves via the fallback).

- [ ] **Step 5: Mac cross-clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add sandbox/src/linux_firecracker/plan.rs
git commit -m "sandbox(firecracker): canonicalize fs_read root before the anchor check (#387)

A symlinked fs_read root whose target escapes the share-anchor set (e.g.
/opt/link -> /etc/shadow) previously passed non_anchor_top_level on its
lexical /opt and leaked the target into the RO image. Resolve the root and
run the existing allowlist on the resolved top-level. RoShare.sources keeps
the original path (copy_tree handles within-tree links).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Full verification + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`.

- [ ] **Step 1: DGX full-workspace acceptance**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && setsid bash -lc "cargo test --workspace > ~/dgx-387.log 2>&1; echo DONE_EXIT=\$? >> ~/dgx-387.log" </dev/null & echo started'`
Then poll: `ssh dgx 'tail -5 ~/dgx-387.log'` until `DONE_EXIT=0` appears.
Expected: `2629 + <new-test-count> passed / 0 failed / 50 ignored`, exit 0, **0 `[SKIP]`**.

- [ ] **Step 2: DGX clippy gate**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'`
Expected: clean, exit 0.

- [ ] **Step 3: Mac Seatbelt suite (final blast-radius confirmation)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-sandbox 2>&1 | tail -5`
Expected: all pass (macOS `canonicalize_policy_paths` via the shared primitive).

- [ ] **Step 4: Update HANDOVER.md + ROADMAP.md** — move #387 to "recently completed" in the header with the test-count delta; tick #387 in ROADMAP with the merge commit (filled at PR time); note the remaining audit siblings #388/#389. Commit with `docs(handover): ...`.

---

## Self-Review

**Spec coverage:** §3.1 shared primitive → Task 1. §3.2 lexical guard unchanged → no task needed (explicitly untouched). §3.3 bwrap canonical-src→original-dest → Task 2. §3.4 Firecracker anchor re-check + field scope → Task 3 (fs_read only; fs_write/guest_mount untouched, matching the field table). §4 testing (primitive on both hosts, backends DGX-authoritative, negative-case) → Tasks 1/2/3 steps + Task 4. §5 verification → Task 4. §7 TOCTOU/limits → documented in code comments (Task 2 Step 5, Task 3 Step 3).

**Placeholder scan:** none — every code step shows complete code; every run step shows the command + expected output.

**Type consistency:** `canonicalize_one(&Path) -> Result<PathBuf, SandboxError>` used identically in Tasks 1/2/3. `build_argv`'s new 4th param `resolve: &dyn Fn(&Path) -> PathBuf` matches the `&identity` / `&resolve` closures passed in Task 2. `push_bind(argv, flag, src, dest)` matches all its updated call sites.
