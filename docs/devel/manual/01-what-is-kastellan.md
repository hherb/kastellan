# 1 — What is kastellan?

## The one-sentence version

kastellan is an always-on personal AI agent that runs on your own machine,
sandboxes every tool it uses at the OS level, reviews its own plans before
acting, and never trusts its own LLM output.

---

## What it does

When running, kastellan:

- listens on Matrix (self-hosted, single-user, federation off, E2E), with a
  low-trust email failover (inbound-only today: a sandboxed worker polls a
  localmail server's `/v1` API — the core never speaks raw IMAP/SMTP)
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

The project is in active development. As of early November 2026 (workspace
version 0.2.0, 27 crates):

- **Sandboxing.** The full parent-side sandbox stack works on both Linux
  (bwrap + a `systemd-run --scope` cgroup) and macOS (Seatbelt), with
  worker-side defence-in-depth on Linux (Landlock + seccomp, including for
  the pure-Python workers via the `kastellan-worker-lockdown-exec` shim).
  Both platforms also have an **opt-in micro-VM backend**: **Firecracker** on
  Linux (sha256-pinned guest kernel verified at every boot, unprivileged VMM
  confinement, warm/idle reuse, persistent-VM workers with a store that
  survives a respawn) and Apple's `container` on macOS. Micro-VM mode is off
  by default and switched on per worker.
- **Agent loop.** A Postgres-backed scheduler runs plan → CASSANDRA review →
  dispatcher chokepoint → sandboxed step, with audit rows at every
  transition and crash recovery. Channel tasks carry **conversational
  continuity**: a follow-up in the same conversation sees the last few turns.
  The planner loop is the **task notebook** (the model keeps its own step
  list and findings, plus a `sift` step for going through long lists); the
  older `classic` loop retired in #993.
- **CASSANDRA.** Constitutional and deterministic data-classification stages,
  a per-tool worker-output prompt-injection guard, and an **escalation path**:
  a step that needs a human is asked of the operator (in their Matrix room,
  or via `kastellan-cli inbox`) instead of being silently blocked. An
  optional model-based guard tier (Shieldstral, served by llama.cpp's
  `llama-server` as a supervised `kastellan-guard` unit) can tighten the
  injection guard's verdict; it fails open, so it is not a security
  boundary. The LLM-based plan-review stages are **not built yet**.
- **Memory.** Four-lane recall (semantic + lexical + graph +
  entity-similarity, fused with RRF); layered prompt assembly (L0 meta-rules,
  the always-in-context L1 index, recalled memories, L3 approved skills); the
  L3 skill lifecycle (crystallise → approve → pin → invoke, for templated and
  agent-authored Python skills) and the large-tool-result handoff cache.
  L4 (episodic digests) is declared but not yet written by anything.
- **Egress.** A per-worker sandboxed egress proxy (host:port allowlist, SSRF
  guard, TLS interception with a credential-leak scanner, certificate
  pinning) that every networked worker is **force-routed through by
  default**.
- **Channels.** Matrix (self-hosted, single-user, federation off, E2E) is the
  primary channel and is live end-to-end. The email failover is
  **inbound-only and off by default**: the sandboxed `email-in` worker polls
  a localmail `/v1` endpoint behind a core-side DMARC + pairing-token gate.
  Outbound email (SMTP replies) is not built yet.
- **Workers in the workspace (Rust):** `prelude` (shared lock-down + the exec
  shim), `shell-exec`, `web-common` (shared net-egress library), `web-fetch`,
  `web-search`, `web-research` (search → fetch → rank passages in one call),
  `mail` (six read-only `mail.*` tools against a localmail archive),
  `email-in`, `python-exec` (no network, curated stdlib), `egress-proxy`,
  `embed-broker` / `search-broker` (trusted sidecars bridging a jailed worker
  to the embedding / SearxNG backend), `matrix` / `matrix-wire`,
  `microvm-run` / `microvm-init` (Firecracker launcher and guest PID 1), and
  the demo workers `kv-demo` / `net-demo`.
- **Python workers** (outside the Cargo workspace, driven over JSON-RPC, each
  with a Rust-side manifest under `core/src/workers/`): `gliner-relex`
  (entity/relation extraction) and `browser-driver` (headless Chromium
  render, opt-in).
- **Operations.** `kastellan-cli install` brings up a supervised per-user
  deployment (`kastellan.target` under `systemd --user` or launchd: Postgres,
  the core and, when configured, the guard backend) in one command; a second
  full-project security audit was completed and remediated in September 2026.

Still on the roadmap: outbound email, the CASSANDRA LLM review stages,
TLS inspection of browser and email-channel traffic, L4 digests, the
frontier-egress worker and the Phase 5 frontier-escalation policy gate.

See [`docs/devel/ROADMAP.md`](../ROADMAP.md) for the phased feature list and
the latest [`docs/devel/handovers/HANDOVER.md`](../handovers/HANDOVER.md) for
what shipped most recently.

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
