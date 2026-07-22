# localmail Mail Worker + Workspace `out/` Activation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the kastellan agent read-only access to the localmail archive — search, message/attachment retrieval as text *and* original-format files — via a sandboxed Rust worker that calls localmail's `/v1` REST API, plus activation of the dormant per-task `Workspace` so binary attachments are delivered as durable files.

**Architecture:** Two phases. **Phase A** activates the per-task `Workspace` (`core/src/workspace.rs`): construct it in the lane runner, thread its `out/` dir to opt-in workers at dispatch (fs_write + `KASTELLAN_WORKER_OUT` env, Landlock in lock-step), and harvest `out/` to a durable artifacts dir at task finalize before the RAII wipe. **Phase B** builds `kastellan-worker-mail`: a Rust worker (prelude stdio loop + `Handler`) that reuses `web-common`'s force-routing-aware HTTP transport (extended for bearer auth), exposes six read-only `mail.*` tools, and delivers attachments into the task `out/`. A precedes B.

**Tech Stack:** Rust (workspace 0.2.0, edition 2021, rustc 1.78 floor), `kastellan-protocol` (JSON-RPC `Handler`), `kastellan-worker-prelude` (`serve_stdio` + sandbox lockdown), `kastellan-worker-web-common` (`reqwest::blocking` + `ProxyConnectGet`), `serde`/`serde_json`, `url`. localmail is Python (external service reached over HTTP).

## Global Constraints

- **AGPL-3.0-only**; AGPL-compatible deps only. No new dep that isn't already in `[workspace.dependencies]` without justification (this plan adds none).
- **Cross-platform: Linux + macOS first-class.** Every change compiles + tests on both. `#[cfg(target_os = "linux")]` code is DGX-authoritative (Mac compiles it out); dual-platform files must be checked on both hosts. Verify Linux via `ssh dgx '<cmd>'` (native aarch64, real bwrap + KVM + live PG).
- **Rust core, Python only inside sandboxed workers.** No in-process Python. The mail worker is Rust; localmail stays a separate HTTP service.
- **Every worker is sandboxed before it runs.** No unsandboxed escape hatch.
- **Secrets never as plaintext in `policy.env`.** Use the file-path pattern (`..._FILE=<path>` env + `0600` file in `fs_read`).
- **Read-only.** Only localmail GET endpoints + `POST /v1/search` (query body, no mutation). No send/delete/modify.
- **Cargo needs env sourced** in non-interactive shells: `source "$HOME/.cargo/env"` before any `cargo` command.
- **Spec:** `docs/superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md`. localmail tweak tracked as [hherb/localmail#196](https://github.com/hherb/localmail/issues/196).

---

# Phase A — Workspace `out/` activation (core)

### Task A1: A per-task output dir helper + artifacts root

**Files:**
- Modify: `core/src/tool_host/scratch.rs` (add `ENV_WORKER_OUT` + `apply_workspace_out`)
- Modify: `core/src/tool_host.rs:33` (re-export the new symbols)
- Modify: `core/src/workspace.rs` (add `artifacts_root()` + `default_artifacts_root()`)
- Test: inline `#[cfg(test)] mod tests` in each modified file

**Interfaces:**
- Produces: `pub const ENV_WORKER_OUT: &str = "KASTELLAN_WORKER_OUT";`
- Produces: `pub fn apply_workspace_out(policy: &mut SandboxPolicy, out_dir: &Path)` — pushes `out_dir` to `policy.fs_write` and `(ENV_WORKER_OUT, out_dir)` to `policy.env`.
- Produces: `pub const ENV_ARTIFACTS_ROOT: &str = "KASTELLAN_ARTIFACTS_ROOT";` and `pub fn default_artifacts_root() -> Result<PathBuf, WorkspaceError>` (→ `$KASTELLAN_ARTIFACTS_ROOT` or `~/.kastellan/artifacts`).

- [ ] **Step 1: Write the failing test for `apply_workspace_out`**

Add to `core/src/tool_host/scratch.rs` inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn apply_workspace_out_pushes_fs_write_and_env() {
    let mut p = kastellan_sandbox::SandboxPolicy::minimal_for_test();
    let dir = std::path::Path::new("/tmp/ws/out");
    apply_workspace_out(&mut p, dir);
    assert!(p.fs_write.iter().any(|d| d == dir), "out dir must be writable");
    assert!(
        p.env.iter().any(|(k, v)| k == ENV_WORKER_OUT && v == "/tmp/ws/out"),
        "KASTELLAN_WORKER_OUT must carry the out dir"
    );
}
```

> If `SandboxPolicy::minimal_for_test()` does not exist, mirror the existing `apply_scratch` test in this file for how it constructs a `SandboxPolicy`; use that exact construction instead.

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core apply_workspace_out_pushes -- --nocapture`
Expected: FAIL — `cannot find function apply_workspace_out`.

- [ ] **Step 3: Implement `ENV_WORKER_OUT` + `apply_workspace_out`**

In `core/src/tool_host/scratch.rs`, after the `ENV_WORKER_SCRATCH` const + `apply_scratch` fn:

```rust
/// Env var naming the per-task workspace `out/` dir for a worker that opts
/// into durable artifact output (mirrors [`ENV_WORKER_SCRATCH`]).
pub const ENV_WORKER_OUT: &str = "KASTELLAN_WORKER_OUT";

/// Bind a per-task `out/` directory into a worker policy: RW `fs_write` +
/// the [`ENV_WORKER_OUT`] env pointer. The `fs_write` push flows to the
/// worker-side Landlock filter via `derive_lockdown_env`, so host and worker
/// agree. Unlike [`apply_scratch`], this dir is task-scoped and NOT wiped by
/// this module — the runner harvests + wipes it (see `scheduler::runner`).
pub fn apply_workspace_out(policy: &mut SandboxPolicy, out_dir: &Path) {
    policy.fs_write.push(out_dir.to_path_buf());
    policy
        .env
        .push((ENV_WORKER_OUT.to_string(), out_dir.to_string_lossy().into_owned()));
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core apply_workspace_out_pushes -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Re-export from `tool_host.rs`**

Modify `core/src/tool_host.rs:33`:

```rust
pub use scratch::{
    apply_workspace_out, prepare_ephemeral_scratch, EphemeralScratch, ENV_WORKER_OUT,
    ENV_WORKER_SCRATCH,
};
```

- [ ] **Step 6: Write the failing test for the artifacts root**

Add to `core/src/workspace.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn artifacts_root_honours_env_override() {
    let tmp = std::env::temp_dir().join("kastellan-artifacts-test");
    std::env::set_var(ENV_ARTIFACTS_ROOT, &tmp);
    let got = default_artifacts_root().unwrap();
    std::env::remove_var(ENV_ARTIFACTS_ROOT);
    assert_eq!(got, tmp);
}
```

- [ ] **Step 7: Implement the artifacts root**

In `core/src/workspace.rs`, after `ENV_WORKSPACE_ROOT`:

```rust
/// Env var overriding the durable artifacts root (harvested worker outputs).
pub const ENV_ARTIFACTS_ROOT: &str = "KASTELLAN_ARTIFACTS_ROOT";

/// Durable root where per-task `out/` deliverables are harvested to, surviving
/// the ephemeral workspace wipe. `$KASTELLAN_ARTIFACTS_ROOT` or
/// `~/.kastellan/artifacts`. No silent `/tmp` fallback (same posture as
/// [`default_root`]).
pub fn default_artifacts_root() -> Result<PathBuf, WorkspaceError> {
    if let Some(root) = std::env::var_os(ENV_ARTIFACTS_ROOT) {
        return Ok(PathBuf::from(root));
    }
    let home = dirs_home()?;
    Ok(home.join(".kastellan").join("artifacts"))
}
```

> Reuse the same home-dir resolution `default_root()` uses (find its helper — likely `dirs_home()` or inline `std::env::var_os("HOME")`); match it exactly rather than introducing a new dependency.

- [ ] **Step 8: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core workspace:: -- --nocapture` and `cargo test -p kastellan-core artifacts_root_honours_env`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add core/src/tool_host/scratch.rs core/src/tool_host.rs core/src/workspace.rs
git commit -m "feat(core): workspace out/ policy helper + artifacts root"
```

---

### Task A2: Harvest `out/` → durable artifacts dir

**Files:**
- Create: `core/src/scheduler/runner/harvest.rs`
- Modify: `core/src/scheduler/runner.rs` (add `mod harvest;`)
- Test: inline in `harvest.rs`

**Interfaces:**
- Produces: `pub(super) fn harvest_outputs(out_dir: &Path, artifacts_root: &Path, task_id: i64) -> Vec<PathBuf>` — moves every entry under `out_dir` into `<artifacts_root>/<task_id>/`, returns the destination paths. Best-effort: logs and skips a file it cannot move; never panics. Empty `out_dir` → empty Vec.

- [ ] **Step 1: Write the failing test**

Create `core/src/scheduler/runner/harvest.rs`:

```rust
//! Harvest a task's workspace `out/` deliverables into a durable artifacts
//! dir before the ephemeral workspace is wiped.

use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harvest_moves_files_and_returns_dest_paths() {
        let base = std::env::temp_dir().join(format!("kastellan-harvest-{}", std::process::id()));
        let out = base.join("out");
        let artifacts = base.join("artifacts");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("booking.pdf"), b"%PDF-1.7 fake").unwrap();

        let dests = harvest_outputs(&out, &artifacts, 42);

        assert_eq!(dests.len(), 1);
        let moved = artifacts.join("42").join("booking.pdf");
        assert!(moved.exists(), "file harvested to artifacts/<task_id>/");
        assert_eq!(std::fs::read(&moved).unwrap(), b"%PDF-1.7 fake");
        assert!(!out.join("booking.pdf").exists(), "source moved, not copied");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn harvest_empty_out_is_empty() {
        let base = std::env::temp_dir().join(format!("kastellan-harvest-empty-{}", std::process::id()));
        let out = base.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert!(harvest_outputs(&out, &base.join("artifacts"), 1).is_empty());
        std::fs::remove_dir_all(&base).ok();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core harvest_moves_files -- --nocapture`
Expected: FAIL — `cannot find function harvest_outputs`.

- [ ] **Step 3: Implement `harvest_outputs`**

Add to `core/src/scheduler/runner/harvest.rs` (above the test module):

```rust
/// Move every entry under `out_dir` into `<artifacts_root>/<task_id>/`,
/// returning the destination paths. Rename first (same-filesystem, atomic);
/// fall back to copy+remove across filesystems. Best-effort: a file that
/// cannot be moved is logged and skipped, never fatal.
pub(super) fn harvest_outputs(out_dir: &Path, artifacts_root: &Path, task_id: i64) -> Vec<PathBuf> {
    let dest_dir = artifacts_root.join(task_id.to_string());
    let mut harvested = Vec::new();

    let entries = match std::fs::read_dir(out_dir) {
        Ok(e) => e,
        Err(_) => return harvested, // out dir absent/unreadable → nothing to harvest
    };
    let mut created_dest = false;
    for entry in entries.flatten() {
        let src = entry.path();
        let Some(name) = src.file_name() else { continue };
        if !created_dest {
            if let Err(e) = std::fs::create_dir_all(&dest_dir) {
                tracing::warn!(task_id, error = %e, dir = ?dest_dir, "harvest: create artifacts dir failed");
                return harvested;
            }
            created_dest = true;
        }
        let dst = dest_dir.join(name);
        match std::fs::rename(&src, &dst) {
            Ok(()) => harvested.push(dst),
            Err(_) => match copy_then_remove(&src, &dst) {
                Ok(()) => harvested.push(dst),
                Err(e) => tracing::warn!(task_id, error = %e, src = ?src, "harvest: move failed, skipped"),
            },
        }
    }
    harvested
}

fn copy_then_remove(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst)?;
    std::fs::remove_file(src)
}
```

- [ ] **Step 4: Register the module**

In `core/src/scheduler/runner.rs`, near the other `mod` declarations (e.g. next to `mod audit_rows;`):

```rust
mod harvest;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core harvest_ -- --nocapture`
Expected: PASS (both harvest tests).

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/runner/harvest.rs core/src/scheduler/runner.rs
git commit -m "feat(core): harvest workspace out/ to durable artifacts dir"
```

---

### Task A3: Dispatcher carries a per-task `out/` dir

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs` (`ToolHostStepDispatcher`: add map + `set_task_out_dir`; clear in `purge_task`; apply in `dispatch_step`)
- Test: inline in `tool_dispatch.rs`

**Interfaces:**
- Consumes: `apply_workspace_out` + `ENV_WORKER_OUT` (Task A1).
- Produces: `fn wants_workspace_out(tool_name: &str) -> bool` (name-based predicate, `matches!(tool_name, "mail")`).
- Produces on `StepDispatcher` trait: `fn set_task_out_dir(&self, task_id: i64, out_dir: std::path::PathBuf);` (default no-op) so the runner can register a per-task out dir; the existing `purge_task` also clears it.

- [ ] **Step 1: Write the failing test**

Add to `core/src/scheduler/tool_dispatch.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn wants_workspace_out_only_for_mail() {
    assert!(wants_workspace_out("mail"));
    assert!(!wants_workspace_out("web-fetch"));
    assert!(!wants_workspace_out("shell-exec"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core wants_workspace_out_only_for_mail`
Expected: FAIL — `cannot find function wants_workspace_out`.

- [ ] **Step 3: Add the predicate + the dispatcher state**

In `core/src/scheduler/tool_dispatch.rs`:

```rust
/// Tools that opt into a per-task workspace `out/` dir (durable file output).
/// Name-based, mirroring `force_route::disable_mitm_for`.
pub(crate) fn wants_workspace_out(tool_name: &str) -> bool {
    matches!(tool_name, "mail")
}
```

Add a field to `ToolHostStepDispatcher` (a `std::sync::Mutex<std::collections::HashMap<i64, std::path::PathBuf>>` named `task_out_dirs`), initialize it in `ToolHostStepDispatcher::new` to an empty map, and add:

```rust
impl ToolHostStepDispatcher {
    fn out_dir_for(&self, task_id: i64) -> Option<std::path::PathBuf> {
        self.task_out_dirs.lock().unwrap().get(&task_id).cloned()
    }
}
```

- [ ] **Step 4: Run predicate test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core wants_workspace_out_only_for_mail`
Expected: PASS.

- [ ] **Step 5: Thread the registration through the `StepDispatcher` trait**

In `core/src/scheduler/inner_loop.rs` (where `StepDispatcher` is defined), add a defaulted method so existing test impls need no change:

```rust
/// Register a per-task workspace `out/` dir so `dispatch_step` can bind it
/// into opt-in workers. Default: no-op (dispatchers without workspace support).
fn set_task_out_dir(&self, _task_id: i64, _out_dir: std::path::PathBuf) {}
```

Implement it on `ToolHostStepDispatcher`:

```rust
fn set_task_out_dir(&self, task_id: i64, out_dir: std::path::PathBuf) {
    self.task_out_dirs.lock().unwrap().insert(task_id, out_dir);
}
```

In `ToolHostStepDispatcher::purge_task`, add the clear alongside the existing handoff-cache purge:

```rust
self.task_out_dirs.lock().unwrap().remove(&task_id);
```

- [ ] **Step 6: Apply the out dir to the policy clone in `dispatch_step`**

In `ToolHostStepDispatcher::dispatch_step`, after `let Some(entry) = self.registry.lookup(&step.tool) else { … }` and before `self.lifecycle.acquire(&step.tool, entry)`, build a per-call entry when the tool opts in:

```rust
let mut owned_entry;
let entry = if wants_workspace_out(&step.tool) {
    if let Some(out_dir) = self.out_dir_for(task_id) {
        owned_entry = entry.clone();
        crate::tool_host::apply_workspace_out(&mut owned_entry.policy, &out_dir);
        &owned_entry
    } else {
        entry // no workspace registered for this task; get_attachment will error clearly
    }
} else {
    entry
};
```

> `entry` was `&ToolEntry` from `registry.lookup`. The `owned_entry` binding must outlive the `acquire` call; declare it in the same scope (as shown) so the borrow is valid.

- [ ] **Step 7: Run the core test suite to verify no regression**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core tool_dispatch -- --nocapture`
Expected: PASS (existing dispatch tests + the new predicate test). If a non-test `StepDispatcher` impl fails to compile, it inherited the defaulted `set_task_out_dir` — no change needed; only `ToolHostStepDispatcher` overrides it.

- [ ] **Step 8: Commit**

```bash
git add core/src/scheduler/tool_dispatch.rs core/src/scheduler/inner_loop.rs
git commit -m "feat(core): dispatcher binds per-task workspace out/ for opt-in tools"
```

---

### Task A4: Runner constructs the Workspace + harvests at finalize

**Files:**
- Modify: `core/src/scheduler/runner.rs` (construct `Workspace` in `drain_lane`, register out dir, harvest before drop)
- Test: `core/tests/workspace_activation_e2e.rs` (new integration test)

**Interfaces:**
- Consumes: `Workspace::new`, `Workspace::outputs`, `default_artifacts_root` (A1); `harvest_outputs` (A2); `StepDispatcher::set_task_out_dir` (A3).

- [ ] **Step 1: Write the failing integration test**

Create `core/tests/workspace_activation_e2e.rs`:

```rust
//! A task that writes into its workspace `out/` has the file harvested to the
//! durable artifacts dir at finalize, and the ephemeral workspace is wiped.

use std::path::Path;

use kastellan_core::workspace::Workspace;

#[test]
fn out_is_harvested_and_ephemeral_tree_wiped() {
    let root = std::env::temp_dir().join(format!("kastellan-wsact-{}", std::process::id()));
    let artifacts = root.join("artifacts");
    let ws = Workspace::with_root(&root.join("ws"), "77").unwrap();
    std::fs::write(ws.outputs().join("out.txt"), b"deliverable").unwrap();

    // Simulate the runner's finalize harvest (the private fn is exercised via
    // the runner in production; here we assert the observable contract).
    let out_dir = ws.outputs().to_path_buf();
    drop_after_harvest(&out_dir, &artifacts, 77);

    assert!(artifacts.join("77").join("out.txt").exists(), "harvested");
    std::fs::remove_dir_all(&root).ok();
}

// Mirror of the runner's harvest-then-drop ordering, kept in the test so this
// file has no dependency on a private module. The production path lives in
// `scheduler::runner::drain_lane`.
fn drop_after_harvest(out_dir: &Path, artifacts_root: &Path, task_id: i64) {
    let dest = artifacts_root.join(task_id.to_string());
    std::fs::create_dir_all(&dest).unwrap();
    for e in std::fs::read_dir(out_dir).unwrap().flatten() {
        std::fs::rename(e.path(), dest.join(e.file_name())).unwrap();
    }
}
```

> This integration test pins the observable contract (harvest destination shape). The wiring into `drain_lane` is verified by the full daemon e2e later; this task's unit-level guard is A2's `harvest_outputs` test.

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test workspace_activation_e2e`
Expected: FAIL to compile if `Workspace` isn't `pub` at `kastellan_core::workspace::Workspace`; if it compiles and passes trivially, proceed — the real assertion is the wiring in Step 3.

- [ ] **Step 3: Wire the Workspace into `drain_lane`**

In `core/src/scheduler/runner.rs`, in `drain_lane` after a task is claimed (`claimed`) and **before** the `run_one(...)` call (line ~261):

```rust
// Per-task workspace: construct, register its out/ with the dispatcher so
// opt-in workers get a durable write dir. Non-fatal on failure (search/text
// tools still work; only attachment delivery needs it).
let workspace = match crate::workspace::Workspace::new(&claimed.id.to_string()) {
    Ok(ws) => {
        dispatcher.set_task_out_dir(claimed.id, ws.outputs().to_path_buf());
        Some(ws)
    }
    Err(e) => {
        tracing::warn!(task_id = claimed.id, error = %e, "workspace: construct failed; no out/ for this task");
        None
    }
};
```

After `run_one(...)` returns and **before** the workspace would drop (i.e. in the finalize block, after `tasks::finalize` / `write_finalize_row`), harvest then drop:

```rust
if let Some(ws) = workspace {
    match crate::workspace::default_artifacts_root() {
        Ok(root) => {
            let got = harvest::harvest_outputs(ws.outputs(), &root, claimed.id);
            if !got.is_empty() {
                tracing::info!(task_id = claimed.id, count = got.len(), "harvested workspace out/ artifacts");
            }
        }
        Err(e) => tracing::warn!(task_id = claimed.id, error = %e, "artifacts root unresolved; out/ not harvested"),
    }
    drop(ws); // wipes the ephemeral <root>/<task_id> tree (in/out/tmp)
}
```

> Place the `workspace` binding so it lives across `run_one` and the finalize block (function-scope `let`, not inside a narrower block). If `drain_lane` loops over tasks, scope the binding to one iteration.

- [ ] **Step 4: Run it to verify it passes + compiles**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test workspace_activation_e2e` and `cargo build -p kastellan-core`
Expected: PASS + clean build.

- [ ] **Step 5: DGX verification (core touches cfg-linux Landlock derivation)**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib -- --nocapture 2>&1 | tail -30'`
Expected: no new failures vs. the `main` baseline.

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/runner.rs core/tests/workspace_activation_e2e.rs
git commit -m "feat(core): activate per-task Workspace with harvest-at-finalize"
```

---

# Phase B — the mail worker

### Task B0: `web-common` bearer-authenticated request methods

**Files:**
- Modify: `workers/web-common/src/http.rs` (add `get_authed` + `post_authed` to `HttpGet`; impl on `ReqwestGet` + `ProxyConnectGet`)
- Test: inline in `http.rs`

**Interfaces:**
- Produces on `HttpGet`: `fn get_authed(&self, url: &Url, bearer: &str, max_body: usize) -> Result<RawResponse, String>` and `fn post_authed(&self, url: &Url, bearer: &str, content_type: &str, body: &[u8], max_body: usize) -> Result<RawResponse, String>`, both defaulting to `Err("authed request: unsupported by this transport".into())` (like the existing `post` default). `RawResponse { status, location, content_type, body }` is reused.

- [ ] **Step 1: Write the failing test (ReqwestGet honours bearer + cap)**

Add to `workers/web-common/src/http.rs` `#[cfg(test)] mod tests` a test using a local `std::net::TcpListener` echo server that asserts the request carried `authorization: Bearer testtok`:

```rust
#[test]
fn reqwest_get_authed_sends_bearer() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let n = sock.read(&mut buf).unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
        assert!(req.contains("authorization: bearer testtok"), "missing bearer: {req}");
        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").unwrap();
    });
    ensure_crypto_provider();
    let t = ReqwestGet::new("test/0").unwrap();
    let url = Url::parse(&format!("http://{addr}/x")).unwrap();
    let resp = t.get_authed(&url, "testtok", 1024).unwrap();
    assert_eq!(resp.status, 200);
    handle.join().unwrap();
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common reqwest_get_authed_sends_bearer`
Expected: FAIL — no method `get_authed`.

- [ ] **Step 3: Add the trait default methods**

In `workers/web-common/src/http.rs`, in `pub trait HttpGet`, after `post`:

```rust
    /// GET with an `Authorization: Bearer` header and a caller-chosen body cap
    /// (larger than the default for attachment downloads). Default: unsupported.
    fn get_authed(&self, _url: &Url, _bearer: &str, _max_body: usize)
        -> Result<RawResponse, String>
    {
        Err("authed request: unsupported by this transport".to_string())
    }

    /// POST a body with `Authorization: Bearer` + `content_type`. Default: unsupported.
    fn post_authed(&self, _url: &Url, _bearer: &str, _content_type: &str, _body: &[u8], _max_body: usize)
        -> Result<RawResponse, String>
    {
        Err("authed request: unsupported by this transport".to_string())
    }
```

- [ ] **Step 4: Implement on `ReqwestGet`**

In `impl HttpGet for ReqwestGet`, add (mirroring the existing `get`/`post`, adding the header and using `max_body` as the cap):

```rust
    fn get_authed(&self, url: &Url, bearer: &str, max_body: usize) -> Result<RawResponse, String> {
        use std::io::Read;
        let resp = self.client.get(url.clone())
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .send().map_err(|e| e.to_string())?;
        read_capped(resp, max_body)
    }

    fn post_authed(&self, url: &Url, bearer: &str, content_type: &str, body: &[u8], max_body: usize)
        -> Result<RawResponse, String>
    {
        use std::io::Read;
        let resp = self.client.post(url.clone())
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body.to_vec())
            .send().map_err(|e| e.to_string())?;
        read_capped(resp, max_body)
    }
```

Extract the shared body-read into a free fn `read_capped` in the same file (factor it out of the existing `get`/`post` too, so there is one capped-read path):

```rust
fn read_capped(resp: reqwest::blocking::Response, max_body: usize) -> Result<RawResponse, String> {
    use std::io::Read;
    let status = resp.status().as_u16();
    let header = |name: reqwest::header::HeaderName| resp.headers().get(&name)
        .and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let location = header(reqwest::header::LOCATION);
    let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();
    let mut body = Vec::new();
    resp.take((max_body as u64) + 1).read_to_end(&mut body).map_err(|e| e.to_string())?;
    if body.len() > max_body {
        return Err(format!("response body exceeds {max_body} bytes"));
    }
    Ok(RawResponse { status, location, content_type, body })
}
```

- [ ] **Step 5: Implement on `ProxyConnectGet`**

Add `get_authed`/`post_authed` to `impl HttpGet for ProxyConnectGet`, mirroring its existing `get`/`post` hyper request construction but adding the header `authorization: Bearer <bearer>` and using `max_body` as the read cap. Follow the exact request-builder pattern already in that impl (it sets request headers on the hyper `Request` builder); add `.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"))`.

> Read the existing `ProxyConnectGet::get`/`post` bodies first and copy their structure verbatim, changing only: the added Authorization header, the method (POST for `post_authed`), and the cap (`max_body` instead of `MAX_BODY_BYTES`).

- [ ] **Step 6: Run the web-common tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common -- --nocapture`
Expected: PASS (new bearer test + existing tests unregressed).

- [ ] **Step 7: DGX check (ProxyConnectGet is the force-routed path)**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-worker-web-common 2>&1 | tail -15'`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add workers/web-common/src/http.rs
git commit -m "feat(web-common): bearer-authenticated get_authed/post_authed with body cap"
```

---

### Task B1: Scaffold `kastellan-worker-mail`

**Files:**
- Create: `workers/mail/Cargo.toml`
- Create: `workers/mail/src/main.rs`
- Create: `workers/mail/src/handler.rs`
- Delete: `workers/mail/.gitkeep`
- Modify: root `Cargo.toml` (`members` list)

**Interfaces:**
- Produces: binary `kastellan-worker-mail`; `MailHandler` implementing `kastellan_protocol::server::Handler`; unknown method → `METHOD_NOT_FOUND`.

- [ ] **Step 1: Create `workers/mail/Cargo.toml`**

```toml
[package]
name        = "kastellan-worker-mail"
description = "Tool worker: read-only access to a localmail archive over its /v1 REST API (search, messages, attachments)."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../../README.md"

[[bin]]
name = "kastellan-worker-mail"
path = "src/main.rs"

[dependencies]
kastellan-protocol          = { path = "../../protocol", version = "0.2.0" }
kastellan-worker-prelude    = { path = "../prelude", version = "0.2.0" }
kastellan-worker-web-common = { path = "../web-common", version = "0.2.0", features = ["fetch"] }
serde                    = { workspace = true }
serde_json               = { workspace = true }
anyhow                   = { workspace = true }
url                      = { workspace = true }
```

> Confirm `web-common`'s `fetch` feature pulls in `ReqwestGet`/`ProxyConnectGet`/`make_get`. If those are gated behind a different feature name, use that feature; check `workers/web-common/Cargo.toml` `[features]`.

- [ ] **Step 2: Create `workers/mail/src/main.rs`**

```rust
//! mail: read-only access to a localmail archive over its /v1 REST API.
//! Search, message + attachment retrieval; attachments delivered as extracted
//! text or as original-format files written to the task workspace out/ dir.
//! Design: docs/superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md

mod client;
mod handler;

use kastellan_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::MailHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
```

- [ ] **Step 3: Create a minimal `workers/mail/src/handler.rs`**

```rust
use kastellan_protocol::{codes, server::Handler, RpcError};

pub struct MailHandler;

impl MailHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self)
    }
}

impl Handler for MailHandler {
    fn call(&mut self, method: &str, _params: serde_json::Value)
        -> Result<serde_json::Value, RpcError>
    {
        Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {method}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = MailHandler;
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }
}
```

> Add a placeholder `workers/mail/src/client.rs` with `//! localmail REST client (filled in Task B2).` so `mod client;` compiles.

- [ ] **Step 4: Add to the workspace + delete the placeholder**

In root `Cargo.toml` `members`, add `"workers/mail",` (alongside the other `workers/*`). Then:

```bash
git rm workers/mail/.gitkeep
```

- [ ] **Step 5: Build + test**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail`
Expected: PASS (`unknown_method_is_method_not_found`).

- [ ] **Step 6: Commit**

```bash
git add workers/mail/Cargo.toml workers/mail/src/main.rs workers/mail/src/handler.rs workers/mail/src/client.rs Cargo.toml
git commit -m "feat(mail): scaffold kastellan-worker-mail worker crate"
```

---

### Task B2: localmail REST client (`client.rs`)

**Files:**
- Modify: `workers/mail/src/client.rs`
- Test: inline

**Interfaces:**
- Produces: `pub struct MailClient { base: Url, token: String, transport: Box<dyn HttpGet>, attachment_cap: usize }`.
- Produces: `MailClient::from_env() -> anyhow::Result<Self>` — reads `KASTELLAN_MAIL_ENDPOINT` (base URL), `KASTELLAN_MAIL_TOKEN_FILE` (reads token from the 0600 file), optional `KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES` (default 25 MiB), and builds the transport via `web_common::http::make_get`.
- Produces: `fn get_json(&self, path: &str) -> Result<serde_json::Value, MailError>`, `fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, MailError>`, `fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>), MailError>` (returns `(content_type, bytes)`).
- Produces: `pub enum MailError { BadParams(String), Upstream { status: u16, body: String }, Transport(String) }`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTransport;
    impl kastellan_worker_web_common::http::HttpGet for FakeTransport {
        fn get(&self, _u: &url::Url) -> Result<kastellan_worker_web_common::http::RawResponse, String> {
            unreachable!()
        }
        fn transport_kind(&self) -> &'static str { "fake" }
        fn get_authed(&self, url: &url::Url, bearer: &str, _max: usize)
            -> Result<kastellan_worker_web_common::http::RawResponse, String>
        {
            assert_eq!(bearer, "tok123");
            assert!(url.path().ends_with("/v1/accounts"));
            Ok(kastellan_worker_web_common::http::RawResponse {
                status: 200, location: None,
                content_type: "application/json".into(),
                body: br#"[{"id":1}]"#.to_vec(),
            })
        }
    }

    #[test]
    fn get_json_uses_bearer_and_parses() {
        let c = MailClient::for_test(
            url::Url::parse("http://127.0.0.1:8000").unwrap(),
            "tok123".into(),
            Box::new(FakeTransport),
        );
        let v = c.get_json("/v1/accounts").unwrap();
        assert_eq!(v[0]["id"], 1);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail get_json_uses_bearer`
Expected: FAIL — `MailClient` undefined.

- [ ] **Step 3: Implement `client.rs`**

```rust
//! localmail REST client. Reuses web-common's transport so force-routing
//! (proxy CONNECT + per-instance CA) works unchanged; adds bearer auth.

use kastellan_worker_web_common::http::{make_get, HttpGet, RawResponse};
use url::Url;

const DEFAULT_ATTACHMENT_MAX_BYTES: usize = 25 * 1024 * 1024;
const JSON_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub enum MailError {
    BadParams(String),
    Upstream { status: u16, body: String },
    Transport(String),
}

pub struct MailClient {
    base: Url,
    token: String,
    transport: Box<dyn HttpGet>,
    attachment_cap: usize,
}

impl MailClient {
    pub fn from_env() -> anyhow::Result<Self> {
        let base = std::env::var("KASTELLAN_MAIL_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_MAIL_ENDPOINT unset"))?;
        let base = Url::parse(&base)
            .map_err(|e| anyhow::anyhow!("KASTELLAN_MAIL_ENDPOINT invalid: {e}"))?;
        let token_file = std::env::var("KASTELLAN_MAIL_TOKEN_FILE")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_MAIL_TOKEN_FILE unset"))?;
        let token = std::fs::read_to_string(&token_file)
            .map_err(|e| anyhow::anyhow!("read token file {token_file}: {e}"))?
            .trim().to_string();
        if token.is_empty() {
            anyhow::bail!("token file {token_file} is empty");
        }
        let attachment_cap = std::env::var("KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_ATTACHMENT_MAX_BYTES);
        let transport = make_get("kastellan-mail/0")?;
        Ok(Self { base, token, transport, attachment_cap })
    }

    #[cfg(test)]
    pub fn for_test(base: Url, token: String, transport: Box<dyn HttpGet>) -> Self {
        Self { base, token, transport, attachment_cap: DEFAULT_ATTACHMENT_MAX_BYTES }
    }

    fn url(&self, path: &str) -> Result<Url, MailError> {
        self.base.join(path).map_err(|e| MailError::BadParams(format!("bad path {path}: {e}")))
    }

    fn check(resp: RawResponse) -> Result<RawResponse, MailError> {
        if (200..300).contains(&resp.status) {
            Ok(resp)
        } else {
            Err(MailError::Upstream {
                status: resp.status,
                body: String::from_utf8_lossy(&resp.body).chars().take(512).collect(),
            })
        }
    }

    pub fn get_json(&self, path: &str) -> Result<serde_json::Value, MailError> {
        let url = self.url(path)?;
        let resp = self.transport.get_authed(&url, &self.token, JSON_MAX_BYTES)
            .map_err(MailError::Transport)?;
        let resp = Self::check(resp)?;
        serde_json::from_slice(&resp.body).map_err(|e| MailError::Transport(format!("bad json: {e}")))
    }

    pub fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, MailError> {
        let url = self.url(path)?;
        let raw = serde_json::to_vec(body).map_err(|e| MailError::BadParams(e.to_string()))?;
        let resp = self.transport
            .post_authed(&url, &self.token, "application/json", &raw, JSON_MAX_BYTES)
            .map_err(MailError::Transport)?;
        let resp = Self::check(resp)?;
        serde_json::from_slice(&resp.body).map_err(|e| MailError::Transport(format!("bad json: {e}")))
    }

    pub fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>), MailError> {
        let url = self.url(path)?;
        let resp = self.transport.get_authed(&url, &self.token, self.attachment_cap)
            .map_err(MailError::Transport)?;
        let resp = Self::check(resp)?;
        Ok((resp.content_type, resp.body))
    }
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail get_json_uses_bearer`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add workers/mail/src/client.rs
git commit -m "feat(mail): localmail REST client with bearer auth over web-common transport"
```

---

### Task B3: `mail.search` (POST /v1/search)

**Files:**
- Modify: `workers/mail/src/handler.rs`
- Test: inline

**Interfaces:**
- Consumes: `MailClient::post_json` (B2).
- Produces: method `mail.search` → forwards `{query, filters?, sort?, limit?, cursor?}` (NOT `smart` — omitted, forced off) to `POST /v1/search`, returns the localmail JSON verbatim.

- [ ] **Step 1: Write the failing test** — a `FakeTransport` whose `post_authed` asserts the path is `/v1/search` and the body contains `"query":"qantas"` and does **not** contain `"smart"`, returning `{"hits":[],"next_cursor":null}`; assert the handler returns that JSON.

```rust
#[test]
fn search_posts_query_without_smart() {
    // FakeTransport.post_authed asserts body has query, lacks "smart"; returns hits:[].
    // (construct MailHandler with client via MailHandler::with_client(...) test ctor)
    let mut h = MailHandler::with_client(fake_client_asserting_search());
    let out = h.call("mail.search", serde_json::json!({"query":"qantas"})).unwrap();
    assert!(out["hits"].is_array());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail search_posts_query`
Expected: FAIL (method unknown / no `with_client`).

- [ ] **Step 3: Refactor `MailHandler` to hold a client + add `with_client`**

Change `MailHandler` to `pub struct MailHandler { client: client::MailClient }`; `from_env` builds `client::MailClient::from_env()?`; add `#[cfg(test)] pub fn with_client(client: client::MailClient) -> Self`.

- [ ] **Step 4: Implement `mail.search` dispatch**

In `Handler::call`, before the method-not-found fallback:

```rust
match method {
    "mail.search" => {
        #[derive(serde::Deserialize)]
        struct P {
            query: String,
            #[serde(default)] filters: Option<serde_json::Value>,
            #[serde(default)] sort: Option<String>,
            #[serde(default)] limit: Option<u32>,
            #[serde(default)] cursor: Option<String>,
        }
        let p: P = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        let mut body = serde_json::json!({ "query": p.query });
        if let Some(f) = p.filters { body["filters"] = f; }
        if let Some(s) = p.sort { body["sort"] = serde_json::json!(s); }
        if let Some(l) = p.limit { body["limit"] = serde_json::json!(l); }
        if let Some(c) = p.cursor { body["cursor"] = serde_json::json!(c); }
        // `smart` deliberately never set — workers do not call the LLM.
        self.client.post_json("/v1/search", &body).map_err(mail_err_to_rpc)
    }
    _ => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
}
```

Add the error mapper:

```rust
fn mail_err_to_rpc(e: client::MailError) -> RpcError {
    match e {
        client::MailError::BadParams(m) => RpcError::new(codes::INVALID_PARAMS, m),
        client::MailError::Upstream { status: 401 | 403, .. } =>
            RpcError::new(codes::POLICY_DENIED, "localmail auth/permission denied (check token/ACL)".to_string()),
        client::MailError::Upstream { status, body } =>
            RpcError::new(codes::OPERATION_FAILED, format!("localmail {status}: {body}")),
        client::MailError::Transport(m) =>
            RpcError::new(codes::OPERATION_FAILED, format!("transport: {m}")),
    }
}
```

- [ ] **Step 5: Run it to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail search_posts_query`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add workers/mail/src/handler.rs
git commit -m "feat(mail): mail.search over POST /v1/search (smart forced off)"
```

---

### Task B4: `mail.get_message`, `mail.list_messages`, `mail.list_accounts` (GET)

**Files:**
- Modify: `workers/mail/src/handler.rs`
- Test: inline

**Interfaces:**
- Consumes: `MailClient::get_json`.
- Produces: `mail.get_message {message_id, full_headers?}` → `GET /v1/messages/{id}?full_headers=`; `mail.list_messages {account_ids?, folder_ids?, limit?, cursor?}` → `GET /v1/messages?…`; `mail.list_accounts {}` → `GET /v1/accounts`.

- [ ] **Step 1: Write failing tests** — a fake client asserting each built path (e.g. `mail.get_message{message_id:5}` → `/v1/messages/5`; `mail.list_messages{limit:10}` → `/v1/messages?limit=10`; `mail.list_accounts` → `/v1/accounts`), returning a stub JSON. One test per method.

- [ ] **Step 2: Run to verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail -- get_message list_messages list_accounts`
Expected: FAIL.

- [ ] **Step 3: Implement the three GET arms**

Add match arms:

```rust
"mail.get_message" => {
    #[derive(serde::Deserialize)]
    struct P { message_id: i64, #[serde(default)] full_headers: bool }
    let p: P = serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
    let path = format!("/v1/messages/{}?full_headers={}", p.message_id, p.full_headers);
    self.client.get_json(&path).map_err(mail_err_to_rpc)
}
"mail.list_messages" => {
    #[derive(serde::Deserialize)]
    struct P {
        #[serde(default)] account_ids: Option<Vec<i64>>,
        #[serde(default)] folder_ids: Option<Vec<i64>>,
        #[serde(default)] limit: Option<u32>,
        #[serde(default)] cursor: Option<String>,
    }
    let p: P = serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
    let mut q: Vec<String> = Vec::new();
    if let Some(a) = &p.account_ids { q.push(format!("account_ids={}", join_ids(a))); }
    if let Some(f) = &p.folder_ids { q.push(format!("folder_ids={}", join_ids(f))); }
    if let Some(l) = p.limit { q.push(format!("limit={l}")); }
    if let Some(c) = &p.cursor { q.push(format!("cursor={}", urlencode(c))); }
    let path = if q.is_empty() { "/v1/messages".to_string() } else { format!("/v1/messages?{}", q.join("&")) };
    self.client.get_json(&path).map_err(mail_err_to_rpc)
}
"mail.list_accounts" => self.client.get_json("/v1/accounts").map_err(mail_err_to_rpc),
```

Add helpers `fn join_ids(v: &[i64]) -> String { v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",") }` and a minimal `fn urlencode(s: &str) -> String` (percent-encode via `url::form_urlencoded` or a small allowlist; confirm localmail's exact query-param shape for `account_ids`/`cursor` at plan-execution time against the running service — see spec Open Questions).

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail -- get_message list_messages list_accounts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add workers/mail/src/handler.rs
git commit -m "feat(mail): mail.get_message / list_messages / list_accounts"
```

---

### Task B5: `mail.get_attachment_text` (GET)

**Files:**
- Modify: `workers/mail/src/handler.rs`
- Test: inline

**Interfaces:**
- Consumes: `MailClient::get_bytes` (attachment text is `text/plain`; return as a JSON string).
- Produces: `mail.get_attachment_text {sha256}` → `GET /v1/attachments/{sha256}/text` → `{ "sha256", "text" }`.

- [ ] **Step 1: Write the failing test** — fake client asserting path `/v1/attachments/abc123/text`, returning `("text/plain", b"extracted body")`; assert `out["text"] == "extracted body"`.

- [ ] **Step 2: Run to verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail get_attachment_text`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
"mail.get_attachment_text" => {
    #[derive(serde::Deserialize)]
    struct P { sha256: String }
    let p: P = serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
    validate_sha256(&p.sha256).map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
    let (_ct, bytes) = self.client.get_bytes(&format!("/v1/attachments/{}/text", p.sha256))
        .map_err(mail_err_to_rpc)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(serde_json::json!({ "sha256": p.sha256, "text": text }))
}
```

Add `fn validate_sha256(s: &str) -> Result<(), String>` requiring exactly 64 lowercase hex chars (prevents path traversal in the URL segment).

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail get_attachment_text`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add workers/mail/src/handler.rs
git commit -m "feat(mail): mail.get_attachment_text"
```

---

### Task B6: `mail.get_attachment` (original bytes → workspace out/)

**Files:**
- Modify: `workers/mail/src/handler.rs`
- Test: inline

**Interfaces:**
- Consumes: `MailClient::get_bytes`; the `KASTELLAN_WORKER_OUT` env (from Phase A).
- Produces: `mail.get_attachment {sha256, filename?}` → `GET /v1/attachments/{sha256}` → writes bytes to `<out>/<safe-name>` (`.partial` then rename), returns `{ "sha256", "filename", "content_type", "size", "path" }`. No bytes in the result.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn get_attachment_writes_to_out_dir_safely() {
    let out = std::env::temp_dir().join(format!("mailout-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    std::env::set_var("KASTELLAN_WORKER_OUT", &out);
    let mut h = MailHandler::with_client(fake_client_returning_pdf()); // returns ("application/pdf", b"%PDF..")
    let sha = "a".repeat(64);
    let out_json = h.call("mail.get_attachment",
        serde_json::json!({"sha256": sha, "filename": "../evil/booking.pdf"})).unwrap();
    let path = std::path::PathBuf::from(out_json["path"].as_str().unwrap());
    assert!(path.starts_with(&out), "must stay within out dir: {path:?}");
    assert!(path.exists());
    assert!(out_json.get("data_base64").is_none(), "no bytes in result");
    std::env::remove_var("KASTELLAN_WORKER_OUT");
    std::fs::remove_dir_all(&out).ok();
}
```

- [ ] **Step 2: Run to verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail get_attachment_writes_to_out`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
"mail.get_attachment" => {
    #[derive(serde::Deserialize)]
    struct P { sha256: String, #[serde(default)] filename: Option<String> }
    let p: P = serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
    validate_sha256(&p.sha256).map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
    let out_dir = std::env::var("KASTELLAN_WORKER_OUT").map_err(|_| RpcError::new(
        codes::OPERATION_FAILED,
        "no task output dir (KASTELLAN_WORKER_OUT unset) — attachment delivery unavailable".to_string(),
    ))?;
    let (content_type, bytes) = self.client.get_bytes(&format!("/v1/attachments/{}", p.sha256))
        .map_err(mail_err_to_rpc)?;
    let name = safe_attachment_name(p.filename.as_deref(), &p.sha256);
    let dir = std::path::Path::new(&out_dir);
    let dest = dir.join(&name);
    let partial = dir.join(format!("{name}.partial"));
    std::fs::write(&partial, &bytes)
        .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("write attachment: {e}")))?;
    std::fs::rename(&partial, &dest)
        .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("finalize attachment: {e}")))?;
    Ok(serde_json::json!({
        "sha256": p.sha256, "filename": name, "content_type": content_type,
        "size": bytes.len(), "path": dest.to_string_lossy(),
    }))
}
```

Add `fn safe_attachment_name(requested: Option<&str>, sha256: &str) -> String`: take only the final path component of `requested` (`Path::new(r).file_name()`), strip anything non-`[A-Za-z0-9._-]`, drop leading dots; if empty, use `attachment`; prefix the first 12 sha256 chars for collision safety → e.g. `a1b2c3d4e5f6_booking.pdf`.

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail get_attachment_writes_to_out`
Expected: PASS — path stays under out dir even with a `../evil/` filename.

- [ ] **Step 5: Commit**

```bash
git add workers/mail/src/handler.rs
git commit -m "feat(mail): mail.get_attachment streams original bytes to workspace out/"
```

---

### Task B7: Core manifest + registration + token-file startup wiring

**Files:**
- Create: `core/src/workers/mail.rs`
- Modify: `core/src/workers/mod.rs` (`pub mod mail;`)
- Modify: `core/src/registry_build.rs:20-28` (add `&crate::workers::mail::MailManifest,`)
- Modify: `core/src/main.rs` (materialize token → 0600 file → set `KASTELLAN_MAIL_TOKEN_FILE` before `build_tool_registry`)
- Test: inline in `mail.rs`

**Interfaces:**
- Consumes: `WorkerManifest`, `ToolEntry`, `SandboxPolicy`, `discover_binary`, `allowlist_to_net_entries` (reused from `web_fetch`), `Vault::materialize`/`redeem`.
- Produces: `MailManifest` (registers when `KASTELLAN_MAIL_ENDPOINT` + `KASTELLAN_MAIL_TOKEN_FILE` are set; `Disabled` when endpoint unset; `Misconfigured` when set-but-broken). Tool docs for all six `mail.*` methods.

- [ ] **Step 1: Write the failing manifest test**

In `core/src/workers/mail.rs` `#[cfg(test)] mod tests`, assert: (a) `resolve` returns `Disabled` when `KASTELLAN_MAIL_ENDPOINT` is unset; (b) the built entry's `policy.net` is `Net::Allowlist` containing exactly the endpoint host:port from the allowlist and nothing else; (c) `policy.fs_read` contains the token file; (d) `tool_docs()` advertises all six methods. Use a stub `ResolveCtx` (mirror the pattern in `core/src/workers/web_fetch.rs` tests).

- [ ] **Step 2: Run to verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core mail_manifest`
Expected: FAIL — module absent.

- [ ] **Step 3: Implement `core/src/workers/mail.rs`**

Model on `core/src/workers/web_fetch.rs`. Key points:
- Consts: `TOOL_NAME = "mail"`, `BIN_ENV = "KASTELLAN_MAIL_BIN"`, `DEFAULT_BIN_NAME = "kastellan-worker-mail"`, `ENDPOINT_ENV = "KASTELLAN_MAIL_ENDPOINT"`, `TOKEN_FILE_ENV = "KASTELLAN_MAIL_TOKEN_FILE"`.
- `mail_entry(binary, endpoint, token_file, allowlist)` builds `SandboxPolicy`:
  - `fs_read: vec![binary.clone(), PathBuf::from(token_file), /etc/resolv.conf, /etc/hosts, /etc/nsswitch.conf]`
  - `net: Net::Allowlist(endpoint_to_net_entries(&allowlist_or_endpoint))` — allow the endpoint host:port. Reuse `super::web_fetch::allowlist_to_net_entries` if the allowlist rows carry ports, else map the single endpoint host:port. (Confirm whether "mail" uses the `tool_allowlists` table or an endpoint-derived allowlist like web-search; the spec says `tool_allowlists` keyed `"mail"` — so override `allowlist_tool()`/`allowlist_kind()` and map its rows.)
  - `env: vec![(ENDPOINT_ENV, endpoint), (TOKEN_FILE_ENV, token_file)]`
  - `profile: Profile::WorkerNetClient`, `cpu_ms: 10_000`, `mem_mb: 256`, `wall_clock_ms: Some(30_000)`, `SingleUse`, all `Option` fields `None`, `ephemeral_scratch: false`, `broker: None`.
- `impl WorkerManifest for MailManifest`: `name()="mail"`, `allowlist_tool()=Some("mail")`, `allowlist_kind()=Some(EntryKind::Domain)` (or the host kind matching an IP-literal/host endpoint — confirm against `tool_allowlists` validation, #469), `tool_docs()` returns the six `ToolDoc`s, and `resolve()`:
  - read `ENDPOINT_ENV` via `ctx.get_env`; if `None` → `Resolution::Disabled { detail: "KASTELLAN_MAIL_ENDPOINT unset".into() }`.
  - read `TOKEN_FILE_ENV`; if `None` or file missing (`!(ctx.exists)(path)`) → `Misconfigured`.
  - `discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME)` → `Misconfigured` if `None`.
  - else `Resolution::Register(mail_entry(...))`.

The six `ToolDoc`s (verbatim summaries):

```rust
fn tool_docs(&self) -> Vec<ToolDoc> {
    vec![
        ToolDoc { name: TOOL_NAME, method: "mail.search",
            summary: "Search the mail archive (hybrid semantic + full-text). Filter by date range, from/to, subject, has_attachment, account/folder. Page with next_cursor.",
            params: &[
                ToolParam { name: "query", description: "free-text search query", required: true },
                ToolParam { name: "filters", description: "object: date_from,date_to,from,to,subject,has_attachment,account_ids,folder_ids,lang", required: false },
                ToolParam { name: "sort", description: "'rank' (default) or 'date'", required: false },
                ToolParam { name: "limit", description: "max hits (default 50)", required: false },
                ToolParam { name: "cursor", description: "next_cursor from a prior page", required: false },
            ] },
        ToolDoc { name: TOOL_NAME, method: "mail.get_message",
            summary: "Fetch one message: headers, plaintext body, and attachment list [{filename,sha256,content_type,size}].",
            params: &[
                ToolParam { name: "message_id", description: "message id from a search/list hit", required: true },
                ToolParam { name: "full_headers", description: "include full headers (default false)", required: false },
            ] },
        ToolDoc { name: TOOL_NAME, method: "mail.list_messages",
            summary: "Browse messages newest-first; filter by account/folder. Page with next_cursor.",
            params: &[
                ToolParam { name: "account_ids", description: "restrict to these account ids", required: false },
                ToolParam { name: "folder_ids", description: "restrict to these folder ids", required: false },
                ToolParam { name: "limit", description: "max rows (default 50)", required: false },
                ToolParam { name: "cursor", description: "next_cursor from a prior page", required: false },
            ] },
        ToolDoc { name: TOOL_NAME, method: "mail.list_accounts",
            summary: "List the mail accounts this agent may read.", params: &[] },
        ToolDoc { name: TOOL_NAME, method: "mail.get_attachment_text",
            summary: "Extracted text of an attachment (server-side PDF/office extraction). Use to READ an attachment.",
            params: &[ToolParam { name: "sha256", description: "attachment sha256 from get_message", required: true }] },
        ToolDoc { name: TOOL_NAME, method: "mail.get_attachment",
            summary: "Save an attachment in its ORIGINAL format (PDF, etc.) to the task output dir; returns its path/size/content_type. Use to DELIVER a file.",
            params: &[
                ToolParam { name: "sha256", description: "attachment sha256 from get_message", required: true },
                ToolParam { name: "filename", description: "suggested filename (sanitized)", required: false },
            ] },
    ]
}
```

- [ ] **Step 4: Register the module + manifest**

`core/src/workers/mod.rs`: add `pub mod mail;`. `core/src/registry_build.rs:20-28`: add `&crate::workers::mail::MailManifest,` to `WORKER_MANIFESTS`.

- [ ] **Step 5: Token-file startup wiring in `main.rs`**

In `core/src/main.rs`, **before** `build_tool_registry` (line ~182), where `pool` and `vault` exist, materialize the token and export the path:

```rust
// Mail worker token: materialize the operator-stored secret into a 0600 file
// and point the manifest at it (path-in-env, plaintext-in-file — the Matrix
// pattern). Only when the mail endpoint is configured.
if std::env::var("KASTELLAN_MAIL_ENDPOINT").is_ok() {
    match kastellan_core::workers::mail::provision_token_file(&pool, &kp, &vault).await {
        Ok(Some(path)) => std::env::set_var("KASTELLAN_MAIL_TOKEN_FILE", path),
        Ok(None) => tracing::warn!("mail: endpoint set but secret 'localmail-agent-token' absent; mail worker will be Misconfigured"),
        Err(e) => tracing::warn!(error = %e, "mail: token provisioning failed; mail worker will be Misconfigured"),
    }
}
```

Implement `provision_token_file(pool, key_provider, vault) -> anyhow::Result<Option<PathBuf>>` in `core/src/workers/mail.rs`: if the secret named `localmail-agent-token` is absent (`db::secrets::list`), return `Ok(None)`; else `vault.materialize(pool, kp, "localmail-agent-token", "daemon:mail-token")` → `redeem` → write the plaintext to a `0600` file under the runtime dir (reuse `write_private` from `channel/matrix/policy.rs`, or replicate: create with mode `0o600`), return its path.

> Confirm the exact `kp`/`vault` binding names in `main.rs` (the Matrix path uses `OsKeyringProvider` + `Vault`); reuse the same instances already constructed for the dispatcher (`vault.clone()` is passed to `ToolHostStepDispatcher::new`).

- [ ] **Step 6: Run the manifest tests + build**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core mail_manifest && cargo build --workspace`
Expected: PASS + clean build (worker binary compiles too).

- [ ] **Step 7: DGX verification (core + cfg-linux + workspace build)**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --workspace 2>&1 | tail -5 && cargo test -p kastellan-core mail_manifest 2>&1 | tail -15 && cargo clippy -p kastellan-core -p kastellan-worker-mail --all-targets -- -D warnings 2>&1 | tail -10'`
Expected: build OK, tests PASS, clippy clean.

- [ ] **Step 8: Commit**

```bash
git add core/src/workers/mail.rs core/src/workers/mod.rs core/src/registry_build.rs core/src/main.rs
git commit -m "feat(core): register mail worker manifest + vault-backed token file"
```

---

### Task B8: Live integration test against `localmail serve`

**Files:**
- Create: `core/tests/mail_worker_e2e.rs`
- Test: this file (`#[ignore]`, opt-in via env)

**Interfaces:**
- Consumes: the full dispatch path (registry → spawn → worker → localmail).

- [ ] **Step 1: Write the ignored e2e test**

Gate on `KASTELLAN_MAIL_E2E_ENDPOINT` + `KASTELLAN_MAIL_E2E_TOKEN` (skip-as-pass with an `eprintln!("[SKIP] …")` when unset, per the repo's suspicious-green convention). The test spawns the mail worker directly (mirror `core/tests/shell_exec_e2e.rs::worker_binary` for locating the built binary), sets `KASTELLAN_MAIL_ENDPOINT` + a temp `KASTELLAN_MAIL_TOKEN_FILE` (0600) + `KASTELLAN_WORKER_OUT` (a tempdir), and drives JSON-RPC over stdio:
  - `mail.list_accounts` → non-empty array;
  - `mail.search {query:"the", limit:2}` → `hits` present, capture a `next_cursor` if any and page once;
  - if a hit has a message with an attachment: `mail.get_attachment {sha256}` → asserts a file exists under `KASTELLAN_WORKER_OUT`, `size > 0`, byte length matches the reported `size`.

```rust
// Skeleton — fill the JSON-RPC round-trip using the same stdio harness as
// core/tests/shell_exec_e2e.rs.
#[test]
fn mail_worker_live_roundtrip() {
    let (Ok(endpoint), Ok(token)) = (
        std::env::var("KASTELLAN_MAIL_E2E_ENDPOINT"),
        std::env::var("KASTELLAN_MAIL_E2E_TOKEN"),
    ) else {
        eprintln!("[SKIP] mail e2e: set KASTELLAN_MAIL_E2E_ENDPOINT + _TOKEN");
        return;
    };
    // ... spawn worker, drive mail.list_accounts / mail.search / mail.get_attachment ...
    let _ = (endpoint, token);
}
```

- [ ] **Step 2: Run it (skips cleanly without a server)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test mail_worker_e2e -- --nocapture`
Expected: `[SKIP]` line, test passes (no server configured).

- [ ] **Step 3: Run it for real against a live localmail (manual)**

Start `localmail serve` with a seeded archive; mint an agent token; then:
Run: `KASTELLAN_MAIL_E2E_ENDPOINT=http://127.0.0.1:8000 KASTELLAN_MAIL_E2E_TOKEN=<tok> cargo test -p kastellan-core --test mail_worker_e2e -- --nocapture --ignored`
Expected: real round-trips PASS; an attachment lands in the temp out dir byte-for-byte.

- [ ] **Step 4: Commit**

```bash
git add core/tests/mail_worker_e2e.rs
git commit -m "test(mail): live localmail serve integration test (ignored, opt-in)"
```

---

### Task B9: Provisioning docs + handover

**Files:**
- Create: `docs/workers/mail.md` (operator provisioning guide)
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Write `docs/workers/mail.md`** — the one-time operator steps from the spec §"Config & provisioning": create a localmail `agent` API user + grant accounts + mint token; `kastellan-cli secret put localmail-agent-token`; set `KASTELLAN_MAIL_ENDPOINT`; add the endpoint to `tool_allowlists` keyed `"mail"`; ensure `localmail serve` is reachable. Note co-located loopback uses the allowlisted-IP-literal proxy carve-out; remote uses HTTPS. Document `KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES` and where delivered files land (`~/.kastellan/artifacts/<task_id>/`).

- [ ] **Step 2: Update HANDOVER.md + ROADMAP.md** — record the mail worker + Workspace activation as done, the new baseline test counts (from the DGX run), and any carried follow-ups (true attachment streaming; artifacts-dir retention/GC; localmail#196 landed check; MCP-OAuth uplink deferred).

- [ ] **Step 3: Commit**

```bash
git add docs/workers/mail.md docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(mail): operator provisioning guide + handover/roadmap"
```

---

## Final verification (before opening the PR)

- [ ] `source "$HOME/.cargo/env" && cargo build --workspace` — clean.
- [ ] DGX authoritative gate: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test --workspace 2>&1 | tail -20 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10'` — no new failures vs. baseline, clippy clean, **0 `[SKIP]`** for containment tests.
- [ ] macOS: `cargo test -p kastellan-worker-mail -p kastellan-worker-web-common -p kastellan-core` green (use a scratch `CARGO_TARGET_DIR` to dodge the rust-analyzer build-lock).
- [ ] Manual live e2e (Task B8 Step 3) run once against a real localmail archive — the Qantas-style flow (search → get_message → get_attachment → file in artifacts dir) works end to end.

## Self-Review notes (spec coverage)

- Worker→HTTP REST + bearer, Rust worker, six `mail.*` tools → Tasks B1–B7. ✓
- Original-format attachments to a durable dir → Phase A + B6. ✓
- Force-routing / proxy transport → B0 (reuse `ProxyConnectGet`) + B7 (`Net::Allowlist` endpoint only). ✓
- Secret handling (no plaintext in env) → B7 token-file pattern. ✓
- Cross-platform + DGX gates → per-task DGX steps + final gate. ✓
- localmail#196 (`content_type`+`size`) is a localmail-side change; consumed by `mail.get_message` docs/output — no kastellan task blocks on it (worker passes the list through verbatim). ✓
- Open questions (exact `/v1/search` shape, query-param encoding, token-mint CLI) are flagged inline in B3/B4/B9 to pin against the live service at execution time — not placeholders in code, but real "confirm against running localmail" checkpoints.
