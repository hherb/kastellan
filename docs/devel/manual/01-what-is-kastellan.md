# 1 — What is kastellan?

## The one-sentence version

kastellan is a always-on personal AI agent that runs on your own machine,
sandboxes every tool it uses at the OS level, reviews its own plans before
acting, and never trusts its own LLM output.

---

## What it does

When running, kastellan:

- listens on Matrix (self-hosted, single-user, federation off, E2E), with email
  (IMAP/SMTP) as a low-trust failover
- receives a task from you (e.g. "research this topic and draft a summary")
- formulates a multi-step plan using a locally-running or frontier LLM
- runs each plan step through **CASSANDRA**, a semantic review pipeline that
  checks constitutional constraints before any tool runs
- executes approved steps by spawning isolated worker processes (one per tool)
- maintains long-term memory in a local Postgres database (vectors, full-text,
  graph relations)
- repeats until the task is done, then waits for the next one

---

## Why does it exist?

Existing personal-agent projects (including several Rust ones in the
"OpenClaw" family) make one of two compromises:

1. Tools run in-process — fast, but a compromised tool library can read
   everything the agent knows.
2. Tools run in a sandbox that wraps the entire runtime — a bit better,
   but all tools share the same sandbox boundary.

kastellan's position: **one OS process + one kernel sandbox per tool invocation,
every time, no exceptions.** A compromised tool reaches at most the network
endpoints in *that tool's* allowlist. It cannot reach the agent's memory, the
next tool's secrets, or the core process.

The second motivating choice is **semantic oversight**. OS sandboxes are
great at blocking "open this socket"; they cannot block "send this medical
record to the wrong person". CASSANDRA reviews each *plan*, not each syscall.

---

## Current status

The project is in active development. As of mid-2026:

- The full parent-side sandbox stack works on both Linux (bwrap +
  cgroup v2) and macOS (Seatbelt, plus an opt-in Apple `container`
  micro-VM backend for workers that need real memory enforcement).
  Worker-side defence-in-depth on Linux (Landlock + seccomp) is shipped,
  including for the pure-Python workers via a `lock_down()`-then-`execve`
  exec shim (`kastellan-worker-lockdown-exec`).
- The scheduler, memory store (semantic + lexical + graph lanes with RRF
  fusion, L0/L1/L3 layers), CASSANDRA review pipeline (real constitutional
  + deterministic rules + a per-tool worker-output prompt-injection guard),
  audit log + JSONL mirror, LLM router, opaque secret references (`Vault`),
  the L3 skill lifecycle (crystallise → approve → pin → invoke, for both
  templated and agent-authored Python skills), the large-tool-result
  handoff cache, and a substantial CLI are all functional.
- **The egress proxy is shipped** — a per-worker sandboxed CONNECT proxy
  enforcing a host:port allowlist + SSRF guard, with force-routing **on by
  default** so a net worker reaches its allowlist only through its own
  egress sidecar (TLS-intercept MITM, credential-leak scanner, and SPKI
  pinning are all implemented behind it).
- **Workers in the workspace today (Rust):** `prelude` (shared init +
  lockdown shim), `shell-exec`, `web-common` (shared net-egress helpers),
  `web-fetch`, `web-search`, `python-exec` (curated-stdlib executor for
  agent-authored Python), `egress-proxy`, plus `matrix` / `matrix-wire`
  (the Matrix channel worker, hermetic parts only).
- **Python workers (built with `uv`, outside the Cargo workspace, driven
  from core over JSON-RPC):** `gliner-relex` (entity/relation extraction)
  and `browser-driver` (headless Chromium render). Each has a Rust-side
  manifest under `core/src/workers/`.
- **Channel:** Matrix (self-hosted, single-user, federation off, E2E) is
  the primary channel; inbound is in progress (hermetic parts shipped, live
  SDK wiring pending). Email failover and the `workers/mail` worker are not
  yet built (`workers/mail` is an empty scaffold).

See `docs/devel/ROADMAP.md` for the phased feature list and the latest
`docs/devel/handovers/HANDOVER.md` for what shipped most recently.

---

## Key design choices to internalise early

| Choice | What it means for contributors |
|--------|-------------------------------|
| AGPL-3.0 only | Every dependency must have a compatible license. Check before adding one. |
| Rust core | No Python in the core process. Python runs only inside sandboxed workers. |
| Linux + macOS first-class | A feature that works on one platform must have an equivalent on the other. |
| No NVIDIA hard dependency | The agent must run on any Linux box and any Mac. |
| One sandbox per worker invocation | There is no "spawn unsandboxed" path. Do not add one. |

These are not preferences. They are load-bearing constraints described in
`docs/architecture.md` and enforced at PR review.
