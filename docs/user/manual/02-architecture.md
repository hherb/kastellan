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
opens, and sends a runner to fetch *you* when an order needs the lord's own
say-so. A **scribe** writes down every messenger that ever left and what
they were sent to do. That's Kastellan.

Now the same idea, in real terms.

---

## The core: the part that thinks

The **core** is the central program (it's the one literally named
`kastellan`). It is small, written in the Rust language, and deliberately
boring: it contains no facility to run arbitrary code, no plugins, no
"download and execute" anything. It owns:

- the **task list** — a durable queue of everything you have asked for,
  kept in the database so that nothing is lost if the machine restarts.
  Quick tasks and long-running ones get separate lanes, so a big research
  job doesn't hold up a quick question;
- the **planning loop** — taking a task, asking the AI model for a plan,
  reviewing that plan, and carrying out the approved steps one at a time;
- the **memory** — what the agent knows and has learned;
- the **policy and review** decisions — what is and isn't allowed;
- the **audit log** — the permanent record;
- the **secrets** — passwords and API keys, which live here and *only* here.

The crucial property: **the core never reaches out to the internet, runs a
downloaded program, or touches an untrusted file directly.** It always
delegates that to a worker. So even though the core holds everything
valuable, it is never the thing exposed to danger.

The one outside connection the core does make is to the **AI model**
itself, and that goes through a single, dedicated piece of the core (the
"model router") rather than from anywhere it likes. Normally that model
runs locally, on your own machine or one you control.

---

## Workers: one locked cell per job

A **worker** is a small, separate program that does exactly one
outside-world job. The current set:

| Worker | What it does |
|--------|--------------|
| **shell-exec** | Runs a command — but only one from a list *you* have approved. |
| **web-fetch** | Fetches a web page from an approved site and extracts the readable text. |
| **web-search** | Runs a web search through a search engine you host yourself (SearxNG). |
| **web-research** | Does a whole research pass in one call: search, fetch the most promising pages, and pick out the passages most relevant to your question. |
| **browser-driver** | Opens a page in a real (headless) browser, for sites that don't work without one. Read-only, and off unless you switch it on. |
| **mail** | Searches and reads your local mail archive, and saves attachments into the task's own folder. Strictly read-only. |
| **python-exec** | Runs a small Python program the agent wrote, with no network at all and only a throw-away scratch folder. |
| **matrix** | Connects to your Matrix chat server — the agent's end of your conversation. |
| **email-in** | Watches for incoming email instructions, for the backup email channel. |
| **gliner-relex** | Picks out the people, places, organisations and relationships mentioned in what the agent reads, for its memory. |

Alongside them run a few **trusted helpers**, each also in its own cell:
the **egress proxy** (the internet doorway, described below), and two small
"broker" helpers that let a locked-in worker use your search engine or your
"meaning" index (for semantic search) *without* that worker getting
internet access of its own.

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
  *Landlock* and *seccomp* that the worker puts on itself before doing
  anything, further restricting which files and which system operations it
  may use. Memory and processor limits are enforced on top.
- On **macOS**: the system's built-in `sandbox-exec` (Seatbelt) facility,
  which the operating system uses to confine its own background services.

Both are driven from **the same description** of what a given worker is
allowed to do, so a worker is confined identically in spirit on either
platform. (The two platforms' mechanisms are not equally strong; Chapter 3
says plainly where they differ.) A sandboxed worker typically:

- **cannot see your files** — only the specific, minimal folders it needs;
- **cannot reach the network** — unless its job requires it, and then only
  through the guarded doorway described below;
- **cannot run other programs** or escalate its own privileges.

For workers that need the strongest possible isolation, Kastellan can go one
step further and run the worker inside a **micro-VM** — a lightweight
virtual machine with its own separate operating-system kernel, a much
harder shell than an ordinary sandbox. On Linux this uses Firecracker (the
technology behind many cloud "serverless" services) and is available for
the Python, web, browser and Matrix workers; on macOS it uses Apple's
`container` tool, for the Python worker and the memory extractor. The
micro-VM's kernel is a pinned, checked copy, verified every time a VM
starts. It is **opt-in per worker** and off by default; when it is not set
up, the worker simply stays in its ordinary sandbox.

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

CASSANDRA also checks **data sensitivity**. Every task carries a
sensitivity level — public, personal, clinical-confidential, or secret —
set from what you asked (or stated by you outright), which can be raised as
the task goes on but never lowered. A plan may not quietly
move data to a step marked as handling *less* sensitive material than it
really is, so, for example, something learned from your private mail cannot
slip out through a step labelled "public".

If a plan would break one of these rules, the agent refuses the task and
tells you which rule and why. These rules are the agent's conscience, and
they are wired in such that the agent cannot reason, be tricked, or be
configured out of them.

Today these checks are **fixed, predictable rules** written into the
program. Further review stages that use an AI model to weigh the genuinely
ambiguous cases are designed but not built yet.

> The sandbox stops *"this worker tried to open a forbidden socket."*
> CASSANDRA stops *"the agent is about to do something it shouldn't, using
> entirely permitted means."* You need both.

---

## Asking you: when a human must decide

Besides "go ahead" and "refused", the reviewer has a third answer:
**"a human must decide this."** When it gives that answer, Kastellan does
not guess and does not quietly refuse. Instead:

1. the task **pauses** (it stops holding any working slot while it waits);
2. a short question goes to **your Matrix room**, describing what the agent
   wants to do and carrying a **one-time code**;
3. you reply with the command and the code, e.g. `/approve 7f3a9c2e1b` or
   `/deny 7f3a9c2e1b` — or answer from the
   command line with `kastellan-cli inbox`;
4. on approval the task resumes, and goes ahead **only with exactly the plan
   you approved** — if the agent then comes up with a different plan, you
   are asked again.

Some details that keep this safe: the code is random and single-use, so
nobody else in a chat can answer for you by guessing; your reply is treated
as a plain yes/no, never as new instructions; and every question has a
**deadline** — if you don't answer in time, the task is refused rather than
left hanging or carried out. Every question and answer is in the audit log.

As noted above, today's built-in rules refuse outright rather than ask, so
in practice these questions will become routine once the AI-assisted
review stages arrive. The path itself is built and works end to end.

---

## Screening what comes back

The reviewer looks at plans *before* they run. A second screen looks at
what a worker brings *back* — a web page, an email, a search result —
before the agent is allowed to read it, because that is where hidden
instructions (prompt injection) arrive. A fixed catalogue of known attack
patterns checks every tool result, and anything it catches is withheld from
the agent and logged.

On top of that you can add an optional **guard model**: a small,
specialised safety AI, running locally, that gives a second opinion on each
result the catalogue let through. It can only ever make the screen
*stricter* — turn "let through" into "blocked", never the reverse. If a
check fails or times out, that result is judged by the catalogue alone,
because the real boundary is still the sandbox and the allowlists.

The guard model is **advisory defence-in-depth, not a gate**: it catches
many attacks but not all of them, so nothing else in the system is loosened
because it is there. When you set it up, it runs as its own supervised
background service (`kastellan-guard`) that starts before the agent and
comes back after a reboot. If you configure it but configure it wrongly,
the agent refuses to start — rather than run with a guard you think is on
but isn't. It is off unless you set it up; the settings are described in
[`docs/deploy/operator-env.md`](../../deploy/operator-env.md).

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
running on your own machine. When a new task arrives, it searches that
memory four ways at once and merges the results:

- by **meaning** — finding notes about the same idea even in different
  words;
- by **keyword** — finding exact names, numbers and phrases;
- by **connection** — following a small map of how people, places and
  things relate to each other;
- by **who or what is mentioned** — finding notes about the same person,
  place or thing.

The relevant pieces are then pulled back into the agent's attention.
Names and relationships it picks up go into a **quarantine** first; you can
review and approve them from the command line before they count as known
facts.

Memory comes in layers. At the bottom are a few **ground rules** that are
always present and that the agent cannot change. Above them sits a short
list of **standing notes** that are always in view (you can add and remove
these yourself), then the general store of things it has learned, and the
**skills** described next.

Two things to note:

- **Only the core can touch the database.** Workers cannot reach it at all.
- The database runs under its **own restricted account** on a local-only
  connection. Even the core's access is limited to exactly what it needs.

---

## Skills: learning from what worked

When a task goes well, Kastellan can save *how* it solved it as a
reusable **skill**: either a recipe of tool steps, or — for computing
tasks — the small Python program it wrote. Skills follow a strict
**trust ladder**, and you control every rung:

1. **Proposed.** A freshly-saved skill is untrusted. The agent cannot use
   it, and it is not even shown to the planner.
2. **Approved.** Once you approve it (`kastellan-cli memory l3 approve`),
   the planner can see it and suggest it, and you can run it yourself.
3. **Pinned.** Only when you also pin it (`kastellan-cli memory l3 pin`)
   may the agent use it on its own.

Approval re-checks the skill first (a Python skill, for instance, is
refused if it refers to any of your stored secrets), and the skill is
checked again every time it runs. A Python skill always runs inside the
same no-network Python cell as any other agent-written code. You can
revoke or delete a skill at any time.

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
message in or out, every memory write, every question it asked you, every
block CASSANDRA or the proxy imposed — is written to an **append-only audit
log**. "Append-only" means entries can be added but not edited or deleted,
enforced by the database itself. A copy is also mirrored to a plain text
file on disk. You can read back, at any time, exactly what the agent did
and why.

---

## How the channel reaches you

You talk to Kastellan over **Matrix**: a self-hosted, single-user,
end-to-end-encrypted chat that only you and the agent share, with
federation (talking to other Matrix servers) turned off so the surface is
as small as possible. Only chat accounts you have explicitly **paired**
with the agent can give it tasks; messages from anyone else are dropped.
The program that connects to Matrix is itself a sandboxed worker, confined
to talking to your one chat server and nothing else.

Each message you send becomes a task. So that follow-ups make sense, a new
task in the same conversation is shown the **last few exchanges** — up to
three, from the past five hours. Those earlier exchanges are screened like
any other input, and their data sensitivity carries over: if an earlier
answer drew on your private mail, the follow-up is treated as private too.

Email can serve as a *lower-trust* backup route in. Because email is so
easily spoofed, an emailed instruction is accepted only through a strict
gate: your **own mail server** must have stamped the message as genuinely
coming from your address (an anti-spoofing verdict called DMARC), the
message body must contain a **secret token** you were given when you
paired that address, and a final **database check** confirms both pieces
of evidence before anything is queued. The whole channel stays off until
you configure it, and it is one-way for now: Kastellan can *receive*
email, but sending replies by email is not built yet — a reply to an
emailed task is recorded in the audit log as undelivered. The part that
listens for email is, of course, a sandboxed worker like all the rest, and
email does not yet carry conversation history from one message to the next.

---

## How it plans: two working styles

The planning loop can work in one of two styles, chosen with one setting
for the whole agent:

- **Classic** (older, opt-in). The AI model sees the whole history of the
  task so far each time it decides the next move. Simple, but on long
  tasks that history grows large and slow, and the model can lose track.
- **Notebook** (the default). The model writes its own **step list** at
  the start, works through **one step at a time**, and ticks each one off
  with a short note and any **findings** — facts together with where they
  came from. Each turn it reads only what that turn needs, so a long task
  stays quick and focused, and the final answer is written from the
  findings. While a notebook task runs you can send it a **correction**
  from the command line ("the invoice is in the other account"), which it
  picks up at its next step.

Notebook mode adds one more step type, **`sift`**, for "find every…"
questions. Instead of examining candidates one by one, the agent makes one
broad search, then reads *every* candidate in a single reviewed step; a
quick yes/no check on each keeps only the true matches, with the details
it was asked for, in a per-task **scrapbook**. The answer can then show
those matches as a table built directly from the scrapbook — so the table
lists what was actually found, not what the model remembers finding.

Either way, nothing about safety changes: every step in both styles goes
through the same CASSANDRA review, the same allowlists and the same
sandboxes, and notes the model writes to itself are screened like any
other text. Notebook mode answered all seven of the project's test
questions, so it became the default; classic stays available until it is
retired.

---

## Putting it together: a task from start to finish

Here is the life of a single request, with every guard in place:

1. **You send a message** over encrypted Matrix: *"Summarise the latest
   post on example.com."*
2. The channel worker passes it to the **core**, which first checks that it
   came from a **paired** account. The message is then **screened for
   hidden instructions** (prompt injection), because a chat peer is
   trusted no more than a random web page.
3. The core pulls relevant **memory** (and, for a follow-up, the recent
   conversation) back into mind and asks the **AI model** for a
   step-by-step plan.
4. **CASSANDRA reviews the plan** against the five principles and the
   data-sensitivity rules. If it's fine, it proceeds; if not, the agent
   refuses and explains; if a human must decide, you get a question in
   Matrix and the task waits for your answer.
5. For the "fetch the page" step, the core spawns a **web-fetch worker** in
   a fresh **sandbox**, with network access only through the **egress
   proxy**, and only to `example.com`.
6. The proxy **checks and logs** the connection, the worker fetches the
   page, and its output is **screened again** for hidden instructions (by
   the catalogue, and by the guard model if you run one) before it's
   allowed to influence the agent.
7. The agent composes a summary, **records everything in the audit log**,
   and replies to you over Matrix.

At no point did the AI's brain get to act without review; at no point did a
worker get loose; at no point did a secret or your memory leave the keep;
and you can read the whole sequence back afterwards.

---

**Next:** [Chapter 3 — How Kastellan keeps you safe](./03-how-kastellan-protects-you.md),
which goes threat by threat: what could go wrong, and what stops it.
