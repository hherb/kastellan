# 4 — Setting up and running Kastellan

This chapter walks you through getting Kastellan onto your machine, starting
it, talking to it, and watching what it does.

> **A note on where things stand.** Kastellan is in active development, so
> today's setup is still "build it from source", not "double-click an
> installer". The good news: once built, a **single command**
> (`kastellan-cli install`, Step 5) takes it all the way to a running,
> self-supervising agent — database, daemon, and service units included — with
> no root required. None of this requires you to write code, but it does
> require you to be comfortable in a terminal. If a command fails, the
> per-component scripts under `scripts/` and the engineering notes in
> `docs/devel/` are the fallback.

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
build everything. For a real install, build the optimised (release) binaries —
that is what the one-command installer in Step 5 expects:

```sh
cargo build --release --workspace
```

This compiles the core agent and all the tool workers. The first build
takes a few minutes; later builds are fast. You can confirm the build is
healthy by running the test suite:

```sh
cargo test --workspace
```

(If you just want to experiment from the source tree without installing, a
plain `cargo build --workspace` produces the same programs under
`target/debug/` — see the "run it straight from the build" note in Step 5.)

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

**On macOS:** install PostgreSQL 18 via Postgres.app or Homebrew, e.g.:

```sh
brew install postgresql@18
```

You only need the binaries here — you do **not** create or start a database
by hand. The one-command install in Step 5 initialises Kastellan's private
cluster, locks it down (dedicated restricted account, local-socket-only, peer
authentication), and migrates it to the right schema for you. (On macOS, if
the installer can't find your PostgreSQL binaries automatically, point it at
them with `--pg-bin-dir`.)

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

Kastellan talks to an OpenAI-style local model server — you tell it where
that server is and which model to use. The one-command install (Step 5)
**defaults to a local Ollama** at `http://127.0.0.1:11434` and pulls its
default models for you (after checking they'll fit in memory), so if that
describes your setup you can skip ahead. Otherwise, pass the details to the
installer:

```sh
kastellan-cli install --llm-url "http://127.0.0.1:8000" --llm-model "<your-model-name>"
```

On a Linux GPU host running vLLM or SGLang, the model server usually listens
on port `8000`; on macOS with Ollama it is `11434`. You can change these at
any time by editing `~/.config/kastellan/kastellan.env` and restarting the
service.

Frontier (cloud) models are deliberately *not* reachable yet — that path is
gated behind a future policy layer and is off by default. Kastellan starts
local-only.

---

## Step 5 — Install and run (one command)

This is the step that ties everything together. From the repository root
(so the installer can find the prompt and seed assets), run:

```sh
./target/release/kastellan-cli install
```

In one go this:

- copies the built programs into `~/.local/lib/kastellan/` and their assets
  into `~/.local/share/kastellan/`;
- initialises Kastellan's own private PostgreSQL database (idempotent — safe
  to re-run);
- writes a tunable configuration file at `~/.config/kastellan/kastellan.env`
  (readable only by you);
- installs the service-supervisor units (`systemd --user` on Linux, `launchd`
  on macOS) so the agent starts on login and **restarts itself with sensible
  backoff if it crashes**;
- starts everything and waits until both the database and the daemon report
  healthy before returning.

No root (`sudo`) is needed — everything lives under your own user account,
which is exactly the security boundary Kastellan is built around. Running
under the supervisor is also what switches on the strongest defaults, such as
the egress proxy's forced routing.

Re-running `kastellan-cli install` after a fresh build is a clean upgrade (it
stops the services, swaps in the new programs, and starts again). To remove
everything:

```sh
kastellan-cli uninstall            # remove the agent and its services
kastellan-cli uninstall --purge    # also delete the database and stored secrets
```

Check that it is healthy at any time with:

```sh
systemctl --user status kastellan.target   # Linux
kastellan-cli secret list                  # both platforms — lists secret names, never values
```

> **Prefer to run it straight from the build?** For quick experiments you can
> skip the install entirely and launch the daemon directly from a debug build
> with `./target/debug/kastellan`. It performs the same fail-safe startup
> (checking the database, connecting under its restricted account, starting
> the audit-log mirror) and shuts down cleanly when stopped — it just isn't
> supervised or started on login. In that mode you configure it with the
> `KASTELLAN_*` environment variables (for example
> `KASTELLAN_LLM_LOCAL_URL` / `KASTELLAN_LLM_LOCAL_MODEL`) instead of the
> `kastellan.env` file.

---

## Step 6 — Set up the chat channel (Matrix)

Day to day, you talk to Kastellan over **Matrix** — a self-hosted,
end-to-end-encrypted chat that only you and the agent share. This channel
**works end to end today**: a message from you runs through the full agent —
memory, plan, CASSANDRA review, sandboxed workers, audit log — and comes back
as a reply. Setting up your own small Matrix homeserver is its own task; the
project includes a full guide and helper scripts:

- The setup guide: `docs/deploy/matrix-homeserver.md`
- Helper script: `scripts/matrix/setup-conduwuit.sh` (sets up a small,
  single-user homeserver with federation turned off)

Once your homeserver is running, point Kastellan at it and tell it which chat
account is the *agent's* (the account the bot logs in as). The simplest way is
to pass the details to the installer — or add the equivalent lines to
`~/.config/kastellan/kastellan.env` and restart:

```sh
kastellan-cli install \
  --matrix-homeserver-url "https://your-homeserver" \
  --matrix-user "@kastellan:your-homeserver"
```

Then tell the agent which account is *you*, so it only ever accepts you as a
partner (in `kastellan.env`, comma-separated if more than one):

```sh
KASTELLAN_MATRIX_PEERS="@you:your-homeserver"
```

Finally, a new chat partner must **pair** before the agent will listen to
them. You issue a single-use, short-lived code from the command line and send
it to the bot once:

```sh
kastellan-cli pair issue        # prints a code to send to the bot from your account
kastellan-cli matrix probe      # optional: confirm the live connection is up
```

No one can message the agent unless you've explicitly paired them in this
way — unpaired messages are dropped and never even reach the agent.

---

## Step 7 — Give it a task and watch it work

Besides chatting over Matrix, you can hand the agent a task directly with the
command-line tool. For example:

```sh
kastellan-cli ask "Summarise the latest news on <topic>"
```

The agent will plan the task, run it through CASSANDRA, carry out the
approved steps in sandboxed workers, and return a result — exactly the flow
described in Chapter 2.

> The installed `kastellan-cli` lives in `~/.local/lib/kastellan/`; add that
> folder to your `PATH` to call it by name, or run it from a source checkout as
> `./target/release/kastellan-cli` (or `./target/debug/kastellan-cli`).

You can optionally tell it how sensitive the task's data is (a
"classification floor"), which raises the bar CASSANDRA applies.

---

## Step 8 — See exactly what it did (the audit log)

Everything the agent does is recorded. To watch the audit log live, like a
rolling feed:

```sh
kastellan-cli audit tail
```

This shows every tool the agent ran, every AI call, every block CASSANDRA
or the egress proxy imposed, and every message in and out — with secrets
redacted. This viewer reads the on-disk mirror, so it works even without a
live database connection. **When in doubt about what the agent is doing,
this is the place to look.**

---

## Turning tools on and off

Several workers are **opt-in** and stay off until you explicitly enable
them, because they widen what the agent can do. You turn each one on with its
switch. In a supervised install these settings go in
`~/.config/kastellan/kastellan.env` (then restart with
`systemctl --user restart kastellan.target`); if you run the daemon by hand
they are ordinary environment variables you `export` first. The names are the
same either way:

```sh
KASTELLAN_PYTHON_EXEC_ENABLE=1        # allow the agent to run Python
KASTELLAN_BROWSER_DRIVER_ENABLE=1     # allow headless browsing
```

Network-using tools are confined to an **allowlist** of sites you control —
for instance, the web-fetch worker will only ever connect to the hosts you
list:

```sh
KASTELLAN_WEB_FETCH_ALLOWLIST='["example.com","docs.example.org"]'
```

The principle throughout: **a tool can do nothing you haven't switched on,
and a network tool can reach nowhere you haven't listed.** Start narrow and
widen deliberately.

---

## A sensible first-run checklist

1. ✅ Built the project (`cargo build --release --workspace`) and the tests pass.
2. ✅ PostgreSQL 18 installed.
3. ✅ **Linux only:** ran the AppArmor sandbox script (Step 3) — don't skip.
4. ✅ Decided on your local AI model (Step 4).
5. ✅ Ran `kastellan-cli install` and saw both services come up healthy (Step 5).
6. ✅ Paired yourself over Matrix, or ran a task from the CLI (Steps 6–7).
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

That's the whole loop: build it, lock down the sandbox, install it in one
command, point it at a model, give it work, and watch the audit log.
Everything Kastellan does, it does within walls it cannot move — and you can
always read back exactly what happened.
