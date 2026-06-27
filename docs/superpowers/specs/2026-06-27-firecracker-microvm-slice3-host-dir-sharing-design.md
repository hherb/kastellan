# Firecracker micro-VM — slice 3: host-dir sharing — design

**Date:** 2026-06-27
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation) / Phase-3 hardening
**Parent spec:** [`2026-06-26-linux-firecracker-microvm-design.md`](2026-06-26-linux-firecracker-microvm-design.md) (staging table, row 3)
**Precedent:** slice 1 (`…-slice1.md`, `linux_firecracker/plan.rs` pure-plan split), slice 2 warm/idle, #360 env cmdline forwarding, #362 run-dir RAII cleanup

## Problem

Firecracker has a **minimal device model with no virtio-fs / 9p**, so a worker's
host `fs_read` paths (e.g. a Python venv) and `fs_write`/`ephemeral_scratch`
cannot be bind-mounted into the guest the way `linux_bwrap` binds them. Slice 1
booted only a `Net::Deny` **in-image** worker (`fs_read`/`fs_write` empty); any
worker that needs to read host files or write disk-backed scratch cannot run in a
micro-VM yet. This slice adds host-directory sharing via **per-spawn block
devices**, the mechanism the parent spec's staging table (row 3) calls for.

## Goal

Give the generic `FirecrackerVm` backend the ability to:

1. Expose a worker's absolute `fs_read` paths **read-only** inside the guest **at
   their original absolute paths** (path identity, matching bwrap semantics).
2. Give the worker a **writable, disk-backed scratch** mount for
   `fs_write`/`ephemeral_scratch`.

Both as **per-spawn ephemeral** devices — built at spawn, discarded on teardown,
**no host write-back**. Default behaviour is unchanged: a policy with empty
`fs_read`/`fs_write` renders a byte-identical config to slice 1/2.

### Scope decision: generic mechanism + synthetic e2e

This slice builds and verifies the **generic** host-dir-sharing mechanism. It is
verified with a **lightweight synthetic consumer** — the firecracker backend
driven directly with a crafted `SandboxPolicy` over the existing python-exec
rootfs — **not** by wiring a real heavy worker. Reasons:

- gliner-relex (the parent spec's literal row-3 example) is a torch worker, and
  **Firecracker has no GPU passthrough**; in-VM it would be CPU-only torch,
  likely too slow to be a real driver. Wiring it is a separate, larger step.
- The mechanism is what unblocks *any* future host-venv consumer; proving it with
  a small fs_read dir + a scratch path is the smallest correct slice.

Wiring a real heavy worker in-VM is deferred (see Out of scope).

## Design decisions locked in (brainstorming)

| Question | Decision |
|----------|----------|
| Verification target | Generic mechanism + synthetic DGX e2e (lightweight consumer) |
| Host write-back | **Ephemeral only** — writable drive discarded on teardown, no copy-out |
| Overlay vs disjoint | **Disjoint** — RO fs_read mounts + a *separate* plain RW scratch drive; **no overlayfs** (so no `CONFIG_OVERLAY_FS` guest-kernel requirement) |
| Path identity | **One RO ext4 mirroring absolute layout + in-guest bind-mounts** (fixed 2-extra-drive ceiling, arbitrary disjoint paths) |
| RW scratch size | Default **64 MiB**, env-overridable |
| tmpfs `/tmp` | **Kept** as-is; the disk-backed RW drive is additive (present only when the policy asks for a writable host-shaped path) |
| RO-root bind-mount targets | **tmpfs anchors** — the rootfs pre-creates a fixed set of empty anchor dirs; the init mounts a tmpfs at each needed anchor so `mkdir -p` of the bind target works on the otherwise read-only root. Overlay-free, no guest-kernel change. **Constraint:** `fs_read` paths must live under a shareable anchor, not under the rootfs's own system dirs (`/usr`,`/bin`,`/lib`,`/etc`). Full generality (switch_root) deferred. |

## Architecture & component split

Mirrors slice 1's layering: pure plan describes *what*, the backend spawn does the
*I/O*, the guest init performs the in-guest mounts.

### 1. Pure `sandbox/src/linux_firecracker/plan.rs`

`build_launch_plan` gains structured, policy-derived mount intent:

- `ro_share: Option<RoShare>` where `RoShare { sources: Vec<PathBuf> }` = the
  absolute `fs_read` roots. `None` when `fs_read` is empty.
- `rw_scratch: Option<RwScratch>` where `RwScratch { mountpoint: PathBuf }` = the
  writable scratch path (the `fs_write` path or the ephemeral-scratch dir). `None`
  when neither is requested.
- A **hex cmdline mount-manifest token** (`kastellan.mounts=<hex>`) appended to
  `boot_args`, encoding the in-guest mount plan (see Transport). Subject to the
  existing `MAX_CMDLINE_BYTES` fail-closed cap (now covering env + mounts).

The ext4 `path_on_host` values are **placeholders** the spawn overrides with
run-dir paths — exactly the pattern `vsock_uds`/`vsock_cid` already use (the pure
plan has no spawn/run-dir context).

`render_firecracker_config` appends up to two extra `drives` entries when present:
RO share (`is_read_only: true`) and RW scratch (`is_read_only: false`). Both
absent → byte-identical to slice 1/2.

### 2. Backend `sandbox/src/linux_firecracker.rs::spawn_under_policy`

Does the per-spawn I/O, into the **run dir** (so the launcher's #362 RAII teardown
removes them on graceful exit, and the orphan-sweep reclaims them on SIGKILL):

- **RO image:** for each `fs_read` path `P`, replicate `P`'s tree under
  `<rundir>/ro-stage/<P>` (copy; hardlink optimization noted as future), then
  `mke2fs -d <rundir>/ro-stage <rundir>/ro-share.ext4` — **non-root, no loop
  mount**.
- **RW image:** `mke2fs <rundir>/rw-scratch.ext4` of the configured size (default
  64 MiB), blank.
- Set the resolved drive paths on the plan, render `fc.json`, spawn the launcher
  (unchanged transport).

cgroup wrapping (`systemd-run` scope) stays outside, as today.

### 3. Guest `workers/microvm-init/src/main.rs`

After `mount_pseudo_fs` and before the vsock/exec path, decode the manifest from
`/proc/cmdline` (new pure `parse_mount_manifest`, mirroring `parse_env_cmdline`)
and:

- mount the RO drive (`/dev/vdb` by attach order) at `/ro-share` (a pre-created
  anchor in the rootfs); then for each fs_read root: mount a tmpfs at the target's
  **top-level anchor** (deduplicated — e.g. `/opt`, pre-created empty in the
  rootfs) so the otherwise read-only root becomes writable there, `mkdir -p` the
  full target path, and bind-mount `/ro-share/<abs>` → `/<abs>`;
- mount the RW drive (next device letter) read-write at the scratch mountpoint
  (typically under the already-writable tmpfs `/tmp`, so no anchor needed).

**RO-root mountpoint handling:** the guest rootfs is mounted read-only (shared
backing file), so bind-mount targets cannot be `mkdir`'d directly. The rootfs
build pre-creates a fixed set of empty **anchor directories** (`/ro-share` for the
share mount + a small set like `/opt /data /srv /mnt` for bind targets); the init
mounts a tmpfs at each needed anchor before `mkdir -p`. **Constraint:** an
`fs_read` path's top-level component must be a shareable anchor, never a rootfs
system dir (`/usr`,`/bin`,`/lib`,`/etc`) — mounting a tmpfs there would hide the
worker's own files. Fail closed in `build_launch_plan` if an `fs_read` path's
top-level component is a reserved system dir.

Device-letter assignment is computed from manifest presence flags in the **fixed
attach order RO-before-RW**, so the init never guesses. The existing tmpfs `/tmp`,
vsock accept, `dup2`, and worker `exec` are unchanged.

## Mount-manifest transport

A new `kastellan.mounts=<hex>` kernel-cmdline token, hex-encoding a tiny manifest:

- the RW scratch mountpoint (if any),
- the list of RO bind-mount absolute paths (if any),
- presence flags fixing the RO-before-RW device order.

Reuses the existing `hex_encode`/`hex_decode` codec and the `MAX_CMDLINE_BYTES`
fail-closed cap (a pathological path count fails the boot rather than emitting a
truncated cmdline). Same cross-crate, manually-synced-constant discipline as
`kastellan.env` (`microvm-init` must not depend on the sandbox crate); a roundtrip
fixture is pinned identically in **both** crates' unit tests.

## Control flow

```
tool_host::spawn_worker
  → backend.spawn_under_policy(policy, program, args)        [FirecrackerVm]
      build_launch_plan(policy,…) → FirecrackerLaunchPlan    (pure: ro_share, rw_scratch, mounts token)
      build RO image  : stage fs_read trees → mke2fs -d → <rundir>/ro-share.ext4
      build RW image  : mke2fs → <rundir>/rw-scratch.ext4
      render_firecracker_config(plan) → fc.json  (+2 drives)
      spawn kastellan-microvm-run as Child (stdio piped)     ← JSON-RPC channel
  → Client::from_child(child)   (unchanged)

in-guest: PID1 kastellan-microvm-init
  → mount /proc,/sys,/tmp(tmpfs)
  → parse_mount_manifest(/proc/cmdline)
  → mount /dev/vdb RO /mnt/ro → bind-mount /mnt/ro/<abs> → /<abs>  (each fs_read root)
  → mount /dev/vdc RW <scratch mountpoint>
  → vsock accept → dup2 fd0/1 → exec worker (unchanged)
```

## Probe

`probe()` gains a fail-closed check that `mke2fs` (e2fsprogs) is on `PATH`, naming
its operator fix. **No new guest-kernel configuration** is required — ext4,
virtio-blk, and bind-mounts are all already present in the slice-1 kernel/rootfs.
(This is the concrete payoff of choosing disjoint mounts over overlayfs, which
would have needed `CONFIG_OVERLAY_FS`.)

## Testing & TDD discipline (rules #1–#2)

Pure, unit-tested **without KVM** (Mac dev box, via cross-clippy + the platform-
agnostic pure fns):

- `build_launch_plan` emits `ro_share`/`rw_scratch` + the manifest token from a
  policy with `fs_read`/`fs_write`; empty policy → both `None` and a baseline
  cmdline.
- `render_firecracker_config` gains exactly the two drives with correct
  `is_read_only`; absent → byte-identical config (a pinned regression).
- manifest hex roundtrip fixture pinned **identically in both crates**
  (sandbox `encode` ↔ microvm-init `parse_mount_manifest`).
- cmdline-cap fail-closed when env + mounts exceed `MAX_CMDLINE_BYTES`.
- `build_launch_plan` **fails closed** when an `fs_read` path's top-level
  component is a reserved rootfs system dir (`/usr`,`/bin`,`/lib`,`/etc`,…) —
  mounting a tmpfs anchor there would hide the worker's own files.
- guest-side `parse_mount_manifest` decode + fail-safe (garbled token → no mounts,
  mirroring `parse_env_cmdline`).

Synthetic **DGX e2e** (`#[ignore]`, real `/dev/kvm` + vsock):

- drive the firecracker backend directly with a crafted
  `SandboxPolicy { fs_read: [hostdir-with-sentinel], <scratch> }` over the existing
  python-exec rootfs;
- assert in-VM Python **reads the host sentinel at its original absolute path**
  and **writes to the scratch mount**;
- no change to python-exec's production manifest (keeps it a generic-mechanism
  test).
- Reuses the slice-2 gotchas: rebuild the **release** launcher
  (`cargo build --release -p kastellan-microvm-run`) and
  `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive ssh
  PATH → e2e SKIP-as-passes silently otherwise).

## Out of scope (deferred, documented)

- **Host write-back / copy-out** — the writable drive is ephemeral; making writes
  visible on the host post-run (full bwrap `fs_write` parity) is a later slice.
- **overlayfs writable views** of a `fs_read` subtree — no current consumer needs
  to write into a read-only path.
- **Full path-identity generality** (`fs_read` under any absolute path, incl.
  rootfs system dirs) — would need a `switch_root` onto a writable tmpfs root or
  overlayfs; deferred behind the anchor-dir constraint above.
- **RO-image content-addressed caching** across spawns — rebuilt per spawn (fine
  for light consumers; flagged for heavy ones, where staging a multi-GB venv every
  spawn would dominate boot time).
- **Wiring a real heavy worker (gliner-relex) in-VM** — heavy rootfs + CPU-only
  torch; separate step.
- **x86_64 acceptance** — DGX is aarch64; the mechanism is arch-neutral but
  unverified on x86_64.

## Known constraints

- The number of extra drives is capped at **2** by design (one RO share image
  regardless of fs_read count + one RW scratch), staying well within Firecracker's
  drive limit.
- The manifest rides the kernel cmdline, so a pathological number of fs_read paths
  fails closed at the `MAX_CMDLINE_BYTES` cap rather than truncating. Acceptable —
  real worker policies have a handful of paths; a future slice can move to a
  larger transport (a config drive or a vsock control message) if a consumer ever
  needs it.
