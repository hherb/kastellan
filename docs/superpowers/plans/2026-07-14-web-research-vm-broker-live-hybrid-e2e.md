# web-research VM×broker live hybrid e2e — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the deferred live full-stack `#[ignore]` e2e (issue #445) proving a web-research worker in a Firecracker VM ranks `hybrid` by embedding through the host broker over vsock port 1026, with the embed host absent from egress — unblocked by a minimal `sidecar_backend` seam.

**Architecture:** Give `NetWorkerSpawn`/`spawn_net_worker` a host `sidecar_backend` (the egress-proxy sidecar always runs on the host; only the worker may run in a VM), mirroring the existing `NetTransportSpawn.sidecar_backend` split. All current callers pass the same backend for both (byte-identical). The e2e then force-routes a FirecrackerVm web-research worker (worker backend = VM, sidecar backend = host) onto a real MITM egress sidecar while the broker rides vsock 1026, and asserts `ranking == "hybrid"`.

**Tech Stack:** Rust 1.96, `kastellan-core`/`kastellan-sandbox`, Firecracker micro-VM backend, tokio, sqlx/Postgres, `#[ignore]` DGX-gated integration test.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new dependency is introduced by this plan.
- **Cross-platform: no OS-only regressions.** The seam is OS-agnostic; the e2e is `#![cfg(target_os = "linux")]` (VM-only, like every `_firecracker_egress_e2e.rs`).
- **Rust core; Python only inside sandboxed workers.** N/A here.
- **Every worker is sandboxed before it runs.** The seam does not add an unsandboxed path; the sidecar still runs under a real backend, the worker under its (VM) backend.
- **Files under 500 LOC where feasible.** `net_worker.rs` is ~393 LOC + one field; the e2e file grows from ~190 to ~430 LOC (under cap).
- **TDD; all tests pass before commit** (rule 6). Toolchain rustc **1.96.0**.
- **DGX driven over SSH as exactly `ssh dgx '<cmd>'`** (the allow-rule is a prefix match — no flags before the hostname).
- **No unverified VM e2e body ships:** the PR opens only after the live test is GREEN on the DGX.

---

### Task 1: The `sidecar_backend` seam (production + all callers)

Add a host `sidecar_backend` to `NetWorkerSpawn` and route the egress-proxy
sidecar spawn through it. All 9 existing constructors pass the same backend for
both → byte-identical. A new recording-double unit test pins that the sidecar
spawns under `sidecar_backend` (not the worker `backend`). Fully Mac-verifiable.

**Files:**
- Modify: `core/src/egress/net_worker.rs` (add field to `NetWorkerSpawn` ~line 31-45; route sidecar spawn ~line 194)
- Modify: `core/src/egress/net_worker/tests.rs` (add `RecordingBackend` + 1 test; add `sidecar_backend` to 4 constructors at lines 118, 151, 192, 222)
- Modify: `core/src/worker_lifecycle/force_route.rs:249` (add `sidecar_backend: backend`)
- Modify: `core/tests/egress_force_routing_e2e.rs` (add `sidecar_backend: backend.as_ref()` to constructors at lines 151, 261, 385)
- Modify: `core/tests/browser_driver_e2e.rs:288` (add `sidecar_backend: backend.as_ref()`)

**Interfaces:**
- Produces: `NetWorkerSpawn { backend, sidecar_backend, proxy_bin, spec, allowlist, worker_name, secret_fingerprints, cert_pins_json, disable_mitm }` — `sidecar_backend: &'a dyn SandboxBackend`. `spawn_net_worker`/`spawn_forced_net_worker` unchanged in signature; the sidecar now spawns under `params.sidecar_backend`, the worker under `params.backend`.
- Consumes: nothing new.

- [ ] **Step 1: Write the failing unit test** in `core/src/egress/net_worker/tests.rs` (append after the `FailBackend` block, ~line 102). This references `sidecar_backend`, which does not exist yet, so it will fail to compile:

```rust
/// A backend that records each spawn attempt and then refuses. Lets a test
/// observe WHICH backend `spawn_net_worker` used for the sidecar vs the worker
/// without a real sandbox — the refusal stops the flow cheaply.
struct RecordingBackend {
    calls: std::sync::atomic::AtomicUsize,
}
impl RecordingBackend {
    fn new() -> Self {
        Self { calls: std::sync::atomic::AtomicUsize::new(0) }
    }
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl SandboxBackend for RecordingBackend {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<std::process::Child, SandboxError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(SandboxError::Backend("test: spawn refused (recorded)".into()))
    }
}

#[test]
fn spawn_net_worker_spawns_sidecar_under_sidecar_backend() {
    // The egress-proxy sidecar must spawn under `sidecar_backend` (the host),
    // NOT `backend` (which may be a VM). The sidecar is spawned first; refused
    // here, it fails closed before the worker backend is ever touched — so
    // `sidecar_backend` sees exactly one spawn (the sidecar) and the worker
    // `backend` sees none. This pins the host-sidecar / VM-worker split the live
    // VM×broker e2e depends on.
    let sidecar_backend = RecordingBackend::new();
    let worker_backend = RecordingBackend::new();
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let spec = allowlist_spec(&policy);
    let allowlist = ["api.example.com:443".to_string()];
    let params = NetWorkerSpawn {
        backend: &worker_backend,
        sidecar_backend: &sidecar_backend,
        proxy_bin: Path::new("/nonexistent/egress-proxy"),
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "web-research",
        secret_fingerprints: &[],
        cert_pins_json: None,
        disable_mitm: false,
    };
    let scratch = tempfile::tempdir().unwrap();
    let res = spawn_net_worker(&params, scratch.path(), |_row| {});
    assert!(res.is_err(), "refused sidecar => fail-closed, no worker");
    assert_eq!(
        sidecar_backend.calls(),
        1,
        "the sidecar must spawn under sidecar_backend"
    );
    assert_eq!(
        worker_backend.calls(),
        0,
        "the worker backend must not be touched when the sidecar fails"
    );
}
```

- [ ] **Step 2: Run it — expect a COMPILE failure** (the `sidecar_backend` field does not exist yet):

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::net_worker 2>&1 | tail -20`
Expected: FAIL — `error[E0063]: missing field ... sidecar_backend` / `no field \`sidecar_backend\``.

- [ ] **Step 3: Add the `sidecar_backend` field to `NetWorkerSpawn`** in `core/src/egress/net_worker.rs`. Insert immediately after the `pub backend` field (after line 34, before `pub proxy_bin`):

```rust
    /// The HOST backend the egress-proxy sidecar runs under. The sidecar is the
    /// real-network egress boundary (`Net::ProxyEgress` + a real host route), so
    /// it ALWAYS runs on the host even when `backend` is a VM. On non-VM paths
    /// pass the same backend for both. Mirrors
    /// [`super::persistent_net::NetTransportSpawn::sidecar_backend`].
    pub sidecar_backend: &'a dyn SandboxBackend,
```

- [ ] **Step 4: Route the sidecar spawn through `sidecar_backend`** in `spawn_net_worker` (`core/src/egress/net_worker.rs` ~line 194). Change the first argument of `spawn_sidecar` from `params.backend` to `params.sidecar_backend`:

```rust
    // 1. Sidecar first; fail-closed on its Err (no worker without a proxy). The
    //    sidecar runs on the HOST backend (`sidecar_backend`) — it is the
    //    real-network egress boundary — while the worker (below) runs under
    //    `backend`, which may be a VM.
    let mut sidecar = spawn_sidecar(
        params.sidecar_backend,
        params.proxy_bin,
        params.allowlist,
        scratch,
        params.worker_name,
        params.cert_pins_json,
        params.disable_mitm,
        false, // short-lived: 1:1 with a single tool-call dispatch (issue #395)
    )
```

(The worker spawn at the bottom of `spawn_net_worker` — `spawn_worker(params.backend, &forced_spec)` — is left UNCHANGED.)

- [ ] **Step 5: Add `sidecar_backend` to every existing constructor** (all pass the same backend for both → byte-identical). Apply each edit:

`core/src/worker_lifecycle/force_route.rs` ~line 250 — after `backend,`:
```rust
                backend,
                sidecar_backend: backend,
```

`core/src/egress/net_worker/tests.rs` — in each of the 4 constructors (lines ~119, 152, 193, 223), after `backend: &backend,`:
```rust
        backend: &backend,
        sidecar_backend: &backend,
```

`core/tests/egress_force_routing_e2e.rs` — in each of the 3 constructors (lines ~152, 262, 386), after `backend: backend.as_ref(),`:
```rust
        backend: backend.as_ref(),
        sidecar_backend: backend.as_ref(),
```

`core/tests/browser_driver_e2e.rs` ~line 289 — after `backend: backend.as_ref(),`:
```rust
        backend: backend.as_ref(),
        sidecar_backend: backend.as_ref(),
```

- [ ] **Step 6: Run the seam tests — expect PASS**:

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::net_worker -- --nocapture`
Expected: PASS — including `spawn_net_worker_spawns_sidecar_under_sidecar_backend`, `spawn_net_worker_fails_closed_when_sidecar_unavailable`, `spawn_forced_net_worker_fails_closed_when_sidecar_unavailable`, `spawn_forced_net_worker_cleans_scratch_on_failure`, `net_worker_spawn_struct_carries_pins_field`, and the 4 `rewrite_worker_policy_*` tests.

- [ ] **Step 7: Build + clippy the whole crate (all targets, so the modified e2e files compile on Linux/CI too — on Mac they are `cfg`-excluded but the lib + non-VM tests still gate):**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`
Expected: exit 0, no warnings. (Note: `browser_driver_e2e.rs` and `egress_force_routing_e2e.rs` are not `cfg(linux)`-gated, so their constructors DO compile on Mac and their edits are checked here; the VM e2e file is Linux-only and its compile is gated in Task 3.)

- [ ] **Step 8: Verify the two existing hermetic pins still pass** (guards no regression to the broker-policy chain):

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --tests 2>&1 | tail -3` (on Mac the `web_research_firecracker_broker_e2e` file is `cfg(linux)`-excluded, so this only confirms the crate's test targets still build). The hermetic pins run on the DGX in Task 3.

- [ ] **Step 9: Commit**

```bash
git add core/src/egress/net_worker.rs core/src/egress/net_worker/tests.rs \
        core/src/worker_lifecycle/force_route.rs \
        core/tests/egress_force_routing_e2e.rs core/tests/browser_driver_e2e.rs
git commit -m "feat(egress,#445): host sidecar_backend for spawn_net_worker (VM-worker MITM force-route)

Add a host \`sidecar_backend\` to NetWorkerSpawn so the egress-proxy sidecar can
run on the host while the worker runs in a VM — mirroring NetTransportSpawn's
split. All current callers pass the same backend for both (byte-identical). New
recording-double unit test pins the sidecar routes to sidecar_backend.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: The live `#[ignore]` VM×broker hybrid e2e

Append the live tier to `core/tests/web_research_firecracker_broker_e2e.rs`:
harness helpers + one `#[ignore]` test. Replace the module-doc "DEFERRED to
issue #445" paragraph with a one-line pointer to the live test. This code is
`#![cfg(target_os = "linux")]` and cannot be compiled on the Mac (the `ring`
C-dep cross wall) — its first compile + run is the DGX gate (Task 3).

**Files:**
- Modify: `core/tests/web_research_firecracker_broker_e2e.rs` (add imports + helpers + the live test; edit the module doc)

**Interfaces:**
- Consumes (from Task 1): `kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn}` with the `sidecar_backend` field.
- Consumes (existing): `kastellan_core::broker::{spawn_broker, BrokerConfig, BrokerKind}`, `kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker`, `kastellan_core::workers::web_research::web_research_firecracker_broker_entry`, `kastellan_core::tool_host::{dispatch, WorkerSpec}`, `kastellan_core::secrets::Vault`, `kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker}`, `kastellan_sandbox::{Net, SandboxBackend, SandboxBackendKind, SandboxBackends}`, `kastellan_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary}`.

- [ ] **Step 1: Edit the module doc.** In `core/tests/web_research_firecracker_broker_e2e.rs`, replace the deferred paragraph (the `* (DGX-only, #[ignore], authored on the DGX in the gate run) …` bullet plus its **DEFERRED to issue #445** sentence, roughly lines 21-36) with:

```rust
//! * `brokered_vm_worker_ranks_hybrid_over_vsock_with_zero_embed_egress`
//!   (DGX-only, `#[ignore]`): the live full-stack tier. Real KVM + vsock + a
//!   real egress-proxy sidecar (SearxNG + content over vsock 1025) + the real
//!   host broker (embed over vsock 1026 → a live embed backend). A web-research
//!   worker booted in a Firecracker VM ranks passages `"hybrid"` with the embed
//!   host absent from egress — strictly stronger than Slice C's host-mode test
//!   because the worker is VM-isolated and its embed reaches the host broker
//!   only over vsock port 1026. Composed from
//!   `web_research_firecracker_egress_e2e.rs` (force-routed web-research VM +
//!   in-guest CA), `net_demo_firecracker_egress_e2e.rs` (host sidecar backend vs
//!   VM worker backend), and `embed_broker_egress_e2e.rs` (`spawn_broker` +
//!   hybrid assertion). Unblocked by the `NetWorkerSpawn.sidecar_backend` seam
//!   (a host egress proxy in front of a VM worker). Closes issue #445.
```

Keep the "Containment vs functionality" section unchanged.

- [ ] **Step 2: Add imports.** After the existing `use` block (~line 55), extend it so the file imports:

```rust
use std::io::{BufRead, BufReader}; // (only if needed by helpers; remove if unused)
use std::path::PathBuf;
use std::sync::Arc;

use kastellan_core::broker::{spawn_broker, BrokerConfig, BrokerKind};
use kastellan_core::secrets::Vault;
use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
use kastellan_core::tool_host::{dispatch, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_research::{
    web_research_firecracker_broker_entry,
};
use kastellan_sandbox::linux_firecracker::{
    build_launch_plan, FirecrackerImage, LinuxFirecracker, BROKER_VSOCK_PORT,
};
use kastellan_sandbox::{Net, SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
};
```

(`build_launch_plan`/`BROKER_VSOCK_PORT`/`Net`/`SandboxBackendKind` are already used by the hermetic pins — merge, do not duplicate. Drop the `std::io` line if the final helpers don't use it.)

- [ ] **Step 3: Add the harness helpers** (mirrored from the sibling VM e2es), appended after the two hermetic pins:

```rust
// ── live-tier harness (DGX-only) ────────────────────────────────────────────

/// Default SearxNG endpoint (loopback). In force-routed mode the egress proxy
/// reaches it via its literal-IP allowlist carve-out — the net_entries derived
/// from this endpoint include the literal `127.0.0.1:8888`. Override with
/// `KASTELLAN_WEB_RESEARCH_ENDPOINT` if SearxNG lives on a routable host.
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";
/// Default embed backend (loopback Ollama). Reached ONLY by the host broker;
/// the worker never has it in egress.
const DEFAULT_EMBED_ENDPOINT: &str = "http://127.0.0.1:11434/v1/embeddings";

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join("web-research.ext4"),
    }
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
        eprintln!("\n[SKIP] firecracker probe failed (need web-research.ext4 + KVM + vsock): {e}\n");
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

/// The VM backend the worker runs under.
fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

/// The HOST backend (bwrap on Linux) for the egress-proxy sidecar AND the embed
/// broker — both are host-side services, never in the VM.
fn host_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(None, None)
}

/// Resolve the host egress-proxy binary, or `[SKIP]` (return `None`).
fn egress_proxy_bin_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
        None
    }
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-research-vm-broker-hybrid-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Bare host of a URL (for the content allowlist entry).
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}
```

- [ ] **Step 4: Add the live test.** Append:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-research rootfs + egress-proxy \
            sidecar + live SearxNG + live embed backend (embeddinggemma). \
            Asserts hybrid ranking from inside a VM with the embed host absent \
            from egress — embed rides vsock 1026 to the host broker."]
async fn brokered_vm_worker_ranks_hybrid_over_vsock_with_zero_embed_egress() {
    if skip_if_no_microvm() {
        return;
    }
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else { return };

    let worker_path = workspace_target_binary("kastellan-worker-web-research");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] web-research worker binary not built; run cargo build --workspace\n");
        return;
    }
    let broker_bin = workspace_target_binary("kastellan-worker-embed-broker");
    if !broker_bin.exists() {
        eprintln!("\n[SKIP] embed-broker binary not built; run cargo build --workspace\n");
        return;
    }

    let searx_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());
    let embed_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_EMBED_ENDPOINT.to_string());

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wrb-d",
        "wrb-l",
        &format!("kastellan-supervisor-test-pg-wrbroker-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let vm_backend = firecracker_backend();
    let host_backend = host_backend();

    // 1. Spawn the real embed broker on the HOST, pointed at the live backend.
    //    Its scratch (and UDS) live under /tmp so the VMM jail can bind the UDS
    //    (confine.rs) and the second vsock relay (port 1026) can reach it.
    // 2. Build the VM×broker manifest entry: embed host absent from egress,
    //    broker spec carries the backend the broker forwards to.
    // 3. Rewrite the policy onto the bound broker UDS (real production rewrite).
    // 4. Force-route the VM worker onto a HOST MITM egress sidecar; broker_uds
    //    survives the clone, so both vsock channels (1025 egress, 1026 broker)
    //    are live. The sidecar delivers its per-instance CA in-guest (the
    //    web-research rootfs ships no system CA).
    // 5. Dispatch web.research and assert hybrid ranking.
    //
    // Allowlist: the SearxNG endpoint host (validate_endpoint + net_entries →
    // the literal 127.0.0.1:8888 for the proxy carve-out) + the content host.
    // NOT the embed host.
    let content_host = "en.wikipedia.org".to_string();
    let allowlist = vec![url_host(&searx_endpoint), content_host];

    let entry = web_research_firecracker_broker_entry(
        worker_path.clone(),
        image_dir(),
        &searx_endpoint,
        &embed_endpoint,
        None, // default embed model (embeddinggemma)
        &allowlist,
    );
    let broker_spec = entry
        .broker
        .as_ref()
        .expect("VM broker-mode entry declares a broker spec");

    let broker_cfg = BrokerConfig::new(BrokerKind::Embed, broker_bin.clone(), std::env::temp_dir());
    let (broker_sidecar, broker_uds) = spawn_broker(&broker_cfg, broker_spec, &*host_backend)
        .expect("spawn embed-broker sidecar under the host sandbox");
    assert!(broker_uds.exists(), "broker must bind its UDS at {broker_uds:?}");

    let policy = rewrite_policy_for_broker(entry.policy, &broker_uds, BrokerKind::Embed);

    // Zero-embed-egress, re-asserted on the live policy (fail-closed on a
    // non-Allowlist variant — matches the hermetic pin).
    match &policy.net {
        Net::Allowlist(entries) => assert!(
            entries.iter().all(|e| !e.starts_with("127.0.0.1:11434")),
            "embed host must be absent from egress; got {entries:?}"
        ),
        other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
    }

    let worker_str = worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: Some(60_000),
    };
    let params = NetWorkerSpawn {
        backend: &*vm_backend,             // worker → VM
        sidecar_backend: &*host_backend,   // egress proxy → host
        proxy_bin: &proxy_bin,
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "web-research",
        secret_fingerprints: &[],
        cert_pins_json: None,
        disable_mitm: false, // MITM: deliver the per-instance CA into the VM
    };
    let mut worker = spawn_forced_net_worker(&params, std::path::Path::new("/tmp"), |_row| {})
        .expect("force-route the VM web-research worker onto a host MITM egress sidecar");

    let result = dispatch(
        &pool,
        &Vault::new(),
        &mut worker,
        "web-research",
        "web.research",
        serde_json::json!({"query": "rust programming language", "max_sources": 2}),
    )
    .await
    .expect("web.research round trip (VM search + fetch over vsock 1025 + brokered embed over vsock 1026)");

    assert_eq!(
        result["ranking"], "hybrid",
        "expected hybrid ranking via the broker over vsock 1026 (embed host absent from egress); \
         embed_note: {:?}",
        result.get("embed_note")
    );

    let _ = worker.close();
    drop(broker_sidecar);
    pool.close().await;
}
```

- [ ] **Step 5: Commit** (the body is verified live in Task 3; commit it now so the DGX pulls a single branch):

```bash
git add core/tests/web_research_firecracker_broker_e2e.rs
git commit -m "test(#445): live VM×broker hybrid e2e — embed over vsock 1026, zero embed egress

Append the deferred live #[ignore] tier: a web-research worker booted in a
Firecracker VM ranks hybrid by embedding through the host broker over vsock
1026, with SearxNG/content over the egress sidecar (vsock 1025) and the embed
host absent from egress. Uses the Task-1 sidecar_backend seam (host proxy, VM
worker). DGX-gated; verified live next.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: DGX live gate (I drive over `ssh dgx`)

First compile + run of the Linux-only e2e, on real KVM. Iterate on the Section-3
SearxNG-through-proxy wrinkle here if the search doesn't reach SearxNG. Only when
the live test is GREEN does the PR open.

**Files:** none (verification + possible Task-2 fixes).

- [ ] **Step 1: Sync the branch to the DGX** (the DGX pulls the branch; commands run as `ssh dgx '<cmd>'`). Confirm services first:

```bash
ssh dgx 'cd ~/src/kastellan && git fetch --quiet && git checkout feat/web-research-vm-broker-live-hybrid-e2e && git pull --quiet --ff-only && git log --oneline -3'
ssh dgx 'curl -s -o /dev/null -w "searxng:%{http_code}\n" "http://127.0.0.1:8888/search?q=test&format=json"; curl -s -o /dev/null -w "ollama:%{http_code}\n" http://127.0.0.1:11434/api/tags'
```
Expected: the branch HEAD; `searxng:200` + `ollama:200` (bring them up if not — SearxNG via `scripts/web-search/setup-searxng.sh`, Ollama `ollama serve` with `embeddinggemma` pulled).

- [ ] **Step 2: Build the DGX artifacts** (release launcher + host proxy + rootfs + workspace):

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo build --release -p kastellan-microvm-run && cargo build -p kastellan-worker-egress-proxy && cargo build --workspace 2>&1 | tail -5'
ssh dgx 'cd ~/src/kastellan && export PATH=$HOME/.local/bin:$PATH && bash scripts/workers/microvm/build-web-research-rootfs.sh 2>&1 | tail -5 && ls -la /var/lib/kastellan/microvm/web-research.ext4'
```
Expected: builds exit 0; `web-research.ext4` present + freshly dated.

- [ ] **Step 3: Run the hermetic pins + the live e2e** (write logs to `~`, not `/tmp` — a workspace run scrubs `/tmp`):

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo test -p kastellan-core --test web_research_firecracker_broker_e2e -- --include-ignored --nocapture > ~/wrb-e2e.log 2>&1; echo EXIT=$?; tail -40 ~/wrb-e2e.log'
```
Expected: both hermetic pins pass; `brokered_vm_worker_ranks_hybrid_over_vsock_with_zero_embed_egress` prints a VM boot then PASSes with `ranking == "hybrid"`, 0 `[SKIP]`. `EXIT=0`.

If it fails at the search step (no CONNECT to SearxNG / empty results), apply the Section-3 fallback: confirm the derived proxy allowlist carries the literal `127.0.0.1:8888` (inspect the egress decision audit rows / `~/wrb-e2e.log`); if the loopback carve-out does not fire, set `KASTELLAN_WEB_RESEARCH_ENDPOINT` to a routable DGX address for SearxNG and re-run. Fix the test body in Task 2 accordingly, recommit, re-sync, re-run.

- [ ] **Step 4: Full-workspace regression gate** on the DGX:

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && setsid bash -lc "cargo test --workspace > ~/wrb-workspace.log 2>&1; echo DONE_EXIT=\$? >> ~/wrb-workspace.log" </dev/null & echo launched'
```
Then poll: `ssh dgx 'tail -3 ~/wrb-workspace.log; grep -c "^test result" ~/wrb-workspace.log'` until `DONE_EXIT=0`.
Expected: `NNNN passed; 0 failed`, 0 `[SKIP]`. Record the new baseline (was 2513/0/42 + this feature's ~1 unit test + 1 ignored e2e).

- [ ] **Step 5: Clippy gate** on the DGX:

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5; echo CLIPPY_EXIT=${PIPESTATUS[0]}'
```
Expected: clean, `CLIPPY_EXIT=0`.

---

### Task 4: Docs, memory, PR (session wrap-up)

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`
- Create (if warranted): a memory note on the seam / VM-MITM-force-route facts
- GitHub: PR + close #445

- [ ] **Step 1: Update ROADMAP** — tick the #445 line under the web-research/embed-broker arc: live VM×broker hybrid e2e DONE, with the DGX baseline delta and the `sidecar_backend` seam noted; add the out-of-scope follow-up (daemon-side VM force-routing) as a new bullet/issue.

- [ ] **Step 2: Update HANDOVER** — move this work into "Recently completed", record the seam + e2e + DGX baseline, and write a fresh "Next TODO (pick one)". Prune to stay concise.

- [ ] **Step 3: File the out-of-scope follow-up issue** — "daemon: force-route VM `Net::Allowlist` workers through a host MITM sidecar in the supervised deployment" (the seam enables it; the daemon default + live supervised validation is a separate slice).

- [ ] **Step 4: Commit docs, push, open the PR** linking #445 (only after Task 3 is GREEN):

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(#445): record the VM×broker live hybrid e2e + sidecar_backend seam"
git push -u origin feat/web-research-vm-broker-live-hybrid-e2e
gh pr create --base main --title "web-research VM×broker: live hybrid e2e + host sidecar_backend seam (#445)" --body "<summary + DGX gate results + Closes #445>"
```

## Self-Review

**Spec coverage:**
- Seam (spec §1) → Task 1. ✅
- Live e2e assembly (spec §2, 9 steps) → Task 2 Step 4 (broker spawn, entry, rewrite, force-route with both backends, zero-egress re-assert, dispatch, hybrid assert, teardown). ✅
- SearxNG-through-proxy wrinkle (spec §3) → Task 2 Step 3 comment + Task 3 Step 3 fallback. ✅
- Mac vs DGX verification (spec Testing) → Task 1 Steps 6-8 (Mac) + Task 3 (DGX). ✅
- Scope boundary / follow-up issue (spec) → Task 4 Step 3. ✅
- Risks (real-net flakiness, sun_path, carve-out) → addressed by `#[ignore]`, `/tmp` scratch roots, Task 3 Step 3. ✅

**Placeholder scan:** the PR `--body` is the only `<…>` placeholder — it is authored at PR time from the live DGX results (Task 3), intentionally not pre-written. No `TODO`/`TBD` in code steps; all code is complete.

**Type consistency:** `NetWorkerSpawn.sidecar_backend: &'a dyn SandboxBackend` (Task 1 Step 3) is consumed identically in Task 1 Step 5 (all callers) and Task 2 Step 4 (`sidecar_backend: &*host_backend`). `RecordingBackend::calls()` defined + used in Task 1 Step 1. `web_research_firecracker_broker_entry(worker, image_dir, searx, embed, model, &allowlist)` arg order matches the hermetic pins already in the file. `spawn_broker(&cfg, broker_spec, &*backend) -> (sidecar, uds)` and `BrokerConfig::new(kind, bin, scratch_root)` match `embed_broker_egress_e2e.rs`.
