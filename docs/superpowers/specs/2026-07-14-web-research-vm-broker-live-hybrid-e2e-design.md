# web-research VM×broker — live full-stack hybrid e2e (issue #445)

**Date:** 2026-07-14
**Status:** design approved, ready for implementation plan
**Issue:** [#445](https://github.com/hherb/kastellan/issues/445)
**Depends on (merged):** VM × embed-broker (PR #446, `17d81cc3`)

## Goal

Add the deferred **live** `#[ignore]` acceptance tier for the VM × embed-broker
feature: drive a real `web.research` through a web-research worker booted in a
Firecracker VM and assert `ranking == "hybrid"` **with the embed backend host
absent from the worker's egress** — proving embed bytes actually flow guest→host
over vsock **port 1026** to a live embed backend, end to end, while SearxNG +
content ride the egress proxy over vsock port 1025.

This is a **functionality** check. The **containment** property
(zero-embed-egress) is already verified hermetically in
`web_research_firecracker_broker_e2e.rs` and does not depend on the live channel
working — so this closes the last plumbing gap, not a security gap.

## The blocker (refined from the issue text)

The issue states the blocker is visibility (`spawn_forced_net_worker` is
`pub(crate)`). That is **stale**: `spawn_net_worker`, `spawn_forced_net_worker`,
and `NetWorkerSpawn` are all `pub` in the `pub mod egress::net_worker` tree and
already reachable from integration tests.

The **real** blocker: `spawn_net_worker` spawns the egress-proxy sidecar and the
worker under the **same** `backend`, so it cannot put a **host** MITM proxy in
front of a **VM** worker. Only the transparent-tunnel `spawn_net_transport`
(`egress::persistent_net`) has the host-`sidecar_backend` / VM-`backend` split
today — and it returns a long-lived `Client`-based `NetClientTransport`, not the
`SupervisedWorker` that the `dispatch` chokepoint takes.

Web-research needs **MITM** (its rootfs ships no system CA — it trusts only the
per-instance proxy CA), and it is a single-use dispatch worker, not a channel.

## Approaches considered

- **A — give `NetWorkerSpawn`/`spawn_net_worker` a host `sidecar_backend`
  (CHOSEN).** Mirrors the proven `NetTransportSpawn.sidecar_backend` split;
  keeps MITM (`disable_mitm: false`, per-instance CA already delivered); returns
  a `SupervisedWorker` so the e2e uses the real `dispatch` path; reuses the real
  production force-route + broker rewrite — no test replica.
- **B — add MITM+CA to `spawn_net_transport`.** It has the backend split but
  returns a persistent channel `Client`, forcing the e2e to bypass `dispatch`.
  Shape mismatch for a single-use worker. Rejected.
- **C — visibility-only widening + hand-wire the sidecar in the test.** The
  fns are already `pub`; hand-wiring host-sidecar-for-VM replicates production
  logic — the anti-pattern the Slice C `/fixall` removed. Rejected.

## Design

### 1. Production seam (minimal — byte-identical for every current caller)

**`core/src/egress/net_worker.rs`** — add one field to `NetWorkerSpawn`:

```rust
/// The HOST backend the egress-proxy sidecar runs under. The sidecar is the
/// real-network egress boundary (Net::ProxyEgress + a real host route), so it
/// ALWAYS runs on the host even when `backend` is a VM. Pass the same backend
/// for both on non-VM paths. Mirrors `NetTransportSpawn.sidecar_backend`.
pub sidecar_backend: &'a dyn SandboxBackend,
```

In `spawn_net_worker`, change the sidecar spawn call
`spawn_sidecar(params.backend, …)` → `spawn_sidecar(params.sidecar_backend, …)`.
The worker still spawns under `params.backend`. Nothing else in the function
moves; `disable_mitm: false` already yields MITM + per-instance CA delivery via
`rewrite_worker_policy(policy, uds, Some(ca))`.

**Callers updated (all pass `backend` for both → byte-identical):**

- `core/src/worker_lifecycle/force_route.rs` — `spawn_worker_maybe_forced` adds
  `sidecar_backend: backend` to the `NetWorkerSpawn` literal. The only
  production constructor; host force-routing behaviour unchanged.
- `core/src/egress/net_worker/tests.rs` — the fail-closed unit tests construct
  `NetWorkerSpawn`; add `sidecar_backend` (same backend). Any other constructor
  found during implementation gets the same treatment.

No change to `rewrite_worker_policy`, `spawn_forced_net_worker`'s wrapper logic,
the sidecar `spawn_sidecar` signature, or the FirecrackerVm backend. The VM
worker's egress channel (proxy_uds → vsock 1025) and broker channel (broker_uds
→ vsock 1026) are both established by the FirecrackerVm backend from the policy,
independent of this seam.

### 2. Live e2e assembly

Append a third test to **`core/tests/web_research_firecracker_broker_e2e.rs`**
(replacing the "DEFERRED to issue #445" doc paragraph with the live body).
`#[ignore]`, DGX-only, `#![cfg(target_os = "linux")]` (already on the file).

Composed from the three templates the file's module doc names:
`web_research_firecracker_egress_e2e.rs` (force-routed web-research VM + in-guest
CA), `net_demo_firecracker_egress_e2e.rs` (host sidecar backend vs VM worker
backend; loopback literal-IP allowlist), `embed_broker_egress_e2e.rs`
(`spawn_broker` + the hybrid assertion).

Steps:

1. **Skip guards** (each `[SKIP]`-as-pass, never fail): `skip_if_no_microvm`
   (firecracker probe + locate/PATH-prepend the release `kastellan-microvm-run`),
   `skip_if_no_supervisor`, `skip_if_sandbox_unavailable`, `pg_bin_dir_or_skip`,
   and resolve the `kastellan-worker-egress-proxy` + `kastellan-worker-web-research`
   + `kastellan-worker-embed-broker` binaries.
2. Bring up the PG cluster; `probe_and_pool`.
3. `spawn_broker(BrokerConfig::new(Embed, broker_bin, /tmp), broker_spec, host_backend)`
   → `(broker_sidecar, broker_uds)` — the broker runs on the **host** backend on
   its own `Net::Allowlist([embed host])`.
4. `entry = web_research_firecracker_broker_entry(worker, image_dir, searx_endpoint,
   embed_endpoint, None, &allowlist)`; `broker_spec = entry.broker`; allowlist =
   `[searx host, en.wikipedia.org]` — the embed host is **not** included.
5. `policy = rewrite_policy_for_broker(entry.policy, &broker_uds, Embed)` — the
   real production rewrite (sets `broker_uds` + injects `KASTELLAN_EMBED_BROKER_UDS`).
6. `spawn_forced_net_worker(NetWorkerSpawn { backend: firecracker_vm,
   sidecar_backend: host, proxy_bin: &egress_proxy_bin, spec: &WorkerSpec { policy:
   &policy, program, args: &[], wall_clock_ms: Some(60_000) }, allowlist: &allowlist,
   worker_name: "web-research", secret_fingerprints: &[], cert_pins_json: None,
   disable_mitm: false }, /tmp, |_row| {})` → a `SupervisedWorker`: a VM worker
   force-routed onto a **host MITM** egress sidecar (proxy_uds + CA delivered
   in-guest via the `/tmp` RO-share), `broker_uds` preserved by the force-route
   clone → the second vsock channel (1026).
7. Re-assert on the live `policy`: the embed host is absent from
   `Net::Allowlist` (fail-closed `panic!` on any non-`Allowlist` variant, matching
   the hermetic pin).
8. `dispatch(&pool, &Vault::new(), &mut worker, "web-research", "web.research",
   {"query": "rust programming language", "max_sources": 2})` (the Slice C
   host-mode query, which reliably surfaces `en.wikipedia.org`) → assert
   `result["ranking"] == "hybrid"`,
   surfacing `result["embed_note"]` in the failure message ("lexical" ⇒ the broker
   channel failed at runtime).
9. Teardown: `worker.close()`, `drop(broker_sidecar)`, `pool.close()`, remove
   scratch.

The two existing hermetic pins are unchanged. Three vsock channels are live at
once: 1024 JSON-RPC, 1025 egress→host proxy, 1026 broker→host embed backend.

### 3. Known DGX wrinkle (resolve during the gate — not a design blocker)

In force-routed mode the egress proxy SSRF-blocks loopback, so the DGX's
`127.0.0.1:8888` SearxNG is not reachable naively. The established fix is the
proxy's **literal-IP allowlist carve-out** (net-demo dials `127.0.0.1:<port>` by
allowlisting the literal `127.0.0.1:8888`; see `egress_force_routing_e2e.rs`).
During DGX iteration, confirm web-research's `from_env` `validate_endpoint` +
`net_entries` align with a literal `127.0.0.1:8888` allowlist entry. If it fights
the carve-out, fall back to pointing SearxNG at a routable DGX address. Content
fetch (`en.wikipedia.org:443`) goes over real net — fine on the DGX (the
`spawn_sidecar` resolver bug is fixed, `e70174b`). This tier is real-net +
`#[ignore]`, the same manual-run posture as the Slice C host-mode test.

## Testing & verification

- **Mac (this session):** `cargo build -p kastellan-core`,
  `cargo clippy -p kastellan-core --lib --all-targets -D warnings`, the
  `egress::net_worker` unit tests, and the two existing hermetic pins in
  `web_research_firecracker_broker_e2e.rs`. The seam is fully Mac-verifiable; the
  live test is `cfg(linux)`-excluded on Mac.
- **DGX (driven over `ssh dgx`):** `export PATH=$HOME/.local/bin:$PATH`;
  `cargo build --release -p kastellan-microvm-run`;
  `cargo build -p kastellan-worker-egress-proxy`;
  `bash scripts/workers/microvm/build-web-research-rootfs.sh`; ensure live
  SearxNG (`kastellan-searxng` :8888) + Ollama (`embeddinggemma`) are up; run
  `cargo test -p kastellan-core --test web_research_firecracker_broker_e2e -- --ignored --nocapture`;
  then a full-workspace `cargo test` + `cargo clippy --workspace --all-targets -D warnings`.
- **Merge gate:** the PR opens only after the live test is **GREEN** on the DGX
  ("no unverified VM e2e body ships").

## Scope boundary

**In scope:** the `sidecar_backend` seam + the live `#[ignore]` e2e.

**Out of scope (→ new follow-up issue):** wiring the daemon so the supervised
deployment actually force-routes VM `Net::Allowlist` workers through a host MITM
sidecar. This seam makes that *possible* (`spawn_worker_maybe_forced` would pass
a host `sidecar_backend` when the worker is a VM); making it the daemon default,
with its own live supervised validation, is a separate production slice.

## Risks

- **Real-net flakiness:** the query relies on SearxNG returning a fetchable
  content host in the allowlist (`en.wikipedia.org`). `#[ignore]` + manual run
  mitigates; a `lexical` result with an `embed_note` in the failure message makes
  a broker-channel failure vs a no-content failure diagnosable.
- **SearxNG loopback carve-out** (Section 3) — the one wiring detail to settle on
  the DGX; a routable-host fallback exists.
- **sun_path length:** the sidecar UDS is `/tmp/egress-<pid>-<seq>/egress.sock`
  and the broker UDS `/tmp/embed-<pid>-<seq>/embed.sock`; both are short and under
  the `/tmp` SHARE_ANCHOR, satisfying `make_worker_scratch_dir`'s 104-byte guard.
