# 1 — What is Kastellan?

## The short version

Kastellan is a **personal AI assistant that runs on your own machine and
acts on your behalf — but only ever inside boundaries it cannot widen for
itself.**

You give it a goal in plain language ("research this topic and draft me a
summary", "check this website every morning and tell me if it changes").
It works out the steps, carries them out, remembers what it learned, and
reports back. It can run for days, unattended, picking up tasks as they
arrive.

The unusual part is not what it does — plenty of AI assistants do similar
things. The unusual part is *how carefully it is boxed in.* Everything it
does is designed around a single question: **what is the worst that can
happen if something goes wrong?** — and the answer is kept deliberately,
provably small.

---

## Why the name?

A **castellan** was the officer a medieval lord trusted to hold a castle.
The castellan had complete authority *inside* the walls and none at all
*outside* them. That is exactly the relationship Kastellan has with your
computer and your accounts: full authority to act within tightly drawn
walls, and no way to act — or to grant itself the power to act — beyond
them.

The name is the design.

---

## What it can do

When fully set up, Kastellan can:

- **Talk to you over secure chat.** Day-to-day you message it over
  [Matrix](https://matrix.org) — a self-hosted, end-to-end-encrypted chat
  system that only you and the agent share. Email can act as a low-trust
  backup channel for notifications.
- **Search and read the web.** It can run web searches and fetch and read
  pages, extracting the readable text for you.
- **Drive a web browser.** For pages that only work with a real browser,
  it can render them headlessly and read the result.
- **Run Python.** It can write and execute small Python programs to compute
  or transform things — inside the strictest cell of all.
- **Remember.** It keeps a long-term memory in a local database — facts,
  past results, and useful "skills" it has learned — and pulls the relevant
  pieces back into mind when a new task resembles an old one.
- **Run continuously.** It maintains a task list and works through it,
  resetting its short-term attention from memory as needed, so it can stay
  useful over long stretches without you babysitting it.

Not all of this is finished — see *Current status* below — but it is all
built to the same security model, described in Chapter 3.

---

## Why does it exist?

Most personal-AI tools make one of two trade-offs:

1. **Tools run inside the main program.** This is fast and simple, but if
   *any* tool — or any third-party code it depends on — is compromised, the
   attacker is now inside the program that holds *all* your data, secrets,
   and memory.
2. **The whole program runs in one big sandbox.** Safer, but every tool
   shares the *same* cell. Break one, and you are loose among all of them.

Kastellan takes a third position: **one separate, locked-down process per
tool, every single time, with no exceptions and no "just this once"
shortcut.** If the web-fetching tool is taken over, the attacker reaches
the handful of websites *that one tool* was allowed to talk to — and
nothing else. Not your memory, not your secrets, not the next tool, not the
agent's brain.

There is a second motivation, just as important. Operating-system sandboxes
are excellent at blocking *mechanical* misbehaviour — "open this network
socket", "read this file". They are useless at blocking *intent* — "email
this confidential document to the wrong person". Both of those could be
made of perfectly ordinary, individually-allowed actions. So Kastellan adds
a second, semantic guard called **CASSANDRA** that reviews each *plan* for
what it is *trying to do*, against rules that nothing can switch off.

In short: **mechanical walls** (the sandboxes) plus **a conscience that
can't be argued with** (CASSANDRA).

---

## The idea you most need to internalise

> **Kastellan does not trust its own AI brain.**

This sounds strange, so it's worth dwelling on. The AI model that proposes
plans and writes text is treated as a *clever but untrustworthy advisor*.
Its suggestions are useful, but they are never automatically believed or
automatically executed. Every plan it proposes is reviewed; every action it
wants is checked against policy and run inside a sandbox; every web page or
email it reads is screened for hidden instructions before it is allowed to
influence anything.

This matters because the biggest real-world risk with AI agents is not a
science-fiction "rogue AI". It is far more mundane: the AI reads a web page
that contains hidden instructions, gets confused, and faithfully tries to
do something harmful that it *thinks* you asked for. That class of attack is
called **prompt injection**, and Kastellan's whole architecture assumes it
will happen and is built to contain it.

---

## What it deliberately is *not*

- **It is not a cloud service.** It runs on hardware you control. Your data
  stays local. There is no Kastellan company server in the middle.
- **It is not tied to any one vendor.** It runs on Linux and on macOS, both
  treated as first-class. It does not require a particular brand of GPU or
  any specific cloud. You can point it at a local AI model or, later, a
  frontier one — your choice.
- **It is not a black box.** Every action it takes is written to an
  append-only audit log you can read. Nothing it does is hidden from you.
- **It does not protect you from yourself.** If *you* tell it to do
  something within its allowed powers, it will. The security model defends
  against compromised tools, hijacked AI output, and impersonators — not
  against the legitimate owner giving legitimate (if unwise) instructions.

---

## Current status (honest snapshot)

Kastellan is under active development. As of mid-2026:

- The full sandboxing stack works on **both Linux and macOS**. On Linux there
  is now also an optional, stronger **micro-VM** cell (a lightweight virtual
  machine) for workers that need the tightest possible isolation — matching the
  micro-VM option macOS already had.
- The planning loop, the CASSANDRA reviewer, the long-term memory, the
  audit log, and the AI-model router are all functional.
- Several real tools work end to end: running allow-listed commands,
  fetching and reading web pages, web search, running Python, and headless
  browsing.
- The **egress proxy** — the guarded internet doorway — is built through
  all its stages (allowlist, forced routing, secret-leak scanning, and
  certificate pinning) and is on by default in the supervised deployment.
- The **Matrix chat channel** now works end to end: you can message the agent
  over encrypted, self-hosted chat and get a reply, with a running deployment
  driven by the system service supervisor.
- A **one-command install** (`kastellan-cli install`) takes a freshly-built
  copy to a running, supervised agent — database and daemon included — without
  needing root.

Where a feature is still being finished, this manual says so. For the exact
engineering ledger, see `docs/devel/ROADMAP.md`.

---

**Next:** [Chapter 2 — How Kastellan is built](./02-architecture.md), which
opens up the box and shows you the parts.
