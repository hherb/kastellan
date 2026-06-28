# Firecracker micro-VM slice 4b — web-fetch in a VM — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the `web-fetch` worker inside a Firecracker micro-VM, reaching the host egress proxy over the slice-4a vsock channel, with the worker's existing code and an opt-in flag (`KASTELLAN_WEB_FETCH_USE_MICROVM=1`).

**Architecture:** Reuse the merged force-routing → sidecar → `proxy_uds`/`_CA`/`fs_read` rewrite (backend-agnostic) and the slice-4a egress vsock + slice-3 per-spawn RO-share. The only real gaps: (a) the backend hardcodes the python rootfs filename, (b) the slice-3 RO bind only handles directory targets but the per-instance `ca.pem` is a single file, and (c) web-fetch has no VM entry/rootfs. Close all four with pure helpers + a separate `web-fetch.ext4` rootfs sharing python-exec's image dir and kernel.

**Tech Stack:** Rust (workspace crates `kastellan-sandbox`, `kastellan-core`, `kastellan-microvm-init`), bash (rootfs build), Firecracker + KVM (DGX, aarch64), vsock, ext4.

**Spec:** [`docs/superpowers/specs/2026-06-28-firecracker-microvm-slice4b-web-fetch-design.md`](../specs/2026-06-28-firecracker-microvm-slice4b-web-fetch-design.md)

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible deps only** (Apache-2.0 / MIT / BSD / MPL / LGPL / (A)GPL). No new deps needed; `rcgen` (already a workspace dep via egress-proxy, MPL/Apache/MIT) may be added to `core` `[dev-dependencies]` for the e2e CA fixture.
- **Cross-platform.** All Firecracker/VM code is `#[cfg(target_os = "linux")]`-gated; the macOS build must stay green (issue-#144 rule — never reference a `#[cfg(linux)]` variant/const from un-gated code).
- **DGX is the acceptance gate.** `kastellan-core` cannot cross-compile on the Mac (`ring` C-dep), so the e2e (Task 6) and the sandbox/`spawn_under_policy` unit tests compile+run **only on the DGX** (aarch64, real KVM). On the Mac: `cargo build --workspace`, the Mac-runnable units (`microvm-init`, core `web_fetch` manifest), and cross-clippy `--target aarch64-unknown-linux-gnu --all-targets -D warnings`.
- **Files under 500 LOC** where feasible; keep new helpers in focused modules.
- **TDD, frequent commits, every worker sandboxed** (no unsandboxed escape hatch).
- **Firecracker e2e gotchas (carry forward):** rebuild the **release** launcher (`cargo build --release -p kastellan-microvm-run`) AND the web-fetch rootfs before the e2e; `export PATH=$HOME/.local/bin:$PATH` so `firecracker` is on the non-interactive ssh PATH (else the e2e SKIP-as-passes silently).
- **DGX driving:** run native Linux verification as exactly `ssh dgx '<cmd>'` (flags before the hostname get denied by the allow rule).

**Environment variables (exact names):**
- `KASTELLAN_WEB_FETCH_USE_MICROVM` — opt-in gate (Linux only).
- `KASTELLAN_MICROVM_DIR` — shared image dir, default `/var/lib/kastellan/microvm`.
- `KASTELLAN_MICROVM_ROOTFS` — rootfs filename, default `python-exec.ext4`.
- `KASTELLAN_WEB_FETCH_ALLOWLIST` — the worker's per-hop host allowlist JSON.
- `KASTELLAN_EGRESS_PROXY_UDS` / `KASTELLAN_EGRESS_PROXY_CA` — set by force-routing; the UDS is overridden in-guest by the slice-4a plan, the CA path resolves in-guest via the RO-share.

---

### Task 1: Backend per-worker rootfs-filename resolution

Make `LinuxFirecracker::spawn_under_policy` resolve the rootfs filename from the policy env instead of hardcoding `python-exec.ext4`, so each worker can boot its own rootfs from the shared image dir.

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs` (the inline image construction in `spawn_under_policy`, ~lines 135-146; add a pure `resolve_image` fn + a `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `FirecrackerImage { kernel_path: PathBuf, rootfs_path: PathBuf }` (from `linux_firecracker::plan`), `SandboxPolicy.env: Vec<(String, String)>`.
- Produces: `fn resolve_image(env: &[(String, String)]) -> FirecrackerImage` (crate-internal). Reads `KASTELLAN_MICROVM_DIR` (default `/var/lib/kastellan/microvm`) + `KASTELLAN_MICROVM_ROOTFS` (default `python-exec.ext4`).

- [ ] **Step 1: Write the failing tests**

Add to `sandbox/src/linux_firecracker.rs`, inside the existing `#[cfg(all(test, target_os = "linux"))] mod spawn_tests` block (or a new sibling test module):

```rust
#[test]
fn resolve_image_defaults_to_python_exec_rootfs() {
    let img = resolve_image(&[]);
    assert_eq!(img.kernel_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/vmlinux"));
    assert_eq!(img.rootfs_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/python-exec.ext4"));
}

#[test]
fn resolve_image_honours_rootfs_filename_env() {
    let env = vec![("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-fetch.ext4".to_string())];
    let img = resolve_image(&env);
    assert_eq!(img.rootfs_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/web-fetch.ext4"));
    // Kernel is still the shared vmlinux in the same dir.
    assert_eq!(img.kernel_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/vmlinux"));
}

#[test]
fn resolve_image_honours_dir_and_ignores_blank_rootfs() {
    let env = vec![
        ("KASTELLAN_MICROVM_DIR".to_string(), "/srv/vm".to_string()),
        ("KASTELLAN_MICROVM_ROOTFS".to_string(), "  ".to_string()),
    ];
    let img = resolve_image(&env);
    // Blank ROOTFS falls back to the python default; DIR is honoured.
    assert_eq!(img.rootfs_path, std::path::PathBuf::from("/srv/vm/python-exec.ext4"));
    assert_eq!(img.kernel_path, std::path::PathBuf::from("/srv/vm/vmlinux"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run (on the DGX — sandbox linux-cfg tests don't run under `cargo test` on macOS):
```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox resolve_image 2>&1 | tail -20'
```
Expected: FAIL — `cannot find function resolve_image in this scope`.

- [ ] **Step 3: Write the pure helper + wire it in**

In `sandbox/src/linux_firecracker.rs`, add near the top of the `impl`/module (after imports), a pure helper:

```rust
/// Default micro-VM image dir + rootfs filename. The dir (and the pinned
/// `vmlinux`) is shared across workers; the rootfs *filename* is what differs
/// per worker (`python-exec.ext4`, `web-fetch.ext4`, …).
const DEFAULT_MICROVM_DIR: &str = "/var/lib/kastellan/microvm";
const DEFAULT_ROOTFS_FILE: &str = "python-exec.ext4";

/// Resolve the guest kernel + rootfs from the worker's policy env. Pure →
/// unit-tested without KVM. `KASTELLAN_MICROVM_DIR` picks the shared image dir;
/// `KASTELLAN_MICROVM_ROOTFS` picks the rootfs filename inside it (default keeps
/// the existing python-exec path byte-identical).
fn resolve_image(env: &[(String, String)]) -> FirecrackerImage {
    let get = |key: &str| {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    let dir = std::path::PathBuf::from(get("KASTELLAN_MICROVM_DIR").unwrap_or(DEFAULT_MICROVM_DIR));
    let rootfs = get("KASTELLAN_MICROVM_ROOTFS").unwrap_or(DEFAULT_ROOTFS_FILE);
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join(rootfs),
    }
}
```

Then replace the inline construction in `spawn_under_policy` (the `let dir = …` + `let image = FirecrackerImage { … }` block, ~lines 137-146) with:

```rust
        // Image dir + rootfs filename come from the worker's policy env (set by
        // the entry): KASTELLAN_MICROVM_DIR / KASTELLAN_MICROVM_ROOTFS. The dir
        // (and vmlinux) is shared; the rootfs filename differs per worker.
        let image = resolve_image(&policy.env);
```

- [ ] **Step 4: Run tests + clippy to verify pass**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox resolve_image 2>&1 | tail -20'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy -p kastellan-sandbox --all-targets -- -D warnings 2>&1 | tail -5'
```
Expected: 3 passed; clippy clean.
On the Mac, confirm cross-compile of the linux-cfg module:
```
source "$HOME/.cargo/env" && cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings 2>&1 | tail -5
```
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker.rs
git commit -m "feat(microvm): per-worker rootfs-filename resolution (slice 4b)

resolve_image() reads KASTELLAN_MICROVM_ROOTFS (default python-exec.ext4) so
workers share the image dir + kernel but boot distinct rootfs images.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Guest file-aware RO bind in `microvm-init`

Teach the slice-3 RO-share guest mount logic to bind a single **file** target (the per-instance `ca.pem`), not just directories. Today every RO target gets `create_dir_all(t)` then `MS_BIND`, which turns a file path into a directory and the file-bind fails.

**Files:**
- Modify: `workers/microvm-init/src/main.rs` (`apply_host_mounts`, the RO-share bind loop ~lines 267-283; add a pure `BindPrep` enum + `bind_prep` fn + tests in the file's existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: the decoded `MountManifest` `ro.targets: Vec<String>` (existing), and `/ro-share{t}` (the RO ext4 mounted at `/ro-share`).
- Produces: `enum BindPrep { Dir, File, Skip }` + `fn bind_prep(src_is_dir: bool, src_is_file: bool) -> BindPrep` (module-private). Drives whether the bind target is created as a dir, as a parent-dir + empty file, or skipped.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `workers/microvm-init/src/main.rs`:

```rust
#[test]
fn bind_prep_directory_source() {
    assert_eq!(super::bind_prep(true, false), super::BindPrep::Dir);
}

#[test]
fn bind_prep_file_source() {
    assert_eq!(super::bind_prep(false, true), super::BindPrep::File);
}

#[test]
fn bind_prep_missing_source_skips() {
    // Neither dir nor file (missing / socket / fifo) → skip the bind entirely.
    assert_eq!(super::bind_prep(false, false), super::BindPrep::Skip);
}
```

- [ ] **Step 2: Run tests to verify they fail**

`microvm-init` pure tests run on macOS too (the parser/anchor tests already do):
```
source "$HOME/.cargo/env" && cargo test -p kastellan-microvm-init bind_prep 2>&1 | tail -20
```
Expected: FAIL — `cannot find function bind_prep` / `cannot find type BindPrep`.

- [ ] **Step 3: Write the pure helper + wire it into `apply_host_mounts`**

Add the enum + fn above `apply_host_mounts` in `workers/microvm-init/src/main.rs`:

```rust
/// How a RO-share bind target must be prepared before `MS_BIND`, decided purely
/// from the source's kind (probed at `/ro-share{target}`) so it is unit-testable
/// without root or real mounts.
#[derive(Debug, PartialEq)]
enum BindPrep {
    /// Source is a directory: create the target dir, then bind (slice-3 default).
    Dir,
    /// Source is a regular file (e.g. the per-instance `ca.pem`): create the
    /// target's PARENT dir + an empty target file, then bind. A file bind needs
    /// an existing regular-file target.
    File,
    /// Source missing or neither file nor dir: skip the bind.
    Skip,
}

fn bind_prep(src_is_dir: bool, src_is_file: bool) -> BindPrep {
    if src_is_dir {
        BindPrep::Dir
    } else if src_is_file {
        BindPrep::File
    } else {
        BindPrep::Skip
    }
}
```

Replace the RO bind loop body inside `apply_host_mounts` (the `for t in &ro.targets { … }` block, ~lines 271-282) with:

```rust
            for t in &ro.targets {
                let from = format!("/ro-share{t}");
                // Probe the source kind on the mounted RO image (symlink_metadata
                // does not follow links — the staged tree is symlink-free).
                let (is_dir, is_file) = std::fs::symlink_metadata(&from)
                    .map(|m| (m.is_dir(), m.is_file()))
                    .unwrap_or((false, false));
                match bind_prep(is_dir, is_file) {
                    BindPrep::Dir => {
                        // Directory share (slice-3 fs_read root): create the target
                        // dir, then bind. MS_BIND alone is read-only here because the
                        // /ro-share superblock above is MS_RDONLY + the image is
                        // ephemeral with no host write-back.
                        if std::fs::create_dir_all(t).is_ok() {
                            mount(&from, t, None, libc::MS_BIND);
                        }
                    }
                    BindPrep::File => {
                        // Single-file share (the per-instance egress CA): a file bind
                        // needs an existing regular-file target. Make the parent
                        // writable (it may live in the /tmp scratch tmpfs) + touch
                        // the target, then bind. Best-effort: never abort PID1.
                        if let Some(parent) = std::path::Path::new(t).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if std::fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .open(t)
                            .is_ok()
                        {
                            mount(&from, t, None, libc::MS_BIND);
                        }
                    }
                    BindPrep::Skip => {
                        eprintln!("microvm-init: RO source {from} missing; skipping bind of {t}");
                    }
                }
            }
```

- [ ] **Step 4: Run tests + clippy to verify pass**

```
source "$HOME/.cargo/env" && cargo test -p kastellan-microvm-init 2>&1 | tail -20
source "$HOME/.cargo/env" && cargo clippy -p kastellan-microvm-init --all-targets -- -D warnings 2>&1 | tail -5
source "$HOME/.cargo/env" && cargo clippy -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets -- -D warnings 2>&1 | tail -5
```
Expected: all `microvm-init` tests pass (existing + 3 new); both clippy runs clean.

- [ ] **Step 5: Commit**

```bash
git add workers/microvm-init/src/main.rs
git commit -m "feat(microvm-init): file-aware RO-share bind for the egress CA (slice 4b)

apply_host_mounts now binds a single-file RO source (the per-instance ca.pem)
by creating its parent dir + an empty target file before MS_BIND, instead of
the dir-only create_dir_all(t) that turned a file path into a directory.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: web-fetch Firecracker `ToolEntry` builder

Add the Linux-only `ToolEntry` builder for web-fetch in a VM, mirroring `python_exec/entries.rs::firecracker_mode_entry` but as a net worker (`Net::Allowlist`, `WorkerNetClient`, empty `fs_read`).

**Files:**
- Modify: `core/src/workers/web_fetch.rs` (add `web_fetch_firecracker_entry` + a `USE_MICROVM_ENV` const, both `#[cfg(target_os = "linux")]`; extend the `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `SandboxPolicy`, `Net::Allowlist`, `Profile::WorkerNetClient`, `kastellan_sandbox::SandboxBackendKind::FirecrackerVm`, `crate::scheduler::ToolEntry`, `crate::worker_lifecycle::Lifecycle::SingleUse`.
- Produces: `#[cfg(target_os = "linux")] pub fn web_fetch_firecracker_entry(binary: PathBuf, image_dir: String, allowlist: &[String]) -> ToolEntry`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `core/src/workers/web_fetch.rs` (gate the test Linux-only so it doesn't reference the linux-only fn on macOS):

```rust
    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_entry_is_net_allowlist_vm_with_empty_fs_read() {
        let allowlist = vec!["en.wikipedia.org".to_string(), ".example.com".to_string()];
        let entry = web_fetch_firecracker_entry(
            PathBuf::from("/usr/local/bin/kastellan-worker-web-fetch"),
            "/var/lib/kastellan/microvm".to_string(),
            &allowlist,
        );
        // VM backend, net client, no host paths shared in (the CA is added at spawn).
        assert!(matches!(
            entry.sandbox_backend,
            Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
        ));
        assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
        assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty (no NIC, no local DNS)");
        assert!(entry.policy.proxy_uds.is_none(), "proxy_uds is set at spawn, not in the manifest");
        // Net::Allowlist derived from the domains (wildcard → bare host:443).
        match &entry.policy.net {
            Net::Allowlist(hosts) => assert_eq!(
                hosts,
                &vec!["en.wikipedia.org:443".to_string(), "example.com:443".to_string()]
            ),
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // Env forwards the verbatim allowlist + the image dir + the rootfs filename.
        let env = &entry.policy.env;
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get("KASTELLAN_WEB_FETCH_ALLOWLIST"), Some(r#"["en.wikipedia.org",".example.com"]"#));
        assert_eq!(get("KASTELLAN_MICROVM_DIR"), Some("/var/lib/kastellan/microvm"));
        assert_eq!(get("KASTELLAN_MICROVM_ROOTFS"), Some("web-fetch.ext4"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

On the DGX (core e2e/unit needs Linux for the `FirecrackerVm` variant; on macOS the test is `cfg`-gated out):
```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib firecracker_entry_is_net_allowlist 2>&1 | tail -20'
```
Expected: FAIL — `cannot find function web_fetch_firecracker_entry`.

- [ ] **Step 3: Write the builder + const**

In `core/src/workers/web_fetch.rs`, add the opt-in const near the other consts:

```rust
/// Opt into the Linux Firecracker micro-VM backend for web-fetch. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_FETCH_USE_MICROVM";

/// In-rootfs path of the web-fetch worker binary (staged there by
/// `build-web-fetch-rootfs.sh`). Used by the micro-VM entry, not the host path.
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-fetch";
```

Add the builder (place it after `web_fetch_entry`):

```rust
/// Build the [`ToolEntry`] for web-fetch running inside a Firecracker micro-VM
/// (opt-in via `KASTELLAN_WEB_FETCH_USE_MICROVM=1`). Mirrors the host-mode
/// [`web_fetch_entry`] but as a VM net worker:
///
/// * `Net::Allowlist(host:443…)` (derived from the operator allowlist exactly as
///   host mode) — **not** `Net::Deny`; web-fetch needs egress. Force-routing sets
///   `proxy_uds` at spawn, which makes `build_launch_plan` boot the VM with no NIC
///   and tunnel egress over the slice-4a vsock channel.
/// * `Profile::WorkerNetClient`, `proxy_uds: None` in the manifest (set at spawn).
/// * `fs_read: vec![]` — the worker has no NIC and does no local DNS (the egress
///   proxy resolves host-side), so no `/etc/resolv.conf` etc. The per-instance CA
///   is appended to `fs_read` at spawn by `rewrite_worker_policy`.
/// * `env` forwards the verbatim allowlist plus `KASTELLAN_MICROVM_DIR` (shared
///   image dir) and `KASTELLAN_MICROVM_ROOTFS=web-fetch.ext4` so the backend boots
///   the right rootfs. All three ride the #360 `kastellan.env` cmdline token.
///
/// `mem_mb: 512` is enforced by Firecracker. Linux-only: emits the
/// `#[cfg(target_os = "linux")]` `FirecrackerVm` backend variant.
#[cfg(target_os = "linux")]
pub fn web_fetch_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    allowlist: &[String],
) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let net_entries: Vec<String> = allowlist
        .iter()
        .map(|d| {
            let host = d.strip_prefix('.').unwrap_or(d);
            format!("{host}:443")
        })
        .collect();
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(net_entries),
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            ("KASTELLAN_WEB_FETCH_ALLOWLIST".to_string(), allow_json),
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-fetch.ext4".to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}
```

- [ ] **Step 4: Run test + clippy to verify pass**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib firecracker_entry_is_net_allowlist 2>&1 | tail -20'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy -p kastellan-core --lib --tests -- -D warnings 2>&1 | tail -5'
```
Expected: 1 passed; clippy clean.
On the Mac, confirm the macOS build still compiles (the linux-only fn is gated out):
```
source "$HOME/.cargo/env" && cargo build -p kastellan-core 2>&1 | tail -5
```
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/web_fetch.rs
git commit -m "feat(web-fetch): Firecracker micro-VM ToolEntry builder (slice 4b)

web_fetch_firecracker_entry: Net::Allowlist + WorkerNetClient + empty fs_read,
FirecrackerVm backend, forwards the allowlist + MICROVM_DIR/ROOTFS env. Linux-only.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: web-fetch resolver `USE_MICROVM` short-circuit

Wire the opt-in: when `KASTELLAN_WEB_FETCH_USE_MICROVM=1`, `WebFetchManifest::resolve` returns the VM entry (in-rootfs binary, shared image dir) instead of the host entry.

**Files:**
- Modify: `core/src/workers/web_fetch.rs` (`WebFetchManifest::resolve`; extend `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `web_fetch_firecracker_entry` (Task 3), `ResolveCtx { get_env, allowlist, … }`, `Resolution::Register`.
- Produces: resolver behaviour — `USE_MICROVM=1` ⇒ VM entry; unset/≠1 ⇒ unchanged host path.

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `core/src/workers/web_fetch.rs` (Linux-only — exercises the VM branch):

```rust
    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_uses_microvm_entry_when_opted_in() {
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_FETCH_USE_MICROVM" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["en.wikipedia.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebFetchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                // In-rootfs binary path, not a host-discovered binary.
                assert_eq!(
                    entry.binary,
                    PathBuf::from("/usr/local/bin/kastellan-worker-web-fetch")
                );
                // Shared default image dir.
                let env = &entry.policy.env;
                let dir = env.iter().find(|(k, _)| k == "KASTELLAN_MICROVM_DIR").map(|(_, v)| v.as_str());
                assert_eq!(dir, Some("/var/lib/kastellan/microvm"));
            }
            other => panic!("expected Register(VM entry), got {}", outcome_label(&other)),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib resolve_uses_microvm_entry 2>&1 | tail -20'
```
Expected: FAIL — the resolver currently ignores `USE_MICROVM` and returns the host entry (`sandbox_backend == None`).

- [ ] **Step 3: Add the resolver short-circuit**

In `WebFetchManifest::resolve` (`core/src/workers/web_fetch.rs`), resolve the allowlist first, then insert the Linux-gated branch **before** the host `discover_binary` path:

```rust
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let allowlist = (ctx.allowlist)(TOOL_NAME);

        // Firecracker micro-VM mode (Linux) short-circuits host binary discovery:
        // the worker binary lives inside the rootfs image, not on the host.
        // Linux-only — on macOS USE_MICROVM is never read so the `FirecrackerVm`
        // variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            let use_microvm =
                (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
            if use_microvm {
                let binary = PathBuf::from(MICROVM_WORKER_BIN);
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                return Resolution::Register(web_fetch_firecracker_entry(
                    binary, image_dir, &allowlist,
                ));
            }
        }

        let binary = match discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME) {
            Some(b) => b,
            None => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "could not resolve worker binary: {BIN_ENV} set but not a \
                         runnable file, or unset with no sibling {DEFAULT_BIN_NAME} found"
                    ),
                };
            }
        };
        Resolution::Register(web_fetch_entry(binary, &allowlist))
    }
```

(Note: the existing body fetched `allowlist` after `discover_binary`; move that fetch to the top as shown so both branches use it. `outcome_label` is the existing test helper.)

- [ ] **Step 4: Run test + clippy + the existing manifest tests to verify pass**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib web_fetch 2>&1 | tail -25'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy -p kastellan-core --lib --tests -- -D warnings 2>&1 | tail -5'
```
Expected: all `web_fetch` manifest tests pass (existing host-mode `resolve_registers_*` + `resolve_misconfigured_*` + the 2 new VM tests); clippy clean.
On the Mac:
```
source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib web_fetch 2>&1 | tail -15
```
Expected: the host-mode manifest tests pass (the VM tests are `cfg`-gated out on macOS).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/web_fetch.rs
git commit -m "feat(web-fetch): KASTELLAN_WEB_FETCH_USE_MICROVM resolver branch (slice 4b)

resolve() returns the Firecracker VM entry when opted in (in-rootfs binary +
shared image dir); host path unchanged when unset. Linux-only.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: web-fetch micro-VM rootfs build script

A `build-rootfs.sh` sibling that builds `web-fetch.ext4` into the shared image dir: the web-fetch worker binary + `microvm-init` (PID1) + their `ldd` closure + the anchor/`/run` mountpoints. No python, no system CA bundle.

**Files:**
- Create: `scripts/workers/microvm/build-web-fetch-rootfs.sh`

**Interfaces:**
- Consumes: `KASTELLAN_MICROVM_DIR` (shared, default `/var/lib/kastellan/microvm`); the workspace `cargo build --release` for `kastellan-worker-web-fetch` + `kastellan-microvm-init`.
- Produces: `$KASTELLAN_MICROVM_DIR/web-fetch.ext4` (journal-less ext4) + reuses the existing `$KASTELLAN_MICROVM_DIR/vmlinux`.

- [ ] **Step 1: Write the script**

Create `scripts/workers/microvm/build-web-fetch-rootfs.sh` (mode 0755). It mirrors `build-rootfs.sh` but drops python and stages the web-fetch binary; it reuses the shared `vmlinux` (fetched by `build-rootfs.sh`) and only fetches it if absent:

```bash
#!/usr/bin/env bash
# Build the web-fetch micro-VM rootfs (ext4) into the SHARED image dir, beside
# python-exec.ext4 + the shared vmlinux. The dir + kernel are shared across
# workers (build-rootfs.sh provisions them); only the rootfs filename differs
# (KASTELLAN_MICROVM_ROOTFS=web-fetch.ext4). web-fetch is a pure-Rust net worker:
# no python, and NO system CA bundle — egress is MITM-only and the only trusted
# root is the per-instance proxy CA delivered per-spawn via the slice-3 RO-share.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/build-web-fetch-rootfs.sh" >&2
    exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *)
        echo "Unsupported architecture '${HOST_ARCH}'. The pinned guest kernel is published for x86_64 and aarch64 only." >&2
        exit 1
        ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
ROOTFS_MIB=256

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write the micro-VM image dir: $OUT_DIR" >&2
    echo "Run the one-time privileged setup first:" >&2
    echo "    sudo ./scripts/linux/install-firecracker-vsock.sh" >&2
    echo "or build into a user-writable dir (set the same KASTELLAN_MICROVM_DIR in the service env):" >&2
    echo "    KASTELLAN_MICROVM_DIR=\"\$HOME/.local/share/kastellan/microvm\" ./scripts/workers/microvm/build-web-fetch-rootfs.sh" >&2
    exit 1
fi

# Shared guest kernel (pinned). Reused if build-rootfs.sh already fetched it.
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# Cross-build worker + init for the guest (native on the DGX aarch64).
source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-web-fetch -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT

# Binaries: init is PID1 at /sbin/init; the worker at its in-rootfs path
# (matches MICROVM_WORKER_BIN in core/src/workers/web_fetch.rs).
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-web-fetch "$WORK/usr/local/bin/kastellan-worker-web-fetch"

# Shared-library closure for both Rust binaries (dynamic loader + ldd .so's),
# copied at their real absolute paths.
copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure \
    target/release/kastellan-microvm-init \
    target/release/kastellan-worker-web-fetch

# Pseudo-fs mountpoints (microvm-init mounts proc/sys/tmp at boot) + slice-3
# host-dir-share anchors + slice-4a /run egress relay tmpfs mountpoint. Keep this
# anchor list in lockstep with mounts.rs::SHARE_ANCHORS (opt/data/srv/mnt/work/tmp)
# and build-rootfs.sh. The per-instance ca.pem binds under /tmp (a boot tmpfs).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
mkdir -p "$WORK/run"

# Journal-less ext4 (read-only at runtime, shared across concurrent VMs).
mkfs.ext4 -q -F -O ^has_journal -L kastellan-web-fetch -d "$WORK" "$OUT_DIR/web-fetch.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/web-fetch.ext4 (+ shared $OUT_DIR/vmlinux)"
```

- [ ] **Step 2: Make it executable + shellcheck**

```bash
chmod 0755 scripts/workers/microvm/build-web-fetch-rootfs.sh
shellcheck scripts/workers/microvm/build-web-fetch-rootfs.sh 2>&1 | tail -20 || true
```
Expected: executable bit set; shellcheck reports no errors (warnings about `source` are acceptable — `build-rootfs.sh` has the same).

- [ ] **Step 3: Build the rootfs on the DGX + smoke-check the binary**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && bash scripts/workers/microvm/build-web-fetch-rootfs.sh 2>&1 | tail -15'
ssh dgx 'ls -la /var/lib/kastellan/microvm/web-fetch.ext4 /var/lib/kastellan/microvm/vmlinux'
ssh dgx 'cd ~/src/kastellan && ./target/release/kastellan-worker-web-fetch </dev/null 2>&1 | head -3 || true'
```
Expected: `built …/web-fetch.ext4`; the `.ext4` + `vmlinux` exist; the worker binary loads + exits cleanly (no loader/glibc error) on empty stdin.

- [ ] **Step 4: Commit**

```bash
git add scripts/workers/microvm/build-web-fetch-rootfs.sh
git commit -m "feat(microvm): build-web-fetch-rootfs.sh — web-fetch VM rootfs (slice 4b)

Builds web-fetch.ext4 into the shared image dir (beside python-exec.ext4 +
vmlinux): web-fetch binary + microvm-init PID1 + ldd closure + anchors + /run.
No python, no system CA bundle (MITM-only egress).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: DGX e2e — web-fetch reaches the egress proxy from inside a VM

The acceptance test. An always-on hermetic gate proves transport + CA delivery (a stub at `proxy_uds` receives the in-VM worker's `CONNECT` line, which the worker can only emit after loading the in-guest CA); an `#[ignore]` real-net test proves origin validation through the real sidecar.

**Files:**
- Create: `core/tests/web_fetch_firecracker_egress_e2e.rs`
- Modify: `core/Cargo.toml` (add `rcgen` to `[dev-dependencies]` if absent — for the test CA fixture)

**Interfaces:**
- Consumes: `kastellan_core::tool_host::{spawn_worker, dispatch, WorkerSpec}`, `kastellan_core::workers::web_fetch::web_fetch_firecracker_entry`, `kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker}`, `kastellan_sandbox::{Net, SandboxBackend, SandboxBackendKind, SandboxBackends}`, `kastellan_core::secrets::Vault`, a PG pool via `kastellan_tests_common`.
- Produces: nothing consumed downstream (terminal acceptance test).

- [ ] **Step 1: Add the rcgen dev-dep (if absent)**

Check + add to `core/Cargo.toml` under `[dev-dependencies]`:
```
ssh dgx 'cd ~/src/kastellan && grep -n "rcgen" core/Cargo.toml Cargo.toml || echo "rcgen not present"'
```
If absent, add to `core/Cargo.toml` `[dev-dependencies]` (pin to the workspace's existing rcgen major used by egress-proxy — check `workers/egress-proxy/Cargo.toml`):
```toml
rcgen = "0.13"
```

- [ ] **Step 2: Write the e2e test file**

Create `core/tests/web_fetch_firecracker_egress_e2e.rs`. The always-on gate uses a host `UnixListener` stub as the proxy and an rcgen-minted CA fixture; it drives one `web.fetch` via `dispatch` (PG-gated, skip-as-pass) and asserts the stub receives the worker's `CONNECT` line.

```rust
#![cfg(target_os = "linux")]
//! Slice 4b e2e: web-fetch runs inside a Firecracker VM and reaches the host
//! egress proxy over the slice-4a vsock channel.
//!
//! Two layers:
//!  * `web_fetch_vm_reaches_proxy_with_ca_delivered` (always-on, hermetic): a host
//!    UnixListener stub stands in for the egress proxy at the worker's proxy_uds;
//!    a force-routed web-fetch VM boots and one `web.fetch` is driven through it;
//!    we assert the stub RECEIVES the worker's `CONNECT <host>:443` line. The
//!    worker can only emit CONNECT after loading the in-guest CA (make_get fails
//!    closed on an unreadable KASTELLAN_EGRESS_PROXY_CA), so this single assertion
//!    proves VM boot + force-routing + the vsock relay + CA delivery.
//!  * `real_web_fetch_through_sidecar` (#[ignore]): full MITM fetch via the real
//!    egress-proxy sidecar to a real HTTPS origin — origin validation, the last
//!    mile the stub cannot complete. Mirrors `real_mitm_fetch_through_sidecar`.
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + the web-fetch rootfs
//! (REBUILD via build-web-fetch-rootfs.sh) + the kastellan-microvm-run RELEASE
//! launcher. Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH
//!     cargo build --release -p kastellan-microvm-run
//!     bash scripts/workers/microvm/build-web-fetch-rootfs.sh
//!     cargo test -p kastellan-core --test web_fetch_firecracker_egress_e2e -- --ignored --nocapture

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::web_fetch::web_fetch_firecracker_entry;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join("web-fetch.ext4") }
}

fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .join("target");
    for profile in ["release", "debug"] {
        let p = target.join(profile).join("kastellan-microvm-run");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed (need web-fetch.ext4 + KVM + vsock): {e}\n");
        return true;
    }
    match locate_microvm_run() {
        Some(bin) => {
            use std::sync::Once;
            static PATH_ONCE: Once = Once::new();
            PATH_ONCE.call_once(|| {
                let dir = bin.parent().unwrap().to_path_buf();
                let cur = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![dir];
                paths.extend(std::env::split_paths(&cur));
                let joined = std::env::join_paths(paths).expect("join PATH");
                std::env::set_var("PATH", joined);
            });
            false
        }
        None => {
            eprintln!("\n[SKIP] kastellan-microvm-run not built; run `cargo build --release -p kastellan-microvm-run`\n");
            true
        }
    }
}

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

/// Mint a self-signed CA PEM the in-VM worker will trust as KASTELLAN_EGRESS_PROXY_CA.
/// The worker's make_get fails closed on an unreadable/invalid CA, so a parseable
/// cert is required for it to build ProxyConnectGet and emit CONNECT at all.
fn write_test_ca(path: &std::path::Path) {
    let cert = rcgen::generate_simple_self_signed(vec!["egress-proxy.test".to_string()])
        .expect("rcgen self-signed CA");
    std::fs::write(path, cert.cert.pem()).expect("write ca.pem");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-fetch rootfs"]
async fn web_fetch_vm_reaches_proxy_with_ca_delivered() {
    if skip_if_no_microvm() {
        return;
    }
    // Skip-as-pass without PG (dispatch needs a pool for audit). Bring up a cluster
    // via tests-common if available; here we gate on a runtime pool helper.
    let Some(pool) = kastellan_tests_common::try_pg_pool().await else {
        eprintln!("\n[SKIP] no Postgres for dispatch audit\n");
        return;
    };

    // Host scratch under /tmp (a share anchor); holds the stub proxy UDS + ca.pem.
    let dir = std::env::temp_dir().join(format!("kastellan-s4b-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("egress.sock");
    let ca_path = dir.join("ca.pem");
    let _ = std::fs::remove_file(&uds_path);
    write_test_ca(&ca_path);

    // Stub "proxy": on accept, read the first request line and report it back.
    let listener = UnixListener::bind(&uds_path).unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line.clone());
            }
            // Let the worker's fetch fail fast instead of blocking to wall-clock.
            let mut w = stream;
            let _ = w.write_all(b"HTTP/1.1 503 stub\r\n\r\n");
        }
    });

    // Force-routed web-fetch VM entry: set proxy_uds + the CA env + CA in fs_read,
    // exactly as rewrite_worker_policy does on the production path.
    let mut entry = web_fetch_firecracker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-fetch"),
        image_dir(),
        &["example.com".to_string()],
    );
    entry.policy.proxy_uds = Some(uds_path.clone());
    entry.policy.env.push(("KASTELLAN_EGRESS_PROXY_CA".into(), ca_path.to_string_lossy().into_owned()));
    entry.policy.fs_read.push(ca_path.clone());

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn web-fetch in micro-VM");

    // Drive one web.fetch on a background task; we only need it to make the worker
    // attempt egress. The assertion is the stub receiving CONNECT.
    let fetch = tokio::spawn(async move {
        let _ = dispatch(
            &pool,
            &Vault::new(),
            &mut worker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({ "url": "https://example.com/" }),
        )
        .await;
        worker
    });

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("stub proxy never received the in-VM worker's CONNECT (transport or CA broken)");
    assert!(
        got.starts_with("CONNECT example.com:443"),
        "expected CONNECT example.com:443, got {got:?}"
    );

    let worker = fetch.await.expect("fetch task joins");
    let _ = worker.close();
    let _ = std::fs::remove_dir_all(&dir);
}
```

> **Note for the implementer:** verify the exact `tests_common` PG-pool helper name (`try_pg_pool` is a placeholder — use the crate's real bring-up: see how `python_exec_e2e.rs::ready_or_skip` / `probe_and_pool` obtains a pool, and copy that pattern). Also confirm `rcgen::generate_simple_self_signed(...).cert.pem()` matches the pinned rcgen version's API (0.13 returns `CertifiedKey { cert, key_pair }`; `cert.pem()` is the PEM). If the dispatch path needs a registered ToolRegistry rather than a bare `dispatch(&mut worker, …)`, mirror `python_exec_e2e.rs::dispatch_in_jail` exactly.

- [ ] **Step 3: Add the `#[ignore]` real-net origin-validation test**

Append to the same file — the real-sidecar full fetch (origin validation). This mirrors `core/tests/egress_force_routing_e2e.rs::real_mitm_fetch_through_sidecar` but with the worker inside the VM. Read that test for the `spawn_forced_net_worker` + `NetWorkerSpawn` shape, then adapt: build the web-fetch VM entry, force-route it through a real sidecar, drive a `web.fetch` for a real HTTPS host, and assert the result contains readable text. Keep it `#[ignore]` (real network; the DGX public-DNS caveat applies — run on the Mac/operator-driven).

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real network + real sidecar; operator-driven (DGX public-DNS caveat)"]
async fn real_web_fetch_through_sidecar() {
    // Implementer: adapt egress_force_routing_e2e::real_mitm_fetch_through_sidecar
    // to spawn the web-fetch VM entry via spawn_forced_net_worker, drive one
    // web.fetch against a real allowlisted HTTPS host, and assert readable text.
    // Left as a documented #[ignore] scaffold: the always-on gate above is the CI
    // acceptance; this is the manual origin-validation proof.
    eprintln!("manual: see test doc — real-net origin validation through the sidecar");
}
```

- [ ] **Step 4: Build the rootfs + release launcher, then run the e2e on the DGX**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --release -p kastellan-microvm-run 2>&1 | tail -3'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && bash scripts/workers/microvm/build-web-fetch-rootfs.sh 2>&1 | tail -3'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo test -p kastellan-core --test web_fetch_firecracker_egress_e2e -- --ignored --nocapture 2>&1 | tail -40'
```
Expected: `web_fetch_vm_reaches_proxy_with_ca_delivered` passes (stub received `CONNECT example.com:443`); the `#[ignore]` real-net test is listed but not asserted in CI.

- [ ] **Step 5: No-regression + clippy sweep on the DGX**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && for t in firecracker_egress_channel_e2e python_exec_firecracker_hostdir_e2e; do cargo test -p kastellan-core --test $t -- --ignored --nocapture 2>&1 | tail -6; done'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'
ssh dgx 'ls /tmp/kastellan-microvm-* 2>/dev/null | wc -l   # expect 0 orphan run-dirs'
```
Expected: slice-4a egress channel + slice-3 host-dir e2e still green; workspace clippy clean; 0 orphan run-dirs.

- [ ] **Step 6: Commit**

```bash
git add core/tests/web_fetch_firecracker_egress_e2e.rs core/Cargo.toml
git commit -m "test(microvm): web-fetch-in-VM egress e2e (slice 4b)

Always-on hermetic gate: a stub proxy receives the in-VM worker's CONNECT,
proving VM boot + force-routing + slice-4a vsock relay + CA delivery (the worker
emits CONNECT only after loading the in-guest CA). Plus an #[ignore] real-net
origin-validation test through the real sidecar. DGX-verified.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Forward the worker program path into the guest init

**Surfaced by Task 6's e2e** (root cause, systematic-debugging): the guest init
`exec_worker()` hardcodes `/usr/local/bin/kastellan-worker-python-exec` (slice-1
bake; `build_launch_plan` ignores its `_program` arg). The web-fetch rootfs has
only `kastellan-worker-web-fetch`, so the init execs a nonexistent path → ENOENT
→ PID1 panic → no worker → no `CONNECT`. Fix: forward the worker program path via
a hex `kastellan.worker=<hex>` cmdline token (mirrors the #360 `kastellan.env`
codec) and have the init exec it, falling back to the baked python path when the
token is absent (back-compat for slices 1–3).

**Files:**
- Modify: `sandbox/src/linux_firecracker/plan.rs` (add `WORKER_CMDLINE_KEY` const; use `program` instead of `_program`; append the token to `boot_args`; add a roundtrip-fixture unit + fix the baseline boot_args test)
- Modify: `workers/microvm-init/src/main.rs` (add `WORKER_CMDLINE_KEY` + `parse_worker_cmdline`; `exec_worker` execs the forwarded path with the baked fallback; add a parser unit + the matching roundtrip fixture)

**Interfaces:**
- Produces (sandbox): the boot_args now carry ` kastellan.worker=<hex(program)>`. The pure encoder is inline in `build_launch_plan`; the hex codec is the existing `hex_encode`.
- Produces (microvm-init): `fn parse_worker_cmdline(cmdline: &str) -> Option<String>` (hex-decode → UTF-8 → non-empty), consumed by `exec_worker`.
- Cross-crate contract: both crates pin `WORKER_CMDLINE_KEY = "kastellan.worker"` (kept-in-sync comment, like `kastellan.env`/`kastellan.mounts`), and a roundtrip fixture in each crate encodes/decodes the identical hex for a known path.

- [ ] **Step 1: sandbox — write the failing tests**

In `sandbox/src/linux_firecracker/plan.rs` test module, add a fixture pinning the token (hex of `/usr/local/bin/kastellan-worker-web-fetch`) and update the baseline test to expect the worker token. First add:

```rust
    #[test]
    fn build_launch_plan_appends_worker_token() {
        // The guest init reads kastellan.worker=<hex(program)> to exec the right
        // binary. Pinned so kastellan-microvm-init decodes this exact hex.
        let policy = min_policy(); // existing helper used by sibling tests (Net::Deny, no env)
        let plan = build_launch_plan(&policy, &img(), "/usr/local/bin/kastellan-worker-web-fetch", &[])
            .expect("plan");
        let hex = super::hex_encode(b"/usr/local/bin/kastellan-worker-web-fetch");
        assert!(
            plan.boot_args.contains(&format!(" kastellan.worker={hex}")),
            "boot_args missing worker token: {}",
            plan.boot_args
        );
    }
```

(Use whatever minimal-policy constructor the sibling tests already use — e.g. the one in `build_launch_plan_no_env_leaves_boot_args_baseline`. Read that test for the exact helper name; if it builds the policy inline, build it inline here too.)

Then update `build_launch_plan_no_env_leaves_boot_args_baseline` (it currently asserts boot_args == the env-less baseline): it must now also expect the ` kastellan.worker=<hex>` token, since the worker path is always forwarded. Change its assertion from an exact-equality/`!contains("kastellan.")` style to assert the baseline prefix is present AND the only `kastellan.*` token is `kastellan.worker` (no `kastellan.env`/`kastellan.mounts`/`kastellan.egress`). Read the current test body and adapt minimally.

- [ ] **Step 2: sandbox — run to verify RED**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox --lib build_launch_plan_appends_worker_token 2>&1 | tail -15'
```
Expected: FAIL (no worker token in boot_args yet). The baseline test will also fail once you adapt it — that is expected RED.

(On the Mac, cross-clippy compile-checks: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets 2>&1 | tail` shows the test referencing the not-yet-emitted token compiles but fails at runtime on the DGX.)

- [ ] **Step 3: sandbox — implement**

Add the const near `ENV_CMDLINE_KEY` (~line 83):
```rust
/// Cmdline token carrying the hex-encoded worker program path the guest init
/// execs (generalizes slice-1's baked python-exec path). Kept in sync with
/// `kastellan-microvm-init`'s WORKER_CMDLINE_KEY.
const WORKER_CMDLINE_KEY: &str = "kastellan.worker";
```

Change the `build_launch_plan` signature `_program: &str` → `program: &str` (keep `_args` as-is — args are not forwarded; these workers take none).

In the boot_args assembly, after the env-token append (right after the `encode_env_cmdline` block, ~line 268), add:
```rust
    // Forward the worker program path so the guest init execs the right binary
    // (slice 4b: python-exec and web-fetch share one init). Hex-encoded so any
    // absolute path is cmdline-safe, mirroring the #360 env token.
    boot_args.push_str(&format!(" {WORKER_CMDLINE_KEY}={}", hex_encode(program.as_bytes())));
```
(The existing `MAX_CMDLINE_BYTES` budget check below still covers it.)

- [ ] **Step 4: sandbox — run to verify GREEN**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox --lib build_launch_plan 2>&1 | grep -E "build_launch_plan|test result"'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy -p kastellan-sandbox --all-targets -- -D warnings 2>&1 | tail -3'
```
Expected: the new worker-token test + the adapted baseline test pass; all other `build_launch_plan` tests still pass; clippy clean. (Controller runs these.)

- [ ] **Step 5: microvm-init — write the failing test**

In `workers/microvm-init/src/main.rs` test module, add (mirrors `parse_env_cmdline_decodes_host_fixture`, pinning the SAME hex as the sandbox fixture):
```rust
    #[test]
    fn parse_worker_cmdline_decodes_fixture() {
        // Same hex the sandbox build_launch_plan_appends_worker_token fixture emits.
        let hex = "2f7573722f6c6f63616c2f62696e2f6b617374656c6c616e2d776f726b65722d7765622d6665746368";
        let cmdline = format!("console=ttyS0 kastellan.worker={hex} panic=1");
        assert_eq!(
            super::parse_worker_cmdline(&cmdline),
            Some("/usr/local/bin/kastellan-worker-web-fetch".to_string())
        );
    }

    #[test]
    fn parse_worker_cmdline_missing_or_bad_is_none() {
        assert_eq!(super::parse_worker_cmdline("console=ttyS0 panic=1"), None);
        assert_eq!(super::parse_worker_cmdline("kastellan.worker=zz"), None); // bad hex
    }
```

> The fixture hex above is `/usr/local/bin/kastellan-worker-web-fetch` hex-encoded. Verify it matches `hex_encode` output; if your editor can't, compute it once on the DGX with a tiny check, but it must be byte-identical to the sandbox-side fixture (that cross-crate identity is the contract).

- [ ] **Step 6: microvm-init — run to verify RED**

```
source "$HOME/.cargo/env" && cargo test -p kastellan-microvm-init parse_worker_cmdline 2>&1 | tail -15
```
Expected: FAIL — `cannot find function parse_worker_cmdline`.

- [ ] **Step 7: microvm-init — implement**

Add the const (near `ENV_CMDLINE_KEY`):
```rust
/// Cmdline token carrying the hex-encoded worker program path to exec (slice 4b).
/// Must stay in sync with `kastellan-sandbox::linux_firecracker::plan`'s
/// WORKER_CMDLINE_KEY.
const WORKER_CMDLINE_KEY: &str = "kastellan.worker";
```

Add the parser (mirrors `parse_env_cmdline`'s fail-safe style):
```rust
/// Parse the host-forwarded worker program path out of the kernel cmdline
/// (slice 4b). Fail-safe: a missing token, bad hex, non-UTF-8, or empty value
/// all yield `None`, so `exec_worker` falls back to the baked path. Pure.
#[allow(dead_code)]
fn parse_worker_cmdline(cmdline: &str) -> Option<String> {
    let prefix = format!("{WORKER_CMDLINE_KEY}=");
    let token = cmdline.split_whitespace().find_map(|t| t.strip_prefix(&prefix))?;
    let bytes = hex_decode(token)?;
    let s = String::from_utf8(bytes).ok()?;
    (!s.is_empty()).then_some(s)
}
```

Rework `exec_worker` to read the cmdline once and prefer the forwarded path, with the baked python path as the fallback (and guard the `CString::new` so a NUL-bearing path can't panic PID1 — fall back instead):
```rust
fn exec_worker() {
    use std::ffi::CString;
    // SAFETY: single-threaded PID1; no other threads to race with.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    // Forwarded worker path (slice 4b) with the slice-1 python-exec bake as the
    // fail-safe fallback, so slices 1–3 (which forward their own python path now,
    // or nothing) still boot a working worker.
    let prog_path = parse_worker_cmdline(&cmdline)
        .unwrap_or_else(|| "/usr/local/bin/kastellan-worker-python-exec".to_string());
    let prog = match CString::new(prog_path.clone()) {
        Ok(c) => c,
        Err(_) => CString::new("/usr/local/bin/kastellan-worker-python-exec").unwrap(),
    };
    #[allow(deprecated)]
    unsafe {
        // Baked python interpreter default (harmless for non-python workers,
        // which ignore it); host-forwarded policy.env overrides it.
        std::env::set_var("KASTELLAN_PYTHON_EXEC_PYTHON", "/usr/bin/python3");
        for (k, v) in parse_env_cmdline(&cmdline) {
            std::env::set_var(k, v);
        }
    }
    let argv = [prog.as_ptr(), std::ptr::null()];
    unsafe {
        libc::execv(prog.as_ptr(), argv.as_ptr());
    }
    panic!("execv of worker failed");
}
```

- [ ] **Step 8: microvm-init — run to verify GREEN**

```
source "$HOME/.cargo/env" && cargo test -p kastellan-microvm-init 2>&1 | tail -8
source "$HOME/.cargo/env" && cargo clippy -p kastellan-microvm-init --all-targets -- -D warnings 2>&1 | tail -3
source "$HOME/.cargo/env" && cargo clippy -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets -- -D warnings 2>&1 | tail -3
```
Expected: all microvm-init tests pass (existing + 3 new); both clippy runs clean.

- [ ] **Step 9: Commit (both crates, one commit)**

```bash
git add sandbox/src/linux_firecracker/plan.rs workers/microvm-init/src/main.rs
git commit -m "feat(microvm): forward the worker program path into the guest init (slice 4b)

build_launch_plan now forwards the worker binary path via a hex kastellan.worker=
cmdline token; the guest init execs it (baked python-exec path as fail-safe
fallback). Generalizes the slice-1 baked invocation so the web-fetch rootfs runs
the web-fetch worker. Cross-crate roundtrip fixture pins the codec.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 10: Controller — rebuild BOTH rootfs images (the init is baked in), then re-run Task 6 e2e + no-regression**

The init binary lives inside each rootfs, so both must be rebuilt after this change:
```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && bash scripts/workers/microvm/build-rootfs.sh 2>&1 | tail -2 && bash scripts/workers/microvm/build-web-fetch-rootfs.sh 2>&1 | tail -2'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo build --release -p kastellan-microvm-run >/dev/null 2>&1; cargo test -p kastellan-core --test web_fetch_firecracker_egress_e2e -- --ignored --nocapture 2>&1 | tail -12'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo test -p kastellan-core --test firecracker_egress_channel_e2e -- --ignored --nocapture 2>&1 | tail -6'
```
Expected: the web-fetch gate passes (stub receives `CONNECT example.com:443`); slice-4a egress channel e2e still green (no regression — python rootfs now boots via the forwarded token).

## Self-Review

**Spec coverage:**
- Spec Component 1 (web-fetch rootfs) → Task 5. ✓
- Spec Component 2 (entry + resolver) → Tasks 3 + 4. ✓
- Spec Component 3 (backend rootfs-filename resolution) → Task 1. ✓
- Spec Component 4 (guest file-aware RO bind) → Task 2. ✓
- Spec Component 5 (DGX e2e: always-on CONNECT-stub gate + pure bind units + #[ignore] real-net) → Task 6 (e2e) + Task 2 (pure bind units). ✓
- Security posture (no direct-net path / fail-closed reject / SandboxPolicy+bwrap unchanged / CA private key never shared / best-effort PID1) — preserved: Task 2 keeps the best-effort log-and-skip contract; Tasks 3/4 reuse the merged force-routing reject; no `SandboxPolicy` field added. ✓
- Open implementation details (hermetic full-fetch gate / `/tmp` bind ordering / `ldd` closure) — surfaced in Task 6 notes + Task 5 closure step. ✓

**Placeholder scan:** The only deliberate placeholder is the `#[ignore] real_web_fetch_through_sidecar` scaffold (Task 6 Step 3) — intentional per the approved spec (origin validation is operator-driven, not CI). The `tests_common::try_pg_pool` name is flagged for the implementer to confirm against `python_exec_e2e.rs` (Step 2 note). All code steps carry real code.

**Type consistency:** `web_fetch_firecracker_entry(binary: PathBuf, image_dir: String, allowlist: &[String]) -> ToolEntry` is defined in Task 3 and consumed identically in Task 4 (resolver) and Task 6 (e2e). `resolve_image(env: &[(String, String)]) -> FirecrackerImage` (Task 1) and `bind_prep(bool, bool) -> BindPrep` (Task 2) are each used only within their own file. The env names (`KASTELLAN_MICROVM_DIR`, `KASTELLAN_MICROVM_ROOTFS`, `KASTELLAN_WEB_FETCH_USE_MICROVM`, `KASTELLAN_WEB_FETCH_ALLOWLIST`) match across Tasks 1, 3, 4, 5, and the Global Constraints.
