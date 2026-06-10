# kastellan — Developer Onboarding Manual

Welcome. This manual walks you through contributing to kastellan from scratch.
The project is a Rust-based personal AI agent with an unusually strong focus on
OS-level security. You do not need to be a Rust expert or a security engineer
to contribute — but you do need to understand a handful of non-negotiable rules
before writing your first line of code.

---

## Who this manual is for

- Developers who know at least one compiled or systems language (C, C++, Go,
  Java) but may be new to Rust.
- Developers who understand what a process and a filesystem are, but have not
  necessarily written kernel-level sandboxing code.
- Anyone curious about how the pieces fit together before diving into the
  source.

---

## Table of contents

### Onboarding (read in order)

| # | File | What you will learn |
|---|------|---------------------|
| 1 | [What is kastellan?](./01-what-is-kastellan.md) | Goals, current status, why it exists |
| 2 | [Dev environment — Linux](./02-dev-env-linux.md) | Install Rust, Postgres, bwrap; first build |
| 3 | [Dev environment — macOS](./03-dev-env-macos.md) | Install Rust, Postgres, sandbox-exec; first build |
| 4 | [Repository tour](./04-repo-tour.md) | Where everything lives |
| 5 | [Build, test, and run](./05-build-test-run.md) | Cargo commands, test flags, interpreting output |
| 6 | [Architecture primer](./06-architecture.md) | Process model, IPC, data flow end-to-end |
| 7 | [Sandboxing explained](./07-sandboxing.md) | bwrap, Landlock, seccomp, Seatbelt — no kernel background needed |
| 8 | [Hard constraints](./08-hard-constraints.md) | The rules that are never negotiated |
| 9 | [Rust patterns used here](./09-rust-patterns.md) | The handful of patterns you will see everywhere |
| 10 | [Your first contribution](./10-first-contribution.md) | Branch, code, test, PR — step by step |

### Subsystem deep dives (read when you touch one)

| # | File | When to read |
|---|------|--------------|
| 11 | [CASSANDRA review pipeline](./11-cassandra-pipeline.md) | Adding a rule, debugging a block, working on the injection guard |
| 12 | [Memory and recall](./12-memory-and-recall.md) | Adding a recall lane, changing promotion, debugging RRF results |
| 13 | [LLM router](./13-llm-router.md) | Adding a backend, changing the wire shape, working on Phase 5 frontier escalation |

---

## Recommended reading paths

**If you are new to both Rust and security engineering:**
Read every file in order, 1 → 10. Skim the code examples; understanding intent
matters more than syntax at this stage. Come back to 11–13 when you start
touching CASSANDRA, memory, or LLM calls.

**If you know Rust but are new to OS sandboxing:**
Start with 1, then 4 → 5 → 6 → 7 → 8 → 10. Skip 9.

**If you are an experienced security engineer picking up Rust:**
Read 1 → 4 → 8. Then jump to 7 (the sandbox chapter is where most of the
interesting design choices live). Then 9 → 10. Subsystem chapters 11–13 are
useful when you reach those areas.

**If you just want to fix a bug and open a PR quickly:**
Read 4, 5, 8 (the hard constraints — non-optional), then 10. Pull in the
matching subsystem chapter (11/12/13) only if the bug is in that area.

---

## Living documents

This manual documents *how to contribute*, not *what to build next*. For
current state and next TODOs, the authoritative sources are:

- `docs/devel/handovers/HANDOVER.md` — what is done, what is green, what is
  next (updated every session).
- `docs/devel/ROADMAP.md` — phased feature list, checked off as shipped.
- `docs/architecture.md` — architectural invariants that reviewers enforce.
- `docs/threat-model.md` — what the security boundary protects and what it
  does not.

---

## Getting help

Open a GitHub issue. Tag it `question` if it is about the codebase; `docs` if
this manual is unclear or wrong.
