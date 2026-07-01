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

---

## The two layers of containment

kastellan uses **two independent sandbox layers per worker**. The idea is that
if one layer has a bug, the other still holds.

### Layer 1: Parent-side sandbox (bwrap on Linux, sandbox-exec on macOS)

The agent core applies this before the worker process even starts.

**On Linux** — bubblewrap (`bwrap`) creates a new set of OS namespaces
for the worker:
- A new filesystem namespace (the worker only sees the directories it needs)
- A new network namespace (no network unless explicitly granted)
- A new user namespace, PID namespace, UTS namespace
- `--die-with-parent` — if the core crashes, the worker dies too

**On macOS** — `sandbox-exec` applies a Seatbelt profile (a TinyScheme
description of what the process can and cannot do).

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
   process can make. The default action is "kill the process". A per-profile
   allow-list of safe syscalls is permitted (`Strict` kills `socket()`,
   `NetClient` permits it, `BrowserClient` and `MlClient` add the extra
   syscalls Chromium / torch empirically need).
3. Starts the JSON-RPC server.

After the filter is installed the restrictions cannot be relaxed — not even by
the process itself (`NO_NEW_PRIVS`).

**Pure-Python workers** (`gliner-relex`, `browser-driver`) can't call the Rust
prelude because bwrap spawns the interpreter directly. They get the same Layer 2
via `kastellan-worker-lockdown-exec`: a tiny shim that applies the rlimits,
`lock_down()` (Landlock + seccomp), then `execve`s the venv script, which
inherits the filter under `NO_NEW_PRIVS`. The shim is discovered fail-closed
(a missing shim binary makes the worker `Misconfigured`, not unsandboxed).

---

## The SandboxPolicy struct (the key abstraction)

When you add a new worker or configure an existing one, you describe what it
needs via `SandboxPolicy`:

```rust
SandboxPolicy {
    fs_read: vec!["/usr", "/lib"],           // directories the worker can read
    fs_write: vec!["/tmp/kastellan/task-42"],  // directories it can write
    net: Net::Deny,                          // no network
    proxy_uds: None,                         // egress-proxy socket (set at spawn)
    persistent_store: None,                  // survives-respawn store (opt-in)
    profile: Profile::WorkerStrict,          // syscall / Seatbelt cluster
    mem_mb: Some(256),                       // memory cap
    cpu_ms: None,                            // no CPU cap
    // ...
}
```

The sandbox backends translate this struct into bwrap arguments (Linux) or
a Seatbelt profile (macOS). You write the policy once; both platforms enforce it.
`persistent_store` is `None` for almost every worker (and renders
byte-identically when unset); a long-lived worker that needs state to survive
a micro-VM respawn sets it to a `PersistentStore { host_backing, guest_mount,
size_mib }` — see the micro-VM section below.

**Important:** `fs_read` paths must be absolute. Relative paths are rejected
at `spawn_under_policy` time with a clear error.

---

## Network: deny, allowlist, and the egress proxy

`SandboxPolicy.net` is one of three values:

- `Net::Deny` — no network at all (a private, empty netns on Linux).
- `Net::Allowlist(hosts)` — the worker may reach the listed `host:port`
  endpoints. In the default **force-routed** deployment
  (`KASTELLAN_EGRESS_FORCE_ROUTING=1`), the worker still runs in a private
  netns with **no direct route**; `proxy_uds` is set at spawn to its own
  egress-proxy sidecar's Unix socket, and the proxy enforces the allowlist +
  an SSRF guard. The worker literally cannot reach anything the proxy didn't
  approve.
- `Net::ProxyEgress` — the egress proxy's *own* policy: it keeps the host
  netns because it is the thing doing real DNS + outbound connections, and it
  is self-enforcing.

This is why "a compromised tool reaches at most the endpoints in that tool's
allowlist" is enforced by the kernel + the proxy, not by convention.

---

## Profiles

`SandboxPolicy.profile` selects the syscall/Seatbelt cluster:

- `WorkerStrict` — minimal; kills `socket()`. Default.
- `WorkerNetClient` — adds the syscalls an outbound-HTTPS client needs.
- `WorkerBrowserClient` — `WorkerNetClient` plus the headless-Chromium set.
- `WorkerMlClient` — `WorkerNetClient` plus the torch/CUDA-probe set (the
  worker stays `Net::Deny`; the socket syscalls have no route out). Renders
  identically to `WorkerStrict` on macOS.

---

## The optional third tier: micro-VM backends

The two layers above are namespace + kernel-filter containment: strong, but the
worker still shares the host kernel. For workers that warrant hardware-level
isolation, a worker can opt into a **micro-VM backend** via
`SandboxBackendKind` — a real guest kernel and virtual hardware, so a kernel
exploit in the worker does not reach the host kernel:

- **Linux — Firecracker.** A minimal KVM micro-VM. `sandbox/src/linux_firecracker/`
  builds the launch plan, `mkfs.ext4` RO/RW share images, and a
  `kastellan.mounts` manifest, then boots the VM through `microvm-run` (the
  launcher) with `microvm-init` as the guest PID 1 bridging vsock↔stdio back to
  the host. It supports host-directory sharing, a warm/idle reuse lifecycle, a
  vsock egress transport (network in a VM), unprivileged-VMM confinement (the
  `firecracker` process is itself wrapped in a bwrap+cgroup jail, on by default),
  and long-lived persistent-VM workers.
- **macOS — Apple `container`.** An opt-in per-worker micro-VM using Apple's
  `container` CLI (macOS Tahoe+), the parity backend that gives macOS real
  memory enforcement.

Both are **opt-in per worker** — the default path stays bwrap (Linux) /
Seatbelt (macOS). The Firecracker backend is gated behind
`KASTELLAN_PYTHON_EXEC_USE_MICROVM=1` for python-exec; VMM confinement is on by
default and opts out with `KASTELLAN_MICROVM_CONFINE_VMM=0`. See the
[Linux micro-VM setup runbook](../runbooks/2026-06-26-linux-microvm-setup.md)
for the one-time host setup (`scripts/linux/install-firecracker-vsock.sh`).

**Persistent store.** A long-lived micro-VM worker can carry a
`SandboxPolicy.persistent_store`. On Firecracker this is a stable, `mkfs`-once
ext4 image attached RW and mounted at `guest_mount` — its contents survive a VM
respawn (a launcher-held `flock` guarantees a single mounter). On bwrap /
Seatbelt the equivalent is a persistent `fs_write` bind. When the field is
`None` (the common case) every backend renders exactly as before.

---

## Resource caps

On Linux, cgroup v2 provides hard memory and CPU caps via `systemd-run`:
- `MemoryMax` from `policy.mem_mb` — the worker is OOM-killed if it exceeds
  this limit.
- `MemorySwapMax=0` — swap is always disabled so an overrun cannot silently
  page to disk.
- `CPUQuota` and `TasksMax` — defaults of 200% and 64 tasks as
  defence-in-depth; overridable via `SandboxPolicy`.

On macOS, `RLIMIT_CPU` is enforced by the worker itself via `setrlimit`, the
only portable cross-platform resource limit currently available.

---

## Checking that sandboxing is really active

A green test run with `[SKIP]` lines means the sandbox tests did not run —
they skipped because the sandbox tool was not working. **This is a silent
false positive.**

Always verify:

```sh
cargo test -p kastellan-sandbox -- --nocapture 2>&1 | grep -E '\[SKIP\]|ok'
```

You want to see `ok` lines, not `[SKIP]` lines.
