# web-research Firecracker micro-VM entry — design

**Date:** 2026-07-09
**Status:** approved (brainstorming) → ready for implementation plan
**Depends on:** web-research composite worker (PR #419), EmbeddingRanker/HybridRanker
(PR #421), parallel fetch (PR #425); the Firecracker net-worker-in-a-VM machinery
from slice 4a/4b/5c (`rewrite_worker_policy`, vsock egress relay, `microvm-init`).

## Goal

Give the `web-research` composite worker an opt-in Firecracker micro-VM execution
mode, mirroring the existing `web-fetch` VM entry. This is the last non-trivial
web-research Slice-4 follow-up. When `KASTELLAN_WEB_RESEARCH_USE_MICROVM=1` (Linux
only), the jailed worker runs inside a Firecracker VM with no NIC and tunnels all
egress (SearxNG search, page fetches, optional embed POSTs) through the host-side
transparent-MITM egress sidecar over the slice-4a vsock relay — the same containment
web-fetch already has.

Non-goals: no change to host-mode behaviour (byte-identical when the gate is unset);
no new ranking/fetch logic; no macOS VM path (the `FirecrackerVm` backend is
Linux-only per the issue-#144 rule).

## Existing pattern being mirrored

`web-fetch` implements VM mode as a **runtime branch inside its single manifest**,
not a separate worker registration:

- `web_fetch_firecracker_entry(binary, image_dir, allowlist)` — `core/src/workers/web_fetch.rs:120-155`,
  `#[cfg(target_os = "linux")]`. Same as the host `web_fetch_entry` but `fs_read: vec![]`,
  in-rootfs binary, `sandbox_backend: Some(FirecrackerVm)`, env adds `KASTELLAN_MICROVM_DIR`
  + `KASTELLAN_MICROVM_ROOTFS=web-fetch.ext4`. Net stays `Net::Allowlist` (it still needs egress).
- The gate `KASTELLAN_WEB_FETCH_USE_MICROVM` is read **inside `WebFetchManifest::resolve`**
  (`web_fetch.rs:179-192`): if `== "1"`, short-circuit host binary discovery and return
  `Register(web_fetch_firecracker_entry(...))`.
- Spawn-time policy rewrite is the **shared** `rewrite_worker_policy` (`core/src/egress/net_worker.rs:147-172`):
  sets `proxy_uds`, drops `/etc/resolv.conf` from `fs_read` (proxy does DNS), injects the
  per-instance MITM CA into `fs_read` + announces `KASTELLAN_EGRESS_PROXY_CA`. web-fetch runs
  MITM (`ca = Some(_)`). No web-research-specific spawn code is needed — this already covers it.
- Rootfs: `scripts/workers/microvm/build-web-fetch-rootfs.sh` bakes the worker + `microvm-init`
  (PID1), an `ldd` closure, **no system CA bundle** (egress is MITM-only; the only trusted root
  is the per-instance proxy CA delivered per spawn), the shared anchor set incl. `/run`, into
  `web-fetch.ext4`.
- e2e: `core/tests/web_fetch_firecracker_egress_e2e.rs`, `#[ignore]` DGX-only, asserts the stub
  proxy receives a `CONNECT <host>:443` line — proving VM boot + force-routing + vsock relay + CA delivery.

`web-research` differs from `web-fetch` only in that it carries a **SearxNG endpoint**
(required) plus an **optional embed endpoint + embed model**, and a **union**
`Net::Allowlist` (endpoint ∪ embed ∪ content hosts). The VM entry must forward all of
that so hybrid ranking survives in VM mode.

## Changes

### 1. `web_research_firecracker_entry` (new, `core/src/workers/web_research.rs`, Linux-gated)

```rust
#[cfg(target_os = "linux")]
pub fn web_research_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry
```

Identical to `web_research_entry_with_embed` **except**:

| Field | host `_with_embed` | VM entry |
|---|---|---|
| `fs_read` | `[binary, /etc/resolv.conf, /etc/hosts, /etc/nsswitch.conf]` | `vec![]` (per-instance CA appended at spawn by `rewrite_worker_policy`) |
| `binary` | host-discovered | `/usr/local/bin/kastellan-worker-web-research` (in-rootfs) |
| `sandbox_backend` | `None` | `Some(SandboxBackendKind::FirecrackerVm)` |
| `env` | endpoint/allowlist [+ embed endpoint/model] | same **plus** `KASTELLAN_MICROVM_DIR=<image_dir>`, `KASTELLAN_MICROVM_ROOTFS=web-research.ext4` |

**Unchanged and preserved:** the union `net_entries(endpoint, embed_endpoint, allowlist)`
→ `Net::Allowlist`; `Profile::WorkerNetClient`; `cpu_ms: 15_000`; `mem_mb: 512`;
`wall_clock_ms: Some(60_000)`; `Lifecycle::SingleUse`; `proxy_uds: None` (set at spawn);
`persistent_store: None`. The embed env is forwarded exactly as the host entry builds it
(including the `"embeddinggemma"` default model when embed_endpoint is set).

To avoid duplicating the shared env/policy assembly between the two entries, factor the common
body into a private helper both call (host passes host `fs_read`/`binary`/`sandbox_backend: None`;
VM passes empty `fs_read`/rootfs binary/`FirecrackerVm` + the two extra `KASTELLAN_MICROVM_*` env
pairs). Keep the change minimal — the union `net_entries` and embed-env logic are already pure and
reused as-is.

### 2. Manifest branch (`WebResearchManifest::resolve`)

Insert, **before** the existing host `Resolution::Register`, the Linux short-circuit mirroring
`web_fetch.rs:179-192`:

```rust
#[cfg(target_os = "linux")]
{
    let use_microvm = (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
    if use_microvm {
        let binary = PathBuf::from(MICROVM_WORKER_BIN);
        let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
        return Resolution::Register(web_research_firecracker_entry(
            binary, image_dir, &endpoint, embed_endpoint.as_deref(),
            embed_model.as_deref(), &allowlist,
        ));
    }
}
```

New consts:
```rust
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_RESEARCH_USE_MICROVM";
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-research";
```

The VM path short-circuits host `discover_binary` entirely (mirrors web-fetch). No new
`WORKER_MANIFESTS` line — `WebResearchManifest` is already registered
(`core/src/registry_build.rs:26`).

### 3. `scripts/workers/microvm/build-web-research-rootfs.sh`

Cloned from `build-web-fetch-rootfs.sh`, changed for web-research:

- `cargo build --release -p kastellan-worker-web-research -p kastellan-microvm-init`
- install `kastellan-microvm-init` → `/sbin/init`; `kastellan-worker-web-research`
  → `/usr/local/bin/kastellan-worker-web-research` (must equal `MICROVM_WORKER_BIN`)
- `ldd` closure for both binaries
- **no system CA bundle** — same MITM-only rationale as web-fetch (SearxNG, embed, and
  content hosts are all reached through the per-instance MITM proxy CA delivered per spawn)
- same anchor set as web-fetch incl. the slice-4a `/run` egress-relay tmpfs mountpoint,
  kept in lockstep with `mounts.rs::SHARE_ANCHORS`
- `mkfs.ext4 -q -F -O ^has_journal -L web-research -d "$WORK" "$OUT_DIR/web-research.ext4" 256M`

`ROOTFS_MIB=256`, shared `OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"`,
shared pinned `vmlinux` — all unchanged from the web-fetch script.

### 4. `core/tests/web_research_firecracker_egress_e2e.rs`

Cloned from `web_fetch_firecracker_egress_e2e.rs`. Whole file `#![cfg(target_os = "linux")]`.
One `#[ignore]` DGX-only test `web_research_vm_reaches_proxy_with_ca_delivered`:

- skip-as-pass guards: `skip_if_no_microvm()` probing `LinuxFirecracker::probe` on
  `web-research.ext4` + locating `kastellan-microvm-run` (prepend its dir to PATH),
  `skip_if_no_supervisor`, `skip_if_sandbox_unavailable`, `pg_bin_dir_or_skip`
- bring up a PG cluster (dispatch needs a pool for the audit sink)
- mint a self-signed CA (`write_test_ca`, rcgen)
- bind a `UnixListener` stub proxy that echoes the first request line then 503s
- build the entry via `web_research_firecracker_entry(...)` with a fixed SearxNG endpoint
  (e.g. `https://searxng.example:8888`) and no embed endpoint; manually apply what
  `rewrite_worker_policy` does (set `proxy_uds`, push `KASTELLAN_EGRESS_PROXY_CA`, push CA
  into `fs_read`) — same as the web-fetch e2e
- spawn with the `FirecrackerVm` backend, drive one `dispatch(... "web-research", "web.research",
  {"query":"...","max_sources":1})`
- **sole assertion:** the stub receives a line starting with `CONNECT searxng.example:8888`
  within a bounded timeout — proving VM boot + force-routing + vsock relay + CA delivery to the
  first egress hop (the SearxNG search). Embed is not exercised (mirrors web-fetch's
  single-CONNECT assertion).

Header doc gives the DGX run recipe (build release launcher + rootfs, `export PATH`, run `--ignored`).

### 5. Docs — loopback-embed caveat

The manifest / VM-entry doc-comment notes: in VM mode the worker has no NIC and all egress
tunnels through the host-side proxy, which SSRF-blocks loopback/private IPs. So the *default*
embed endpoint (local Ollama `127.0.0.1:11434`) is **unreachable** in VM mode → the query embed
fails → the existing degrade-to-lexical-with-signal path sets `ranking:"lexical"` + an `embed_note`.
For hybrid ranking in VM mode, point `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` at a **routable**
embed host. Host mode is unaffected. This is a documented operational caveat, not a code branch.

## Testing

- **TDD unit (host, Mac):** a test asserting `web_research_firecracker_entry` produces the
  expected policy shape — empty `fs_read`, `sandbox_backend == FirecrackerVm`, rootfs binary,
  the two `KASTELLAN_MICROVM_*` env pairs present, and the union `Net::Allowlist` identical to
  the host entry's (endpoint ∪ embed ∪ content). Written before the entry (watch it fail on the
  missing fn). A second test pins that the embed env is forwarded into the VM entry.
- **Manifest branch test (host, Mac):** with a `get_env` stub returning `"1"` for
  `KASTELLAN_WEB_RESEARCH_USE_MICROVM`, `resolve` returns the VM entry (sandbox_backend set);
  unset → the host entry (sandbox_backend `None`, host `fs_read`). This is the byte-identical-when-unset guard.
- **e2e (DGX):** item 4 above — real KVM boot + CONNECT-to-proxy assertion.

## Verification plan

**Mac (dev box, Seatbelt, rustc 1.96):**
- `cargo build --workspace` exit 0
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo clippy -p kastellan-core --target aarch64-unknown-linux-gnu --lib -- -D warnings` — the
  only Mac-side compile of the `#[cfg(target_os="linux")]` VM entry + manifest branch (note: core
  has the `ring` C-dep cross wall for `--all-targets`/test; the `--lib` clippy is the achievable check —
  fall back to shipping the Linux compile to the DGX if the lib cross-clippy also hits the linker)
- `cargo test -p kastellan-worker-web-research` + `cargo test -p kastellan-core --lib workers::web_research` green

**DGX (native aarch64, real KVM, over `ssh dgx '<cmd>'`):**
- `cargo build --release -p kastellan-microvm-run` (fresh launcher — stale-launcher gotcha)
- `scripts/workers/microvm/build-web-research-rootfs.sh` → `/var/lib/kastellan/microvm/web-research.ext4`
- `export PATH=$HOME/.local/bin:$PATH` (firecracker off the non-interactive ssh PATH)
- `cargo test -p kastellan-core --test web_research_firecracker_egress_e2e -- --ignored --nocapture`
  → `web_research_vm_reaches_proxy_with_ca_delivered` GREEN (not skip-as-pass — confirm no `[SKIP]`)
- full-workspace regression optional: `cargo test --workspace` should hold the 2367/0/38 baseline
  (this change adds host unit tests + one DGX-gated e2e; the baseline count rises accordingly),
  `cargo clippy --workspace --all-targets -- -D warnings` clean natively

## Cross-platform / invariants

- All VM code is `#[cfg(target_os = "linux")]`; macOS never reads the gate and never compiles the
  `FirecrackerVm` variant (issue-#144 rule).
- Host path is byte-identical when `KASTELLAN_WEB_RESEARCH_USE_MICROVM` is unset.
- The worker is sandboxed before it runs (VM = stronger containment than bwrap); no unsandboxed
  escape hatch. Trust boundary unchanged: URLs come from SearxNG, every fetch is `hit_allowed` +
  egress-gated, and in VM mode egress is additionally force-routed through the MITM sidecar.
- The egress **sidecar** stays a host bwrap process (the 5c invariant); only the **worker** moves
  into the VM.

## File-size / structure

`core/src/workers/web_research.rs` is 279 LOC today; the VM entry + branch + shared helper +
2 unit tests will push it up but should stay under the 500-LOC cap. Re-check with `wc -l` at the
end; if it crosses, lift the tests to a sibling `web_research/tests.rs` (the established pattern) —
do not prod-split for this feature.
