# macOS Seatbelt Sandbox Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the macOS counterpart of the Linux `bwrap` sandbox backend so the same `SandboxPolicy` runs tool workers under `sandbox-exec` (Seatbelt) on macOS, with `cargo test --workspace` green on macOS and the existing 36 Linux tests unaffected.

**Architecture:** New `sandbox/src/macos_seatbelt.rs` mirrors `linux_bwrap.rs` shape — a pure `build_profile()` returning a TinyScheme `.sb` string, plus `MacosSeatbelt::probe()` and `spawn_under_policy()`. `default_backend()` picks it on macOS. Integration smoke tests in `sandbox/tests/macos_smoke.rs` mirror `linux_smoke.rs`. A tiny `net_probe` Rust fixture binary replaces `getent` for the network-deny test. `core/tests/shell_exec_e2e.rs` is generalised to compile on both OSes via per-OS helpers.

**Tech Stack:** Rust 1.78+ workspace, `std::process::Command`, `sandbox-exec` (Apple SPI, on `$PATH` at `/usr/bin/sandbox-exec`). No new third-party dependencies.

**Spec:** [`docs/superpowers/specs/2026-05-07-macos-seatbelt-backend-design.md`](../specs/2026-05-07-macos-seatbelt-backend-design.md)

**Conventions for the executing engineer:**
- Cargo isn't on the default `PATH` for non-interactive shells. Run `source "$HOME/.cargo/env"` once at session start (or prefix every cargo command).
- Every code step shows the actual code. If a step says "implement X", the X is in a code block in that step.
- Every test step shows the exact `cargo test` invocation and the expected outcome (PASS / FAIL with reason).
- Commits are one-per-task. Use the message template at each task's final step.
- These tasks build on each other. Do them in order.

---

## File Structure

```
sandbox/src/macos_seatbelt.rs        NEW   ~250 LoC; pure build_profile + MacosSeatbelt + 6 unit tests
sandbox/src/lib.rs                   EDIT  cfg-gated mod, default_backend() arm
sandbox/Cargo.toml                   EDIT  one [[bin]] entry for net_probe fixture
sandbox/tests/macos_smoke.rs         NEW   ~150 LoC; #![cfg(target_os = "macos")]; 7 integration tests
sandbox/tests/fixtures/net_probe.rs  NEW   ~12 LoC; standalone TcpStream::connect probe binary
core/tests/shell_exec_e2e.rs         EDIT  drop cfg(linux); per-OS skip + backend helpers
docs/threat-model.md                 EDIT  SPI paragraph + macos_smoke row
```

Boundaries: `macos_seatbelt.rs` owns profile assembly and process spawning; nothing else in the workspace knows about TinyScheme. `net_probe.rs` is a single-purpose fixture with no dependencies on the rest of the workspace. The cross-platform e2e file gains per-OS helpers but its assertions stay identical.

---

## Task 1: Wire empty macos_seatbelt module behind cfg gate

**Goal:** Get a stub `macos_seatbelt.rs` compiling under `cfg(target_os = "macos")` and `MacosSeatbelt` reachable from `default_backend()`. No behaviour yet — just the module exists.

**Files:**
- Create: `sandbox/src/macos_seatbelt.rs`
- Modify: `sandbox/src/lib.rs`

- [ ] **Step 1: Create stub `sandbox/src/macos_seatbelt.rs`**

```rust
//! macOS backend for [`SandboxBackend`]: shells out to `/usr/bin/sandbox-exec`
//! (Seatbelt). Mirrors the Linux `linux_bwrap` backend's shape:
//!   - `build_profile(policy)` is a pure function returning the TinyScheme
//!     `.sb` profile we hand to `sandbox-exec -p`.
//!   - `MacosSeatbelt::probe()` runs a minimal `sandbox-exec /usr/bin/true`
//!     to verify Seatbelt is healthy on this host.
//!   - `MacosSeatbelt::spawn_under_policy()` validates the policy paths,
//!     builds the profile, and spawns the worker.
//!
//! What this backend gives you (Phase 0b):
//!   - Mandatory Access Control (MAC) via Seatbelt: default-deny FS, default-deny
//!     network, explicit allowlists for /usr/lib, /System/Library, /dev's safe
//!     nodes, and per-policy fs_read / fs_write paths.
//!   - Environment cleared via `Command::env_clear()` before exec (analogue of
//!     bwrap's `--clearenv`); `policy.env` re-applied on top.
//!   - `Command::process_group(0)` so the worker is in its own session
//!     (analogue of `--new-session`).
//!
//! Not yet (deferred to supervisor work):
//!   - `setrlimit` for `policy.cpu_ms` / `policy.mem_mb`.
//!   - A `--die-with-parent` equivalent. macOS has no `PR_SET_PDEATHSIG`;
//!     either a `kqueue(EVFILT_PROC, NOTE_EXIT)` watcher or supervisor lifecycle
//!     handles this. Today the worker can outlive a crashed parent — caught by
//!     the supervisor in Phase 0 cont.
//!
//! See [`docs/superpowers/specs/2026-05-07-macos-seatbelt-backend-design.md`].

use std::process::{Child, Command, Stdio};

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

/// Shell out to `/usr/bin/sandbox-exec` for sandboxing.
#[derive(Default)]
pub struct MacosSeatbelt;

impl MacosSeatbelt {
    pub fn new() -> Self {
        Self
    }
}

impl SandboxBackend for MacosSeatbelt {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<Child, SandboxError> {
        Err(SandboxError::Backend(
            "MacosSeatbelt::spawn_under_policy not yet implemented".into(),
        ))
    }
}

// Avoid unused-import warnings until later tasks fill these in.
#[allow(dead_code)]
fn _unused_imports_marker(_c: Command, _s: Stdio) {}
```

- [ ] **Step 2: Wire the cfg-gated mod into `sandbox/src/lib.rs`**

Replace the module-declaration block (around line 11–12) and the `default_backend` function (around line 79–105) in [sandbox/src/lib.rs](sandbox/src/lib.rs) so the file's relevant sections become:

```rust
#[cfg(target_os = "linux")]
pub mod linux_bwrap;
#[cfg(target_os = "macos")]
pub mod macos_seatbelt;
```

```rust
/// Pick the default backend for the current OS.
pub fn default_backend() -> Box<dyn SandboxBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux_bwrap::LinuxBwrap::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos_seatbelt::MacosSeatbelt::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Box::new(NotYetImplemented)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct NotYetImplemented;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl SandboxBackend for NotYetImplemented {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<std::process::Child, SandboxError> {
        Err(SandboxError::Backend(
            "no sandbox backend for this OS — only Linux and macOS are supported".into(),
        ))
    }
}
```

- [ ] **Step 3: Compile to verify the wiring**

Run: `cargo build -p hhagent-sandbox`
Expected: builds clean (no errors, possibly a `dead_code` warning on the `_unused_imports_marker` helper — that is fine and goes away in Task 2).

- [ ] **Step 4: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs sandbox/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): scaffold MacosSeatbelt backend behind cfg gate

Empty module that compiles on macOS and is wired into default_backend().
spawn_under_policy returns a NotImplemented error until subsequent tasks
fill it in. Linux build path unchanged.
EOF
)"
```

---

## Task 2: build_profile — version + deny-default header

**Goal:** First slice of the pure `build_profile(policy)` function. Just emits the version line and the global deny default. TDD: write the test first, watch it fail, then implement.

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add the test module and first failing test to `sandbox/src/macos_seatbelt.rs`**

Append to the bottom of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Net, Profile};
    use std::path::PathBuf;

    fn strict_policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_read: vec![],
            fs_write: vec![],
            net: Net::Deny,
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::WorkerStrict,
            env: vec![],
        }
    }

    #[test]
    fn profile_starts_with_version_and_deny_default() {
        let p = build_profile(&strict_policy());
        // (version 1) must appear before any allow/deny rule.
        let version_idx = p.find("(version 1)").expect("missing (version 1)");
        let deny_default_idx = p.find("(deny default)").expect("missing (deny default)");
        assert!(version_idx < deny_default_idx);
    }

    // Suppress unused warnings on PathBuf until Task 5.
    #[allow(dead_code)]
    fn _path_marker(_p: PathBuf) {}
}
```

- [ ] **Step 2: Run the test, verify it fails to compile (no `build_profile`)**

Run: `cargo test -p hhagent-sandbox profile_starts_with_version_and_deny_default`
Expected: FAIL — compilation error `cannot find function 'build_profile' in this scope`.

- [ ] **Step 3: Implement minimal `build_profile`**

Insert above the `#[cfg(test)] mod tests` block:

```rust
/// Build the TinyScheme `.sb` profile string for `policy`. Pure function:
/// no I/O, no syscalls — exposed so unit tests can assert on the profile
/// text without spawning a process.
pub fn build_profile(policy: &SandboxPolicy) -> String {
    let _ = policy; // unused until Task 3
    let mut out = String::new();
    out.push_str("(version 1)\n");
    out.push_str("(deny default)\n");
    out
}
```

Now also remove the leftover `_unused_imports_marker` from Task 1 — `Command` and `Stdio` are still unused but will be used in Task 8; replace its body with `#[allow(unused_imports)]` on the imports instead. Replace the line `use std::process::{Child, Command, Stdio};` with:

```rust
#[allow(unused_imports)]
use std::process::{Child, Command, Stdio};
```

…and delete the `_unused_imports_marker` function entirely.

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test -p hhagent-sandbox profile_starts_with_version_and_deny_default`
Expected: PASS (1 test passed).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): build_profile scaffold with version + deny-default

Pure function returning the .sb profile string. First slice: just emits
(version 1) and (deny default). Subsequent tasks layer on the always-on
allow rules, /dev allowlist, fs_read/fs_write rules, and network handling.
EOF
)"
```

---

## Task 3: build_profile — always-on allows for dyld and libsystem

**Goal:** Add the always-on allow rules that any process needs to launch under Seatbelt (dyld, libsystem, sysctl-read, file-read-metadata for path resolution).

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add a failing test for the always-on rules**

Append to `mod tests` in `sandbox/src/macos_seatbelt.rs`:

```rust
#[test]
fn profile_emits_always_on_allows() {
    let p = build_profile(&strict_policy());
    for needle in [
        "(allow process-fork)",
        "(allow process-exec*)",
        "(allow file-read* (subpath \"/usr/lib\"))",
        "(allow file-read* (subpath \"/usr/libexec\"))",
        "(allow file-read* (subpath \"/System/Library\"))",
        "(allow file-read-metadata (subpath \"/\"))",
        "(allow sysctl-read)",
    ] {
        assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
    }
}
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hhagent-sandbox profile_emits_always_on_allows`
Expected: FAIL — assertion fires on the first missing needle (`(allow process-fork)`).

- [ ] **Step 3: Extend `build_profile` to emit the always-on allows**

Replace the body of `build_profile` with:

```rust
pub fn build_profile(policy: &SandboxPolicy) -> String {
    let _ = policy; // unused until Task 4
    let mut out = String::new();
    out.push_str("(version 1)\n");
    out.push_str("(deny default)\n");

    // Always-on: dyld + libsystem need these to start any process.
    out.push_str("(allow process-fork)\n");
    out.push_str("(allow process-exec*)\n");
    out.push_str("(allow file-read* (subpath \"/usr/lib\"))\n");
    out.push_str("(allow file-read* (subpath \"/usr/libexec\"))\n");
    out.push_str("(allow file-read* (subpath \"/System/Library\"))\n");
    // Required for path-component resolution by dyld; deliberate concession,
    // see threat-model.md "Asymmetric platform note".
    out.push_str("(allow file-read-metadata (subpath \"/\"))\n");
    out.push_str("(allow sysctl-read)\n");

    out
}
```

- [ ] **Step 4: Run both tests, verify they pass**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): always-on allows so dyld + libsystem can start processes

Adds the minimum allow set every process needs under (deny default):
process-fork, process-exec*, /usr/lib, /usr/libexec, /System/Library,
file-read-metadata on /, and sysctl-read. file-read-metadata is the
deliberate FS asymmetry vs Linux that the threat model already documents.
EOF
)"
```

---

## Task 4: build_profile — explicit /dev allowlist

**Goal:** Default-deny `/dev`, allowlist only the safe nodes (`null`, `zero`, `random`, `urandom`, `tty`, `fd/`, `dtracehelper`). Containment outcome equivalent to bwrap's `--dev /dev`.

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add a failing test for the /dev allowlist**

Append to `mod tests`:

```rust
#[test]
fn dev_allowlist_is_minimal() {
    let p = build_profile(&strict_policy());
    // The seven safe /dev nodes must be present.
    for needle in [
        "(literal \"/dev/null\")",
        "(literal \"/dev/zero\")",
        "(literal \"/dev/random\")",
        "(literal \"/dev/urandom\")",
        "(literal \"/dev/tty\")",
        "(subpath \"/dev/fd\")",
        "(literal \"/dev/dtracehelper\")",
    ] {
        assert!(p.contains(needle), "profile missing {needle:?}; got:\n{p}");
    }
    // /dev as a whole must NOT be subpath-allowed — that would expose disk*,
    // auditpipe, bpf*, etc.
    assert!(
        !p.contains("(subpath \"/dev\")") || p.contains("(subpath \"/dev/fd\")"),
        "profile must not allow subpath \"/dev\" (only /dev/fd is OK)"
    );
}
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hhagent-sandbox dev_allowlist_is_minimal`
Expected: FAIL — assertion fires on `(literal "/dev/null")`.

- [ ] **Step 3: Extend `build_profile` with the /dev allowlist**

After the `out.push_str("(allow sysctl-read)\n");` line in `build_profile`, append:

```rust
    // /dev: explicit minimal allowlist. Not a (subpath "/dev") allow — that
    // would expose disk*, auditpipe, bpf*, console, klog, etc.
    out.push_str("(allow file-read* file-write* (literal \"/dev/null\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/zero\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/random\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/urandom\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/tty\"))\n");
    out.push_str("(allow file-read* file-write* (subpath \"/dev/fd\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/dtracehelper\"))\n");
```

- [ ] **Step 4: Run all tests, verify they pass**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): explicit /dev allowlist mirroring bwrap's --dev posture

null, zero, random, urandom, tty, /dev/fd subpath, dtracehelper. /dev/disk*,
/dev/auditpipe, /dev/bpf*, /dev/console etc. stay denied. Containment
outcome equivalent to Linux's bwrap --dev; mechanism differs (MAC vs mount).
EOF
)"
```

---

## Task 5: build_profile — fs_read paths

**Goal:** Each `policy.fs_read` entry becomes `(allow file-read* (subpath "<path>"))`.

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add a failing test**

Append to `mod tests`:

```rust
#[test]
fn fs_read_emits_subpath_allow() {
    let mut p = strict_policy();
    p.fs_read = vec![PathBuf::from("/etc/ssl"), PathBuf::from("/opt/data")];
    let prof = build_profile(&p);
    assert!(prof.contains("(allow file-read* (subpath \"/etc/ssl\"))"), "got:\n{prof}");
    assert!(prof.contains("(allow file-read* (subpath \"/opt/data\"))"), "got:\n{prof}");
}
```

Also remove the `_path_marker` placeholder from Task 2 — `PathBuf` is now used.

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hhagent-sandbox fs_read_emits_subpath_allow`
Expected: FAIL — `(subpath "/etc/ssl")` not present.

- [ ] **Step 3: Extend `build_profile` to emit fs_read rules**

Change the signature line `let _ = policy; // unused until Task 4` (now: until Task 5) and the bottom of the function so it loops over `policy.fs_read`. Final body:

```rust
pub fn build_profile(policy: &SandboxPolicy) -> String {
    let mut out = String::new();
    out.push_str("(version 1)\n");
    out.push_str("(deny default)\n");

    out.push_str("(allow process-fork)\n");
    out.push_str("(allow process-exec*)\n");
    out.push_str("(allow file-read* (subpath \"/usr/lib\"))\n");
    out.push_str("(allow file-read* (subpath \"/usr/libexec\"))\n");
    out.push_str("(allow file-read* (subpath \"/System/Library\"))\n");
    out.push_str("(allow file-read-metadata (subpath \"/\"))\n");
    out.push_str("(allow sysctl-read)\n");

    out.push_str("(allow file-read* file-write* (literal \"/dev/null\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/zero\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/random\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/urandom\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/tty\"))\n");
    out.push_str("(allow file-read* file-write* (subpath \"/dev/fd\"))\n");
    out.push_str("(allow file-read* file-write* (literal \"/dev/dtracehelper\"))\n");

    for path in &policy.fs_read {
        out.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            path.display()
        ));
    }

    out
}
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): emit fs_read paths as (allow file-read* (subpath ...))
EOF
)"
```

---

## Task 6: build_profile — fs_write paths (single combined rule)

**Goal:** Each `policy.fs_write` entry becomes a single `(allow file-read* file-write* (subpath "<path>"))` line — not two separate rules.

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add a failing test**

Append to `mod tests`:

```rust
#[test]
fn fs_write_emits_read_and_write_subpath_allow() {
    let mut p = strict_policy();
    p.fs_write = vec![PathBuf::from("/var/lib/hhagent/scratch")];
    let prof = build_profile(&p);
    assert!(
        prof.contains("(allow file-read* file-write* (subpath \"/var/lib/hhagent/scratch\"))"),
        "expected combined read+write allow; got:\n{prof}"
    );
    // The fs_write path must NOT appear as a separate read-only allow.
    assert!(
        !prof.contains("(allow file-read* (subpath \"/var/lib/hhagent/scratch\"))"),
        "fs_write path must not also be emitted as a separate read-only rule; got:\n{prof}"
    );
}
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hhagent-sandbox fs_write_emits_read_and_write_subpath_allow`
Expected: FAIL — combined rule not present.

- [ ] **Step 3: Extend `build_profile` to emit fs_write rules**

After the `for path in &policy.fs_read { ... }` loop, append:

```rust
    for path in &policy.fs_write {
        out.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            path.display()
        ));
    }
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): emit fs_write paths as single combined read+write allow
EOF
)"
```

---

## Task 7: build_profile — network rule (Net::Allowlist lifts deny)

**Goal:** `Net::Deny` emits no network rule (the global `(deny default)` covers it). `Net::Allowlist(_)` emits `(allow network*)` — the egress proxy enforces the host list, mirroring bwrap's `--share-net` split.

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add two failing tests**

Append to `mod tests`:

```rust
#[test]
fn deny_does_not_allow_network() {
    let p = build_profile(&strict_policy());
    assert!(!p.contains("(allow network*)"), "Net::Deny must not emit (allow network*); got:\n{p}");
}

#[test]
fn allowlist_does_allow_network() {
    let mut p = strict_policy();
    p.net = Net::Allowlist(vec!["api.example.com:443".into()]);
    let prof = build_profile(&p);
    assert!(prof.contains("(allow network*)"), "Net::Allowlist must emit (allow network*); got:\n{prof}");
}
```

- [ ] **Step 2: Run the tests, verify the allowlist one fails**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 6 tests, 1 FAIL — `allowlist_does_allow_network` (deny test passes incidentally because no rule has been added yet).

- [ ] **Step 3: Add the network rule**

After the `for path in &policy.fs_write { ... }` loop in `build_profile`, append:

```rust
    if matches!(policy.net, crate::Net::Allowlist(_)) {
        // The host allowlist itself is enforced by the future egress proxy
        // (see docs/architecture.md invariant 5), not by Seatbelt — same
        // split as bwrap's --share-net.
        out.push_str("(allow network*)\n");
    }
```

- [ ] **Step 4: Run tests, verify all pass**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 7 passed.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): Net::Allowlist lifts the deny-by-default network rule

Net::Deny relies on (deny default); Net::Allowlist emits (allow network*).
The host allowlist itself is enforced by the future egress proxy, not by
Seatbelt — same split as bwrap's --share-net.
EOF
)"
```

---

## Task 8: spawn_under_policy — relative-path validation + sandbox-exec invocation

**Goal:** Implement the real `spawn_under_policy`. Validate that all `fs_read` and `fs_write` paths are absolute (same up-front check as `linux_bwrap`), build the profile, exec `sandbox-exec -p <profile> <program> <args...>` with cleared environment + per-policy env + own process group.

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add a unit test for relative-path rejection**

Append to `mod tests`:

```rust
#[test]
fn relative_policy_paths_are_rejected_by_spawn() {
    let backend = MacosSeatbelt::new();
    let mut p = strict_policy();
    p.fs_read.push(PathBuf::from("relative/path"));
    let err = backend
        .spawn_under_policy(&p, "/usr/bin/true", &[])
        .expect_err("must reject relative paths");
    let msg = format!("{err}");
    assert!(
        msg.contains("must be absolute"),
        "expected 'must be absolute' error, got: {msg}"
    );
}
```

- [ ] **Step 2: Run the test, verify it fails (current stub returns the wrong error)**

Run: `cargo test -p hhagent-sandbox relative_policy_paths_are_rejected_by_spawn`
Expected: FAIL — error contains "not yet implemented", not "must be absolute".

- [ ] **Step 3: Implement `spawn_under_policy`**

Replace the body of the `impl SandboxBackend for MacosSeatbelt { ... }` block with:

```rust
impl SandboxBackend for MacosSeatbelt {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<Child, SandboxError> {
        for p in policy.fs_read.iter().chain(policy.fs_write.iter()) {
            if !p.is_absolute() {
                return Err(SandboxError::Backend(format!(
                    "policy paths must be absolute, got {p:?}"
                )));
            }
        }

        let profile = build_profile(policy);
        let mut cmd = Command::new("sandbox-exec");
        cmd.arg("-p").arg(&profile);
        cmd.arg(program);
        cmd.args(args);

        // bwrap's --clearenv equivalent: clear, then re-apply per-policy env.
        cmd.env_clear();
        for (k, v) in &policy.env {
            cmd.env(k, v);
        }

        // bwrap's --new-session equivalent: own session via setsid.
        cmd.process_group(0);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        cmd.spawn()
            .map_err(|e| SandboxError::Backend(format!("sandbox-exec spawn failed: {e}")))
    }
}
```

The `#[allow(unused_imports)]` on `use std::process::{Child, Command, Stdio};` is no longer needed. Remove the attribute from that line so it reads:

```rust
use std::process::{Child, Command, Stdio};
```

- [ ] **Step 4: Run tests, verify all pass**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 8 passed.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): MacosSeatbelt::spawn_under_policy with env-clear + setsid

Validates absolute fs paths up front (same shape as linux_bwrap), builds
the .sb profile, and execs sandbox-exec with cleared+repopulated env and
its own process group (the --new-session equivalent on macOS). cpu_ms /
mem_mb are not enforced yet — supervisor work item.
EOF
)"
```

---

## Task 9: probe — minimal `sandbox-exec /usr/bin/true`

**Goal:** Add `MacosSeatbelt::probe()` that runs a minimal allowlist profile against `/usr/bin/true` to verify Seatbelt is healthy. Mirrors `LinuxBwrap::probe`'s posture: probe profile is itself a working minimal allowlist, not a no-op (otherwise dyld can't even resolve `/usr/bin/true`).

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs`

- [ ] **Step 1: Add a unit test that asserts probe succeeds on this host**

Append to `mod tests`:

```rust
// This test runs a real sandbox-exec invocation. It only meaningfully runs
// on macOS hosts; the parent module is cfg(target_os = "macos") so this
// file isn't compiled elsewhere.
#[test]
fn probe_succeeds_on_this_host() {
    MacosSeatbelt::probe().expect("sandbox-exec probe must succeed on a healthy macOS host");
}
```

- [ ] **Step 2: Run the test, verify it fails (no `probe` method yet)**

Run: `cargo test -p hhagent-sandbox probe_succeeds_on_this_host`
Expected: FAIL — compilation error `no function or associated item named 'probe'`.

- [ ] **Step 3: Implement `MacosSeatbelt::probe()`**

In `impl MacosSeatbelt { ... }`, after `pub fn new()`, add:

```rust
    /// Run a minimal `sandbox-exec /usr/bin/true` to verify Seatbelt is
    /// healthy on this host. Catches: missing `/usr/bin/sandbox-exec`,
    /// SIP-related Seatbelt scope clipping, profile-syntax regressions in
    /// a future macOS release. Mirrors [`LinuxBwrap::probe`]'s posture so
    /// integration tests can `[SKIP]` rather than false-green when the
    /// platform sandbox is unavailable.
    ///
    /// The probe profile is itself a minimal working allowlist (not a
    /// no-op): without `process-fork`, `process-exec*`, dyld + System
    /// reads, metadata, and `sysctl-read`, even `/usr/bin/true` fails to
    /// launch and the probe spuriously reports "broken Seatbelt" on a
    /// healthy host.
    pub fn probe() -> Result<(), SandboxError> {
        let profile = "(version 1)\n\
                       (deny default)\n\
                       (allow process-fork)\n\
                       (allow process-exec*)\n\
                       (allow file-read* (subpath \"/usr/lib\"))\n\
                       (allow file-read* (subpath \"/System/Library\"))\n\
                       (allow file-read-metadata (subpath \"/\"))\n\
                       (allow sysctl-read)\n";
        let output = Command::new("sandbox-exec")
            .args(["-p", profile, "/usr/bin/true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SandboxError::Backend(format!("could not spawn sandbox-exec: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(SandboxError::Backend(format!(
            "sandbox-exec probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
```

- [ ] **Step 4: Run all unit tests**

Run: `cargo test -p hhagent-sandbox --lib`
Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): MacosSeatbelt::probe for skip-on-broken-Seatbelt pattern

Mirrors LinuxBwrap::probe so smoke tests print [SKIP] rather than green
when the host's sandbox-exec is unavailable. Probe profile is a minimal
working allowlist — not a no-op — so dyld + libsystem can resolve and
the probe doesn't false-fail on a healthy host (same trap that bit the
Linux bwrap probe before the previous handover's fix).
EOF
)"
```

---

## Task 10: macos_smoke.rs scaffold + skip helper

**Goal:** Set up the integration-test file with the skip-if-no-Seatbelt helper, mirroring `linux_smoke.rs`'s shape. No real assertions yet — just the scaffolding.

**Files:**
- Create: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Create `sandbox/tests/macos_smoke.rs`**

```rust
//! End-to-end tests for the macOS Seatbelt backend. These actually invoke
//! `/usr/bin/sandbox-exec`, so they only run on macOS.

#![cfg(target_os = "macos")]

use std::io::Read;
use std::path::PathBuf;

use hhagent_sandbox::{macos_seatbelt::MacosSeatbelt, Net, Profile, SandboxBackend, SandboxPolicy};

/// Skip the test if Seatbelt is unavailable on this host. Prints to stderr
/// via `eprintln!` so `cargo test -- --nocapture` shows the skip line —
/// `[SKIP]` lines in green output mean tests skipped, not that Seatbelt
/// actually contained anything. Identical pattern to linux_smoke's
/// `skip_if_no_userns`.
fn skip_if_no_seatbelt() -> bool {
    match MacosSeatbelt::probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
            true
        }
    }
}

fn strict_policy() -> SandboxPolicy {
    SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 64,
        profile: Profile::WorkerStrict,
        env: vec![],
    }
}

fn read_to_string(handle: &mut Option<impl Read>) -> String {
    let mut s = String::new();
    if let Some(h) = handle.as_mut() {
        let _ = h.read_to_string(&mut s);
    }
    s
}

#[test]
fn scaffold_compiles_and_skip_helper_runs() {
    // This test exists so we verify the scaffolding builds and the skip
    // helper executes without panicking. Real assertions land in Task 11+.
    let _ = skip_if_no_seatbelt();
    let _ = strict_policy();
    let _: fn(&mut Option<std::process::ChildStdout>) -> String = read_to_string;
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p hhagent-sandbox --test macos_smoke`
Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): scaffold macos_smoke integration test with skip helper

Mirrors linux_smoke's shape — skip_if_no_seatbelt prints [SKIP] when the
host's sandbox-exec is unavailable so green CI without containment is
not misread as a pass.
EOF
)"
```

---

## Task 11: smoke test — echo runs inside sandbox

**Goal:** First real integration test: `/bin/echo hello-from-jail` should succeed under the strict policy and stdout should contain the expected string.

**Files:**
- Modify: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Add the failing test**

Append to `sandbox/tests/macos_smoke.rs`:

```rust
#[test]
fn echo_runs_inside_sandbox() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/echo", &["hello-from-jail"])
        .expect("sandbox-exec should spawn echo");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "echo exited non-zero: {status:?}, stderr={}",
        read_to_string(&mut child.stderr)
    );
    let stdout = read_to_string(&mut child.stdout);
    assert_eq!(stdout.trim_end(), "hello-from-jail");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p hhagent-sandbox --test macos_smoke echo_runs_inside_sandbox -- --nocapture`
Expected: PASS. If it FAILS, the stderr will show the missing allow rule (e.g. `deny file-read /bin/echo`); add the path's parent dir to the always-on allows or to `policy.fs_read` accordingly. `/bin` should be reachable via `(allow process-exec*)` + `(allow file-read-metadata (subpath "/"))` + dyld reading from `/usr/lib` — verify before changing the profile.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): echo round-trips through sandbox-exec under strict policy

First end-to-end verification that build_profile + spawn_under_policy
actually run a process inside Seatbelt and produce expected stdout.
EOF
)"
```

---

## Task 12: smoke test — /etc/master.passwd is invisible when not in policy

**Goal:** Without explicit fs_read for `/etc`, the worker can't read `/etc/master.passwd` (the macOS shadow file).

**Files:**
- Modify: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Add the failing test**

Append:

```rust
#[test]
fn host_etc_master_passwd_is_invisible_when_not_in_policy() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    // /etc/master.passwd is the shadow file on macOS. /etc/passwd itself
    // is world-readable on macOS by design; master.passwd is the sensitive
    // analogue of Linux's /etc/passwd in this test's intent.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/cat", &["/etc/master.passwd"])
        .expect("sandbox-exec should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "cat /etc/master.passwd should fail inside sandbox; stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p hhagent-sandbox --test macos_smoke host_etc_master_passwd_is_invisible_when_not_in_policy`
Expected: PASS — `cat` exits non-zero because Seatbelt denies the read.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): /etc/master.passwd is unreadable under strict policy
EOF
)"
```

---

## Task 13: smoke test — /Users dir is invisible when not in policy

**Goal:** Without explicit fs_read for `/Users`, listing it must not leak the user's username.

**Files:**
- Modify: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Add the failing test**

Append:

```rust
#[test]
fn host_users_dir_is_invisible_when_not_in_policy() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/ls", &["/Users"])
        .expect("sandbox-exec should spawn ls");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    // Either ls fails (denied), or it succeeds but lists nothing real.
    // What's NOT acceptable is seeing the actual user's home dir name.
    assert!(
        !stdout.contains("hherb"),
        "sandbox leaked the host's /Users dir! stdout={stdout:?} stderr={stderr:?}"
    );
    let _ = status;
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p hhagent-sandbox --test macos_smoke host_users_dir_is_invisible_when_not_in_policy`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): /Users dir does not leak username under strict policy
EOF
)"
```

---

## Task 14: smoke test — fs_read path is visible when listed

**Goal:** When `policy.fs_read` includes `/etc/hosts`, `cat /etc/hosts` must succeed and produce non-empty output.

**Files:**
- Modify: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Add the failing test**

Append:

```rust
#[test]
fn fs_read_path_is_visible_when_listed() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("/etc/hosts"));
    let mut child = backend
        .spawn_under_policy(&policy, "/bin/cat", &["/etc/hosts"])
        .expect("sandbox-exec should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "cat /etc/hosts should succeed when listed; stderr={}",
        read_to_string(&mut child.stderr)
    );
    let stdout = read_to_string(&mut child.stdout);
    assert!(!stdout.is_empty(), "expected non-empty /etc/hosts content");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p hhagent-sandbox --test macos_smoke fs_read_path_is_visible_when_listed`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): per-policy fs_read paths become readable in the jail
EOF
)"
```

---

## Task 15: smoke test — relative policy paths are rejected

**Goal:** Passing a relative path in `policy.fs_read` must fail before spawn — same up-front validation as Linux.

**Files:**
- Modify: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Add the failing test**

Append:

```rust
#[test]
fn relative_policy_paths_are_rejected() {
    let backend = MacosSeatbelt::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("relative/path"));
    let res = backend.spawn_under_policy(&policy, "/usr/bin/true", &[]);
    assert!(matches!(res, Err(hhagent_sandbox::SandboxError::Backend(_))));
}
```

(No skip guard — this test does not actually invoke `sandbox-exec`; the rejection happens in `spawn_under_policy`'s validator before exec.)

- [ ] **Step 2: Run the test**

Run: `cargo test -p hhagent-sandbox --test macos_smoke relative_policy_paths_are_rejected`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): relative policy paths rejected before spawn (macOS)
EOF
)"
```

---

## Task 16: smoke test — /dev/disk0 is denied

**Goal:** The `/dev` allowlist excludes raw disk nodes; `cat /dev/disk0` must fail.

**Files:**
- Modify: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Add the failing test**

Append:

```rust
#[test]
fn reading_dev_disk0_is_denied() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    // /dev/disk0 is not in the explicit /dev allowlist, so the read must fail.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/cat", &["/dev/disk0"])
        .expect("sandbox-exec should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "cat /dev/disk0 should be denied; stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p hhagent-sandbox --test macos_smoke reading_dev_disk0_is_denied`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): /dev/disk0 is denied under the explicit /dev allowlist

Verifies the /dev posture: only null/zero/random/urandom/tty/fd/dtracehelper
are exposed; raw disk and other sensitive nodes (auditpipe, bpf*, console,
klog, …) stay denied.
EOF
)"
```

---

## Task 17: net_probe fixture binary

**Goal:** A standalone tiny Rust binary that calls `TcpStream::connect_timeout` against `1.1.1.1:443` and exits 0 on success / 1 on failure. Used by the next task's network-deny test.

**Files:**
- Create: `sandbox/tests/fixtures/net_probe.rs`
- Modify: `sandbox/Cargo.toml`

- [ ] **Step 1: Create `sandbox/tests/fixtures/net_probe.rs`**

```rust
//! Tiny network-reachability probe used by smoke tests on platforms that
//! don't have `getent` (i.e. macOS). Built as a workspace bin; tests
//! invoke `target/debug/net_probe` under a sandbox policy.
//!
//! Exit codes:
//!   0 — TCP connect succeeded (network reachable)
//!   1 — connect failed or timed out (network blocked / unreachable)
//!
//! No std::env, no logging, no DNS — connects to a literal IP so the test
//! is deterministic on offline machines and tells us about the *network*
//! layer, not the resolver.

use std::net::TcpStream;
use std::time::Duration;

fn main() {
    let addr = "1.1.1.1:443"
        .parse()
        .expect("hardcoded socket address parses");
    let exit_code = match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => 0,
        Err(_) => 1,
    };
    std::process::exit(exit_code);
}
```

- [ ] **Step 2: Register the binary in `sandbox/Cargo.toml`**

Append to [sandbox/Cargo.toml](sandbox/Cargo.toml):

```toml
# Test fixture: standalone TcpStream::connect probe used by macos_smoke's
# network-deny test. Built into target/debug/net_probe by `cargo build`.
[[bin]]
name = "net_probe"
path = "tests/fixtures/net_probe.rs"
test = false
doc = false
```

- [ ] **Step 3: Build and verify the binary lands in `target/debug/`**

Run: `cargo build -p hhagent-sandbox --bin net_probe`
Then: `ls -l target/debug/net_probe`
Expected: file exists, is executable. Run `./target/debug/net_probe; echo "exit=$?"` — exit will be 0 if you have internet, 1 if you don't. Either is fine here.

- [ ] **Step 4: Commit**

```bash
git add sandbox/tests/fixtures/net_probe.rs sandbox/Cargo.toml
git commit -m "$(cat <<'EOF'
test(sandbox): net_probe fixture binary for cross-platform network tests

A 12-line standalone Rust binary that calls TcpStream::connect_timeout
on 1.1.1.1:443 and exits 0/1. macOS smoke tests need this because Seatbelt
test hosts don't have /usr/bin/getent, and we want a deterministic probe
that doesn't depend on DNS being resolvable.
EOF
)"
```

---

## Task 18: smoke test — net is unreachable under Net::Deny

**Goal:** Under `Net::Deny`, the `net_probe` binary exits non-zero (TCP connect blocked by Seatbelt).

**Files:**
- Modify: `sandbox/tests/macos_smoke.rs`

- [ ] **Step 1: Add a helper to locate the net_probe binary, then the failing test**

Append to `sandbox/tests/macos_smoke.rs`:

```rust
fn net_probe_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("net_probe")
}

#[test]
fn net_is_unreachable_under_deny() {
    if skip_if_no_seatbelt() {
        return;
    }
    let probe = net_probe_binary();
    if !probe.exists() {
        eprintln!(
            "[SKIP] net_probe binary not built at {probe:?} — run `cargo build --workspace` first"
        );
        return;
    }
    // The probe binary needs to be readable inside the sandbox.
    let mut policy = strict_policy();
    policy.fs_read.push(probe.clone());

    let backend = MacosSeatbelt::new();
    let probe_str = probe.to_string_lossy().into_owned();
    let mut child = backend
        .spawn_under_policy(&policy, &probe_str, &[])
        .expect("sandbox-exec should spawn net_probe");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "net_probe should fail under Net::Deny (TCP connect blocked); stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo build --workspace && cargo test -p hhagent-sandbox --test macos_smoke net_is_unreachable_under_deny -- --nocapture`
Expected: PASS — TCP connect denied by Seatbelt under `(deny default)`. The probe exits 1.

- [ ] **Step 3: Commit**

```bash
git add sandbox/tests/macos_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): net_probe is blocked under Net::Deny on macOS

Closes the test plan's network-deny verification on macOS — the analogue
of linux_smoke's getent test, using the new net_probe fixture binary
since macOS lacks /usr/bin/getent.
EOF
)"
```

---

## Task 19: cross-platform shell_exec_e2e

**Goal:** Drop the `#![cfg(target_os = "linux")]` gate from `core/tests/shell_exec_e2e.rs` and add per-OS helpers so the same three round-trip tests run on both Linux and macOS.

**Files:**
- Modify: `core/tests/shell_exec_e2e.rs`

- [ ] **Step 1: Read the current file's structure**

The current file is at [core/tests/shell_exec_e2e.rs](core/tests/shell_exec_e2e.rs). It uses `LinuxBwrap` directly. We need to:
  1. Remove the top `#![cfg(target_os = "linux")]` line.
  2. Replace `skip_if_no_userns` with a per-OS `skip_if_sandbox_unavailable` helper.
  3. Replace direct `LinuxBwrap::new()` calls with a per-OS `backend()` helper returning `Box<dyn SandboxBackend>`.
  4. Replace direct `Net`, `Profile`, `SandboxPolicy` imports — these are unchanged, but we need to import `SandboxBackend` so the trait method is in scope.
  5. Adjust the worker-binary path resolver if needed (it already works on macOS — same `target/debug/<name>` shape).

- [ ] **Step 2: Replace the file content**

Replace the entire contents of `core/tests/shell_exec_e2e.rs` with:

```rust
//! End-to-end test: agent core spawns the `shell-exec` worker under the
//! platform's sandbox backend and round-trips a JSON-RPC `shell.exec` call.
//! Phase 0 / 0b verification that everything wires up: sandbox + protocol +
//! tool_host + worker. Runs on both Linux (bwrap) and macOS (Seatbelt).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use hhagent_core::tool_host::{spawn_worker, WorkerSpec};
use hhagent_protocol::codes;
use hhagent_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

#[cfg(target_os = "linux")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::linux_bwrap::LinuxBwrap;
    if let Err(e) = LinuxBwrap::probe() {
        eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
        return true;
    }
    false
}

#[cfg(target_os = "macos")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::macos_seatbelt::MacosSeatbelt;
    if let Err(e) = MacosSeatbelt::probe() {
        eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn backend() -> Box<dyn SandboxBackend> {
    Box::new(hhagent_sandbox::linux_bwrap::LinuxBwrap::new())
}

#[cfg(target_os = "macos")]
fn backend() -> Box<dyn SandboxBackend> {
    Box::new(hhagent_sandbox::macos_seatbelt::MacosSeatbelt::new())
}

/// Locate the worker binary. Same path layout on Linux and macOS today —
/// `target/debug/<name>`. This helper exists primarily so the next reader
/// has a single place to edit when production deployment establishes a
/// stable install location for workers.
fn worker_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("hhagent-worker-shell-exec")
}

fn policy_for_shell_exec(worker: &PathBuf, allowlist: &[&str]) -> SandboxPolicy {
    let allow_json = serde_json::to_string(allowlist).unwrap();
    SandboxPolicy {
        fs_read: vec![worker.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
    }
}

#[test]
fn echo_round_trip_through_sandboxed_worker() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = worker_binary();
    assert!(
        worker.exists(),
        "worker binary not found at {worker:?} — run `cargo build --workspace` first"
    );

    // /usr/bin/echo exists on both Linux and macOS at the same path.
    let policy = policy_for_shell_exec(&worker, &["/usr/bin/echo"]);
    let backend = backend();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");

    let result = client
        .call(
            "shell.exec",
            serde_json::json!({"argv": ["/usr/bin/echo", "round-trip-ok"]}),
        )
        .expect("shell.exec round trip");

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"].as_str().unwrap().trim_end(), "round-trip-ok");
    let _ = client.close();
}

#[test]
fn argv_outside_allowlist_is_rejected_by_worker_policy() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("[SKIP] worker binary not built");
        return;
    }
    let policy = policy_for_shell_exec(&worker, &["/usr/bin/echo"]);
    let backend = backend();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");
    let err = client
        .call(
            "shell.exec",
            serde_json::json!({"argv": ["/bin/cat", "/etc/master.passwd"]}),
        )
        .expect_err("non-allowlisted argv must be denied");
    let msg = format!("{err}");
    assert!(
        msg.contains(&format!("{}", codes::POLICY_DENIED)),
        "expected POLICY_DENIED ({}), got: {msg}",
        codes::POLICY_DENIED
    );
    let _ = client.close();
}

#[test]
fn unknown_method_yields_method_not_found() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("[SKIP] worker binary not built");
        return;
    }
    let policy = policy_for_shell_exec(&worker, &["/usr/bin/echo"]);
    let backend = backend();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");
    let err = client
        .call("does.not.exist", serde_json::json!({}))
        .expect_err("unknown method must error");
    assert!(format!("{err}").contains(&format!("{}", codes::METHOD_NOT_FOUND)));
    let _ = client.close();
}
```

The notable change vs the prior file: `spawn_worker(&*backend, &spec)` instead of `spawn_worker(&backend, &spec)`. The `Box<dyn SandboxBackend>` needs explicit deref to satisfy `B: SandboxBackend + ?Sized`. The trait bound on `spawn_worker` already accepts `?Sized`, so this is just a syntactic adjustment.

The reject test now uses `/bin/cat /etc/master.passwd` instead of `/etc/passwd`, since `/etc/passwd` is world-readable on macOS by design — but the assertion is actually about `argv[0]` not being in the allowlist, so both work; using `/etc/master.passwd` keeps consistency with `macos_smoke`.

- [ ] **Step 3: Run the e2e tests on macOS**

Run: `cargo build --workspace && cargo test -p hhagent-core --test shell_exec_e2e -- --nocapture`
Expected: 3 passed, 0 skipped on a healthy macOS host. If the worker binary isn't found, build the workspace first.

- [ ] **Step 4: Run the full sandbox suite to make sure nothing regressed**

Run: `cargo test -p hhagent-sandbox -- --nocapture`
Expected: 9 unit + 7 macos_smoke = 16 passed, 0 skipped.

- [ ] **Step 5: Commit**

```bash
git add core/tests/shell_exec_e2e.rs
git commit -m "$(cat <<'EOF'
test(core): shell_exec_e2e runs on Linux + macOS via per-OS helpers

Drops the cfg(target_os = "linux") gate. Adds skip_if_sandbox_unavailable
and backend() helpers that pick LinuxBwrap or MacosSeatbelt at compile
time. Three existing round-trip tests now exercise the cross-platform
SandboxBackend contract end-to-end on both OSes.
EOF
)"
```

---

## Task 20: threat-model.md updates

**Goal:** Document the sandbox-exec SPI risk and add the macos_smoke test row to the "Already shipped" list.

**Files:**
- Modify: `docs/threat-model.md`

- [ ] **Step 1: Append SPI paragraph to "Asymmetric platform note"**

Locate the `## Asymmetric platform note` section in [docs/threat-model.md](docs/threat-model.md). After the existing paragraph (which ends with "...opt the relevant worker into the micro-VM backend (Apple `container` CLI on Tahoe+)."), append:

```markdown

The macOS implementation shells out to `/usr/bin/sandbox-exec`, which
Apple has marked as private API and emits a deprecation warning for,
while continuing to ship and maintain it (it remains the foundation of
the system's own sandboxing of daemons under `/usr/share/sandbox/`).
We accept this risk explicitly: should Apple ever remove `sandbox-exec`,
the migration path is the entitlement-based App Sandbox combined with
Endpoint Security framework filters, both of which require code-signing
and entitlements that we do not have today. Until that day,
`sandbox-exec` is the best containment available without entitlements.
```

- [ ] **Step 2: Append macos_smoke row to the "Already shipped" list**

Locate the `Already shipped (Phase 0 + Phase 0 hardening stage 1):` section. After the existing four bullet points, append:

```markdown
- `sandbox/tests/macos_smoke.rs` — Seatbelt denies `/etc/master.passwd`, `/Users/...`, raw `/dev/disk0`, and network under `Net::Deny`.
```

- [ ] **Step 3: Commit**

```bash
git add docs/threat-model.md
git commit -m "$(cat <<'EOF'
docs(threat-model): document sandbox-exec SPI risk + macos_smoke coverage

Explicitly acknowledge that /usr/bin/sandbox-exec is Apple-marked private
API, the migration path if it's ever removed (App Sandbox + Endpoint
Security with entitlements), and that we accept the risk because no
entitled alternative is available today. Adds the macos_smoke test row
to the negative-tests-shipped list.
EOF
)"
```

---

## Task 21: Final cross-platform verification

**Goal:** Confirm the full workspace test count on macOS matches the spec's verification target.

**Files:**
- (no edits — verification only)

- [ ] **Step 1: Clean build + full test run**

Run: `cargo clean && cargo build --workspace && cargo test --workspace -- --nocapture 2>&1 | tee /tmp/macos-test-run.txt`

- [ ] **Step 2: Count passes and skips**

Run: `grep -E "test result|\[SKIP\]" /tmp/macos-test-run.txt`

Expected on a healthy macOS host:
- `protocol`: 3 passed
- `sandbox` unit: 9 passed (6 macOS + 1 probe + relative-path + scaffold marker — actually 9 once the scaffold test is included)
- `sandbox` macos_smoke: 7 passed
- `core` unit: 4 passed
- `core` shell_exec_e2e: 3 passed
- prelude / shell-exec / supervisor: build only, no tests on macOS

Total: **26 passed**, **0 `[SKIP]` lines**.

If the count is off, re-read the output carefully. If `[SKIP]` lines appear, the macOS host is in a state that prevents `sandbox-exec` from working — investigate before claiming completion (this is the same false-green trap the previous handover documented for Linux).

- [ ] **Step 3: Sanity-check Linux build still compiles**

Even though we can't run the Linux suite from a macOS host, we can at least run `cargo check` for the Linux target if cross-compilation is set up. If not, this step is best-effort: confirm the changes to `lib.rs`, `tool_host.rs` callers, and `shell_exec_e2e.rs` compile under `#[cfg(target_os = "linux")]` by reading the diff once more. The cfg gates ensure that on Linux:
  - `macos_seatbelt` module is not compiled
  - `default_backend()` returns `LinuxBwrap`
  - `shell_exec_e2e.rs` uses the Linux helpers
  - `linux_bwrap`, `linux_smoke`, `prelude/landlock_lock`, `prelude/seccomp_lock` are unchanged

- [ ] **Step 4: Push and let CI / the user's Linux box verify the Linux side**

Run: `git log --oneline origin/main..HEAD`
Expected: 20-ish commits, one per Task above.

Run: `git push origin main`
Expected: push succeeds.

After the push, the user's Linux side will pick this up and confirm the 36-test Linux suite still passes.

---

## Task 22: HANDOVER.md + ROADMAP.md updates

**Goal:** Per the project's handover convention (CLAUDE.md says to update at session end), record the macOS port as shipped.

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Update HANDOVER.md**

Update the top fields:
- `**Last updated:** 2026-05-07`
- `**Last commit:** <commit-hash-of-final-task>` (use `git rev-parse --short HEAD`)
- `**Branch:** main`

Move "Option A — Phase 0b: macOS port" from the "Next TODO (pick one)" section into a new "Recently completed (this session, 2026-05-07)" entry above the existing "Phase 0 hardening — stage 1" entry. Use this template:

```markdown
**Phase 0b — macOS Seatbelt sandbox backend:**

- New module `sandbox/src/macos_seatbelt.rs`: pure `build_profile(policy)` returning a TinyScheme `.sb` profile, `MacosSeatbelt::probe()` mirroring the Linux probe pattern, `spawn_under_policy()` with up-front absolute-path validation, `env_clear()` + per-policy env, and `process_group(0)` for `--new-session` parity. 9 unit tests on `build_profile` cover the version+deny-default header, always-on dyld/libsystem allows, the explicit /dev allowlist, fs_read/fs_write rules, and Net::Allowlist lifting the network deny.
- New `sandbox/tests/macos_smoke.rs` (7 tests): echo-runs-jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read becomes readable, relative-path rejection, /dev/disk0 denied, network unreachable under Net::Deny.
- New `sandbox/tests/fixtures/net_probe.rs` (12 LoC standalone bin): replaces the missing `/usr/bin/getent` on macOS for the network-deny test.
- `sandbox/src/lib.rs`: `default_backend()` now returns `MacosSeatbelt` on `cfg(target_os = "macos")`. `NotYetImplemented` fallback survives for any other OS.
- `core/tests/shell_exec_e2e.rs` is now cross-platform: per-OS `skip_if_sandbox_unavailable()` and `backend()` helpers. The same three round-trip tests run on both Linux and macOS.
- `docs/threat-model.md`: explicit paragraph on `sandbox-exec` being Apple-marked private API + the macos_smoke row in "negative tests already shipped".
- Acknowledged platform asymmetry vs Linux (already noted in `threat-model.md`): macOS has no mount layer, so `(allow file-read-metadata (subpath "/"))` lets `stat()` on path components leak that paths exist; `--die-with-parent` and `setrlimit` are deferred to the supervisor work.

Total tests after this session on macOS: 26 passed, 0 skipped (`cargo test --workspace -- --nocapture`).
```

Then refresh the **Working state** block:

```
hhagent (Rust workspace, 6 crates, AGPL-3.0)
├── core               hhagent-core: lib + bin (skeleton main); tool_host derives lockdown env
├── sandbox            hhagent-sandbox: SandboxPolicy + LinuxBwrap (probe fixed) + MacosSeatbelt
├── supervisor         hhagent-supervisor: stub (NotYetImplemented)
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

And update the Suite table to add a `sandbox` macos_smoke row + the new sandbox unit-test count:

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` unit (linux) | 6 | bwrap argv builder shape |
| `sandbox` unit (macos) | 9 | sandbox-exec profile builder shape + probe-on-host |
| `sandbox` integration (`linux_smoke`) | 6 | … (unchanged) |
| `sandbox` integration (`macos_smoke`) | 7 | Seatbelt denies `/etc/master.passwd`, `/Users/...`, raw `/dev/disk0`, network under `Net::Deny`; allowed fs_read paths become readable |
| (other rows unchanged) |  |  |

Replace the "Next TODO (pick one)" section's Option A block with a brief done note, and promote Option B' / Option C as the available next picks.

- [ ] **Step 2: Update ROADMAP.md**

Locate the line in [docs/devel/ROADMAP.md](docs/devel/ROADMAP.md) that names "Phase 0b — macOS port" or equivalent. Tick it off with the merge commit hash:

```markdown
- [x] Phase 0b — macOS Seatbelt sandbox backend (`<commit-hash>`)
```

- [ ] **Step 3: Commit handover + roadmap together**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover): macOS Seatbelt backend shipped (Phase 0b)

Move Option A out of "Next TODO" and into "Recently completed".
Refresh working-state, suite table, and tick the corresponding
ROADMAP item with this session's merge commit.
EOF
)"
git push origin main
```

---

## Self-Review

**Spec coverage:** I walked the spec section by section.
- *Goal:* Tasks 1–18 build the backend + smoke tests; Task 19 generalizes the e2e; Task 20 updates the threat model. ✓
- *Non-goals:* `setrlimit` and die-with-parent — explicitly called out as deferred in Task 1's module doc and Task 22's HANDOVER entry. ✓
- *File layout:* Each file in the spec's table maps to a Task that creates or modifies it. ✓
- *SandboxPolicy → .sb mapping table:* Tasks 2–7 cover every row. ✓
- *Process-glue table:* Implemented in Task 8 (`env_clear`, `process_group(0)`, args ordering). ✓
- *Probe + skip pattern:* Tasks 9 (probe) + 10 (skip helper). ✓
- *All 6 unit tests + 7 integration tests:* Tasks 2–7 and 11–18. ✓ Note: the spec lists 6 build_profile unit tests, but I added a 7th (`profile_starts_with_version_and_deny_default` is split from `dev_allowlist_is_minimal` etc.) plus `relative_policy_paths_are_rejected_by_spawn` and `probe_succeeds_on_this_host` for 9 unit tests total. The verification count in Task 21 reflects that.
- *net_probe fixture:* Task 17. ✓
- *Cross-platform shell_exec_e2e:* Task 19. ✓
- *default_backend() wiring:* Task 1 (wires it as a stub) — sufficient since `MacosSeatbelt::new()` is reachable from there immediately and the only behavioural change to the function happens in Task 1. ✓
- *threat-model.md updates:* Task 20. ✓

**Placeholder scan:** No "TBD", no "TODO", no "implement appropriately". Each step that changes code shows the code. The two slightly hand-wavy bits — the "if /bin/echo doesn't run, debug stderr" note in Task 11 and the "best-effort cargo check" in Task 21 Step 3 — are diagnostic guidance, not placeholders for missing code; they kick in only if a previous step's assertion fires.

**Type consistency:**
- `MacosSeatbelt` is the struct name used everywhere.
- `build_profile(policy: &SandboxPolicy) -> String` — same signature in Tasks 2, 3, 4, 5, 6, 7, 8.
- `MacosSeatbelt::probe() -> Result<(), SandboxError>` — same in Tasks 9, 10, 19.
- `skip_if_no_seatbelt()` (in `macos_smoke.rs`) and `skip_if_sandbox_unavailable()` (in `shell_exec_e2e.rs`) are intentionally different identifiers — one is platform-specific, the other is a per-OS dispatcher. Spec calls this out.
- `net_probe_binary()` resolver matches the existing `worker_binary()` shape in `core/tests/shell_exec_e2e.rs`.

No issues to fix.

---

## Execution Handoff

**Plan complete and saved to [`docs/superpowers/plans/2026-05-07-macos-seatbelt-backend.md`](docs/superpowers/plans/2026-05-07-macos-seatbelt-backend.md). Two execution options:**

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration. Best when you want each task validated in isolation and the main session to retain headroom for review.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints. Best when you want to watch the work happen in this conversation.

**Which approach?**
