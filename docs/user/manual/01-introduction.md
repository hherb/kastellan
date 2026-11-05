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
  system that only you and the agent share. You can also hand it a task
  straight from the command line on the machine it runs on.
- **Follow a conversation.** When you send a follow-up in the same chat
  ("and the one before that?"), it sees the last few exchanges of that
  conversation — up to three, from the past five hours — so you don't have
  to repeat yourself. Whatever was sensitive in those earlier exchanges
  stays treated as sensitive in the follow-up.
- **Ask you before it acts, when a human must decide.** If its reviewer
  judges that a plan needs your say-so, the task pauses and a short
  question arrives in your Matrix room. You answer with `/approve` or
  `/deny` and the one-time code the question shows you (or from the
  command line). An unanswered question expires and the task is refused,
  never quietly carried out. (The machinery is in place; today's built-in
  rules refuse outright rather than ask, so questions become routine only
  once the planned AI-assisted review stages arrive — see below.)
- **Take instructions by email, as a backup.** Email can act as a
  lower-trust second way to *reach* it: instructions you send by email are
  accepted only through a strict authenticity gate, and the channel stays
  off until you switch it on. (Replies back out by email aren't built
  yet.)
- **Search and read the web.** It can run web searches, fetch and read
  pages, and do a whole research pass in one go — search, read the most
  promising pages, and pull out the passages that answer your question.
- **Drive a web browser.** For pages that only work with a real browser,
  it can render them headlessly and read the result (an optional extra
  you switch on).
- **Search and read your own email archive.** If you keep a local archive
  of your mail, Kastellan can search it, read messages, and save copies of
  attachments into a task's own folder — strictly read-only: it cannot
  alter, send, or delete anything in the archive.
- **Run Python.** It can write and execute small Python programs to compute
  or transform things — inside the strictest cell of all, with no network
  and only a throw-away scratch folder.
- **Remember, and learn skills.** It keeps a long-term memory in a local
  database — facts, past results, and the people, places and things it
  has come across — and pulls the relevant pieces back into mind when a
  new task resembles an old one. When a task goes well it can save the way
  it solved it as a reusable **skill** — including a small Python program
  it wrote. A skill does nothing until *you* approve it, and the agent may
  only use one on its own once you have also "pinned" it.
- **Work through long, list-heavy questions.** In an optional working
  style called **notebook mode**, it writes its own step list, works one
  step at a time, and keeps notes and findings as it goes, so a long task
  doesn't lose track of what it has already found. For "find all the…"
  questions it can sweep a whole list of candidates in one step and hand
  you the matches as a table.
- **Run continuously.** It keeps a durable task list and works through it,
  picking tasks up again after a restart, so it can stay useful over long
  stretches without you babysitting it.

Not all of this is finished, and some of it is opt-in — see *Current
status* below — but it is all built to the same security model, described
in Chapter 3.

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

Kastellan is under active development. As of early November 2026 (latest
published release: **v0.2.0**; everything below describes the current
development line):

**Working today**

- The full sandboxing stack works on **both Linux and macOS**, with an
  optional, stronger **micro-VM** cell (a lightweight virtual machine) on
  each platform for the workers that need the tightest isolation.
- The planning loop, the CASSANDRA reviewer, the long-term memory, the
  audit log, and the AI-model router are all functional.
- The tools work end to end: running allow-listed commands, fetching and
  reading web pages, web search, one-call web research, reading your local
  mail archive (read-only), running Python, and headless browsing (opt-in).
- The **egress proxy** — the guarded internet doorway — is built through
  all its stages (allowlist, forced routing, secret-leak scanning, and
  certificate pinning) and is on by default in the supervised deployment.
- The **Matrix chat channel** works end to end, follow-up messages carry
  the recent conversation, and the reviewer's **"ask the operator"** path
  is wired through: a question reaches you in your Matrix room and is
  answered there.
- An optional **guard model** — a small, local safety-checking AI that
  gives a second opinion on whether something the agent read is trying to
  hijack it — can run as its own supervised service next to the agent.
  It is an extra layer, not a gate: nothing else is loosened because it
  is there, and it is off unless you set it up.
- **Learned skills**, including agent-written Python skills, work under
  your approval: the agent proposes, you approve (and optionally pin)
  from the command line.
- Email can reach the agent as a strictly **gated backup channel**: an
  emailed instruction is accepted only when your own mail server's
  anti-spoofing verdict and a secret pairing token both check out. It is
  off by default.
- A **one-command install** (`kastellan-cli install`) takes a freshly-built
  copy to a running, supervised agent — database and daemon included — without
  needing root.

**Experimental or opt-in**

- **Notebook mode** (and its "sweep a whole list" step, called `sift`) is
  new and still being tuned on real questions. The classic planner remains
  the default; notebook mode is one setting away.

**Not built yet**

- **Replies by email.** Kastellan can receive email instructions but cannot
  yet send email back.
- **AI-assisted review of plans.** CASSANDRA's checks on a plan today are
  fixed, predictable rules (the five principles and the data-sensitivity
  rules). The planned extra review stages that use an AI model to weigh
  ambiguous plans are not built yet — so in practice the reviewer approves
  or refuses today, and the "ask you" path waits for those stages.
- **Using a big cloud AI model.** Kastellan runs on a local model today;
  the controlled route out to a "frontier" model is still to come.
- **Automatic memory-refresh over very long runs** (summarising its
  working context into memory and starting fresh) is designed but not
  built.

Where a feature is still being finished, this manual says so. For the exact
engineering ledger, see `docs/devel/ROADMAP.md`.

---

**Next:** [Chapter 2 — How Kastellan is built](./02-architecture.md), which
opens up the box and shows you the parts.
