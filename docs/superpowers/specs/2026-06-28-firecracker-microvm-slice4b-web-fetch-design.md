# Firecracker micro-VM — slice 4b: first real net worker in a VM (web-fetch)

**Date:** 2026-06-28
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation) / Phase-3 hardening
**Parent:** [`2026-06-26-linux-firecracker-microvm-design.md`](2026-06-26-linux-firecracker-microvm-design.md) (slice table, row 4)
**Precedent:** slice 1 (`linux_firecracker.rs` + `microvm-run` + `microvm-init`, PR #364), slice 3 host-dir sharing (PR #371), slice 4a egress vsock transport (PR #373); bwrap force-routing (`sandbox/src/linux_bwrap.rs`, `core/src/egress/`); python-exec `firecracker_mode_entry` (`core/src/workers/python_exec/entries.rs`)

## Problem

Slice 4a built the **transport**: a force-routed `Net::Allowlist + proxy_uds`
worker boots inside a Firecracker guest with **no virtio-net device**, dials the
in-guest UDS `/run/kastellan-egress.sock`, and that relays over a second,
guest-initiated vsock channel (port 1025) → launcher reverse-relay → the host
egress-proxy UDS. Slice 4a proved the genuinely-novel guest-initiated vsock
direction on real hardware, but **with a python-exec worker carrying a boot-time
self-test knob and a host echo stub** — no real net worker, no real egress proxy,
no per-instance MITM CA delivered into the guest.

The first real consumer is **web-fetch**. The web-fetch worker already speaks the
egress proxy on the bwrap path: when `KASTELLAN_EGRESS_PROXY_UDS` is set it uses
`ProxyConnectGet` (CONNECT-over-UDS, end-to-end TLS) and trusts **only**
`KASTELLAN_EGRESS_PROXY_CA` — the proxy's per-instance MITM CA
(`workers/web-common/src/http.rs`, `make_get`/`make_get_inner`). The one thing
missing inside a VM is that **CA file**: it is minted by the egress sidecar at the
host path `<scratch>/ca.pem`, and the worker's `KASTELLAN_EGRESS_PROXY_CA` points
at that host absolute path — which does not exist inside the guest's separate
filesystem. Without it the in-guest worker cannot validate the proxy's MITM leaf
and every HTTPS fetch fails closed.

## Goal

Run `web-fetch` inside a Firecracker VM, reaching an allowlisted host through the
**real** host egress proxy over the slice-4a vsock channel, with the worker's
**existing, unchanged** code. Opt-in (`KASTELLAN_WEB_FETCH_USE_MICROVM=1`),
default off; the bwrap path stays byte-identical.

### Scope

- **In:** web-fetch micro-VM rootfs; web-fetch `firecracker_mode_entry` + resolver
  short-circuit; CA-into-guest delivery (the one real gap); a DGX e2e.
- **Out:** any worker but web-fetch (browser-driver / web-search VM variants);
  a generalized "net-worker-in-VM" abstraction (defer until a 2nd consumer
  exists); a system CA bundle in-guest (MITM-only ⇒ only the per-instance proxy
  CA is ever trusted); long-lived / channel net workers (slice 5).

## Background: what already composes (and what does not)

The two mechanisms 4b leans on are **backend-agnostic** and already wired:

- **Force-routing** (`core/src/worker_lifecycle/force_route.rs`,
  `core/src/egress/net_worker.rs`): `spawn_forced_net_worker` mints a per-worker
  scratch dir under `std::env::temp_dir()` (i.e. `/tmp/egress-<pid>-<seq>/`),
  spawns the egress sidecar (which writes `ca.pem` + binds the host UDS there),
  then `rewrite_worker_policy` sets `policy.proxy_uds = <host UDS>`, injects
  `KASTELLAN_EGRESS_PROXY_CA = <host ca.pem>` into `policy.env`, and **adds the
  CA path to `policy.fs_read`**. None of this branches on `SandboxBackendKind`.
- **The Firecracker plan** (`sandbox/src/linux_firecracker/plan.rs`): already
  turns `Net::Allowlist + proxy_uds` into `net_enabled=false` + egress vsock 1025
  + the `KASTELLAN_EGRESS_PROXY_UDS → /run/kastellan-egress.sock` override
  (slice 4a), and folds **all of `policy.fs_read`** into the per-spawn slice-3
  RO-share ext4 (`build_share_images`, built fresh into the run dir each spawn).

So a force-routed web-fetch worker with `sandbox_backend = FirecrackerVm`
**already** drives the whole chain *except* that the CA file does not actually
materialise inside the guest. Two facts make that the only real gap:

1. **`/tmp` is the worker scratch tmpfs in-guest**, and the slice-3 guest mount
   logic deliberately skips tmpfs-anchoring `/tmp`
   (`workers/microvm-init/src/main.rs`, `anchor_of` / the `unique_anchors`
   comment). The CA lives under `/tmp` (the egress scratch root). That is fine —
   `/tmp` is in the `SHARE_ANCHORS` allowlist, so `build_launch_plan` accepts the
   `fs_read` entry — but it means the CA bind target must be created inside the
   already-mounted `/tmp` tmpfs.
2. **The slice-3 RO bind treats every source as a directory**: for each RO target
   `t` it does `create_dir_all(t)` then `MS_BIND` (`apply_host_mounts`,
   `microvm-init/src/main.rs:271-282`). For a single **file** like `ca.pem` that
   creates a *directory* named `ca.pem` and the file-bind fails. The RO-share has
   only ever carried directory `fs_read` roots (python-exec slice 3); web-fetch's
   CA is its first single-file `fs_read` entry.

## Architecture & data flow

```
planner → tool_host::dispatch("web-fetch", {url})
  → spawn_worker_maybe_forced  (force-routing ON, Net::Allowlist)
      → spawn egress sidecar: mints <scratch>/ca.pem, binds host UDS
      → rewrite_worker_policy: proxy_uds=<host UDS>,
                               env += KASTELLAN_EGRESS_PROXY_CA=<host ca.pem>,
                               fs_read += <host ca.pem>
  → LinuxFirecracker::spawn_under_policy
      → build_launch_plan: Net::Allowlist+proxy_uds ⇒ net_enabled=false,
                           egress vsock 1025, override KASTELLAN_EGRESS_PROXY_UDS
                           → /run/kastellan-egress.sock,
                           RO-share sources=[ca.pem], kastellan.env=<hex>
                           (incl. _CA path + KASTELLAN_WEB_FETCH_ALLOWLIST)
      → build_share_images: stage ca.pem → ro-share.ext4 (per-spawn)
      → microvm-run launcher: pre-bind <uds>_1025 reverse-relay → host sidecar UDS; boot FC
  → guest microvm-init:
      → /run tmpfs + egress relay child (slice 4a)
      → apply_host_mounts: bind ca.pem at its host abs path  ← FILE-AWARE (NEW)
      → apply kastellan.env; exec web-fetch (unchanged)
  → web-fetch: KASTELLAN_EGRESS_PROXY_UDS set ⇒ ProxyConnectGet, trust only _CA
      → dial /run/kastellan-egress.sock → vsock 1025 → launcher → host sidecar
      → CONNECT host:443 → sidecar allowlist + SSRF + MITM → origin
      → proxy MITM leaf signed by the per-instance CA (now trusted in-guest) ⇒ TLS OK
  → readable text/JSON back up the same path → dispatch → planner
```

The worker reaches the network **only** through the proxy relay; the guest kernel
has no NIC. Stronger containment than the bwrap private-netns path, identical
worker code.

## Component changes

### 1. web-fetch micro-VM rootfs — `scripts/workers/microvm/build-web-fetch-rootfs.sh` (new)

Sibling of `build-rootfs.sh`. Stages `kastellan-worker-web-fetch` at
`/usr/local/bin/` + its `ldd` shared-library closure at absolute paths; pre-creates
the same pseudo-fs + anchor dirs + **`/run`** mountpoint the python-exec rootfs
has (`/proc /sys /tmp /dev /ro-share /opt /data /srv /mnt /work /run`); journal-less
ext4 (`-O ^has_journal`, mounted RO, shared across concurrent VMs). **No python
stdlib. No `ca-certificates` bundle** — MITM-only means the only trusted root is
the per-instance proxy CA delivered per-spawn. Factor any genuinely shared rootfs
setup (anchor `mkdir`, pseudo-fs, mkfs invocation) into a small sourced helper if
cheap; otherwise duplicate with a kept-in-sync comment (the existing house style
for the cross-crate constants). The image **dir** (and the pinned `vmlinux`) is
**shared** with python-exec — the default `KASTELLAN_MICROVM_DIR`
(`/var/lib/kastellan/microvm`). The two workers are disambiguated by **rootfs
filename**, not by dir: the build script emits `web-fetch.ext4` alongside
`python-exec.ext4`, and the backend gains per-worker rootfs-filename resolution
(see Component 3) reading a new `KASTELLAN_MICROVM_ROOTFS` env (default
`python-exec.ext4`, byte-identical for the existing python path). Sharing the dir
avoids duplicating the ~30 MB kernel and a second provisioned dir; the filename
env is the only differentiator.

### 2. web-fetch `firecracker_mode_entry` — `core/src/workers/web_fetch.rs` (new builder + resolver branch)

Linux-only `ToolEntry` builder, mirroring `python_exec/entries.rs::firecracker_mode_entry`:

- `net: Net::Allowlist(host:443…)` (derived from the operator allowlist exactly as
  the host-mode `web_fetch_entry` does), **not** `Net::Deny` — web-fetch needs egress.
- `profile: Profile::WorkerNetClient`, `proxy_uds: None` (set at spawn by
  force-routing), `sandbox_backend: Some(SandboxBackendKind::FirecrackerVm)`.
- `fs_read: vec![]` — **no `/etc/resolv.conf` / `/etc/hosts` / `/etc/nsswitch.conf`**:
  the worker has no NIC and does no local DNS; the egress proxy (host-side)
  resolves. (Those `/etc/*` paths would also be rejected by `build_launch_plan`'s
  share-anchor allowlist anyway.) The CA is appended to `fs_read` at spawn by
  `rewrite_worker_policy`.
- `env`: the verbatim `KASTELLAN_WEB_FETCH_ALLOWLIST` JSON, plus
  `KASTELLAN_MICROVM_DIR` (the shared image dir the resolver picked) and
  `KASTELLAN_MICROVM_ROOTFS=web-fetch.ext4` so the backend boots the right rootfs.
  All three ride the #360 `kastellan.env` cmdline token into the guest (harmless
  there — the guest ignores the two backend-config vars, exactly as it already
  ignores python-exec's forwarded `KASTELLAN_MICROVM_DIR`).
- `mem_mb: 512`, `cpu_ms: 10_000`, `wall_clock_ms: Some(30_000)`, `SingleUse`
  (match host-mode web-fetch).

Resolver (`WebFetchManifest::resolve`) gains a Linux-`cfg`-gated `USE_MICROVM`
short-circuit identical in shape to python-exec's: when
`KASTELLAN_WEB_FETCH_USE_MICROVM=1`, return the firecracker entry with the in-rootfs
binary path (`/usr/local/bin/kastellan-worker-web-fetch`) and the shared image dir
(`KASTELLAN_MICROVM_DIR`, default `/var/lib/kastellan/microvm`); else fall through
to today's host-mode `web_fetch_entry`. macOS build is untouched (the
`FirecrackerVm` variant and the env const are `#[cfg(target_os = "linux")]`-gated
— issue-#144 rule).

### 3. Backend rootfs-filename resolution — `sandbox/src/linux_firecracker.rs`

`spawn_under_policy` currently hardcodes `rootfs_path: dir.join("python-exec.ext4")`,
so a web-fetch worker would boot the python rootfs. Extract a pure
`resolve_image(env) -> FirecrackerImage` that reads `KASTELLAN_MICROVM_DIR`
(default `/var/lib/kastellan/microvm`) **and** a new `KASTELLAN_MICROVM_ROOTFS`
filename (default `python-exec.ext4` — the existing python path stays
byte-identical), joining `dir/<rootfs>`. Unit-tested without root (default →
`python-exec.ext4`; `ROOTFS=web-fetch.ext4` → that file; `DIR` override honoured;
empty `ROOTFS` → default). `spawn_under_policy` calls it instead of the inline
literal.

### 4. Guest file-aware RO bind — `workers/microvm-init/src/main.rs` (`apply_host_mounts`)

The only guest-side change. Extract a pure helper to decide bind shape, then act:

- For each RO target `t`, stat the **source** `/ro-share{t}` (already mounted RO):
  - **directory** → today's behaviour: `create_dir_all(t)` + `MS_BIND` (unchanged).
  - **regular file** → `create_dir_all(parent(t))`, create the empty target file
    (`OpenOptions::create`/`File::create`), then `MS_BIND`.
- Best-effort, never aborts PID1 (the slice-3 contract): a failed stat/create/bind
  logs + skips, exactly like the existing NUL-safe `mount` wrapper.
- The `/tmp` anchor is already skipped from tmpfs-anchoring (it is the scratch
  tmpfs), so a `/tmp/egress-<pid>-<seq>/ca.pem` target just needs its parent dir
  created inside that tmpfs — which the file branch does.

No cross-crate wire change: `RoShare.sources` already carries individual paths and
the `kastellan.mounts` encoder is path-agnostic; file vs directory is decided
entirely in-guest by stat.

### 5. DGX e2e — `core/tests/web_fetch_firecracker_egress_e2e.rs` (new)

Layered, mirroring `firecracker_egress_channel_e2e.rs` (4a) + `python_exec_e2e.rs`:

- **Always-on DGX gate — transport + boot + CA delivery, hermetically** (real KVM):
  a host `UnixListener` **stub stands in for the egress proxy** at the worker's
  `proxy_uds` (exactly the slice-4a echo pattern, one level up); the test boots a
  force-routed web-fetch VM and drives one `web.fetch` for an **allowlisted** host,
  then asserts the stub **receives the worker's `CONNECT <host>:443` line** within
  the wall-clock window. This single assertion proves the whole chain end-to-end:
  VM boot + force-routing + the slice-4a vsock relay (`/run/...sock` → vsock 1025 →
  launcher → host stub) **and CA delivery** — because the worker's `make_get` /
  `ProxyConnectGet::with_trust` **fails closed on an unreadable
  `KASTELLAN_EGRESS_PROXY_CA`** (pinned by `make_get_inner`'s own unit), so it
  cannot emit `CONNECT` at all unless the per-instance `ca.pem` actually
  materialised in-guest. No real origin and no upstream-trust plumbing needed.
- **Always-on, pure:** the `microvm-init` file-vs-directory bind helper has
  RED→GREEN units (file → parent-dir + touch + bind; dir → today's path;
  missing → skip), runnable on macOS — guards the bind *logic* without a VM.
- **`#[ignore]` real-net, end-to-end origin validation:** a full MITM fetch through
  the **real** sidecar to a real HTTPS origin — the worker validates the proxy's
  MITM leaf against the delivered CA and returns readable text. The last mile the
  hermetic gate cannot cover (a stub can't complete TLS). Mirrors the existing
  `real_mitm_fetch_through_sidecar`; carries the DGX public-DNS caveat (memory
  `dgx-realnet-egress-tests-fail`) — operator-driven on the Mac, not a CI gate.
- No-regression: slice-1 e2e, slice-2 warm/idle, slice-3 host-dir, slice-4a egress
  channel all still green; 0 orphan run-dirs.

## Testing & TDD discipline (rules #1–#2)

- **Pure-first, unit-tested without root:** the file-vs-directory decision in
  `microvm-init` is a pure helper over a `stat`-like probe seam (file / dir /
  missing → bind shape) — RED→GREEN units, runnable on macOS like the existing
  `microvm-init` parser/anchor tests. The web-fetch `firecracker_mode_entry` +
  resolver branch get manifest units pinning `Net::Allowlist`, `WorkerNetClient`,
  `FirecrackerVm`, empty `fs_read`, and the forwarded allowlist env (cross-clippy
  on the Mac for the linux-cfg builder).
- **DGX is the acceptance gate** (real KVM, aarch64). `kastellan-core` cannot
  cross-compile on the Mac (`ring` C-dep), so the e2e compiles + runs **only** on
  the DGX. Mac side: `cargo build --workspace`, the Mac-runnable units, and
  cross-clippy `--target aarch64-unknown-linux-gnu --all-targets -D warnings` for
  the sandbox / microvm-init / core linux-cfg modules.
- **Firecracker e2e gotchas** (carried from slices 1/3/4a): rebuild the **release**
  launcher (`cargo build --release -p kastellan-microvm-run`) **and** the web-fetch
  rootfs before the e2e (a stale release launcher or stale rootfs silently shadows
  source changes — #362 false leak); `export PATH=$HOME/.local/bin:$PATH` so
  `firecracker` is on the non-interactive ssh PATH (else the e2e skip-as-passes
  silently).

## Security posture

- **No direct-net path:** the guest carries no virtio-net device (`net_enabled =
  false`); a bare `Net::Allowlist` without `proxy_uds` is still fail-closed
  rejected by `build_launch_plan` (slice 4a). Egress is *only* via the proxy relay.
- **`SandboxPolicy` + the bwrap backend stay byte-unchanged.** The host↔guest CA
  path delivery is entirely a Firecracker-backend + guest-init concern.
- **CA confidentiality unchanged:** the proxy's CA **private key never leaves the
  sidecar process** (egress-proxy `ca.rs`); only the public `ca.pem` is shared, and
  it now rides the per-spawn, ephemeral, no-write-back RO ext4 — discarded with the
  run dir. The worker trusts that CA *only* (no system bundle), so a compromised
  worker cannot validate any origin the proxy did not MITM.
- **Best-effort PID1 contract preserved:** the new file-bind branch never panics
  PID1 (log + skip on any failure), matching the slice-3 NUL-safe / slice-4a
  fork-failure handling.

## Open implementation details (for the plan phase)

These are acceptable deferrals (the feature is testing-only, nothing in production
depends on it yet) — resolve them during TDD, not now:

- **Hermetic always-on full-fetch gate.** The always-on gate proves transport +
  CA delivery (the worker emits `CONNECT` only after loading the in-guest CA), but
  not the final origin-validation TLS leg (the stub can't complete TLS). A loopback
  HTTPS origin would let the *whole* fetch run always-on, but the proxy validates
  upstream against webpki and would reject a self-signed loopback cert. Closing this
  needs either a webpki-chaining cert or a proxy test knob to trust an extra
  upstream root — the same gap `real_mitm_fetch_through_sidecar` lives with. Until
  then, origin validation's proof is the `#[ignore]` real-net test.
- **`/tmp` scratch ↔ CA bind ordering** in the guest init: the CA binds into the
  same `/tmp` tmpfs the worker scratch uses. web-fetch has no per-call `/tmp` wipe
  (that is python-exec's `wipe_scratch_contents`), so the risk is low, but pin the
  ordering with the e2e (mount/bind before `exec`).
- **web-fetch `ldd` closure**: confirm the actual shared-library set when building
  the rootfs (rustls/ring are largely static; the closure is expected to be thin),
  and whether any runtime data files beyond the binary are needed.

## Out of scope (→ slice 5 and beyond)

- Generalised "force-routed net worker in a VM" mechanism for browser-driver /
  web-search (defer until a 2nd consumer exists — YAGNI).
- Long-lived / channel net workers in a VM (jailer + persistent-thread spawn).
- A system CA bundle in-guest (only needed if a future worker must validate
  non-MITM origins directly — not the MITM-only egress posture).
- Per-worker image-dir conventions / a unified OCI rootfs source of truth
  (tracked separately; this slice ships a standalone `build-web-fetch-rootfs.sh`).
