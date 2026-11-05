# 7 — Sandboxing explained

This chapter explains what sandboxing is and how kastellan uses it, without
assuming prior kernel or security engineering experience.

---

## What is a sandbox?

A sandbox is a set of kernel-enforced restrictions placed on a process. After
the restrictions are applied, the process cannot do things it is not allowed
to do — even if it is compromised.

Think of it as a room with locked doors. The process can do its job
(the doors its work needs are open), but it cannot access anything outside
the room (all other doors are locked by the OS kernel, not by the application).

---

## Why does kastellan sandbox its workers?

The agent runs untrusted code in workers: arbitrary shell commands, Python
scripts, web fetches triggered by content the agent read online. Any of these
could be malicious (prompt injection, supply-chain backdoor, etc.).

If a worker is compromised, the sandbox limits the damage to:

- that worker's own scratch directory
- the network endpoints in that worker's allowlist
- nothing else (not memory, not other tools' secrets, not the agent core)

There is no way to spawn a worker without a sandbox: `tool_host` has no
"unsandboxed" path, and none may be added.

---

## The two layers of containment

kastellan uses **two independent sandbox layers per worker**. The idea is that
if one layer has a bug, the other still holds.

### Layer 1: Parent-side sandbox (bwrap on Linux, Seatbelt on macOS)

The agent core applies this before the worker process even starts.

**On Linux** — bubblewrap (`bwrap`) creates a new set of OS namespaces
for the worker. The argv is built by a pure function,
`linux_bwrap::build_argv()` (in `sandbox/src/linux_bwrap.rs`), so it can be
tested without spawning anything. Every jail carries:

- `--unshare-all` — new user, PID, IPC, UTS, cgroup and network namespaces
- the `USERNS_LOCKDOWN_FLAGS` pair, `--unshare-user --disable-userns` — the
  worker cannot create a nested user namespace (the first step of most
  unprivileged kernel exploits)
- `--die-with-parent` — if the core dies, the worker dies too
- `--new-session` — no access to the parent's terminal
- `--as-pid-1` — signals stay contained
- `--clearenv` — no host environment variables leak in; the only variables
  set come from `policy.env` via `--setenv`

A detail worth knowing: `--unshare-all` only *tries* to unshare the user
namespace, and bwrap refuses `--disable-userns` unless `--unshare-user` is
given explicitly. A jail missing the pair does not run at all. There are
**three** places that build a bwrap argv — the startup probe
(`LinuxBwrap::probe`), the worker spawn (`build_argv_with_resolver`) and the
Firecracker VMM jail (`linux_firecracker/confine.rs`) — and all three take the
pair from the one constant. Copies that spelled the flags out by hand drifted
and broke. **A new jail uses the constant; it does not re-list the flags.**

**On macOS** — `sandbox-exec` applies a Seatbelt profile (a TinyScheme
description of what the process can and cannot do), built by the pure
function `macos_seatbelt::build_profile()`. It is default-deny for files and
network. The environment is cleared and the worker gets its own session, as on
Linux. macOS has no direct equivalent of `--die-with-parent`.

Both platforms use the same `SandboxPolicy` struct to describe what the worker
should be allowed to do. The platform-specific code translates that policy.

### Layer 2: Worker-side sandbox (Landlock + seccomp on Linux)

After the worker process starts, it installs a *second* layer on itself before
serving any JSON-RPC request. A Rust worker calls
`kastellan_worker_prelude::serve_stdio`, which:

1. Applies **Landlock** — a kernel mechanism that restricts which files the
   process can access (an allow-list, not a deny-list). Both RW (from
   `fs_write`) and RO (from `fs_read`) rules are derived so a net worker can
   still read `/etc/resolv.conf`.
2. Applies **seccomp-bpf** — a filter that restricts which system calls the
   process can make. Any syscall not on the profile's allow-list kills the
   process. Namespace creation (`unshare`, `setns`, `clone` with a new-namespace
   flag), `mount`, `ptrace`, `bpf`, module loading and `io_uring` are never
   allowed.
3. Starts the JSON-RPC server.

After the filter is installed the restrictions cannot be relaxed — not even by
the process itself (`NO_NEW_PRIVS`).

On macOS this layer is a no-op: Seatbelt is the kernel-enforced layer there.

**Pure-Python workers** (`gliner-relex`, `browser-driver`) can't call the Rust
prelude because bwrap spawns the interpreter directly. They get the same Layer 2
via `kastellan-worker-lockdown-exec`: a tiny shim that applies the rlimits,
`lock_down()` (Landlock + seccomp), then `execve`s the venv script, which
inherits the filter under `NO_NEW_PRIVS`. The shim is discovered fail-closed
(a missing shim binary makes the worker `Misconfigured`, not unsandboxed).

---

## The SandboxPolicy struct (the key abstraction)

When you add a new worker or configure an existing one, you describe what it
needs via `SandboxPolicy` (in `sandbox/src/lib.rs`):

```rust
SandboxPolicy {
    fs_read: vec!["/etc/ssl".into()],          // read-only paths
    fs_write: vec!["/tmp/kastellan/task-42".into()], // writable paths
    net: Net::Deny,                            // no network
    cpu_ms: 0,                                 // 0 = no CPU-time rlimit
    mem_mb: 256,                               // memory cap
    profile: Profile::WorkerStrict,            // syscall / Seatbelt cluster
    cpu_quota_pct: None,                       // backend default (200%)
    tasks_max: None,                           // backend default (64)
    env: vec![],                               // the ONLY env the jail sees
    proxy_uds: None,                           // egress-proxy socket (set at spawn)
    broker_uds: None,                          // broker sidecar socket (set at spawn)
    persistent_store: None,                    // survives-respawn store (opt-in)
}
```

The sandbox backends translate this struct into bwrap arguments (Linux) or
a Seatbelt profile (macOS). You write the policy once; both platforms enforce it.
`proxy_uds` and `broker_uds` are filled in by the core's spawn path, never by
a worker manifest. `persistent_store` is `None` for almost every worker (and
renders byte-identically when unset); a long-lived worker that needs state to
survive a respawn sets it to a `PersistentStore { host_backing, guest_mount,
size_mib }` — see the micro-VM section below.

**Important:** `fs_read` paths must be absolute. Relative paths are rejected
at `spawn_under_policy` time with a clear error; the Linux backends also
reject paths containing `..` (macOS canonicalizes instead). Symlinked
host-side *sources* are canonicalized before binding — bwrap and the
Firecracker backend bind the canonical source at the original destination. This applies to host paths only; guest-side paths stay lexical.

---

## Network: deny, allowlist, and the egress proxy

`SandboxPolicy.net` is one of three values, and on Linux the network
namespace follows from it together with `proxy_uds`:

| Policy | Linux jail | Who uses it |
|--------|-----------|-------------|
| `Net::Deny` | private, empty netns | most workers (shell-exec, python-exec, …) |
| `Net::Allowlist(hosts)` **with** `proxy_uds` set | private netns (no `--share-net`) + the proxy's Unix socket bound in with `--bind` | every networked worker in the default deployment |
| `Net::Allowlist(hosts)` **without** `proxy_uds` | `--share-net` (host netns) | legacy posture, only when force-routing is off |
| `Net::ProxyEgress` | `--share-net` (host netns) | the egress proxy itself |

**Force-routing** (`KASTELLAN_EGRESS_FORCE_ROUTING=1`) is set in the
supervised core unit by default. Every `Net::Allowlist` worker then gets its
own egress-proxy sidecar at spawn: the worker has no direct route, and the
proxy enforces the host:port allowlist, resolves DNS itself, refuses
private/loopback/link-local addresses (SSRF), scans intercepted TLS for the
worker's own secrets, and checks certificate pins. Opting in is fail-closed —
if the proxy binary cannot be found, the daemon refuses to start rather than
run network workers unrouted. On macOS the Seatbelt profile denies all outbound
traffic except that one socket. Under the Firecracker backend the worker
reaches its sidecar over vsock instead of a bound socket — same proxy,
different transport.

`Net::ProxyEgress` is the proxy's *own* policy: it keeps the host netns
because it is the thing doing real DNS and outbound connections.

This is why "a compromised tool reaches at most the endpoints in that tool's
allowlist" is enforced by the kernel + the proxy, not by convention.

---

## Profiles

`SandboxPolicy.profile` selects the syscall/Seatbelt cluster:

- `WorkerStrict` — minimal; kills `socket()`. Default.
- `WorkerNetClient` — adds the syscalls an outbound-HTTPS client needs.
- `WorkerBrowserClient` — `WorkerNetClient` plus the headless-Chromium set
  (browser-driver only; it re-grants Seatbelt clusters the strict profile
  denies).
- `WorkerMlClient` — `WorkerNetClient` plus the torch set (gliner-relex). The
  worker stays `Net::Deny`; the socket syscalls have no route out. Renders
  identically to `WorkerStrict` on macOS.
- `WorkerMatrixClient` — `WorkerNetClient` plus the matrix-rust-sdk SQLite
  additions (the Matrix channel worker). Renders identically to
  `WorkerNetClient` on macOS.

---

## The optional third tier: micro-VM backends

The two layers above are namespace + kernel-filter containment: strong, but the
worker still shares the host kernel. For workers that warrant hardware-level
isolation, a worker can opt into a **micro-VM backend** via
`SandboxBackendKind` (`Bwrap` / `FirecrackerVm` on Linux, `Seatbelt` /
`Container` on macOS) — a real guest kernel and virtual hardware, so a kernel
exploit in the worker does not reach the host kernel.

A micro-VM *replaces* bwrap or Seatbelt for that one worker; it is not stacked
on top. The worker-side layer does not fully carry over: inside a Firecracker
guest the worker still installs seccomp, but not Landlock (the pinned guest
kernel is built without it, and the backend warns on every spawn); on the
Apple `container` backend the worker-side layer is absent for gliner-relex and
unverified for python-exec. The
[threat model](../../threat-model.md) states these trade-offs in full.

### Linux — Firecracker

A minimal KVM micro-VM. `sandbox/src/linux_firecracker/` builds the launch
plan, `mkfs.ext4` RO/RW share images and a `kastellan.mounts` manifest, then
boots the VM through `kastellan-microvm-run` (the launcher) with
`kastellan-microvm-init` as the guest PID 1 bridging vsock↔stdio back to the
host. It supports host-directory sharing, a warm/idle reuse lifecycle, vsock
egress (network in a VM, through the same egress proxy), and long-lived
persistent-VM workers.

- **Pinned guest kernel.** The guest kernel is sha256-pinned and verified at
  **every** VM boot (`sandbox/src/guest_kernel_pin.rs`), fail-closed, with no
  override. The one-time `sudo scripts/linux/install-firecracker-vsock.sh`
  installs it `root:root` in a root-owned image directory so the agent's user
  cannot replace it. The boot-time check still matters where that ownership
  does not hold (an older install, or a non-default `KASTELLAN_MICROVM_DIR`).
- **Confined VMM.** The `firecracker` process itself runs inside a bwrap +
  cgroup jail, on by default (opt out with `KASTELLAN_MICROVM_CONFINE_VMM=0`).
  The privileged `jailer` strategy is not implemented.
- **Opt-in per worker.** `KASTELLAN_PYTHON_EXEC_USE_MICROVM=1`,
  `KASTELLAN_WEB_FETCH_USE_MICROVM=1`, `KASTELLAN_WEB_SEARCH_USE_MICROVM=1`,
  `KASTELLAN_WEB_RESEARCH_USE_MICROVM=1`,
  `KASTELLAN_BROWSER_DRIVER_USE_MICROVM=1`, and
  `KASTELLAN_MATRIX_USE_MICROVM=1` for the persistent Matrix worker. Without
  the host setup, `LinuxFirecracker::probe()` fails closed and the worker stays
  on bwrap.

See the [Linux micro-VM setup runbook](../runbooks/2026-06-26-linux-microvm-setup.md)
for the one-time host setup.

### macOS — Apple `container`

An opt-in per-worker micro-VM using Apple's `container` CLI (macOS Tahoe+),
for `python-exec` (`KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1`) and
`gliner-relex` (`KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`). It is the parity
backend that gives macOS real memory enforcement (`-m`, with a 200 MiB floor),
CPU and task caps.

### Images must match the code under test

A micro-VM boots an **image**, and each image holds its own copy of the
binaries it runs. A code change is invisible to the VM tier until the image
is rebuilt — so the micro-VM test suites check freshness and refuse a stale
image instead of silently testing old code:

- **Firecracker rootfs images** (`*.ext4`) bake binaries from
  `target/release/`. The gate compares the baked binary's content with the
  current release build (`tests-common/src/microvm/freshness.rs`). Rebuild
  with `bash scripts/build-release.sh` first, then
  `bash scripts/workers/microvm/rebuild-all-rootfs.sh`. Every
  `build-*-rootfs.sh` needs the installer above to have run: builds only verify
  the guest kernel, never create it.
- **Apple `container` images** are cross-built for the guest, so their
  binaries can never equal the host's. That gate compares the image's build
  time with the modification times of its sources instead. This is why
  CLAUDE.md says never `touch` source files to force a rebuild — it makes a
  fresh image look stale.

By default a missing VM fixture prints `[SKIP]` and the test passes. Set
`KASTELLAN_MICROVM_REQUIRE_E2E=1` (or run the `microvm` / `container` profile
of `scripts/run-e2e-gate.sh`) to turn those skips into failures — see
[chapter 5](./05-build-test-run.md).

**Persistent store.** A long-lived micro-VM worker can carry a
`SandboxPolicy.persistent_store`. On Firecracker this is a stable, `mkfs`-once
ext4 image attached RW and mounted at `guest_mount` — its contents survive a VM
respawn (a launcher-held `flock` guarantees a single mounter). On bwrap /
Seatbelt the equivalent is a persistent bind of a host directory. When the
field is `None` (the common case) every backend renders exactly as before.

---

## Resource caps

On Linux, `sandbox/src/linux_cgroup.rs` wraps `bwrap` in
`systemd-run --user --scope`, which puts the worker in its own cgroup v2
before its namespaces are created:

- `MemoryMax` from `policy.mem_mb` — the worker is OOM-killed if it exceeds
  this limit.
- `MemorySwapMax=0` — swap is always disabled so an overrun cannot silently
  page to disk.
- `CPUQuota` and `TasksMax` — from `cpu_quota_pct` and `tasks_max`, with
  defaults of 200% and 64 tasks as defence-in-depth.

`policy.cpu_ms` is enforced on both OSes by the worker itself, as
`setrlimit(RLIMIT_CPU)` from the prelude.

On macOS, Seatbelt has no memory, CPU-bandwidth or task-count primitive, so
those three fields have no effect there. The opt-in Apple `container` backend
is what enforces them on a Mac.

---

## Checking that sandboxing is really active

A green test run with `[SKIP]` lines means the sandbox tests did not run —
they skipped because the sandbox tool was not working (on Ubuntu 24.04+, most
often because the bwrap AppArmor profile is not installed; see
[chapter 2](./02-dev-env-linux.md)). **This is a silent false positive.**

Always verify:

```sh
cargo test -p kastellan-sandbox -- --nocapture 2>&1 | grep -E '\[SKIP\]|ok'
```

You want to see `ok` lines, not `[SKIP]` lines. To make a skip *fail* instead,
set `KASTELLAN_SANDBOX_REQUIRE_E2E=1`, or run a profile of
`scripts/run-e2e-gate.sh` that sets it.
