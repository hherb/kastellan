# 1 — What is hhagent?

## The one-sentence version

hhagent is a always-on personal AI agent that runs on your own machine,
sandboxes every tool it uses at the OS level, reviews its own plans before
acting, and never trusts its own LLM output.

---

## What it does

When running, hhagent:

- listens on secure messaging channels (Telegram, Signal) and email (IMAP/SMTP)
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

hhagent's position: **one OS process + one kernel sandbox per tool invocation,
every time, no exceptions.** A compromised tool reaches at most the network
endpoints in *that tool's* allowlist. It cannot reach the agent's memory, the
next tool's secrets, or the core process.

The second motivating choice is **semantic oversight**. OS sandboxes are
great at blocking "open this socket"; they cannot block "send this medical
record to the wrong person". CASSANDRA reviews each *plan*, not each syscall.

---

## Current status

The project is in active development. As of mid-2026:

- The full parent-side sandbox stack is working on both Linux (bwrap +
  cgroup v2) and macOS (Seatbelt). Worker-side defence-in-depth on Linux
  (Landlock + seccomp) is shipped.
- The scheduler, memory store (semantic + lexical + graph lanes with RRF
  fusion), CASSANDRA review pipeline (constitutional + deterministic
  stages + a worker-output prompt-injection guard), audit log + JSONL
  mirror, LLM router (Phase 0 local-only egress), and a growing CLI are
  all functional.
- **Workers in the workspace today:** `prelude` (shared init), `shell-exec`
  (allow-listed argv, no shell interpretation). Both ship and are
  integration-tested end-to-end.
- **Workers scaffolded on disk but not yet in the workspace build:**
  `gliner-relex` (Python entity extraction), `python-exec`, `web-fetch`,
  `browser-driver`, `mail`. These are in-progress directories — they are
  excluded from `[workspace.members]` until they're ready to compile.
- The egress proxy (per-worker outbound allowlist enforcement) is the
  next major infrastructure piece.

See `docs/devel/ROADMAP.md` for the phased feature list and the latest
`docs/devel/handovers/HANDOVER.md` for what shipped this week.

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
