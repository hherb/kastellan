# web-research VM × embed-broker — a second vsock channel (design)

**Date:** 2026-07-13
**Status:** approved (brainstorm) → ready for implementation plan
**Author:** session hand-off (embed-broker arc, last orthogonal follow-up)
**Related:** `2026-07-09-web-research-firecracker-microvm-entry-design.md` (VM entry),
`2026-07-{09,10}-embed-broker-sidecar-*` (host-mode broker Slices A–C),
the slice-4a egress vsock tunnel.

## 1. Problem & goal

A `USE_MICROVM` web-research worker in a Firecracker VM can only reach the network
through the **slice-4a egress tunnel** (guest→host over vsock, to the host
egress-proxy sidecar). That proxy SSRF-blocks loopback / RFC1918 / CGNAT, so the
**default embed endpoint** (local Ollama `127.0.0.1:11434`) is unreachable from a
VM. Today the VM worker therefore degrades to **lexical-only** ranking with an
`embed_note` (documented; issue #429 tracks a resolve-time warning).

Host-mode already solved the identical problem with the **trusted embed-broker
sidecar** (Slices A–C): the broker runs host-side (so it *can* reach loopback),
the worker reaches it over a bound UDS, and the embed host is **dropped from the
worker's `Net::Allowlist` entirely**. Slice C proved the security-critical
property: *hybrid ranking with zero embed egress*.

**Goal:** extend the embed broker to VM mode, so a VM web-research worker gets
**hybrid ranking against a local embed backend with zero embed egress** — the
Slice-C property, now for VMs. The manifest currently **refuses**
`USE_EMBED_BROKER` + `USE_MICROVM` together (warns and ignores the broker gate,
`web_research.rs::resolve`); this feature makes the combination work.

### Non-goals

- **No generic "guest→host UDS tunnel" abstraction** (Approach B). We clone the
  proven egress channel as a parallel broker channel. Generalizing the two
  channels into one reusable multi-channel abstraction is deferred until a second
  broker-kind VM consumer (e.g. `search-broker` + VM) actually exists — it would
  refactor the working, DGX-proven egress path for no present benefit (YAGNI).
- **No `search-broker` + VM** this slice (embed only). The channel is built
  kind-agnostic (see §4) so that future consumer is free plumbing, but it is not
  wired.
- **No worker-binary change.** The in-VM `kastellan-worker-web-research` binary is
  byte-identical; the tunnel is transparent to it.
- **No rootfs *script* change.** The `/run` tmpfs already exists in every rootfs
  (the egress relay uses it). A rebuild is needed only because `microvm-init`
  changed.

## 2. Architecture — the broker as a third vsock port

Firecracker's hybrid-vsock device is **singular and port-multiplexed**. Two ports
ride it today: **1024** = JSON-RPC bridge (host→guest), **1025** = egress
(guest→host). This feature adds a **third port, 1026** = embed broker
(guest→host), over the *same* device. No second vsock device is created.

```
in-VM web-research worker  (UNCHANGED binary)
  reads env KASTELLAN_EMBED_BROKER_UDS = /run/kastellan-broker.sock   ← FC plan rewrote it to the guest path
     │
     ▼
/run/kastellan-broker.sock  (guest UDS, bound by microvm-init)        [NEW guest relay child, port 1026]
     │  AF_VSOCK connect(VMADDR_CID_HOST=2, port 1026)
     ▼
Firecracker hybrid-vsock  (one device; ports 1024/1025/1026 multiplexed)
     │
     ▼
<vsock_uds>_1026  (host, guest-initiated)  →  microvm-run reverse-relay  →  host broker UDS
     │
     ▼
embed-broker sidecar  (host process, Net::Allowlist=[embed host:port])  →  reaches loopback Ollama
```

The egress channel (1025) and broker channel (1026) are **independent and
coexist** on one VM: egress is `proxy_uds`-driven (flips off the NIC — a private
netns), broker is `broker_uds`-driven and has **no** netns effect (mirrors the
bwrap invariant `broker_uds_is_bound_without_touching_netns`). A force-routed VM
worker runs both simultaneously without interference.

### Spawn-time flow (unchanged chokepoint)

`spawn_worker_with_optional_broker` (`worker_lifecycle/force_route.rs`) is the
single cold-spawn chokepoint and needs **no change**:

1. **Broker spawns host-side** (`spawn_broker`) → host broker UDS. The broker's
   own `Net::Allowlist=[embed host:port]` lets it reach loopback (it is a host
   process, not force-routed).
2. **`rewrite_policy_for_broker`** sets `policy.broker_uds = <host UDS>` and injects
   `KASTELLAN_EMBED_BROKER_UDS = <host UDS path>` — **exactly as host mode**.
3. **`spawn_worker_maybe_forced`** → force-routing's `rewrite_worker_policy` sets
   `proxy_uds` + CA (egress), preserving the broker fields (struct clone) → the
   **Firecracker backend** `spawn_under_policy` → `build_launch_plan` now sees
   `broker_uds = Some(..)` and stands up the broker vsock channel, **overriding
   the worker's `KASTELLAN_EMBED_BROKER_UDS` env to the guest path**.

So the only *new* behaviour lives in the sandbox/launcher/guest layers plus the
`resolve()` branch; the core broker machinery is reused verbatim.

## 3. Components (changes by layer)

### 3.1 `sandbox/src/linux_firecracker/plan.rs`

- New constants: `BROKER_VSOCK_PORT: u32 = 1026`, `GUEST_BROKER_UDS: &str =
  "/run/kastellan-broker.sock"` (kept in sync with `microvm-init`, same manual
  cross-crate contract as `WORKER_VSOCK_PORT`/`EGRESS_VSOCK_PORT`).
- New `FirecrackerLaunchPlan` fields (parallel to the egress pair):
  `broker_vsock_port: Option<u32>`, `broker_host_uds: Option<PathBuf>`.
- In `build_launch_plan`: when `policy.broker_uds` is `Some(uds)`, set
  `broker_vsock_port = Some(BROKER_VSOCK_PORT)`, `broker_host_uds = Some(uds)`.
  **`broker_uds` does NOT affect the net/NIC/`net_enabled` decision** (the match
  at `plan.rs:238` on `(net, proxy_uds)` is untouched).
- **Guest env override (kind-agnostic — see §4):** when `broker_host_uds.is_some()`,
  rewrite the *one* worker-env entry whose **value equals `policy.broker_uds`'s
  path string** → `GUEST_BROKER_UDS`. (`rewrite_policy_for_broker` guarantees
  exactly one such entry: `KASTELLAN_EMBED_BROKER_UDS = <host UDS path>`.) This
  mirrors the egress override (`plan.rs:338-345`) but matches by value, not by a
  hardcoded key, so the sandbox crate stays broker-kind-agnostic.
- Cmdline token: emit ` kastellan.broker=1` when `broker_vsock_port.is_some()`
  (parallel to ` kastellan.egress=1`). Counts against the existing 2048-byte
  `COMMAND_LINE_SIZE` cap — small, no cap change expected.
- The rendered Firecracker config is **unchanged** (still one vsock device); the
  broker is a new *port*, surfaced host-side as `<vsock_uds>_1026`.

### 3.2 `sandbox/src/linux_firecracker.rs` (spawn / launcher argv)

- `launcher_argv`: when `broker_host_uds`/`broker_vsock_port` are `Some`, append
  `--broker-uds <host path>` + `--broker-vsock-port 1026` (parallel to
  `--egress-uds`/`--egress-vsock-port`).

### 3.3 `sandbox/src/linux_firecracker/confine.rs`

- In `build_vmm_jail_argv`: when `plan.broker_host_uds` is `Some`, `--bind` it
  host==jail path rw, so the confined (bwrap) launcher can reach the host broker
  UDS — identical to the egress-host-UDS bind at `confine.rs:114-120`.

### 3.4 `workers/microvm-run/`

- `egress_relay.rs`: the relay is already generic over `(base_uds, port,
  target_uds)`, and `parse_egress_relay_args(uds, port)` is already channel-neutral
  in signature (two `Option<String>` → `Option<(String, u32)>`), so **reuse both
  as-is**. Add a `--broker-uds`/`--broker-vsock-port` arg read in `main.rs` and
  call `spawn_egress_relay(&vsock_uds, 1026, broker_host_uds)` a **second time**,
  before booting Firecracker (the listener must exist before the guest dials).
  Optional clarity rename `spawn_egress_relay`→`spawn_reverse_relay` +
  `parse_egress_relay_args`→`parse_relay_args` (mechanical, body unchanged) — the
  plan decides; not load-bearing.

### 3.5 `workers/microvm-init/` (guest PID1)

- `cmdline.rs`: mirror the constants `BROKER_VSOCK_PORT = 1026`,
  `GUEST_BROKER_UDS = "/run/kastellan-broker.sock"`; parse `kastellan.broker=1`
  (a `BrokerConfig { enabled }`, parallel to `EgressConfig`).
- `guest/egress.rs` (or a new `guest/relay.rs`): **parameterize the hardcoded
  port.** Generalize `setup_egress_relay()` → `setup_relay(guest_uds: &str,
  vsock_port: u32)` and `relay_one_connection(conn)` →
  `relay_one_connection(conn, vsock_port)` (the port is hardcoded at
  `guest/egress.rs:163` today). Egress becomes `setup_relay(GUEST_EGRESS_UDS,
  EGRESS_VSOCK_PORT)`; broker becomes `setup_relay(GUEST_BROKER_UDS,
  BROKER_VSOCK_PORT)`. The `/run` tmpfs mount is made **idempotent / shared** so
  whichever channel sets up first mounts it (both are enabled in VM×broker).
- `main.rs`: `if broker.enabled { setup_relay(GUEST_BROKER_UDS,
  BROKER_VSOCK_PORT); }` alongside the egress setup. (No broker self-test in v1;
  the egress self-test pattern can be mirrored later if the DGX gate wants an
  in-boot certification line.)

### 3.6 `core/src/workers/web_research.rs` (`resolve` + a VM broker entry)

- `resolve()`: when `use_microvm && use_broker` (embed endpoint present), **stop
  warning-and-ignoring** the broker gate. Emit a **VM broker entry**: the union of
  the VM entry (`sandbox_backend = FirecrackerVm`, empty `fs_read`, force-routable)
  and broker mode (embed host **dropped** from `Net::Allowlist`, embed **model**
  env only — not the endpoint env, `broker: Some(BrokerSpec::embed(endpoint))`).
  Preserve the existing precedence: `use_microvm && !use_broker` → the current
  direct/degrade VM entry (byte-identical).
- New constructor, e.g. `web_research_firecracker_broker_entry(binary, image_dir,
  endpoint, embed_endpoint, embed_model, allowlist)`: `net_entries(endpoint,
  None, allowlist)` (embed host absent), `broker = Some(BrokerSpec::embed(...))`,
  `broker_uds: None` (set at spawn), the `KASTELLAN_MICROVM_*` env, and the embed
  model in env via the broker-mode env builder. Factor with the existing
  `web_research_firecracker_entry` / `broker_env` to avoid duplication (both files
  already have `base_env`/`broker_env` seams).

### 3.7 rootfs

- No script change. Rebuild `web-research.ext4` (and any other rootfs whose e2e
  runs) because `microvm-init` (PID1) changed. `/run` tmpfs already present.

## 4. Key design decisions

1. **Worker binary + `rewrite_policy_for_broker` unchanged.** Core sets the host
   UDS as the relay target and injects the host path env; the FC plan overrides
   that env to the guest path — the exact egress precedent. Nothing broker-mode-
   specific leaks into the worker.
2. **Kind-agnostic env override (value-match, not keyed).** The FC plan rewrites
   the env entry whose *value* equals `policy.broker_uds`, not a hardcoded
   `KASTELLAN_EMBED_BROKER_UDS` key. This keeps `sandbox` free of broker-kind
   knowledge (unlike the egress override, which hardcodes its one fixed key) and
   makes `search-broker` + VM free plumbing later. Safe because
   `rewrite_policy_for_broker` injects exactly one env entry equal to the unique
   per-worker `<scratch>/embed.sock` host path — no coincidental collision.
   *(Alternative considered: hardcode the embed key + an embed-specific guest path,
   mirroring egress literally — simpler to read, but bakes "embed" into `sandbox`.
   Rejected for kind-agnosticism at negligible extra cost.)*
3. **Generic guest UDS path `/run/kastellan-broker.sock`.** A worker binds at most
   one broker socket (`BrokerKind` doc), so one generic guest path suffices for any
   kind — no per-kind guest path needed.
4. **Fail-closed, matching host mode.** A VM worker with `broker: Some` but no
   discovered embed-broker config → the chokepoint refuses to spawn (the manifest
   already dropped the embed host from egress; a silent fallback would leave no
   backend route *and* skip containment). No new fail path — reuses the host-mode
   chokepoint refusal.
5. **Egress + broker coexist.** `broker_uds` never changes the NIC/netns decision;
   the two channels are independent ports over one device.

## 5. Testing strategy (TDD)

**Mac (hermetic, pure) — the primary implementation gate:**
- `plan.rs`: `broker_uds` sets `broker_vsock_port`/`broker_host_uds`; emits
  `kastellan.broker=1`; overrides the broker UDS env to `GUEST_BROKER_UDS` by
  value-match; leaves `net_enabled`/the NIC decision untouched; a plan with both
  `proxy_uds` and `broker_uds` carries both channel pairs.
- `linux_firecracker.rs` (cfg(linux), cross-clippy only on Mac): `launcher_argv`
  includes the broker pair when set. (aarch64 cross-clippy hits the known #144
  `ring` wall for `core`, so `core`'s Linux compile is DGX-verified.)
- `microvm-init` `cmdline.rs`: `kastellan.broker=1` parses to enabled; absent →
  disabled.
- `web_research.rs`: `resolve()` VM×broker branch → VM backend **and** `broker =
  Some(Embed)` **and** embed host absent from `Net::Allowlist` **and** no direct
  embed-endpoint env **and** embed model env present; VM-without-broker stays
  byte-identical; the old warn-and-ignore path is gone.
- Workspace `cargo build` + `cargo clippy --all-targets -D warnings` clean.

**DGX (real KVM + vsock + live PG + live Ollama) — the correctness gate:**
- New `core/tests/web_research_firecracker_broker_e2e.rs` (`#[ignore]`,
  DGX-only), mirroring `web_research_firecracker_egress_e2e.rs` +
  `embed_broker_egress_e2e.rs`:
  - Hermetic-stub tier: a host `UnixListener` stub stands in for the embed broker
    at the worker's `broker_uds`; boot a VM×broker worker; assert the stub
    **receives the worker's embed request over the 1026 tunnel** (proves VM boot +
    broker vsock relay + guest-env override + guest UDS bind). Egress may use its
    own stub proxy so the SearxNG search doesn't block.
  - Live tier (if a live Ollama is reachable host-side): drive `web.research`
    end-to-end and assert `ranking == "hybrid"` **with the embed host absent from
    the VM's `Net::Allowlist`** — the VM analogue of Slice C's
    `brokered_worker_ranks_hybrid_with_zero_embed_egress`, strictly stronger than
    the host-mode test because the worker is VM-isolated.
- Full DGX `cargo test --workspace` + `clippy --workspace --all-targets -D
  warnings` green; record the new baseline. Last *measured* DGX baseline was
  **2416/0/40** (2026-07-11, embed-broker Slice C); the pure-Rust #443/#444 merges
  since then added unit tests that also run on the DGX, so re-measure this session
  rather than trusting the recorded number. Expect +N passed unit tests, +1
  ignored e2e.

**Build/run reminders for the DGX gate** (from the FC e2e gotchas):
`export PATH=$HOME/.local/bin:$PATH`; `cargo build --release -p
kastellan-microvm-run`; `bash scripts/workers/microvm/build-web-research-rootfs.sh`
(rebuilds `microvm-init` into the rootfs); the embed-broker binary must be built
+ discovered (exe-sibling or `KASTELLAN_EMBED_BROKER_BIN`).

## 6. Risks & mitigations

- **VM-only plumbing bugs invisible on Mac** (the 5b-4b lesson: the DGX gate
  caught 2 real merged-code bugs the matrix VM path had). *Mitigation:* full DGX
  discharge this session, not a deferred gate.
- **`/run` double-mount / relay ordering.** Idempotent `/run` mount; both host
  reverse-relays bound *before* Firecracker boot; guest UDS bound in the parent
  *before* `exec`. *Mitigation:* mirror the egress ordering exactly.
- **Value-match env override fragility** if some other env entry ever equalled the
  broker host UDS path. *Mitigation:* the path is a unique per-worker
  `<scratch>/embed.sock`; a unit test pins the single-match assumption.
- **Cmdline size.** `kastellan.broker=1` adds ~18 bytes; well under the 2048 cap.
- **File-size cap.** `plan.rs` is already 1061 LOC (prod ~485; a test-lift
  candidate). Keep new plan code minimal; if it pushes prod materially, note the
  test-lift on the Item 9b backlog rather than splitting in this feature PR.

## 7. Verification checklist (session-end)

- [ ] Mac: build + clippy `-D warnings` clean; all new/affected unit tests green.
- [ ] DGX: rootfs + release launcher rebuilt; new broker e2e `--ignored` green;
      full-workspace `cargo test` + clippy green; new baseline recorded.
- [ ] `resolve()` no longer warns-and-ignores VM×broker; issue #429 note updated
      (VM×broker now reaches a routable-OR-loopback embed backend host-side).
- [ ] HANDOVER.md + ROADMAP.md updated; PR opened, linked to the embed-broker arc.
