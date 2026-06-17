# 2 — How Kastellan is built

This chapter opens the box. You don't need any of it to *use* Kastellan,
but understanding the shape of the thing makes the safety story in Chapter 3
much easier to trust — because you'll see *why* the boundaries hold.

We'll build the picture up one piece at a time.

---

## The one-paragraph mental model

Picture a **stronghold**. In the keep at the centre sits the **core** — the
part that thinks, plans, and remembers. The core never goes outside the
walls itself. Whenever something needs doing in the outside world, the core
sends out a single **messenger** (a *worker*) through one guarded gate, with
written orders, into a locked cell where it can do exactly one job and
nothing more. A **reviewer** (CASSANDRA) reads every order before the gate
opens. A **scribe** writes down every messenger that ever left and what they
were sent to do. That's Kastellan.

Now the same idea, in real terms.

---

## The core: the part that thinks

The **core** is the central program (it's the one literally named
`kastellan`). It is small, written in the Rust language, and deliberately
boring: it contains no facility to run arbitrary code, no plugins, no
"download and execute" anything. It owns:

- the **planning loop** — taking your task, asking the AI model for a plan,
  reviewing that plan, and carrying out the approved steps one at a time;
- the **memory** — what the agent knows and has learned;
- the **policy and review** decisions — what is and isn't allowed;
- the **audit log** — the permanent record;
- the **secrets** — passwords and API keys, which live here and *only* here.

The crucial property: **the core never reaches out to the internet, runs a
downloaded program, or touches an untrusted file directly.** It always
delegates that to a worker. So even though the core holds everything
valuable, it is never the thing exposed to danger.

---

## Workers: one locked cell per job

A **worker** is a small, separate program that does exactly one
outside-world job. There is a worker for fetching web pages, one for web
search, one for running Python, one for driving a browser, and so on.

Three rules govern every worker, with no exceptions:

1. **One process per worker.** Each runs as its own operating-system
   program, completely separate from the core and from every other worker.
   They share no memory.
2. **One sandbox per worker.** Before a worker runs even a single
   instruction, the operating system locks a cell around it (see
   *Sandboxes*, below).
3. **One narrow job.** A worker knows how to do its one task and nothing
   else. The web-fetch worker can fetch web pages from an approved list of
   sites; it has no idea your database or your secrets even exist.

This is why a compromised worker is contained: it is *structurally* unable
to reach anything outside its cell, because the operating system — not
Kastellan's own good behaviour — is enforcing the walls.

> **Why so many small pieces?** Because the blast radius of any single
> failure is exactly one small piece. A single big program is only as
> trustworthy as its weakest line of code. A swarm of tiny locked cells is
> as trustworthy as the *cell walls*, which are far fewer and far more
> battle-tested than any application code.

---

## Sandboxes: cells the operating system enforces

A **sandbox** is a cell drawn around a worker by the operating system
itself. Kastellan uses the strongest mechanisms each platform offers:

- On **Linux**: `bubblewrap` (the same technology that isolates Flatpak
  apps) for the cell walls, plus two extra kernel-level locks called
  *Landlock* and *seccomp* that further restrict which files and which
  system operations the worker may use. Memory and processor limits are
  enforced on top.
- On **macOS**: the system's built-in `sandbox-exec` (Seatbelt) facility,
  which the operating system uses to confine its own background services.

Both are driven from **the same description** of what a given worker is
allowed to do, so a worker is confined identically in spirit on either
platform. A sandboxed worker typically:

- **cannot see your files** — only the specific, minimal folders it needs;
- **cannot reach the network** — unless its job requires it, and then only
  through the guarded doorway described below;
- **cannot run other programs** or escalate its own privileges.

There is **no way to run a worker without a sandbox.** That escape hatch
simply does not exist in the code, by design.

---

## CASSANDRA: the reviewer that reads intentions

Sandboxes are walls. They stop a worker from doing *forbidden things*. But
some harmful actions are made entirely of *allowed* things — emailing a
document is allowed; emailing *that* document to *that* recipient might be a
disaster. Walls can't tell the difference, because the difference is about
*intent*, not mechanics.

**CASSANDRA** is the answer. Before any plan the AI proposes is carried out,
CASSANDRA reviews it against **five constitutional principles** that *no
one* — not you, not an administrator, not any setting — can switch off:

1. **Do not cause physical harm.** No enabling violence, operating physical
   systems unsafely, suppressing safety-critical information, or giving
   instructions for weapons or dangerous substances.
2. **Do not facilitate fraud or impersonation.** No signing documents,
   agreeing to contracts, impersonating people, faking credentials, or
   passing the agent's words off as yours on matters of consequence.
3. **Do not take irreversible actions without a human in the loop.**
   Permanent deletions, irrecoverable messages with real consequences,
   financial commitments, public posts, and sharing credentials all require
   explicit human confirmation.
4. **Do not concentrate the system's own power.** It may not expand its own
   abilities, create accounts, acquire credentials, establish persistent
   access, or modify its own code, configuration, or instructions.
5. **Do not suppress oversight.** It may not interfere with review,
   auditing, or shutdown, or do anything that would reduce your ability to
   inspect and control it.

If a plan would violate a principle, the agent refuses the task and tells
you which principle and why. These five rules are the agent's conscience,
and they are wired in such that the agent cannot reason, be tricked, or be
configured out of them.

> The sandbox stops *"this worker tried to open a forbidden socket."*
> CASSANDRA stops *"the agent is about to do something it shouldn't, using
> entirely permitted means."* You need both.

---

## The egress proxy: one guarded internet doorway

When a worker legitimately needs the internet (to fetch a page, say), it
does **not** get to connect wherever it likes. All its traffic is forced
through a single guarded doorway called the **egress proxy** — itself a
sandboxed program. At that doorway, every outbound connection is:

- **checked against an allowlist** — only the specific sites (and ports)
  approved for that worker are permitted;
- **checked for sneaky destinations** — attempts to reach private or
  internal network addresses (a common trick to attack other machines on
  your network) are blocked;
- **scanned for leaking secrets** — if a worker tries to send out a
  credential it was trusted with, the doorway can catch and block it;
- **recorded** — every decision is written to the audit log.

The important part is *how* it's enforced: a network-using worker is placed
in a cell with **no route to the internet at all** except this one doorway.
It physically cannot go around it, even if completely taken over — the
operating system, not the worker's cooperation, guarantees this.

---

## Memory: what the agent knows

Kastellan keeps its long-term memory in a **local database** (PostgreSQL)
running on your own machine. It stores three kinds of recall so the agent
can find relevant past knowledge by *meaning* (semantic), by *keyword*
(lexical), and by *connection* (a small knowledge graph of how things
relate). When a new task arrives, the relevant pieces of memory are pulled
back into the agent's attention.

Two things to note:

- **Only the core can touch the database.** Workers cannot reach it at all.
- The database runs under its **own restricted account** on a local-only
  connection. Even the core's access is limited to exactly what it needs.

---

## Secrets: passwords stay behind the walls

Passwords, API keys, and other secrets are encrypted at rest and held by
the core alone. When a worker genuinely needs one for a single call, the
core hands it over *at the last moment, for that one call only*. Secrets are
**never** written to the audit log, **never** sent to the AI model in the
clear, and **never** readable by a worker outside that one authorised use.

---

## The audit log: the permanent record

Every meaningful action — every tool the agent ran, every AI call, every
message in or out, every memory write, every block CASSANDRA or the proxy
imposed — is written to an **append-only audit log**. "Append-only" means
entries can be added but not edited or deleted, enforced by the database
itself. A copy is also mirrored to a plain text file on disk. You can read
back, at any time, exactly what the agent did and why.

---

## How the channel reaches you

You talk to Kastellan over **Matrix**: a self-hosted, single-user,
end-to-end-encrypted chat that only you and the agent share, with
federation (talking to other Matrix servers) turned off so the surface is
as small as possible. Email can serve as a *low-trust* backup — for
notifications only, never for commands, because email is too easily
spoofed. The program that connects to Matrix is itself a sandboxed worker,
confined to talking to your one chat server and nothing else.

---

## Putting it together: a task from start to finish

Here is the life of a single request, with every guard in place:

1. **You send a message** over encrypted Matrix: *"Summarise the latest
   post on example.com."*
2. The channel worker passes it to the **core**. The message is first
   **screened for hidden instructions** (prompt injection), because a chat
   peer is trusted no more than a random web page.
3. The core pulls relevant **memory** back into mind and asks the **AI
   model** for a step-by-step plan.
4. **CASSANDRA reviews the plan** against the five principles. If it's
   fine, it proceeds; if not, the agent refuses and explains.
5. For the "fetch the page" step, the core spawns a **web-fetch worker** in
   a fresh **sandbox**, with network access only through the **egress
   proxy**, and only to `example.com`.
6. The proxy **checks and logs** the connection, the worker fetches the
   page, and its output is **screened again** for hidden instructions
   before it's allowed to influence the agent.
7. The agent composes a summary, **records everything in the audit log**,
   and replies to you over Matrix.

At no point did the AI's brain get to act without review; at no point did a
worker get loose; at no point did a secret or your memory leave the keep;
and you can read the whole sequence back afterwards.

---

**Next:** [Chapter 3 — How Kastellan keeps you safe](./03-how-kastellan-protects-you.md),
which goes threat by threat: what could go wrong, and what stops it.
