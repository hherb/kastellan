# web-search Firecracker micro-VM entry — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the web-search worker an opt-in, Linux-only Firecracker micro-VM execution mode (direct + VM×search-broker), mirroring web-research's VM support, reusing all #445/#446/#448 plumbing.

**Architecture:** Two new `*_firecracker_entry` `ToolEntry` constructors in `core/src/workers/web_search.rs` + a Linux-gated `resolve()` short-circuit gated on `KASTELLAN_WEB_SEARCH_USE_MICROVM=1` (2×2 with `USE_BROKER`), a `web-search.ext4` rootfs build script, and a DGX-gated e2e. No sandbox/microvm/broker code changes — the VM broker vsock channel (port 1026) is already `BrokerKind`-agnostic, and `spawn_worker_with_optional_broker` is already kind-parameterized.

**Tech Stack:** Rust (workspace, rustc 1.96.0), `kastellan-sandbox` (`FirecrackerVm` backend), `kastellan-core` manifests, bash (rootfs), tokio (e2e). Firecracker/KVM/vsock on the DGX (aarch64) for live acceptance.

**Spec:** `docs/superpowers/specs/2026-07-15-web-search-firecracker-microvm-entry-design.md`

## Global Constraints

- **Cross-platform, but this feature is Linux-only.** Every new code path is `#[cfg(target_os = "linux")]` (the `FirecrackerVm` variant does not exist on macOS — the #144 rule). The host path stays **byte-identical when `USE_MICROVM` is unset**.
- **`core` cannot be compiled or tested for Linux on the macOS dev box** (`ring` C-dep = the #144 cross wall). So: write the Linux-gated tests first, but their RED→GREEN is verified on the **DGX** (Task 5). On the **Mac**, each task's gate is `cargo build --workspace` + `cargo clippy --workspace --all-targets -D warnings` + the cross-platform host-path web_search tests. `cargo build` on macOS does NOT compile `#[cfg(target_os="linux")]` blocks, so a typo there is caught only on the DGX.
- **AGPL-3.0; AGPL-compatible deps only.** No new dependencies are introduced.
- **Files under 500 LOC where feasible** (rule #4). Task 1 lifts `web_search.rs`'s tests so the parent's production stays well under 500.
- **VM sizing:** `cpu_ms: 15_000`, `mem_mb: 512` (matches web-research's VM entry).
- **In-rootfs worker binary path:** `/usr/local/bin/kastellan-worker-web-search` (baked by the rootfs script; a VM worker's `program` is the in-guest path, NEVER the host `target/` path — see the memory note `vm-worker-in-rootfs-binary-path`).
- **Commits:** stage specific files (never `git add -A`). End every commit message with:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- **Run all cargo commands in the FOREGROUND** (no `&`/background) — source the env first: `source "$HOME/.cargo/env"`.

---

### Task 1: Lift `web_search.rs` tests into a `web_search/tests.rs` submodule

Pure mechanical move (rule #4 housekeeping) so the VM-entry code + tests in Task 2 land in a focused file and the parent's production stays under 500 LOC. Public API byte-identical. Fully Mac-verifiable.

**Files:**
- Modify: `core/src/workers/web_search.rs` (replace the inline `#[cfg(test)] mod tests { … }` block, currently lines ~286–553, with a `#[cfg(test)] mod tests;` declaration)
- Create: `core/src/workers/web_search/tests.rs` (the moved test body)

**Interfaces:**
- Consumes: nothing new.
- Produces: a `#[cfg(test)] mod tests;` child of the `web_search` module. Its tests use `use super::*;` (resolves to the `web_search` module, same as before). This is the file Task 2 appends VM-entry tests to.

- [ ] **Step 1: Create `core/src/workers/web_search/tests.rs`** with the exact body currently inside `web_search.rs`'s `mod tests { … }` (everything between the outer braces — the `use super::*;`, `use std::path::Path;`, the `ctx(...)` helper, all `#[test]` fns, and `outcome_label`). Do NOT wrap it in another `mod tests { }` — the file *is* the module body.

- [ ] **Step 2: Replace the inline test module in `web_search.rs`** — delete the whole `#[cfg(test)]\nmod tests {\n … \n}` block at the bottom of the file and put in its place exactly:

```rust
#[cfg(test)]
mod tests;
```

- [ ] **Step 3: Run the web-search tests to verify the move is behaviour-preserving**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core web_search -- --nocapture`
Expected: PASS — the same test count as before the move (8 web_search unit tests: `resolve_registers_with_net_client_policy_and_endpoint_net`, `resolve_https_endpoint_maps_to_port_443`, `web_search_has_no_db_argv0_allowlist`, `resolve_derives_worker_allowlist_from_endpoint_not_db`, `resolve_misconfigured_when_no_binary_found`, `resolve_broker_mode_drops_egress_and_declares_search_broker`, `resolve_direct_mode_unchanged_when_use_broker_unset`, `resolve_injects_max_batch_env_when_set`, `resolve_omits_max_batch_env_when_unset`, `resolve_skips_blank_max_batch_env`).

- [ ] **Step 4: Clippy + build gate**

Run: `source "$HOME/.cargo/env" && cargo build --workspace && cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: clean (exit 0, no warnings).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/web_search.rs core/src/workers/web_search/tests.rs
git commit -m "refactor(web-search): lift manifest tests into web_search/tests.rs

Mechanical test-lift (public API byte-identical) ahead of the VM-entry
additions, keeping the parent file's production under the 500-LOC guideline.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Add both Firecracker VM entries + the `resolve()` 2×2 short-circuit

Adds `web_search_firecracker_entry` (direct VM) and `web_search_firecracker_broker_entry` (VM × search-broker), the `USE_MICROVM`/`MICROVM_WORKER_BIN` consts, and the Linux-gated `resolve()` short-circuit. The one detail that differs from web-research: the VM branch must also thread the `web.search_batch` cap env.

**Files:**
- Modify: `core/src/workers/web_search.rs` (add consts after the existing consts ~line 48; add the two fns after `web_search_broker_entry` ~line 176; restructure `resolve()` ~lines 258–283)
- Test: `core/src/workers/web_search/tests.rs` (append three Linux-gated tests)

**Interfaces:**
- Consumes: `net_entries_from_endpoint`, `host_allowlist_from_endpoint`, `maybe_inject_max_batch`, `ENDPOINT_ENV`, `USE_BROKER_ENV`, `MAX_BATCH_QUERIES_ENV` (all already in `web_search.rs`); `crate::broker::BrokerSpec::search`; `kastellan_sandbox::SandboxBackendKind::FirecrackerVm`.
- Produces:
  - `pub fn web_search_firecracker_entry(binary: PathBuf, image_dir: String, endpoint: &str, allowlist: &[String]) -> ToolEntry` (Linux-only)
  - `pub fn web_search_firecracker_broker_entry(binary: PathBuf, image_dir: String, endpoint: &str) -> ToolEntry` (Linux-only)
  - These are consumed by the e2e in Task 4.

- [ ] **Step 1: Write the failing Linux-gated tests** — append to `core/src/workers/web_search/tests.rs`:

```rust
#[cfg(target_os = "linux")]
#[test]
fn resolve_uses_direct_microvm_entry_when_opted_in() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_MICROVM" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
                "VM entry emits the FirecrackerVm backend"
            );
            assert!(entry.policy.fs_read.is_empty(), "VM entry has empty fs_read");
            assert!(entry.broker.is_none(), "direct VM entry has no broker");
            // The in-rootfs binary path, NOT the discovered host path.
            assert_eq!(
                entry.binary,
                PathBuf::from("/usr/local/bin/kastellan-worker-web-search")
            );
            // Endpoint host:port allowlist preserved (routable-SearxNG path).
            match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert_eq!(hosts, &vec!["searx.example.org:8888".to_string()])
                }
                other => panic!("expected Net::Allowlist, got {other:?}"),
            }
            // Rootfs env forwarded.
            assert!(entry
                .policy
                .env
                .iter()
                .any(|(k, v)| k == "KASTELLAN_MICROVM_ROOTFS" && v == "web-search.ext4"));
            assert!(entry.policy.env.iter().any(|(k, _)| k == "KASTELLAN_MICROVM_DIR"));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_uses_broker_microvm_entry_when_both_opted_in() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_MICROVM" => Some("1".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_BROKER" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
            );
            // Broker declared, carrying the SearxNG endpoint it forwards to.
            let spec = entry.broker.as_ref().expect("broker set in VM broker mode");
            assert_eq!(spec.kind, crate::broker::BrokerKind::Search);
            assert_eq!(spec.endpoint, "http://127.0.0.1:8888/search");
            // Zero direct egress — empty allowlist.
            match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert!(hosts.is_empty(), "broker VM worker must have no egress: {hosts:?}")
                }
                other => panic!("expected empty Net::Allowlist, got {other:?}"),
            }
            // No direct endpoint env leaked to the worker in broker mode.
            assert!(entry.policy.env.iter().all(|(k, _)| k != ENDPOINT_ENV));
            assert!(entry.policy.fs_read.is_empty());
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_vm_entry_still_injects_batch_cap() {
    // The batch cap must survive the VM short-circuit (it applies in the host path
    // after entry construction; the VM branch returns early, so it must thread it
    // too — else a batched search in the VM ignores the operator cap).
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_MICROVM" => Some("1".to_string()),
        "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("5".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(
                entry.policy.env.iter().any(|(k, v)| k == MAX_BATCH_QUERIES_ENV && v == "5"),
                "batch cap must be injected in VM mode: {:?}",
                entry.policy.env
            );
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}
```

- [ ] **Step 2: Add the consts** — in `web_search.rs`, after the existing consts (after `WEB_SEARCH_BATCH_METHOD`, ~line 48):

```rust
/// Opt into the Linux Firecracker micro-VM backend for web-search. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_SEARCH_USE_MICROVM";

/// In-rootfs path of the web-search worker binary (staged there by
/// `build-web-search-rootfs.sh`). Used by the micro-VM entries, not the host path.
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-search";
```

- [ ] **Step 3: Add the two VM-entry functions** — in `web_search.rs`, after `web_search_broker_entry` (~line 176):

```rust
/// Build the [`ToolEntry`] for web-search running inside a Firecracker micro-VM
/// (opt-in via `KASTELLAN_WEB_SEARCH_USE_MICROVM=1`). Mirrors
/// `web_fetch_firecracker_entry`: empty `fs_read` (no NIC / local DNS — the
/// per-instance MITM CA is appended at spawn by `rewrite_worker_policy`), the
/// in-rootfs worker binary, `sandbox_backend = FirecrackerVm`, and the
/// `KASTELLAN_MICROVM_DIR` / `KASTELLAN_MICROVM_ROOTFS=web-search.ext4` env. Keeps
/// the endpoint-derived `Net::Allowlist` + endpoint/allowlist env, so the worker
/// reaches a **routable** SearxNG through the host MITM egress sidecar.
///
/// Loopback-SearxNG caveat: in VM mode egress force-routes through the host proxy,
/// which SSRF-blocks loopback, so a `127.0.0.1` SearxNG is unreachable here — use
/// the broker VM entry ([`web_search_firecracker_broker_entry`], `USE_BROKER=1`)
/// for a loopback SearxNG. Linux-only: emits the `FirecrackerVm` backend variant.
#[cfg(target_os = "linux")]
pub fn web_search_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    allowlist: &[String],
) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(net_entries_from_endpoint(endpoint)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            ("KASTELLAN_WEB_SEARCH_ALLOWLIST".to_string(), allow_json),
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-search.ext4".to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
    }
}

/// Build the web-search [`ToolEntry`] running inside a Firecracker micro-VM **AND**
/// reaching a host-side search-broker (VM × broker; opt-in via `USE_MICROVM=1` +
/// `USE_BROKER=1`). Combines the VM entry (empty `fs_read`, `FirecrackerVm`
/// backend) with broker mode: `Net::Allowlist` is **empty** (zero direct egress),
/// no endpoint env is injected, and `broker: Some(BrokerSpec::search(endpoint))`
/// tells core's cold-spawn chokepoint to spawn the broker + bind its UDS. In the
/// VM the broker rides a second vsock channel (port 1026); the FC plan rewrites the
/// injected `KASTELLAN_SEARCH_BROKER_UDS` to the in-guest relay path.
///
/// Because the broker runs host-side, this is the ONLY way a VM web-search worker
/// reaches a **loopback** SearxNG (the egress proxy SSRF-blocks loopback). Linux-only.
#[cfg(target_os = "linux")]
pub fn web_search_firecracker_broker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
) -> ToolEntry {
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        // No direct egress — the broker holds the only route to SearxNG.
        net: Net::Allowlist(vec![]),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-search.ext4".to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,  // set at spawn (force-routing)
        broker_uds: None, // set at spawn (rewrite_policy_for_broker)
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: Some(crate::broker::BrokerSpec::search(endpoint)),
    }
}
```

- [ ] **Step 4: Restructure `resolve()`** — replace the current `resolve()` body (which discovers the binary first) with the version below. It computes `endpoint`/`use_broker` first, inserts the Linux-gated VM short-circuit, then falls through to host binary discovery. Note the batch cap is read + applied in **each** branch (VM and host) to keep the borrow trivial and both paths symmetric:

```rust
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let endpoint = (ctx.get_env)(ENDPOINT_ENV).unwrap_or_default();
        // Broker mode: the worker reaches SearxNG only through a trusted
        // search-broker sidecar (so a force-routed worker can use a loopback
        // SearxNG). The broker owns the SearxNG allowlist; the worker gets none.
        let use_broker = (ctx.get_env)(USE_BROKER_ENV).unwrap_or_default().trim() == "1";

        // Firecracker micro-VM mode (Linux) short-circuits host binary discovery:
        // the worker binary lives inside the rootfs image, not on the host.
        // Linux-only — on macOS USE_MICROVM is never read so the `FirecrackerVm`
        // variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            let use_microvm = (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
            if use_microvm {
                let binary = PathBuf::from(MICROVM_WORKER_BIN);
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                // VM × broker: the broker runs host-side and the VM worker reaches
                // it over the vsock UDS (port 1026), so a loopback SearxNG works in
                // VM mode.
                let entry = if use_broker {
                    web_search_firecracker_broker_entry(binary, image_dir, &endpoint)
                } else {
                    let allowlist = host_allowlist_from_endpoint(&endpoint);
                    web_search_firecracker_entry(binary, image_dir, &endpoint, &allowlist)
                };
                let entry = maybe_inject_max_batch(entry, (ctx.get_env)(MAX_BATCH_QUERIES_ENV));
                return Resolution::Register(entry);
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
        let entry = if use_broker {
            web_search_broker_entry(binary, &endpoint)
        } else {
            let allowlist = host_allowlist_from_endpoint(&endpoint);
            web_search_entry(binary, &endpoint, &allowlist)
        };
        let entry = maybe_inject_max_batch(entry, (ctx.get_env)(MAX_BATCH_QUERIES_ENV));
        Resolution::Register(entry)
    }
```

- [ ] **Step 5: Mac gate — build + clippy + host-path tests** (the Linux-gated tests won't compile/run on macOS; their RED→GREEN is in Task 5)

Run: `source "$HOME/.cargo/env" && cargo build --workspace && cargo clippy -p kastellan-core --all-targets -- -D warnings && cargo test -p kastellan-core web_search`
Expected: build exit 0; clippy clean; the existing host-path web_search tests still PASS (the resolve restructure is behaviour-preserving on the host path — same 8 pass; the 3 new Linux-gated tests are `#[cfg]`-skipped on macOS and simply don't appear).

- [ ] **Step 6: Commit**

```bash
git add core/src/workers/web_search.rs core/src/workers/web_search/tests.rs
git commit -m "feat(web-search): Firecracker micro-VM entries (direct + VM x search-broker)

Add web_search_firecracker_entry + web_search_firecracker_broker_entry and a
Linux-gated resolve() short-circuit gated on KASTELLAN_WEB_SEARCH_USE_MICROVM=1
(2x2 with USE_BROKER), mirroring web-research. The VM branch also threads the
web.search_batch cap env. Host path byte-identical when USE_MICROVM is unset.
Reuses the #446 BrokerKind-agnostic vsock-1026 channel for BrokerKind::Search.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: web-search micro-VM rootfs build script

A near-clone of `build-web-research-rootfs.sh`: bakes the web-search worker + init into `web-search.ext4`. No compile on macOS; syntax-check only.

**Files:**
- Create: `scripts/workers/microvm/build-web-search-rootfs.sh`

**Interfaces:**
- Consumes: `kastellan-worker-web-search`, `kastellan-microvm-init` (release binaries), the shared `vmlinux`.
- Produces: `web-search.ext4` in `$KASTELLAN_MICROVM_DIR` (default `/var/lib/kastellan/microvm`), consumed by Task 4's e2e + `KASTELLAN_MICROVM_ROOTFS=web-search.ext4` in the manifest.

- [ ] **Step 1: Create `scripts/workers/microvm/build-web-search-rootfs.sh`** with exactly:

```bash
#!/usr/bin/env bash
# Build the web-search micro-VM rootfs (ext4) into the SHARED image dir, beside
# python-exec.ext4 + web-fetch.ext4 + web-research.ext4 + the shared vmlinux. The
# dir + kernel are shared across workers (build-rootfs.sh provisions them); only
# the rootfs filename differs (KASTELLAN_MICROVM_ROOTFS=web-search.ext4). web-search
# is a pure-Rust net worker: no python, and NO system CA bundle — egress (the
# SearxNG search) is MITM-only and the only trusted root is the per-instance proxy
# CA delivered per-spawn via the slice-3 RO-share. In broker mode there is no direct
# egress at all (the host search-broker holds the only route).
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash, not sh: ./scripts/workers/microvm/build-web-search-rootfs.sh" >&2
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
    echo "    KASTELLAN_MICROVM_DIR=\"\$HOME/.local/share/kastellan/microvm\" ./scripts/workers/microvm/build-web-search-rootfs.sh" >&2
    exit 1
fi

# Shared guest kernel (pinned). Reused if build-rootfs.sh already fetched it.
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# Cross-build worker + init for the guest (native on the DGX aarch64).
source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-web-search -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT

# Binaries: init is PID1 at /sbin/init; the worker at its in-rootfs path
# (matches MICROVM_WORKER_BIN in core/src/workers/web_search.rs).
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-web-search "$WORK/usr/local/bin/kastellan-worker-web-search"

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
    target/release/kastellan-worker-web-search

# Pseudo-fs mountpoints (microvm-init mounts proc/sys/tmp at boot) + slice-3
# host-dir-share anchors + slice-4a /run egress relay tmpfs mountpoint. Keep this
# anchor list in lockstep with mounts.rs::SHARE_ANCHORS (opt/data/srv/mnt/work/tmp)
# and build-rootfs.sh. The per-instance ca.pem binds under /tmp (a boot tmpfs).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
mkdir -p "$WORK/run"

# Journal-less ext4 (read-only at runtime, shared across concurrent VMs).
mkfs.ext4 -q -F -O ^has_journal -L web-search -d "$WORK" "$OUT_DIR/web-search.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/web-search.ext4 (+ shared $OUT_DIR/vmlinux)"
```

- [ ] **Step 2: Make it executable and syntax-check**

Run: `chmod +x scripts/workers/microvm/build-web-search-rootfs.sh && bash -n scripts/workers/microvm/build-web-search-rootfs.sh && echo OK`
Expected: `OK` (no syntax errors). If `shellcheck` is installed, also: `shellcheck scripts/workers/microvm/build-web-search-rootfs.sh` — expect no errors (the template already passes; matching it keeps it clean).

- [ ] **Step 3: Commit**

```bash
git add scripts/workers/microvm/build-web-search-rootfs.sh
git commit -m "feat(web-search): build-web-search-rootfs.sh (micro-VM ext4)

Clone of build-web-research-rootfs.sh: bakes kastellan-worker-web-search +
microvm-init into web-search.ext4 in the shared image dir. No python, no system
CA bundle (MITM-only; broker mode needs no CA).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: DGX-gated e2e — `web_search_firecracker_egress_e2e.rs`

Two `#[ignore]` tests: (1) a direct-entry CONNECT-to-stub-proxy gate (proves VM boot + force-route + vsock + CA); (2) a manager-level VM×broker live e2e (proves real search results in a VM with zero direct worker egress, driving the real `SingleUseLifecycle::acquire` daemon path). The whole file is `#![cfg(target_os = "linux")]` so it is absent on macOS (`cargo build --workspace` on the Mac stays green).

**Files:**
- Create: `core/tests/web_search_firecracker_egress_e2e.rs`

**Interfaces:**
- Consumes: `web_search_firecracker_entry`, `web_search_firecracker_broker_entry` (Task 2); `kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager}`; `kastellan_core::worker_lifecycle::force_route::{ForceRoutingConfig, DecisionSinkFactory}`; `kastellan_core::broker::{BrokerConfig, BrokerConfigs, BrokerKind}`; `kastellan_core::egress::audit::EgressAuditRow`; `kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec}`; `kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker}`; `kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends}`; `kastellan_tests_common::{…}`.
- Produces: nothing (test-only).

- [ ] **Step 1: Create `core/tests/web_search_firecracker_egress_e2e.rs`** with exactly:

```rust
#![cfg(target_os = "linux")]
//! web-search micro-VM e2e: web-search runs inside a Firecracker VM.
//!
//! Two DGX-only (`#[ignore]`) tests — both need real KVM + vsock + the web-search
//! rootfs (REBUILD via build-web-search-rootfs.sh) + the kastellan-microvm-run
//! RELEASE launcher:
//!
//! * `web_search_vm_reaches_proxy_with_ca_delivered` (DIRECT entry): a host
//!   UnixListener stub stands in for the egress proxy at the worker's proxy_uds; a
//!   force-routed web-search VM boots and one `web.search` is driven through it; we
//!   assert the stub RECEIVES the worker's `CONNECT <searxng-host>:<port>` line.
//!   The worker can only emit CONNECT after loading the in-guest CA, so this single
//!   assertion proves VM boot + force-routing + the vsock relay + CA delivery.
//!   (Mirror of web_research_firecracker_egress_e2e + web_fetch's single-CONNECT gate.)
//!
//! * `brokered_web_search_vm_returns_results_with_zero_egress` (VM x BROKER, live):
//!   drives the real `SingleUseLifecycle::acquire` daemon path — the manager
//!   resolves the VM worker backend from `entry.sandbox_backend = Some(FirecrackerVm)`
//!   and the host search-broker backend from `resolve(None, None)`. The VM worker
//!   holds an EMPTY egress allowlist and reaches a live loopback SearxNG only over
//!   vsock 1026 to the host search-broker; we assert a non-empty `results` array AND
//!   the SearxNG host:port never appears in a worker egress decision (zero direct
//!   egress). Needs a live SearxNG (e.g. 127.0.0.1:8888) + the egress-proxy +
//!   search-broker binaries.
//!
//! Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo build --workspace   # egress-proxy + search-broker host binaries
//!     bash scripts/workers/microvm/build-web-search-rootfs.sh
//!     cargo test -p kastellan-core --test web_search_firecracker_egress_e2e -- --ignored --nocapture

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use kastellan_core::broker::{BrokerConfig, BrokerConfigs, BrokerKind};
use kastellan_core::egress::audit::EgressAuditRow;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
use kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager};
use kastellan_core::workers::web_search::{
    web_search_firecracker_broker_entry, web_search_firecracker_entry,
};
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable,
    unique_suffix, workspace_target_binary,
};

/// SearxNG endpoint the DIRECT-entry VM worker searches first. The host part must
/// appear in the worker's CONNECT (host:port), so we pin a non-443 port to make the
/// assertion sharp.
const SEARXNG_ENDPOINT: &str = "https://searx.example.org:8888/search";
/// Default live SearxNG for the broker test (loopback; reached only via the broker).
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join("web-search.ext4") }
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

/// Skip unless a bootable web-search micro-VM is available. Also prepends the
/// `kastellan-microvm-run` build dir to PATH (the Firecracker backend spawns the
/// launcher by bare name; it is off the default SSH PATH — see the memory note
/// `firecracker-e2e-stale-release-launcher`). Idempotent via `Once`.
fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed (need web-search.ext4 + KVM + vsock): {e}\n");
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

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-search-firecracker-egress-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Mint a self-signed CA PEM the in-VM worker trusts as KASTELLAN_EGRESS_PROXY_CA.
/// The worker's make_get fails closed on an unreadable/invalid CA, so a parseable
/// cert is required for it to build ProxyConnectGet and emit CONNECT at all.
fn write_test_ca(path: &std::path::Path) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let key_pair = KeyPair::generate().expect("keypair");
    let mut params = CertificateParams::new(vec!["egress-proxy.test".to_string()]).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key_pair).expect("self-signed");
    std::fs::write(path, cert.pem()).expect("write ca.pem");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-search rootfs"]
async fn web_search_vm_reaches_proxy_with_ca_delivered() {
    if skip_if_no_microvm() || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ws-d",
        "ws-l",
        &format!("kastellan-supervisor-test-pg-websearch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Host scratch under /tmp (a share anchor); holds the stub proxy UDS + ca.pem.
    let dir = std::env::temp_dir().join(format!("kastellan-ws-vm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("egress.sock");
    let ca_path = dir.join("ca.pem");
    let _ = std::fs::remove_file(&uds_path);
    write_test_ca(&ca_path);

    // Stub "proxy": on accept, read the first request line and report it back, then
    // send a fast 503 so the worker's request fails fast instead of blocking.
    let listener = UnixListener::bind(&uds_path).unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line.clone());
            }
            let mut w = stream;
            let _ = w.write_all(b"HTTP/1.1 503 stub\r\n\r\n");
        }
    });

    // Force-routed web-search DIRECT VM entry: set proxy_uds + the CA env + CA in
    // fs_read, exactly as rewrite_worker_policy does on the production path.
    //
    // The allowlist MUST include the SearxNG endpoint host: the worker's `from_env`
    // runs `validate_endpoint(endpoint, allowlist)` and fails closed (never serves)
    // if the endpoint host is off-allowlist — so without `searx.example.org` here
    // the worker would never search and never emit CONNECT. In production the
    // endpoint-derived allowlist carries it for the same reason.
    let mut entry = web_search_firecracker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-search"),
        image_dir(),
        SEARXNG_ENDPOINT,
        &["searx.example.org".to_string()],
    );
    entry.policy.proxy_uds = Some(uds_path.clone());
    entry.policy.env.push((
        "KASTELLAN_EGRESS_PROXY_CA".into(),
        ca_path.to_string_lossy().into_owned(),
    ));
    entry.policy.fs_read.push(ca_path.clone());

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn web-search in micro-VM");

    // Drive one web.search on a background task; we only need it to attempt egress.
    let search = tokio::spawn(async move {
        let _ = dispatch(
            &pool,
            &Vault::new(),
            &mut worker,
            "web-search",
            "web.search",
            serde_json::json!({ "query": "hello world" }),
        )
        .await;
        (worker, pool)
    });

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("stub proxy never received the in-VM worker's CONNECT (transport or CA broken)");
    assert!(
        got.starts_with("CONNECT searx.example.org:8888"),
        "expected CONNECT searx.example.org:8888 (the SearxNG search), got {got:?}"
    );

    let (worker, pool) = search.await.expect("search task joins");
    let _ = worker.close();
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Bare host of a URL (for the zero-egress absence check).
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

fn egress_proxy_bin_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
        None
    }
}

/// Live manager-level proof (#448 pattern): a VM web-search worker acquired through
/// the real `SingleUseLifecycle::acquire` reaches a live loopback SearxNG ONLY over
/// vsock 1026 to the host search-broker, with an EMPTY egress allowlist. Asserts a
/// non-empty `results` array AND the SearxNG host:port absent from egress decisions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-search rootfs + egress proxy + \
            search-broker + live SearxNG. Drives SingleUseLifecycle::acquire for a \
            VM web-search worker; asserts real results with zero direct egress."]
async fn brokered_web_search_vm_returns_results_with_zero_egress() {
    if skip_if_no_microvm() || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else {
        return;
    };
    let broker_bin = workspace_target_binary("kastellan-worker-search-broker");
    if !broker_bin.exists() {
        eprintln!("\n[SKIP] search-broker binary not built; run cargo build --workspace\n");
        return;
    }

    let searx_endpoint = std::env::var("KASTELLAN_WEB_SEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wsb-d",
        "wsb-l",
        &format!("kastellan-supervisor-test-pg-websearchbroker-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // The VM broker-mode manifest entry (sandbox_backend = Some(FirecrackerVm),
    // broker = Some(Search), empty Net::Allowlist). Worker runs from the
    // rootfs-baked path; the broker is a host binary.
    let entry = web_search_firecracker_broker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-search"),
        image_dir(),
        &searx_endpoint,
    );

    // Capture every egress decision so we can assert zero direct SearxNG egress.
    let decisions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_src = Arc::clone(&decisions);
    let make_sink: DecisionSinkFactory = Box::new(move || {
        let d = Arc::clone(&sink_src);
        Box::new(move |row: EgressAuditRow| {
            d.lock().unwrap().push(format!("{} {}", row.action, row.payload));
        })
    });
    let force = Arc::new(ForceRoutingConfig::new(
        proxy_bin,
        std::env::temp_dir(),
        make_sink,
        None, // no cert pins
    ));

    // Real host search-broker config (scratch under /tmp so the VMM jail can bind
    // its UDS and the vsock-1026 relay can reach it).
    let broker_configs = BrokerConfigs {
        search: Some(Arc::new(BrokerConfig::new(
            BrokerKind::Search,
            broker_bin,
            std::env::temp_dir(),
        ))),
        ..Default::default()
    };

    // The real production manager. It resolves the worker backend from
    // entry.sandbox_backend (FirecrackerVm) AND the sidecar/broker backend from
    // resolve(None, None) (host bwrap) — the #448 behaviour, now for web-search.
    let sandboxes = Arc::new(SandboxBackends::default_for_current_os());
    let mgr = SingleUseLifecycle::with_force_routing(sandboxes, Some(force), broker_configs);

    let mut handle = mgr
        .acquire("web-search", &entry)
        .await
        .expect("acquire a force-routed VM web-search worker through the manager");

    let result = dispatch(
        &pool,
        &Vault::new(),
        handle.worker_mut(),
        "web-search",
        "web.search",
        serde_json::json!({"query": "rust programming language"}),
    )
    .await
    .expect("web.search round trip through the daemon-managed VM worker");

    for line in decisions.lock().unwrap().iter() {
        eprintln!("[egress-decision] {line}");
    }

    // Payoff: real results even though the worker had an EMPTY egress allowlist —
    // the search rode the broker over vsock 1026.
    let results = result["results"].as_array().expect("results must be an array");
    assert!(
        !results.is_empty(),
        "expected a non-empty results array via the broker (VM worker has zero egress); got {result:?}"
    );

    // Zero direct egress: the SearxNG host:port must never appear in a worker egress
    // decision. Match host AND port (loopback host is shared on the DGX; only the
    // port distinguishes SearxNG).
    let searx_url = url::Url::parse(&searx_endpoint).ok();
    let searx_host = url_host(&searx_endpoint);
    let searx_port = searx_url.as_ref().and_then(url::Url::port).unwrap_or(8888);
    let host_needle = format!("\"host\":\"{searx_host}\"");
    let port_needle = format!("\"port\":{searx_port}");
    let leaked: Vec<_> = decisions
        .lock()
        .unwrap()
        .iter()
        .filter(|d| d.contains(&host_needle) && d.contains(&port_needle))
        .cloned()
        .collect();
    assert!(
        leaked.is_empty(),
        "SearxNG {searx_host}:{searx_port} must be absent from worker egress decisions \
         (search must ride the broker); leaked: {leaked:?}"
    );

    let _ = handle.worker_mut().kill();
    pool.close().await;
}
```

- [ ] **Step 2: Mac gate — workspace build + clippy stay green** (the whole file is `#![cfg(target_os = "linux")]`, so on macOS it compiles to nothing; this only confirms no accidental macOS breakage)

Run: `source "$HOME/.cargo/env" && cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0, clean. (The e2e's real compile + run is on the DGX in Task 5.)

- [ ] **Step 3: Commit**

```bash
git add core/tests/web_search_firecracker_egress_e2e.rs
git commit -m "test(web-search): DGX-gated Firecracker VM egress e2e

Two #[ignore] tests: a direct-entry CONNECT-to-stub gate (VM boot + force-route +
vsock + CA) and a manager-level VM x search-broker live e2e (real results in a VM
with zero direct worker egress, driving SingleUseLifecycle::acquire).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: DGX native-Linux verification (the acceptance gate)

Compile + run everything Linux on the DGX (aarch64, real KVM + vsock + live PG + live SearxNG). This is where the Task 2 Linux-gated unit tests and the Task 4 e2es get their RED→GREEN. Drive the DGX from the Mac as exactly `ssh dgx '<cmd>'` (the allow-rule is a prefix match — flags before the hostname get denied; see the memory note `dgx-native-linux-verification-over-ssh`). Write run logs to `~` not `/tmp` (the DGX scrubs `/tmp` mid-run — memory note `dgx-run-logs-tmp-scrubbed`).

**Files:** none (verification; fix-commits only if a defect surfaces).

- [ ] **Step 1: Sync the branch to the DGX and build**

```bash
ssh dgx 'cd ~/src/kastellan && git fetch --quiet origin && git checkout feat/web-search-microvm-entry && git pull --quiet --ff-only'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo build --workspace 2>&1 | tail -5'
```
Expected: build exit 0 (this is the FIRST real compile of the `#[cfg(target_os="linux")]` manifest code).

- [ ] **Step 2: Run the Linux-gated web_search unit tests (Task 2 RED→GREEN)**

```bash
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core web_search -- --nocapture 2>&1 | tail -25'
```
Expected: PASS — the 3 new Linux-gated tests (`resolve_uses_direct_microvm_entry_when_opted_in`, `resolve_uses_broker_microvm_entry_when_both_opted_in`, `resolve_vm_entry_still_injects_batch_cap`) now run and pass, plus the 8 host-path tests.

- [ ] **Step 3: Build the release launcher + the web-search rootfs**

```bash
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo build --release -p kastellan-microvm-run 2>&1 | tail -3'
ssh dgx 'export PATH=$HOME/.local/bin:$PATH && source ~/.cargo/env && cd ~/src/kastellan && bash scripts/workers/microvm/build-web-search-rootfs.sh 2>&1 | tail -5'
```
Expected: `built /var/lib/kastellan/microvm/web-search.ext4 (+ shared …/vmlinux)`.

- [ ] **Step 4: Run the two DGX e2e tests**

Ensure a live SearxNG is up (the DGX runs `kastellan-searxng` on `127.0.0.1:8888`; start it if needed). Then:

```bash
ssh dgx 'export PATH=$HOME/.local/bin:$PATH && source ~/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --test web_search_firecracker_egress_e2e -- --ignored --nocapture 2>&1 | tail -40'
```
Expected: both tests GREEN — `web_search_vm_reaches_proxy_with_ca_delivered` (stub receives `CONNECT searx.example.org:8888`) and `brokered_web_search_vm_returns_results_with_zero_egress` (non-empty `results`, SearxNG `127.0.0.1:8888` absent from `[egress-decision]` lines). 0 `[SKIP]`.

If the live broker e2e fails for an environmental reason (SearxNG down, etc.) rather than a code defect, and it cannot be resolved in-session, fall back to Approach B: mark `brokered_web_search_vm_returns_results_with_zero_egress` verification as deferred, file a follow-up GitHub issue, and keep the direct-entry gate as the acceptance proof. Record the decision in the handover. (The broker×VM code path stays covered by the Task 2 Linux-gated unit tests.)

- [ ] **Step 5: Full-workspace regression + clippy on the DGX** (write the log to `~`, not `/tmp`)

```bash
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && setsid bash -lc "cargo test --workspace > ~/ws-vm-search.log 2>&1; echo DONE_EXIT=\$? >> ~/ws-vm-search.log" </dev/null & echo launched'
# poll until DONE_EXIT appears, then:
ssh dgx 'grep -E "test result|DONE_EXIT|FAILED" ~/ws-vm-search.log | tail -30'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'
```
Expected: full workspace `cargo test` all green, **0 `[SKIP]`**; clippy clean. New baseline = **2516/0/44 + 3 new unit tests + 2 new ignored e2es = 2519/0/46** (adjust if the exact counts differ; record the observed numbers).

- [ ] **Step 6: (only if Step 4/5 surfaced a code defect) fix + commit + re-verify**, then continue. No commit if all green.

---

### Task 6: Update HANDOVER.md + ROADMAP.md, push, open the PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Update HANDOVER.md** — set the header "Latest work" to this feature (branch `feat/web-search-microvm-entry`, PR number after opening), with the verification results (Mac + DGX baselines, the observed test counts from Task 5). Move the previous "Next TODO" web-search-VM option to "done"; write a fresh "Next TODO (pick one)" (candidates: browser-driver VM entry — heavier; Phase-2 IMAP/Telegram channels — needs a spec; the file-split backlog — re-`wc -l` first). Update "Current state" `main` HEAD after merge. Bump the "Last updated" date. Keep it concise (prune per the bottom-of-file checklist; stay under ~500 lines).

- [ ] **Step 2: Update ROADMAP.md** — add a terse one-line `[x]` entry under the egress/VM phase recording the web-search Firecracker entry (commit/PR hash, DGX baseline), mirroring the web-fetch/web-research VM-entry lines. Note it as the 3rd Mechanism-2 consumer proving the #446 broker channel is `BrokerKind`-agnostic.

- [ ] **Step 3: Push + open the PR**

```bash
git push -u origin feat/web-search-microvm-entry
gh pr create --base main --title "web-search Firecracker micro-VM entry (direct + VM x search-broker)" --body "$(cat <<'EOF'
Generalizes the #445/#448 VM-worker + host-sidecar/broker mechanism (Mechanism 2)
to web-search as a 3rd production consumer (after web-fetch, web-research).

## What
- `web_search_firecracker_entry` (direct VM) + `web_search_firecracker_broker_entry`
  (VM x search-broker) + a Linux-gated `resolve()` short-circuit gated on
  `KASTELLAN_WEB_SEARCH_USE_MICROVM=1` (2x2 with `USE_BROKER`). Host path
  byte-identical when unset. The VM branch also threads the `web.search_batch` cap env.
- `scripts/workers/microvm/build-web-search-rootfs.sh` -> `web-search.ext4`.
- DGX-gated `web_search_firecracker_egress_e2e.rs`: a direct-entry CONNECT gate +
  a manager-level VM x search-broker live e2e (real results in a VM, zero direct egress).
- Test-lift of `web_search.rs` tests -> `web_search/tests.rs` (rule #4).

## Why it's cheap
Reuses ALL #445/#446/#448 plumbing. The #446 broker vsock channel (port 1026) was
built `BrokerKind`-agnostic; this proves it (`BrokerKind::Search` rides it with zero
new sandbox/microvm/core plumbing). No worker-binary change.

## Verification
- Mac (Seatbelt, rustc 1.96): host-path web_search tests green, `cargo build --workspace`
  + `cargo clippy --workspace --all-targets -D warnings` clean.
- DGX (native aarch64, real KVM+vsock+PG+live SearxNG): Linux-gated unit tests green,
  both e2es GREEN, full workspace `cargo test` <BASELINE> + clippy clean, 0 [SKIP].

Spec: `docs/superpowers/specs/2026-07-15-web-search-firecracker-microvm-entry-design.md`
Plan: `docs/superpowers/plans/2026-07-15-web-search-firecracker-microvm-entry.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Commit the doc updates** (if not already committed before the PR body references the number — commit then `gh pr edit` the number in, or push after)

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: HANDOVER + ROADMAP for web-search Firecracker micro-VM entry

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git push
```

- [ ] **Step 5: Do NOT self-merge.** Leave the PR AWAITING REVIEW+MERGE (project convention). Report the PR URL + the observed DGX baseline to the operator.

---

## Self-Review (author checklist — completed)

**Spec coverage:** §5.1 manifest (consts + 2 fns + resolve 2×2 + batch-cap threading) → Task 2. §5.2 rootfs → Task 3. §5.3 test-lift → Task 1. §6.1 unit tests → Task 2 (Linux-gated). §6.2 e2e (both tests, manager-level broker harness) → Task 4. §7 verification (Mac + DGX) → Tasks 2/5. §9 deliverables → all tasks + Task 6 docs/PR. No gaps.

**Placeholder scan:** No TBD/TODO/"add error handling"/"similar to Task N". Every code step shows complete code. The only non-literal is the final DGX baseline count (2519/0/46), explicitly flagged as "record the observed numbers" — a measured value, not a placeholder.

**Type consistency:** `web_search_firecracker_entry(binary, image_dir, endpoint, allowlist)` and `web_search_firecracker_broker_entry(binary, image_dir, endpoint)` — signatures identical in Task 2 (definition), Task 4 (use), and the spec's Interfaces. `BrokerConfigs { search: Some(...), ..Default::default() }`, `BrokerKind::Search`, `SingleUseLifecycle::with_force_routing`, `ForceRoutingConfig::new(proxy_bin, root, make_sink, None)`, `DecisionSinkFactory`, `EgressAuditRow` — all verified against `web_research_vm_force_route_daemon_e2e.rs` + `core/src/broker/config.rs`. `KASTELLAN_MICROVM_ROOTFS=web-search.ext4` consistent across the manifest, the rootfs script, and `firecracker_image()`.
