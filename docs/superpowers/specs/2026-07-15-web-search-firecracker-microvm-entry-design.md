# web-search Firecracker micro-VM entry — design

**Date:** 2026-07-15
**Status:** Approved (brainstorming) → ready for implementation plan
**Branch (proposed):** `feat/web-search-microvm-entry`
**Related:** #445 / #446 / #448 (VM × host-sidecar/broker mechanism), #440 (search-broker sidecar), web-fetch VM entry (PR #375), web-research VM entry (PR #428) + VM×broker (PR #446)

## 1. Summary

Give the **web-search** worker an opt-in Firecracker micro-VM execution mode, so
the search worker can run under KVM kernel-level isolation (above its current
bwrap + seccomp + Landlock stack), reaching either a routable SearxNG (through
the host MITM egress sidecar) or a **loopback** SearxNG (through the host-side
**search-broker** over a second vsock channel, port 1026).

This generalizes the "worker-in-a-VM + sidecar/broker-on-the-host" mechanism
(Mechanism 2 — the `spawn_worker_maybe_forced` / `spawn_worker_with_optional_broker`
cold-spawn chokepoint with a host `sidecar_backend`, made the supervised default
by #448) to a **third production consumer** after web-fetch and web-research. It
also proves the #446 broker vsock channel is genuinely **broker-kind-agnostic**:
`BrokerKind::Search` rides port 1026 with **zero new sandbox / microvm / core
plumbing** — the sandbox crate's own comment already promises "a future
search-broker VM is free plumbing"
(`sandbox/src/linux_firecracker/plan.rs:376-378`).

Opt-in, **Linux-only**. When the gate is unset the host path is **byte-identical**
to today.

## 2. Goals

- A `web_search_firecracker_entry` (direct VM) and a
  `web_search_firecracker_broker_entry` (VM × search-broker), mirroring
  web-research's two `*_firecracker_entry` functions.
- A `KASTELLAN_WEB_SEARCH_USE_MICROVM=1` gate wired into `resolve()` as a
  Linux-gated short-circuit, giving the same 2×2 matrix web-research has
  (`USE_MICROVM × USE_BROKER`).
- A `scripts/workers/microvm/build-web-search-rootfs.sh` producing
  `web-search.ext4`.
- Mac-verifiable unit coverage (host paths) + DGX-verifiable Linux-gated unit
  coverage (VM entries) + a DGX-gated e2e file with **two** tests: a direct-entry
  CONNECT-to-stub gate and a **live broker e2e** (real results in a VM through the
  broker with zero worker egress).
- Bundle the pending `web_search.rs` test-lift (→ `web_search/tests.rs`) so the
  parent file's production stays under the 500-LOC guideline.

## 3. Non-goals (YAGNI)

- **No host-mode behavior change.** The existing direct + host-broker paths are
  untouched and byte-identical when `USE_MICROVM` is unset.
- **No new sandbox / microvm / broker code.** The VM broker channel (port 1026),
  the value-match env→guest-path rewrite, the `spawn_worker_with_optional_broker`
  chokepoint, the host `sidecar_backend` seam, and `BrokerKind::Search` all
  already exist. This change is a manifest entry + a rootfs script + tests.
- **No worker-binary change.** `kastellan-worker-web-search` already carries the
  `BrokeredSearchProvider` path (from #440); `web.search` and `web.search_batch`
  work unchanged inside the VM.
- **No macOS VM path** (Linux-only, like every existing VM entry).
- **browser-driver** stays out of scope (heavier — Chromium rootfs; a separate,
  likely multi-session effort).

## 4. Background — the mechanism being generalized

There are two distinct "net-worker-in-a-VM" mechanisms in the tree:

- **Mechanism 1** — `core/src/egress/persistent_net.rs`
  (`spawn_net_transport` / `NetClientTransport`): for **long-lived** workers that
  hold a connection and do their own TLS. Production consumer: the Matrix channel.
  `net-demo` is its test rig. **Not used here.**
- **Mechanism 2** — `core/src/worker_lifecycle/force_route.rs` +
  `core/src/egress/net_worker.rs` (`spawn_worker_maybe_forced`,
  `spawn_worker_with_optional_broker`, `NetWorkerSpawn.sidecar_backend`): for
  **single-use / cold-spawn** request→response workers. Production consumers:
  web-fetch and web-research. web-search is `SingleUse` too, so it belongs here.

web-search is already `SingleUse` + `WorkerNetClient` + endpoint-derived
`Net::Allowlist` — byte-for-byte the shape `web_fetch_firecracker_entry` already
implements. The lifecycle facades (`SingleUseLifecycle::acquire`,
`IdleTimeoutLifecycle::acquire`) already resolve the host `sidecar_backend` via
`resolve(None, None)` and pass it through the chokepoint, so a VM worker's
sidecar/broker automatically runs on the host with no per-worker wiring.

**Why the broker×VM path is "free plumbing":** in broker mode the worker is
injected `KASTELLAN_SEARCH_BROKER_UDS = <host broker UDS path>` by
`rewrite_policy_for_broker`, and `policy.broker_uds` is set to the same path. The
Firecracker plan (`build_launch_plan`) matches the env entry **by value** (the
unique per-worker host UDS path), not by a hardcoded kind-specific key, and
rewrites it to the in-guest relay path `/run/kastellan-broker.sock`; the launcher
(`microvm-run`) and guest init (`microvm-init`) run a generic reverse-relay on
port 1026 to the host broker UDS. None of this is embed-specific. So a
`BrokerSpec::search(endpoint)` VM worker reaches the host search-broker over the
identical channel the embed broker uses.

## 5. Design

### 5.1 Manifest — `core/src/workers/web_search.rs` (the only core file changed)

**New consts (Linux-gated, #144 rule — never referenced on macOS):**

```rust
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_SEARCH_USE_MICROVM";
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-search";
```

**New function — direct VM entry** (mirror of `web_fetch_firecracker_entry` +
`web_search_entry`):

```rust
#[cfg(target_os = "linux")]
pub fn web_search_firecracker_entry(
    binary: PathBuf,          // MICROVM_WORKER_BIN (in-rootfs path)
    image_dir: String,        // KASTELLAN_MICROVM_DIR
    endpoint: &str,
    allowlist: &[String],
) -> ToolEntry
```

- `fs_read: vec![]` (no NIC/local DNS; the per-instance MITM CA is appended at
  spawn by `rewrite_worker_policy`).
- `net: Net::Allowlist(net_entries_from_endpoint(endpoint))` (endpoint host:port,
  same as the host entry).
- `Profile::WorkerNetClient`, `cpu_ms: 15_000`, `mem_mb: 512` (matching
  web-research's VM entry; a VM needs a whole-OS baseline).
- env: the existing endpoint + `KASTELLAN_WEB_SEARCH_ALLOWLIST` env **plus**
  `KASTELLAN_MICROVM_DIR = image_dir` and
  `KASTELLAN_MICROVM_ROOTFS = "web-search.ext4"`.
- `sandbox_backend: Some(SandboxBackendKind::FirecrackerVm)`, `proxy_uds: None`
  (set at spawn), `broker: None`, `lifecycle: SingleUse`, `wall_clock_ms: Some(60_000)`.

**New function — VM × search-broker entry** (mirror of
`web_research_firecracker_broker_entry` + `web_search_broker_entry`):

```rust
#[cfg(target_os = "linux")]
pub fn web_search_firecracker_broker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,           // forwarded to the broker via BrokerSpec::search
) -> ToolEntry
```

- `fs_read: vec![]`, `net: Net::Allowlist(vec![])` (**empty** — the worker has
  zero direct egress; the broker holds the only route to SearxNG).
- No endpoint/allowlist env (the worker never reaches SearxNG directly); only the
  `KASTELLAN_MICROVM_DIR` + `KASTELLAN_MICROVM_ROOTFS` env.
- `broker: Some(crate::broker::BrokerSpec::search(endpoint))`.
- `proxy_uds: None`, `broker_uds: None` (both set at spawn), `FirecrackerVm`
  backend, `SingleUse`.

**`resolve()` — Linux-gated short-circuit** (inserted before host binary
discovery, mirroring web-research):

```rust
#[cfg(target_os = "linux")]
{
    let use_microvm = (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
    if use_microvm {
        let binary = PathBuf::from(MICROVM_WORKER_BIN);
        let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
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
```

**Correctness detail that differs from web-research:** web-search has the
`web.search_batch` size-cap env (`maybe_inject_max_batch`), applied after entry
construction in the host path. The VM branch **must also** thread it (as shown
above) so a batched search inside the VM still respects the operator cap
(`KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`). web-research has no batch cap, so this
is the one place the web-search 2×2 is not a literal copy.

**Resulting 2×2 matrix:**

| `USE_MICROVM` | `USE_BROKER` | entry | egress posture |
| --- | --- | --- | --- |
| 0 | 0 | `web_search_entry` (existing) | host worker, endpoint `Net::Allowlist`, force-routable |
| 0 | 1 | `web_search_broker_entry` (existing #440) | host worker, empty allowlist, host search-broker |
| 1 | 0 | `web_search_firecracker_entry` (**new**) | VM worker, endpoint `Net::Allowlist`, host MITM sidecar |
| 1 | 1 | `web_search_firecracker_broker_entry` (**new**) | VM worker, empty allowlist, host search-broker over vsock 1026 |

### 5.2 Rootfs — `scripts/workers/microvm/build-web-search-rootfs.sh`

Clone of `build-web-research-rootfs.sh`:

- Builds `kastellan-worker-web-search` + `kastellan-microvm-init` (`--release`).
- Stages init as PID1 (`/sbin/init`) and the worker at
  `/usr/local/bin/kastellan-worker-web-search` (matches `MICROVM_WORKER_BIN`).
- Copies the ldd shared-library closure for both binaries at their real absolute
  paths.
- **No Python, no system CA bundle** (MITM-only; the per-instance proxy CA is
  delivered per-spawn via the slice-3 RO-share; the broker/loopback path needs no
  CA at all).
- Output: `web-search.ext4` in the shared `KASTELLAN_MICROVM_DIR`, beside the
  shared `vmlinux` and the other worker rootfs images.

### 5.3 File-size housekeeping (rule #4)

`web_search.rs` is 542 LOC today; the new prod code adds ~120. To keep the
parent's production well under 500, **lift `#[cfg(test)] mod tests` into
`core/src/workers/web_search/tests.rs`** (via `#[path]` or a `mod tests;`
submodule, matching the established test-lift pattern — e.g. `force_route/tests.rs`,
`memory_l3/run/tests.rs`). Public API byte-identical; pure mechanical move plus
the new VM-entry test cases. This clears the "web_search.rs (542 LOC) test-lift"
item already noted on the file-split backlog.

## 6. Testing

### 6.1 Unit tests

- **Host-path (cross-platform, unchanged):** the existing web_search resolve
  tests continue to pass and stay in `web_search/tests.rs`.
- **VM-entry (Linux-gated → run on the DGX; `core` can't cross-compile to Linux
  on the Mac — the #144 `ring` wall):**
  - `resolve()` returns `web_search_firecracker_entry` for
    `USE_MICROVM=1, USE_BROKER=0`: `FirecrackerVm` backend, empty `fs_read`,
    endpoint `Net::Allowlist`, `KASTELLAN_MICROVM_ROOTFS=web-search.ext4`, no broker.
  - `resolve()` returns `web_search_firecracker_broker_entry` for
    `USE_MICROVM=1, USE_BROKER=1`: `FirecrackerVm` backend, **empty**
    `Net::Allowlist`, no endpoint env, `broker == BrokerSpec::search(endpoint)`.
  - batch-cap env is still injected in VM mode when
    `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` is set (the correctness detail in 5.1).

### 6.2 DGX-gated e2e — new `core/tests/web_search_firecracker_egress_e2e.rs`

Mirrors `web_research_firecracker_egress_e2e.rs` + `search_broker_egress_e2e.rs`:

- **`web_search_vm_reaches_proxy_with_ca_delivered` (`#[ignore]`, DGX):** a host
  `UnixListener` stub stands in for the egress proxy at the worker's `proxy_uds`;
  a force-routed web-search **direct** VM boots and one `web.search` is driven
  through it; assert the stub receives the worker's
  `CONNECT <searxng-host>:<port>` line. Proves VM boot + force-routing + vsock
  relay + CA delivery (the worker can only emit CONNECT after loading the in-guest
  CA). Non-443 endpoint port to make the assertion sharp.
- **`brokered_web_search_vm_returns_results_with_zero_egress` (`#[ignore]`,
  DGX, live):** the web-search **broker** VM (empty allowlist) reaches a live
  loopback SearxNG (`127.0.0.1:8888`) only through the host-side search-broker over
  vsock 1026; dispatch `web.search`; assert a non-empty `results` array **despite
  the worker having zero direct egress**. This proves the search-broker×VM
  generalization end-to-end. **Preferred harness:** drive the real cold-spawn
  chokepoint at the manager level — `SingleUseLifecycle::acquire` with force-routing
  + broker config (mirroring the #448 `web_research_vm_force_route_daemon_e2e.rs`),
  so it proves the *daemon* path (host sidecar/broker + VM worker) works for
  web-search, not just a hand-assembled spawn. **Acceptable fallback** if the
  manager harness proves awkward: hand-assemble (host `spawn_broker` + VM
  `spawn_worker` + `dispatch`, adapting `search_broker_egress_e2e.rs`) — still
  proves zero-egress results, just not the manager wiring.

Both `[SKIP]` cleanly (never fail) without PG / supervisor / worker binary /
sandbox / (for the live one) a live SearxNG — the standard e2e posture.

## 7. Verification plan

**Mac (Seatbelt, rustc 1.96):**
- Host-path web_search unit tests green.
- `cargo build --workspace` exit 0.
- `cargo clippy --workspace --all-targets -D warnings` clean.
- The `cfg(linux)` manifest block **cannot** be Mac-cross-checked (`core`'s `ring`
  C-dep = the #144 cross wall) → its Linux compile is DGX-verified, same as every
  prior VM entry.

**DGX (native aarch64, real KVM + vsock + live PG + live SearxNG):**
- `bash scripts/workers/microvm/build-web-search-rootfs.sh` → `web-search.ext4`.
- `cargo build --release -p kastellan-microvm-run` (stale-launcher gotcha).
- `export PATH=$HOME/.local/bin:$PATH` (firecracker off the non-interactive SSH
  PATH, else the e2e SKIP-as-passes).
- New Linux-gated unit tests green; both e2e tests green under `--ignored`.
- Full-workspace `cargo test` + `cargo clippy --workspace --all-targets -D warnings`
  clean, **0 `[SKIP]`**. New baseline = current 2516/0/44 + the new Linux-gated
  unit tests + 2 new ignored e2e tests.

## 8. Risks & mitigations

- **VM broker channel not actually kind-agnostic.** Mitigated: verified by reading
  `plan.rs` (value-match rewrite), `microvm-init` (`parse_broker_config` +
  generic `setup_relay`), `microvm-run` (`spawn_reverse_relay`), and the
  `BrokerKind`-parameterized `broker/` module. No embed-specific logic on the VM
  path. If a hidden coupling surfaces on the DGX, the fallback is to file it and
  land only the direct VM entry (still a valid Mechanism-2 generalization).
- **Live broker e2e finicky on the DGX** (Approach A). Fallback: drop to
  Approach B — keep the direct-entry CONNECT gate, file the live broker e2e as a
  follow-up issue (the #445 pattern). No code difference; the broker×VM path stays
  covered by the Linux-gated unit tests.
- **Loopback-SearxNG unreachable in direct VM mode.** By design: the direct VM
  entry force-routes through the SSRF-guarding proxy, which blocks loopback — a
  loopback SearxNG requires broker mode (`USE_BROKER=1`), exactly as web-research's
  loopback-embed caveat. Documented in the entry doc-comments; no operator warning
  channel today (parallels web-research issue #429).

## 9. Deliverables

1. `core/src/workers/web_search.rs` — two new `*_firecracker_entry` fns + consts +
   the Linux-gated `resolve()` short-circuit (with batch-cap threading); tests
   lifted to `web_search/tests.rs` + new VM-entry test cases.
2. `scripts/workers/microvm/build-web-search-rootfs.sh` — new rootfs builder.
3. `core/tests/web_search_firecracker_egress_e2e.rs` — new DGX-gated e2e (2 tests).
4. HANDOVER.md + ROADMAP.md updates; commit; PR to `main` linking #448's arc.
