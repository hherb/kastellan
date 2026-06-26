# Linux micro-VM backend (Firecracker) — design

**Date:** 2026-06-26
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation) / Phase-3 hardening
**Precedent:** `MacosContainer` micro-VM (`2026-05-21-macos-container-spike-notes.md`, `2026-06-25-python-exec-macos-microvm-design.md`); bwrap backend (`sandbox/src/linux_bwrap.rs`)

## Problem

On Linux (the DGX Spark, the primary production host) every worker already runs
under the project's strongest baseline: bwrap namespaces + per-profile
`seccomp` + Landlock + a cgroup that enforces `mem_mb`/`cpu_quota_pct`/`tasks_max`.
That is a **same-kernel** boundary: a worker, the LLM that drives it, a
compromised dependency, or agent-authored Python that finds a Linux-kernel
local-privilege-escalation 0-day reaches the **host kernel** — seccomp and
Landlock only *narrow* the reachable syscall/LSM surface, they do not remove the
shared kernel.

macOS now has a **separate-kernel** option for the highest-risk worker
(`MacosContainer` micro-VM, opt-in for `python-exec` and gliner-relex). Linux
has no micro-VM backend at all. The `SandboxBackendKind` enum comment already
anticipates one (`FirecrackerVm`), and the `python-exec` macOS-microvm spec
explicitly flagged the Linux counterpart as "separately-tracked future item."
This is that item.

## Goal

Add a **generic** Linux micro-VM `SandboxBackend` — `SandboxBackendKind::FirecrackerVm`
— that **any** `ToolEntry` can opt into, giving a worker a throwaway **guest
kernel** as a blast wall on top of the existing namespace/seccomp/Landlock/cgroup
layers. Default behaviour is unchanged (workers stay on bwrap). The backend is
**defense-in-depth on Linux, not a parity fix** (unlike the macOS VM, which
closed a real Seatbelt `mem_mb` gap).

### Driver: general hardening for all workers

The chosen driver is *general hardening* — the backend must be **generic**, not
`python-exec`-shaped, so browser-driver, gliner-relex, matrix, etc. can each opt
in over time. But "all workers" is the **end-state**, reached by a **staged
rollout**: net workers (egress-proxy UDS into a VM), GPU/torch workers, and
long-lived channel workers each add a distinct hard sub-problem. The backend
abstraction is generic from slice 1; the *consumers* arrive incrementally.

### Cross-platform framing

All work here is `#[cfg(target_os = "linux")]`-gated. macOS keeps Seatbelt + the
`Container` micro-VM. Both OSes converge on "the same `SandboxBackendKind` exposes
a separate-kernel micro-VM option," satisfying the symmetry instinct without a
macOS-only or Linux-only divergence in the abstraction.

## VMM choice: Firecracker

Firecracker (Rust, Apache-2.0; AWS Lambda/Fargate) over cloud-hypervisor, Kata,
and QEMU-microvm:

- **Smallest TCB** — the security-first choice; a minimal device model (block,
  vsock, net, serial, rng, balloon).
- **aarch64-supported**, fast boot, already the **named target** in the
  `SandboxBackendKind` docstring.
- Ships a **`jailer`** binary (chroot + cgroup + uid-drop + namespaces) for
  defense-in-depth around the VMM itself.

**Rejected:** Kata (heavy containerd/CRI stack, large TCB, breaks the
spawn-`Child`-with-stdio contract); QEMU-microvm (largest TCB, slower); cloud-
hypervisor (richer but larger TCB — kept as a documented escape hatch, see
"Known constraints").

## Feasibility — spike evidence (DGX, 2026-06-26)

A real boot was run on the DGX (aarch64, kernel `6.17.0-1021-nvidia`) with
Firecracker **v1.16.0** and the official CI aarch64 `vmlinux-6.1.102` +
`ubuntu-22.04.ext4`:

- **KVM works under the agent user's own perms** — `/dev/kvm` is RW via an ACL
  grant; **no operator action needed for KVM itself.** Firecracker booted a real
  aarch64 guest.
- **~70 ms boot to userspace** — rootfs mounted at `[0.068 s]`, `init=/bin/sh`
  reached at `[0.071 s]` (the macOS `container` micro-VM is ~700 ms; ~10× faster
  warm).
- **Clean stdin → guest-shell → stdout round-trip over serial** — a fed command
  echoed `GUEST_SENTINEL_OK` + `Linux 6.1.102 aarch64` back. This validates the
  **"launcher-process-is-the-Child"** transport model.
- **Serial is shared with kernel printk** — the same `ttyS0` carried `EXT4-fs`,
  `Freeing unused kernel memory`, and (in a misconfigured run) a `Kernel panic`.
  **Direct proof that serial-as-JSON-RPC is corruptible** → vsock is the right
  transport.
- **vsock is operator-gated** — `/dev/vhost-vsock` exists (`vhost_vsock.ko`
  present) but is `root:kvm` with no ACL, so the worker user can't open it
  without a one-time grant; `CONFIG_VIRTIO_VSOCKETS=m`, `CONFIG_VIRTIO_CONSOLE=y`.

## Transport (chosen): vsock + host launcher

The `SandboxBackend` contract returns a `std::process::Child` whose `stdin`/
`stdout` **are** the JSON-RPC pipe (`Client::from_child`). Firecracker's own
process stdio is the guest **serial console** (kernel boot logs), not a clean
JSON-RPC stream. Therefore:

- A small **`kastellan-microvm-run`** launcher binary **is** the `Child`. It
  boots Firecracker (`--no-api --config-file`), routes the guest kernel console
  to a **separate log fd (never stdout)**, opens the firecracker-managed **vsock
  UDS**, connects to the guest's vsock port, and copies `stdin↔vsock` /
  `vsock↔stdout`. From `tool_host`'s view it is a normal stdio worker — mirrors
  how Apple `container run -i` is itself the `Child` on macOS.
- In-guest, a tiny **PID1 `kastellan-microvm-init`** mounts `/proc`,`/sys`,`/tmp`
  (tmpfs), connects the vsock port and **`dup2`s it onto fd 0/1**, applies the
  worker env, then `exec`s the worker. **The existing `serve_stdio` worker is
  unchanged** — the init performs the vsock↔stdio adaptation.

**Fallback (documented, not built):** on a host without `/dev/vhost-vsock`,
serial transport is possible with disciplined kernel-log suppression
(`console=` off, `quiet`, `loglevel=0`), but it is single-channel and fragile;
vsock is preferred wherever available.

## Architecture & components

### 1. Enum + registry (`sandbox/src/lib.rs`)
- New `#[cfg(target_os = "linux")] FirecrackerVm` variant on `SandboxBackendKind`
  (never overload `Container`; the docstring already calls for a distinct name).
- `SandboxBackends` gains a Linux `firecracker: Arc<dyn SandboxBackend>` slot;
  `resolve()` gets a `(Some(FirecrackerVm), image)` arm, image-aware like the
  container arm (the existing `ToolEntry.container_image` field doubles as the
  rootfs-image tag — no new field).

### 2. Backend (`sandbox/src/linux_firecracker.rs`)
Same pure-fn-then-spawn shape as `linux_bwrap.rs`:
- Pure **`build_launch_plan(policy, program, args) -> FirecrackerLaunchPlan`** —
  kernel path, rootfs path, `vcpu_count`/`mem_size_mib` from `policy.mem_mb`/
  `cpu_quota_pct`, vsock CID+port, env, `Net` mode → net device present/absent.
  **Unit-testable without KVM.** Rejects relative `fs_read`/`fs_write` paths up
  front (matches bwrap).
- **`spawn_under_policy()`** renders the plan into the launcher argv and returns
  the launcher `Child` (stdio piped for JSON-RPC). cgroup wrapping
  (`systemd-run`, like bwrap) stays **outside** the launcher for host-side
  defense-in-depth.
- **`probe()`** fail-closed: firecracker binary on `PATH`, `/dev/kvm` RW,
  guest kernel + rootfs present, `/dev/vhost-vsock` RW. Each failure names its
  operator fix — the device-access failures point at
  `scripts/linux/install-firecracker-vsock.sh`; the missing-image failures at
  `build-rootfs.sh`.

### 3. Launcher binary (`workers/microvm-run/`, crate `kastellan-microvm-run`)
*Is* the `Child`. Boots Firecracker, bridges `stdin↔vsock`/`vsock↔stdout`,
routes kernel console to a log fd, RAII-tears-down (kill firecracker, remove
sockets/overlay) on EOF or worker exit.

### 4. Guest image (`scripts/workers/microvm/build-rootfs.sh` + PID1 init)
R1 (minimal-now, OCI-source-later): a minimal ext4 containing a pinned guest
kernel (Firecracker CI `vmlinux`), the **cross-built** worker binary
(bind-mounted `rust` container, like the macOS `build-image.sh`), python for
python-exec, and `kastellan-microvm-init`. A **later slice** converges the
Linux-VM and macOS-VM images onto **one OCI `Containerfile` as source of truth**
(OCI rootfs → ext4) for cross-platform symmetry.

## Control flow (slice 1, `Net::Deny` in-image worker)

```
tool_host::spawn_worker
  → backend.spawn_under_policy(policy, program, args)      [FirecrackerVm]
      build_launch_plan(policy,…) → FirecrackerLaunchPlan  (pure)
      spawn kastellan-microvm-run as Child (stdio piped)   ← JSON-RPC channel
          ├─ write firecracker config (kernel, rootfs, vsock, vcpu/mem, no net)
          ├─ boot firecracker (kernel console → log fd, NOT stdout)
          ├─ connect host vsock UDS → guest port
          └─ copy stdin↔vsock / vsock↔stdout
  → Client::from_child(child)   (unchanged — sees clean JSON-RPC)

in-guest: PID1 kastellan-microvm-init
          → mount /proc,/sys,/tmp → connect vsock, dup2→fd0/1
          → exec worker → serve_stdio (unchanged)
```

## Networking, filesystem, lifecycle (all staged)

**Networking** — by `Net` variant:
- `Net::Deny` (python-exec, gliner-relex) → **no virtio-net device**. Slice 1.
- `Net::Allowlist` + `proxy_uds` (force-routed net workers) → the host egress-
  proxy UDS is unreachable from inside a VM, so a later slice forwards it over a
  **second vsock channel**: the launcher proxies `guest-vsock-port-N ↔ host proxy
  UDS`; the in-guest init exposes it as a local UDS the worker's existing
  `ProxyBridge` dials. **Slice 4.**

**Filesystem** — Firecracker has **no virtio-fs / 9p** (minimal device model), so
host-dir sharing is not a bind-mount:
- `fs_read`/`fs_write` empty + in-image (the python-exec container-mode posture)
  → **slice 1**, nothing to share.
- Arbitrary host `fs_read` (e.g. gliner-relex venv) → a **per-spawn read-only
  ext4 block device** built from those paths, attached as a second drive;
  `fs_write`/`ephemeral_scratch` → a **per-spawn writable overlay drive**.
  **Slice 3.**

**Lifecycle** — **no new machinery.** The backend is stateless (per the trait);
it plugs into the existing `SingleUse`/`IdleTimeout`/`CompositeLifecycle` via
`resolve()` exactly like `MacosContainer`. Warm reuse = keep the VM booted +
per-call `/tmp` wipe — **reuses the warm/idle work shipped in #358 verbatim.**
**Slice 2.**

**Memory/CPU enforcement** — `mem_mb` → firecracker `mem_size_mib` (hard,
KVM-enforced; an in-guest OOM cannot touch the host); `cpu_quota_pct`/`vcpu_count`
→ machine-config. Host-side cgroup wrapping stays outside as defense-in-depth.

## Staged rollout

| # | Scope | Key deliverables | Verification |
|---|-------|------------------|--------------|
| **1** | Backend + boot a `Net::Deny` in-image worker over vsock | `FirecrackerVm` variant + `resolve()` arm; `linux_firecracker.rs` (pure `build_launch_plan` + `spawn` + `probe`); `kastellan-microvm-run` launcher; `build-rootfs.sh` + `kastellan-microvm-init`; python-exec `firecracker_mode_entry` opt-in (`KASTELLAN_PYTHON_EXEC_USE_MICROVM=1`, Linux-cfg, mirrors `container_mode_entry`) | DGX e2e: real boot, `print(6*7)→42`; **mem-cap → MemoryError**; net-deny (no egress); >64 KiB params file-channel |
| **2** | Warm/idle lifecycle parity | reuse `IdleTimeout` + per-call `/tmp` wipe (verbatim from #358) | DGX: warm reuse boots VM once; `/tmp` sentinel gone call-to-call |
| **3** | Host-dir sharing | per-spawn RO ext4 drive from `fs_read`; writable overlay drive from `fs_write`/`ephemeral_scratch` | a worker with a host venv (gliner-relex) runs in-VM |
| **4** | Net workers | egress-proxy UDS forwarded over a 2nd vsock channel; `Net::Allowlist` | force-routed worker reaches allowlist host via proxy, in-VM |
| **5** | Jailer hardening + long-lived/channel workers | launcher `--jailer` (chroot/cgroup/uid-drop); lifecycle for long-lived | jailer-confined boot; matrix-style worker stable in-VM |

This session writes this spec + the **slice-1** implementation plan in detail;
slices 2–5 are sketched here and get their own plan when picked.

## Testing & TDD discipline (rules #1–#2)

Pure, unit-tested **without KVM** (Mac dev box runs these):
- `build_launch_plan` — config/argv shape, `Net`-mode device gating, `mem_mb`→
  `mem_size_mib` + `cpu_quota_pct`→machine-config mapping, vsock CID/port,
  relative-path rejection.
- the firecracker-config JSON builder, the vsock-bridge framing, `probe`'s
  capability checks (each missing-capability → its operator-fix message).

Only the **e2e boot** needs the DGX (`#[ignore]`, real `/dev/kvm` + vsock): the
slice-1 round-trip, mem-cap enforcement, net-deny, param file-channel.

## Known constraints & escape hatches

- **No virtio-fs/9p in Firecracker** — host-dir sharing is via per-spawn block
  devices (slice 3). If this ever proves too painful for "all workers," a
  `cloud-hypervisor` backend (which *has* virtio-fs) is an **additive sibling**
  `SandboxBackendKind`, not a rewrite — the generic abstraction makes this cheap.
- **vsock is operator-gated** — one-time DGX setup is a single privileged script,
  `sudo scripts/linux/install-firecracker-vsock.sh` (persists the `vhost_vsock`
  module + ACL-grants the worker user `/dev/vhost-vsock` via a udev rule),
  mirroring the established `install-bwrap-apparmor-profile.sh` pattern; the
  Firecracker `probe()` points operators to it. The per-user, non-root
  `kastellan-cli install` deliberately does **not** perform this — the daemon
  never self-escalates. Plus the per-user `install-firecracker.sh` +
  `build-rootfs.sh`. Captured in a runbook with the slice-1 PR.
- **aarch64-only verification today** — the DGX is aarch64; x86_64 hosts are
  untested but Firecracker supports both (the backend is arch-neutral; the rootfs
  build script is arch-parameterized).

## Out of scope

- Net workers, GPU/torch passthrough, long-lived channel workers (slices 3–5).
- OCI-source-of-truth image unification (later slice; R1 ships minimal-rootfs
  first).
- x86_64 acceptance (Firecracker supports it; not verified on this hardware).
- Replacing bwrap as the default — the VM is always opt-in per `ToolEntry`.
