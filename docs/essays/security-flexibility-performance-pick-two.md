# Security, Flexibility, Performance — Pick Two

There's an old engineering proverb that for any non-trivial system you get to
pick two of *fast*, *cheap*, *good*. Autonomous agent systems have their own
version of the triangle, and it bites harder because the thing you're sandboxing
is an LLM that will, given enough turns, try things its designers never imagined.
For kastellan the three corners are **Security**, **Flexibility**, and
**Performance** — and the honest position is that you cannot maximize all three.
kastellan makes its choice explicit: **Security and Flexibility, with Performance
as the deliberate sacrifice.**

## Security is not negotiable, so it isn't really a variable

The first thing to notice is that one corner has been removed from the trade
entirely. kastellan's threat-model invariant — worst-case compromise of the LLM, a
tool, a dependency, or agent-authored Python reaches *at most* the agent's own OS
user, its own Postgres role, its own scratch filesystem, and the allowlisted
endpoints for the *one* compromised tool — is a hard constraint, not a tuning
knob. "One process per worker, one OS sandbox per worker" and "there is no
spawn-unsandboxed escape hatch" are stated as inviolable. Once you fix Security
as a floor rather than a dial, the interesting question collapses to a single
axis: **of the remaining two, which do you trade against the other?**

## Flexibility is the chosen second corner

kastellan spends almost all of its design budget buying flexibility:

- **Vendor neutrality** — AGPL-compatible dependencies only, no NVIDIA/DGX hard
  dependency despite the primary host being a Spark. The system must run on any
  Linux box and on macOS.
- **Cross-platform parity** — the `SandboxBackend` trait is implemented twice
  from one `SandboxPolicy` (`linux_bwrap.rs`, future `macos_seatbelt.rs`). That's
  flexibility purchased at the cost of writing every containment guarantee twice.
- **Hybrid LLM routing** — local-first, escalating to a frontier model only under
  explicit policy. The router exists precisely so the model behind the agent can
  change.
- **Arbitrary Python workers** — the whole point of the worker model is that you
  can drop in new, untrusted, even agent-authored code and still hold the
  invariant.

Every one of these is a flexibility win, and every one of them costs performance.

## Performance is what gets paid out

The architecture's central decisions read almost like a list of performance
sacrifices made in the name of the other two corners:

- **Process-per-worker + sandbox-per-worker.** Every tool call pays for a fresh
  bwrap (or Seatbelt) spin-up: namespace unsharing, `--clearenv`, mount setup, a
  new PID-1. That is real latency on the hot path that an in-process design would
  never pay.
- **stdio JSON-RPC as the *sole* IPC.** Line-delimited JSON over stdin/stdout is
  the slowest plausible transport — serialize, copy across a pipe, deserialize —
  chosen because it's MCP-compatible, language-neutral, and trivially auditable.
  A shared-memory ABI would be faster and far less flexible/auditable.
- **"Rust core, Python only inside sandboxed workers; no PyO3."** The explicit
  refusal to embed Python in-process forecloses the single biggest performance
  shortcut available, because in-process means in-blast-radius.
- **Core-only memory and LLM access.** Workers can't touch Postgres or the model
  directly; everything routes back through the core. More hops, tighter
  containment.

None of these are accidents or things to optimize away later. They are the
*price* of holding Security fixed while keeping Flexibility high.

## Why this is the right two for *this* system

The choice maps cleanly to what kastellan is: a **personal**, security-first agent,
not a high-throughput multi-tenant service. A personal agent runs at human
conversational cadence. The marginal cost of a sandbox spin-up or a JSON
round-trip is invisible against the seconds an LLM already spends thinking, and
utterly worth it when the downside of getting it wrong is an autonomous process
with your credentials doing something irreversible. (The display-blackout
incident — an over-broad `kill(-1)` fanout — is a small reminder of how cheaply
an agent can reach further than intended.) For a system at this scale, **latency
is recoverable; a blast-radius violation is not.**

The systems that pick the *other* two — Flexibility and Performance, sacrificing
Security — are the ones that run untrusted tool code in-process for speed and end
up one prompt-injection away from a full compromise. The systems that pick
Security and Performance, sacrificing Flexibility, are the locked-down
single-vendor appliances that are fast and safe but can only ever do the one
thing they shipped with.

kastellan's bet is that for a personal agentic system the durable winners are
Security and Flexibility, and that Performance is the corner you can afford to
give back — because you can always make a contained, flexible system faster, but
you can rarely make a fast, flexible, *insecure* one safe after the fact.

**Pick two. kastellan picks the two you can't bolt on later.**
