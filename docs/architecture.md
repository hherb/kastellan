# kastellan — Architecture

> **Status: skeleton.** This file will grow as the build progresses. Authoritative source for design decisions is the design plan; this doc captures architecture detail for code reviewers.

## High-level diagram

![kastellan architecture overview: Matrix/email channels feed the Rust agent core; the dispatcher chokepoint is the only door into per-tool worker processes, each in its own kernel sandbox; networked workers leave the host only through the per-worker egress proxy; Postgres and local inference are host-local services](architecture-overview.svg)

> Source: [`architecture-overview.svg`](architecture-overview.svg). The
> [security-architecture.svg](security-architecture.svg) and
> [security-request-flow.svg](security-request-flow.svg) diagrams add the
> CASSANDRA review pipeline and trace a single request through every gate.

## Process model

- One **agent core** binary (`kastellan`).
- One **tool worker** process per tool, each in its own sandbox.
- One **channel adapter** process per channel — Matrix (self-hosted, single-user, federation off, E2E) as the primary channel, with email (IMAP/SMTP) as a low-trust cross-transport failover.
- One **egress proxy** process (TLS-terminating, allowlist-enforcing).
- One **Postgres** instance (own role, UDS-only, peer auth).
- One **inference server** (vLLM / SGLang / llama.cpp / Ollama, OpenAI HTTP).

## IPC

JSON-RPC ([Model Context Protocol](https://modelcontextprotocol.io)-compatible) over Unix domain sockets. Language-agnostic, no shared memory, no in-process untrusted code.

## Cross-platform

| Concern              | Linux                               | macOS                                          |
| -------------------- | ----------------------------------- | ---------------------------------------------- |
| Sandbox              | bubblewrap + Landlock + seccomp-bpf | `sandbox-exec` (Seatbelt)                      |
| Service supervisor   | `systemd --user`                    | `launchd` (LaunchAgents)                        |
| Local LLM serving    | vLLM / SGLang on GPU                | llama.cpp / Ollama (Metal/MLX)                 |
| Keyring              | libsecret (`secret-tool`)           | Keychain (`security`)                          |
| Optional micro-VM    | Firecracker / Podman+crun           | Apple `container` CLI (macOS Tahoe+)           |

The same `SandboxPolicy` and `ServiceSpec` Rust structs drive both backends.

## Module map (Rust)

See [`core/src/lib.rs`](../core/src/lib.rs), [`sandbox/src/lib.rs`](../sandbox/src/lib.rs), [`supervisor/src/lib.rs`](../supervisor/src/lib.rs). Modules are stubbed and will be filled in across phases.

## Invariants

These are load-bearing rules. Breaking any of them weakens the threat model in [`threat-model.md`](threat-model.md). Reviewers should refuse PRs that violate them.

1. **Process-per-worker, sandbox-per-worker.** Every tool invocation runs in its own OS process under its own bwrap (Linux) or `sandbox-exec` (macOS) jail. No in-process tool execution; no two workers share a process or sandbox.
2. **Dispatcher chokepoint.** Every action — tool call, channel I/O, scheduled routine, REPL command — enters the worker layer through a single function (today: [`core::tool_host::spawn_worker`](../crates/core/src/tool_host.rs); future: a thin `ToolHost::dispatch()` wrapper). That function is the *only* site that authors a `WorkerCommand`, the *only* site that consults policy, and the *only* site that writes the audit-log entry. New entry points (channels, routines) call into this function — they never spawn workers themselves.
3. **No in-process untrusted code.** No PyO3, no `wasmtime` host functions executing agent-authored code, no plugin `dlopen`. The only untrusted code path leaves the core process boundary first.
4. **Secrets live behind the host boundary.** Secrets are decrypted in the core process at the moment of injection into a worker call; they are never written to logs, never sent to the LLM unmasked, and never readable from a worker outside that single call.
5. **Every byte that crosses the trust boundary is scanned once.** The egress proxy (Phase 3 onward) is the single inspection point for outbound and inbound traffic — credential-leak scan, TLS pin check, host allowlist. Workers do not get a second chance to elide it.

Adjacent OpenClaw-derived projects ([nearai/ironclaw](https://github.com/nearai/ironclaw), [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw)) relax invariant 1 (in-process WASM tools / in-process trait tools respectively). Their implementations of invariants 4 and 5 — and ZeroClaw's [`crates/zeroclaw-runtime/src/security/`](https://github.com/zeroclaw-labs/zeroclaw/tree/main/crates/zeroclaw-runtime/src/security) sandbox backends — are useful reading; their tool-execution model is not the design we are building.
