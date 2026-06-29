# Firecracker micro-VM slice 5a — VMM confinement (unprivileged) — design

**Date:** 2026-06-29
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation) / Phase-3 hardening
**Precedent:** the Firecracker arc — slice-1 backend (`2026-06-26-linux-firecracker-microvm-design.md`),
slice-2 warm/idle, slice-3 host-dir sharing, slice-4a/4b net workers; the bwrap
backend (`sandbox/src/linux_bwrap.rs`) + cgroup wrapper (`sandbox/src/linux_cgroup.rs`).

## Problem

The Firecracker backend (slices 1–4b) gives a worker a throwaway **guest kernel**
as a blast wall. But the **VMM host-side tooling is unconfined**: the backend
spawns the `kastellan-microvm-run` launcher with a bare
`Command::new("kastellan-microvm-run")` — no host-side cgroup, no namespace jail
around the launcher or the `firecracker` process it spawns. Two concrete gaps:

1. **No host-side cgroup on FC workers.** bwrap workers run inside a
   `systemd-run --user --scope` transient cgroup (`linux_cgroup.rs`) that enforces
   `MemoryMax`/CPU/`TasksMax`. FC workers get **none** of that on the host side.
   The *guest* RAM is KVM-enforced, but the `firecracker` process itself, its
   threads, and the launcher are unbounded on the host.
2. **VMM breakout reaches the daemon's full user context.** A hypothetical
   compromise of `firecracker` (the VMM) or the launcher — a device-emulation
   bug, a vsock-handling bug — runs with the daemon user's full filesystem and
   process visibility. Firecracker is a small audited Rust TCB with its own
   built-in seccomp, so this is second-order, but it is the one layer the arc
   left open.

Firecracker ships a **`jailer`** binary that closes both (chroot + cgroup +
uid-drop + namespaces), and the slice-1 spec named it as the slice-5 deliverable.
**But `jailer` requires real root at runtime** (it does `chroot`, `mknod`,
`setuid`-drop, cgroup creation). That collides head-on with kastellan's hard
invariant: the daemon is a **per-user, non-root** process and **never
self-escalates** (the per-user `kastellan-cli install` deliberately performs no
privileged setup; on the DGX the daemon reaches `/dev/kvm` and `/dev/vhost-vsock`
via group/ACL grants, not root). A literal jailer would need a privileged/system
deployment tier or a setuid helper — both cross the non-root line.

## Goal

Confine the VMM **unprivileged**, delivering jailer's *guarantees* (mount/pid/net
namespace isolation + chroot-equivalent + a host-side cgroup) without root, by
**reusing the sandbox layers kastellan already ships**: the unprivileged **bwrap**
jail (user-namespace mount/pid/net isolation + seccomp) and the
**`systemd-run --user --scope`** cgroup wrapper. A true `jailer` strategy remains
a documented, additive future sibling for a privileged deployment tier; this slice
builds the **seam** for it but not the strategy itself.

Non-goals for 5a: the long-lived/channel-worker-in-VM lifecycle (slice 5b), a
true jailer, dedicated-uid drop, GPU passthrough.

## Approach (chosen): wrap the launcher; firecracker inherits the jail

The `SandboxBackend` contract is unchanged: `spawn_under_policy` returns a
`std::process::Child` whose stdio is the JSON-RPC pipe. Today that Child is the
bare launcher. Slice 5a **prepends the same `systemd-run --user --scope` + `bwrap`
wrapper the bwrap workers already use**, so the launcher runs inside a
cgroup-bounded, namespace-isolated jail:

```
systemd-run --user --scope --quiet --collect  (MemoryMax/CPU/TasksMax)
  -- bwrap <vmm-jail binds> --unshare-all --die-with-parent --new-session
           --as-pid-1 --clearenv [--setenv …]
    -- kastellan-microvm-run --config-file … --vsock-uds … --run-dir …
        └─ Command::new("firecracker") …      ← CHILD of the launcher
           └─ inherits the bwrap namespaces automatically (no extra work)
              └─ guest VM (separate kernel)
```

Because `firecracker` is spawned **as the launcher's child**, it lands in the same
mount/pid/net namespaces and the same cgroup — we confine **one** process and the
VMM follows for free. Only the launcher's `stdin`/`stdout` cross the jail boundary
(fd 0/1 are inherited through bwrap exactly as for every bwrap worker). The
firecracker-created vsock UDS, `fc.log`, and the slice-3 RO/RW ext4 share images
all live in the per-spawn run-dir, which is bound into the jail, so the launcher's
vsock bridge reaches the UDS at its in-jail path natively — **no cross-uid
permission dance** (the literal-jailer pain point).

The **launcher code barely changes**. The work is almost entirely in the backend's
spawn path (building the wrapper argv) plus one new pure argv builder.

### Why not a literal jailer

| | bwrap + cgroup (5a) | jailer |
|---|---|---|
| Root at runtime | **no** | **yes** (chroot/mknod/setuid) |
| New operator uid + device grants | no | yes |
| vsock-UDS cross-uid permissions | n/a (same uid) | must be solved |
| Reuses shipped code | yes (`linux_bwrap`, `linux_cgroup`) | new integration |
| Fits "daemon never self-escalates" | yes | no |
| uid-drop to a lower-priv user | **no** (same uid in userns) | yes |

The only guarantee bwrap+cgroup gives up vs. jailer is the **uid-drop**; in the
unprivileged user-namespace model the launcher stays the daemon's uid (mapped
inside the ns). For a privileged deployment that wants the uid-drop too, the
`Jailer` strategy is the documented escape hatch (seam below).

## Architecture & components

### 1. The VMM-jail bwrap policy — new pure function

`sandbox/src/linux_firecracker/confine.rs` (new module):

```rust
/// Pure: build the bwrap argv that jails the launcher + firecracker.
/// Mirrors linux_bwrap::build_argv but binds only what the VMM tooling
/// touches. Unit-testable without KVM.
pub fn build_vmm_jail_argv(plan: &FirecrackerLaunchPlan, run_dir: &Path)
    -> Result<Vec<String>, SandboxError>;
```

Binds — **only** what `firecracker` + the launcher need:

- `--dev-bind /dev/kvm /dev/kvm`, `--dev-bind /dev/vhost-vsock /dev/vhost-vsock`
  (device ACL/group grants the daemon uid holds carry through the userns; the
  underlying real-uid check still applies)
- `--ro-bind` the `vmlinux` kernel, the rootfs ext4, the `firecracker` binary, the
  `kastellan-microvm-run` binary (+ any required shared libs — resolved like the
  browser-driver interpreter-dep auto-bind precedent if firecracker isn't fully
  static)
- `--bind` (RW) the per-spawn `run_dir` — firecracker writes the vsock UDS +
  `fc.log` here; the slice-3 RO/RW ext4 share images live here
- **net-worker arm (force-routed web-fetch, slice-4a):** `--bind` the host
  egress-proxy UDS named in `plan.egress_host_uds`. Egress rides the vsock relay,
  not host networking, so `--unshare-net` stays safe.
- baseline invariants identical to the worker bwrap builder:
  `--unshare-all --die-with-parent --new-session --as-pid-1 --clearenv`; env only
  via `--setenv` from `plan.env`.

Relative `fs_read`/`fs_write`/image paths are rejected up front (matches bwrap).

### 2. The confinement seam — the jailer-ready dispatch point

`sandbox/src/linux_firecracker.rs`:

```rust
/// How the VMM (launcher + firecracker) is confined on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmmConfinement {
    /// Bare spawn — today's behaviour. Selected by the explicit opt-out.
    None,
    /// systemd-run --user --scope cgroup + bwrap jail. Default.
    BwrapCgroup,
    // Future: Jailer — privileged (root) chroot + uid-drop + cgroup + netns.
    //         An additive sibling for a privileged/system deployment tier.
    //         Documented escape hatch; NOT built in 5a.
}
```

`spawn_under_policy` selects the strategy (from env, §4) and builds the final argv:

- `None` → today's `[MICROVM_RUN_BIN, …launcher flags…]` (byte-identical).
- `BwrapCgroup` →
  `build_systemd_run_argv(&policy) ++ ["--"] ++ build_vmm_jail_argv(plan, run_dir) ++ ["--"] ++ launcher_argv(…)`.
  The cgroup ceilings come straight from the worker's own `SandboxPolicy`
  (`mem_mb` / `cpu_quota_pct` / `tasks_max`) — no derived struct; reuse the
  existing `build_systemd_run_argv`, which already reads exactly these fields.

The Child returned is still the outermost process (`systemd-run`), whose stdio is
forwarded through `--scope` (foreground) → bwrap (fd inherit) → launcher → the
JSON-RPC pipe. This is exactly the bwrap-worker stdio chain.

### 3. Probe — fail-closed capability checks (only when enabled)

`linux_firecracker::probe` gains two checks, evaluated **only when confinement is
enabled** (the default):

- bwrap usable — reuse `LinuxBwrap::probe()` (catches the missing unprivileged-
  userns AppArmor profile, pointing at `install-bwrap-apparmor-profile.sh`).
- cgroup usable — reuse `linux_cgroup::cgroup_probe()` (catches a missing
  `systemd --user` session).

Each failure names its operator fix. When the operator has set the opt-out
(`KASTELLAN_MICROVM_CONFINE_VMM=0`), these checks are skipped (the `None` path has
no bwrap/cgroup dependency).

### 4. Default-ON, opt-out, fail-closed

New orthogonal env flag **`KASTELLAN_MICROVM_CONFINE_VMM`**, read by the FC
backend, independent of the per-worker `*_USE_MICROVM` flags:

- **unset / `1` / `true` → `BwrapCgroup`** (default).
- **`0` / `false` → `None`** (explicit opt-out escape hatch).

**Fail-closed:** when confinement is enabled and the probe fails, the VM worker
**refuses to spawn** — there is **no silent bare-spawn fallback** (that would drop
containment to a false green, which the project forbids). A host that can run VMs
but not bwrap-userns recovers explicitly via `=0`.

This flag is irrelevant to bwrap workers (they never reach the FC backend) and to
a deployment that hasn't opted into VMs at all (`*_USE_MICROVM` unset).

### Default-ON merge gate (process note)

Because flipping the default reroutes the existing green slice-1/2/4b e2e through
bwrap, **the PR merges only once the DGX confined-boot e2e is green** (proving
`/dev/kvm` + `/dev/vhost-vsock` survive the bwrap user namespace). If that
interaction proves unworkable, the documented fallback is to **ship the flag
default-OFF** (mechanism opt-in, flip later) — a one-line default change, not a
redesign.

## Control flow (slice 5a, default `BwrapCgroup`)

```
tool_host::spawn_worker
  → backend.spawn_under_policy(policy, program, args)      [FirecrackerVm]
      resolve_image(policy.env)
      build_launch_plan(policy, image, program, args)      (pure)
      run_dir = make_spawn_dir(); plan.vsock_uds/cid set
      build_share_images(plan, run_dir, env)               (slice 3, unchanged)
      write fc.json
      confine = confinement_from_env(policy.env)            (default BwrapCgroup)
      match confine {
        None        => argv = launcher_argv(plan, …)
        BwrapCgroup => argv = build_systemd_run_argv(&policy)
                            ++ "--" ++ build_vmm_jail_argv(plan, run_dir)   (pure)
                            ++ "--" ++ launcher_argv(plan, …)
      }
      Command::new(argv[0]).args(argv[1..])
              .stdin/out(piped).spawn()                      ← JSON-RPC channel
  → Client::from_child(child)   (unchanged)

inside the jail: kastellan-microvm-run (PID 1 of the pid-ns)
  → Command::new("firecracker") … (inherits the jail)
  → vsock bridge stdin↔vsock / vsock↔stdout
guest: kastellan-microvm-init → exec worker → serve_stdio (all unchanged)
```

## Testing & TDD discipline (rules #1–#2)

**Mac (pure, no KVM) — the bulk of the coverage:**
- `build_vmm_jail_argv` shape: `/dev/kvm` + `/dev/vhost-vsock` `--dev-bind` present;
  kernel/rootfs/firecracker/launcher `--ro-bind` present; run-dir `--bind` present;
  `--unshare-all`/`--die-with-parent`/`--as-pid-1`/`--clearenv` present; env only
  via `--setenv`; **net-worker arm** binds `egress_host_uds` only when force-routed;
  relative-path rejection.
- `confinement_from_env`: unset/`1`/`true` → `BwrapCgroup`; `0`/`false` → `None`
  (case/trim-insensitive, fail-safe to the secure default on a malformed value —
  i.e. anything not clearly an opt-out confines).
- strategy assembly: `BwrapCgroup` argv begins with `systemd-run`, contains the two
  `--` separators in order, and ends with the launcher argv; `None` argv is
  byte-identical to today's `launcher_argv`.
- probe-message tests for the two new fail-closed checks.

**DGX (real KVM) — `#[ignore]`, gated on `/dev/kvm` + vsock:**
- **confined boot (new default path):** `KASTELLAN_MICROVM_CONFINE_VMM` unset, a
  python-exec VM boots under `systemd-run + bwrap` and runs `print(6*7)→42`; the
  `mem_mb` cap still enforced; the launcher's vsock bridge still reaches the UDS.
  This is the **merge gate**.
- **explicit-opt-out no-regression:** `KASTELLAN_MICROVM_CONFINE_VMM=0` pins the
  `None` strategy still boots bare (byte-identical spawn).
- **net worker:** a force-routed web-fetch VM (slice-4b) boots confined and still
  reaches the egress proxy over the vsock relay (the egress-UDS bind works inside
  the jail).
- **no-regression** slice-1/2/4b suites pass with confinement on.

## Out of scope (→ later slices)

- **True `jailer`** (privileged tier) — the `VmmConfinement::Jailer` sibling;
  needs root, a dedicated uid + device grants, chroot-relative path rewrite, and a
  deployment-model decision. Documented seam only.
- **Long-lived / channel-worker-in-VM** (slice 5b) — persistent-thread supervision
  of a VM kept booted for a channel's lifetime; the matrix/IMAP consumer.
- **Dedicated-uid drop** for the unprivileged path (would need a setuid helper or a
  privileged tier — folds into the jailer escape hatch).
- **Making confinement mandatory with no opt-out** — the `=0` hatch stays for hosts
  without bwrap-userns.

## Known constraints & escape hatches

- **KVM/vsock through the bwrap user namespace is the load-bearing unknown** — the
  DGX confined-boot e2e is the proof and the merge gate (§4). Containers run
  firecracker with `/dev/kvm` bound, so the precedent is good, but the
  user-namespace + device-ACL interaction is verified only on real hardware.
- **firecracker shared libs** — if the pinned `firecracker` binary isn't fully
  static, its `ldd` closure must be `--ro-bind`'d into the jail (reuse the
  browser-driver interpreter-dep auto-bind approach). Resolved during the DGX e2e.
- **bwrap becomes a hard dependency of the (now default) confined VM path** — hosts
  that run VMs but lack the unprivileged-userns AppArmor profile must either install
  it (`install-bwrap-apparmor-profile.sh`) or set `KASTELLAN_MICROVM_CONFINE_VMM=0`.
  The probe names both fixes.
- **aarch64-only verification today** — same as the rest of the arc; the argv
  builders are arch-neutral.
