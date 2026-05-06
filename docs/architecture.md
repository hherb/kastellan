# hhagent — Architecture

> **Status: skeleton.** This file will grow as the build progresses. Authoritative source for design decisions is the design plan; this doc captures architecture detail for code reviewers.

## High-level diagram

```
                 ┌─────────────── HOST (Linux user account) ───────────────┐
                 │                                                          │
 messages ─┐     │  ┌───────────────── AGENT CORE (Rust) ─────────────┐    │
 IMAP    ──┼─────┼─►│  scheduler · context manager · memory orch.     │    │
 webhooks  │     │  │  policy/capability gate · LLM router · audit    │    │
 ──────────┘     │  └───────┬────────────┬───────────────┬────────────┘    │
                 │          │ MCP JSON-RPC over UDS      │                 │
                 │  ┌───────▼──┐ ┌───────▼──┐ ┌─────────▼─┐ ┌──────────┐  │
                 │  │ python   │ │ browser  │ │ web-fetch │ │ mail     │  │
                 │  │ exec     │ │ driver   │ │ (HTTPS    │ │ (IMAP/   │  │
                 │  │ (no net, │ │ (Playw.) │ │  + host   │ │  SMTP)   │  │
                 │  │ scratch) │ │          │ │ allowlist)│ │          │  │
                 │  └──────────┘ └──────────┘ └───────────┘ └──────────┘  │
                 │   each in bwrap+landlock+seccomp (Linux) /              │
                 │           sandbox-exec Seatbelt (macOS)                 │
                 │  ┌────── Postgres (own role, peer auth, UDS only) ─────┐│
                 │  │  pgvector · pg_search/BM25 · Apache AGE             ││
                 │  └─────────────────────────────────────────────────────┘│
                 │  ┌─── Inference (vLLM/SGLang on Linux,                ─┐│
                 │  │     llama.cpp/Ollama on macOS) — OpenAI HTTP        ││
                 │  └─────────────────────────────────────────────────────┘│
                 └──────────────────────────────────────────────────────────┘
```

## Process model

- One **agent core** binary (`hhagent`).
- One **tool worker** process per tool, each in its own sandbox.
- One **channel adapter** process per channel (Telegram, Signal, IMAP).
- One **egress proxy** process (TLS-terminating, allowlist-enforcing).
- One **Postgres** instance (own role, UDS-only, peer auth).
- One **inference server** (vLLM / SGLang / llama.cpp / Ollama, OpenAI HTTP).

## IPC

JSON-RPC ([Model Context Protocol](https://modelcontextprotocol.io)-compatible) over Unix domain sockets. Language-agnostic, no shared memory, no in-process untrusted code.

## Cross-platform

| Concern              | Linux                               | macOS                                          |
| -------------------- | ----------------------------------- | ---------------------------------------------- |
| Sandbox              | bubblewrap + Landlock + seccomp-bpf | `sandbox-exec` (Seatbelt) + `setrlimit`        |
| Service supervisor   | `systemd --user`                    | `launchd` (LaunchAgents)                        |
| Local LLM serving    | vLLM / SGLang on GPU                | llama.cpp / Ollama (Metal/MLX)                 |
| Keyring              | libsecret (`secret-tool`)           | Keychain (`security`)                          |
| Optional micro-VM    | Firecracker / Podman+crun           | Apple `container` CLI (macOS Tahoe+)           |

The same `SandboxPolicy` and `ServiceSpec` Rust structs drive both backends.

## Module map (Rust)

See [`core/src/lib.rs`](../core/src/lib.rs), [`sandbox/src/lib.rs`](../sandbox/src/lib.rs), [`supervisor/src/lib.rs`](../supervisor/src/lib.rs). Modules are stubbed and will be filled in across phases.
