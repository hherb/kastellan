# Kastellan — User Manual

> A castellan is the officer a lord entrusts to hold a stronghold:
> full authority within the walls, none to act beyond them.

Welcome. This manual explains what Kastellan is, how it is built, how it
keeps you safe, and how to set it up and run it. It is written for
**power users** — people comfortable installing software and editing a
configuration file, but who are *not* necessarily programmers or security
specialists.

You do not need to read any source code to understand this manual. Where a
technical term is unavoidable, it is explained in plain language the first
time it appears, and again in the [Glossary](#glossary) at the end of this
page.

---

## What you will get out of reading this

By the end you should be able to answer, in your own words:

- **What is Kastellan, and what is it *for*?**
- **How is it put together?** What are the moving parts, and why are there
  so many of them?
- **Why is it safe to let an AI act on my behalf?** What can go wrong, and
  what stops each thing from going wrong?
- **How do I install it, start it, talk to it, and watch what it's doing?**

---

## How to read this manual

The four chapters are meant to be read in order, but each stands on its own.

| # | Chapter | What it covers |
|---|---------|----------------|
| 1 | [What is Kastellan?](./01-introduction.md) | The idea in plain language: an always-on personal agent that you can trust because it cannot widen its own boundaries. |
| 2 | [How Kastellan is built](./02-architecture.md) | The architecture, explained with everyday analogies. Why it is many small isolated pieces instead of one big program. |
| 3 | [How Kastellan keeps you safe](./03-how-kastellan-protects-you.md) | The specific things that could go wrong with an AI agent, and the specific defence that stops each one. |
| 4 | [Setting up and running Kastellan](./04-setup-and-run.md) | Installing the pieces, starting the agent, talking to it, and watching what it does. |

**If you only have ten minutes:** read Chapter 1, then skim the table in
Chapter 3. That gives you the whole idea and the safety story.

**If you are deciding whether to run it at all:** read Chapter 1 and
Chapter 3 in full. Chapter 3 is the honest account of what is and is not
protected.

**If you have already decided and want it running:** read Chapter 4, and
keep Chapter 1 handy for the vocabulary.

---

## A note on honesty

Kastellan is in active development. This manual describes the design and
the parts that work today, and it tells you plainly when something is still
being built or has a known limitation. A security tool that oversells
itself is worse than useless, so where there is a gap, you will find it
named rather than hidden — especially in Chapter 3.

For the precise, engineer-facing record of what is finished versus
in-progress, the authoritative sources are `docs/devel/ROADMAP.md` and
`docs/threat-model.md`. This manual is the friendly version; those are the
ledger.

---

## Glossary

A handful of words appear throughout. Here they are in one place.

- **Agent** — a program that takes a goal from you, decides on its own
  steps, and carries them out, rather than waiting for you to click every
  button. Kastellan is an agent.
- **The core** — the central Kastellan program: the part that thinks,
  plans, remembers, and decides. It never touches the outside world
  directly; it delegates that to workers.
- **Worker** — a small, separate, locked-down program that does one
  outside-world job (fetch a web page, run some Python, search the web).
  Each runs in its own cell.
- **Sandbox** — an operating-system-enforced "cell" around a worker that
  strictly limits what files it can see, what it can run, and where it can
  connect. Even if a worker is completely taken over, the sandbox holds.
- **CASSANDRA** — Kastellan's plan reviewer. Before any plan runs, it is
  checked against five unchangeable rules. The sandbox stops *forbidden
  actions*; CASSANDRA stops *forbidden intentions*.
- **LLM** (Large Language Model) — the AI "brain" that proposes plans and
  writes text. Kastellan deliberately treats the LLM's output as
  *untrusted* — useful, but never automatically believed.
- **Prompt injection** — an attack where text the agent reads (a web page,
  an email) secretly contains instructions meant to hijack the agent.
- **Egress proxy** — a single guarded "doorway" through which all of a
  worker's internet traffic must pass, so it can be checked and limited.
- **Audit log** — an append-only, tamper-resistant record of every action
  the agent takes. You can always read back exactly what it did.
- **Matrix** — a secure, self-hosted chat system. It is how you talk to
  Kastellan day to day.
