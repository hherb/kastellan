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
serving any JSON-RPC request. It calls `kastellan_worker_prelude::serve_stdio`,
which:

1. Applies **Landlock** — a kernel mechanism that restricts which files the
   process can access (an allow-list, not a deny-list).
2. Applies **seccomp-bpf** — a filter that restricts which system calls the
   process can make. The default action is "kill the process". About 110
   safe syscalls are explicitly allowed.
3. Starts the JSON-RPC server.

After `restrict_self()` and `apply_filter()` return, the restrictions cannot
be relaxed — not even by the process itself.

---

## The SandboxPolicy struct (the key abstraction)

When you add a new worker or configure an existing one, you describe what it
needs via `SandboxPolicy`:

```rust
SandboxPolicy {
    fs_read: vec!["/usr", "/lib"],           // directories the worker can read
    fs_write: vec!["/tmp/kastellan/task-42"],  // directories it can write
    net: Net::Deny,                          // no network
    mem_mb: Some(256),                       // memory cap
    cpu_ms: None,                            // no CPU cap
    // ...
}
```

The sandbox backends translate this struct into bwrap arguments (Linux) or
a Seatbelt profile (macOS). You write the policy once; both platforms enforce it.

**Important:** `fs_read` paths must be absolute. Relative paths are rejected
at `spawn_under_policy` time with a clear error.

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
