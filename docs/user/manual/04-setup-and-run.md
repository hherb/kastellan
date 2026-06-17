# 4 — Setting up and running Kastellan

This chapter walks you through getting Kastellan onto your machine, starting
it, talking to it, and watching what it does.

> **A note on where things stand.** Kastellan is in active development, so
> today's setup is closer to "build it from source and run it" than
> "double-click an installer". The steps below are real and current, but
> the polished, one-command install is still being built. None of this
> requires you to write code — but it does require you to be comfortable in
> a terminal. If a command fails, the per-component scripts under
> `scripts/` and the engineering notes in `docs/devel/` are the fallback.

---

## Before you start

**You will need:**

- A computer running **Linux** (Ubuntu 24.04 or newer is the best-tested) or
  **macOS**. Both are fully supported.
- A terminal, and permission to install software (`sudo` on Linux,
  Homebrew on macOS).
- Roughly half an hour for the first setup.
- For the AI brain: access to a **local AI model server** (such as Ollama
  on macOS or vLLM/SGLang on a Linux GPU box). Kastellan does not ship a
  model; it talks to one you run.

**You do *not* need:** an NVIDIA GPU specifically, a cloud account, or any
particular vendor's product. Kastellan is vendor-neutral by design.

---

## Step 1 — Get the code and the build tools

Kastellan is built with Rust. Install the Rust toolchain (from
[rustup.rs](https://rustup.rs)), then make it available in your shell:

```sh
source "$HOME/.cargo/env"
```

Get the source (clone the repository), then from the project's top folder
build everything:

```sh
cargo build --workspace
```

This compiles the core agent and all the tool workers. The first build
takes a few minutes; later builds are fast. You can confirm the build is
healthy by running the test suite:

```sh
cargo test --workspace
```

---

## Step 2 — Install the local database (PostgreSQL)

Kastellan keeps its memory and its audit log in a local PostgreSQL database
that runs entirely on your machine.

**On Linux (Ubuntu):** a script installs the right version for you:

```sh
scripts/linux/install-postgres.sh
```

This installs the PostgreSQL **binaries** only — it does **not** start a
shared, system-wide database. Kastellan runs its *own* private database
instance, against its own data folder, reachable only over a local socket
(no network port at all).

**On macOS:** install via Homebrew, e.g.:

```sh
brew install postgresql@18
```

The database is then initialised, locked down (dedicated restricted
account, local-socket-only, peer authentication), and migrated to the right
schema automatically the first time the agent starts up. You don't run those
steps by hand.

---

## Step 3 — One-time sandbox setup (Linux only)

This step is what makes the security model actually work on Linux, so don't
skip it.

Modern Ubuntu restricts the kernel feature Kastellan uses to build its
sandbox cells. A one-time command grants the sandbox tool (`bubblewrap`) the
narrow permission it needs — the *same approach Flatpak uses*:

```sh
sudo scripts/linux/install-bwrap-apparmor-profile.sh
```

> **Why this matters — read this.** Without this step, the sandbox cannot
> create its cells, and (to fail safely) the sandbox tests **skip silently**
> rather than run. A "green" result with skipped sandbox tests means *the
> containment was never actually exercised* — a false sense of safety. After
> running the command above, the sandbox is real and the tests genuinely
> confine their workers. If you are on Linux, do this before trusting the
> agent with anything.

**On macOS** there is nothing to do here — the system's `sandbox-exec` works
out of the box.

---

## Step 4 — Point Kastellan at your AI model

Kastellan talks to an OpenAI-style local model server. You tell it where
that server is, and which model to use, with environment variables:

```sh
export KASTELLAN_LLM_LOCAL_URL="http://127.0.0.1:11434"   # e.g. Ollama on macOS
export KASTELLAN_LLM_LOCAL_MODEL="<your-model-name>"
```

On a Linux GPU host running vLLM or SGLang, the default port is typically
`8000`; on macOS with Ollama it is `11434`. Use whatever your model server
listens on.

Frontier (cloud) models are deliberately *not* reachable yet — that path is
gated behind a future policy layer and is off by default. Kastellan starts
local-only.

---

## Step 5 — Set up the chat channel (Matrix)

Day to day, you talk to Kastellan over **Matrix** — a self-hosted,
end-to-end-encrypted chat that only you and the agent share. Setting up your
own small Matrix homeserver is its own task; the project includes a full
guide and helper scripts:

- The setup guide: `docs/deploy/matrix-homeserver.md`
- Helper script: `scripts/matrix/setup-conduwuit.sh` (sets up a small,
  single-user homeserver with federation turned off)

Once your homeserver is running, you point Kastellan at it and tell it which
chat account is *you* (so it only accepts you as a partner):

```sh
export KASTELLAN_MATRIX_HOMESERVER="https://your-homeserver"
export KASTELLAN_MATRIX_PEERS="@you:your-homeserver"
```

A new chat partner must then **pair** using a single-use code you issue from
the command line, so no one can message the agent unless you've explicitly
let them in.

> The live Matrix connection is the one part of the channel still being
> finished. Until it lands, you can drive the agent directly from the
> command line (next step), which exercises the exact same planning,
> review, sandbox, and audit machinery.

---

## Step 6 — Start the agent

With the pieces in place, start the core daemon:

```sh
./target/debug/kastellan
```

It performs a fail-safe startup — checking the database, connecting under
its restricted account, and starting the audit-log mirror — then waits for
work. It shuts down cleanly when you stop it.

For a long-running, always-on deployment, Kastellan installs itself under
your platform's service supervisor (`systemd --user` on Linux, `launchd` on
macOS) so it starts on login and restarts itself with sensible backoff if it
crashes. The supervisor definitions live in the `supervisor` component;
running supervised is what turns on the strongest defaults (for example, the
egress proxy's forced routing).

---

## Step 7 — Give it a task and watch it work

You can hand the agent a task directly with the command-line tool. For
example:

```sh
./target/debug/kastellan-cli ask "Summarise the latest news on <topic>"
```

The agent will plan the task, run it through CASSANDRA, carry out the
approved steps in sandboxed workers, and return a result — exactly the flow
described in Chapter 2.

You can optionally tell it how sensitive the task's data is (a
"classification floor"), which raises the bar CASSANDRA applies.

---

## Step 8 — See exactly what it did (the audit log)

Everything the agent does is recorded. To watch the audit log live, like a
rolling feed:

```sh
./target/debug/kastellan-cli audit tail
```

This shows every tool the agent ran, every AI call, every block CASSANDRA
or the egress proxy imposed, and every message in and out — with secrets
redacted. This viewer reads the on-disk mirror, so it works even without a
live database connection. **When in doubt about what the agent is doing,
this is the place to look.**

---

## Turning tools on and off

Several workers are **opt-in** and stay off until you explicitly enable
them, because they widen what the agent can do. You enable each by setting
its switch before starting the agent. For example:

```sh
export KASTELLAN_PYTHON_EXEC_ENABLE=1        # allow the agent to run Python
export KASTELLAN_BROWSER_DRIVER_ENABLE=1     # allow headless browsing
```

Network-using tools are confined to an **allowlist** of sites you control —
for instance, the web-fetch worker will only ever connect to the hosts you
list:

```sh
export KASTELLAN_WEB_FETCH_ALLOWLIST='["example.com","docs.example.org"]'
```

The principle throughout: **a tool can do nothing you haven't switched on,
and a network tool can reach nowhere you haven't listed.** Start narrow and
widen deliberately.

---

## A sensible first-run checklist

1. ✅ Built the project (`cargo build --workspace`) and the tests pass.
2. ✅ PostgreSQL installed.
3. ✅ **Linux only:** ran the AppArmor sandbox script (Step 3) — don't skip.
4. ✅ Pointed it at your local AI model (Step 4).
5. ✅ Started the daemon and saw it come up cleanly (Step 6).
6. ✅ Gave it a harmless task and got a sensible answer (Step 7).
7. ✅ Watched that task appear in the audit log (Step 8).
8. ✅ Only then enabled extra tools and network allowlists, one at a time.

---

## Where to go next

- **Something not working?** The per-component scripts under `scripts/` and
  the engineering notes under `docs/devel/` are the detailed fallback.
- **Want the precise security guarantees and their current limits?**
  `docs/threat-model.md` is the authoritative, technical version of
  Chapter 3.
- **Curious about the internals?** The developer manual lives under
  `docs/devel/manual/`.

That's the whole loop: build it, lock down the sandbox, point it at a model,
start it, give it work, and watch the audit log. Everything Kastellan does,
it does within walls it cannot move — and you can always read back exactly
what happened.
