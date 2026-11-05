# 4 — Setting up and running Kastellan

This chapter walks you through getting Kastellan onto your machine, starting
it, talking to it, and watching what it does. At the end there is a
reference list of every command a day-to-day operator uses.

> **A note on where things stand (early November 2026; the source tree
> carries version 0.2.0).** Kastellan is in active development, so today's
> setup is still "build it from source", not "double-click an installer".
> The good news: once built, a **single command** (`kastellan-cli install`,
> Step 5) takes it all the way to a running, self-supervising agent —
> database, daemon, and service units included — with no root required.
> None of this requires you to write code, but it does require you to be
> comfortable in a terminal. If a command fails, the per-component scripts
> under `scripts/` and the engineering notes in `docs/devel/` are the
> fallback.

---

## Before you start

**You will need:**

- A computer running **Linux** (Ubuntu 24.04 or newer is the best-tested) or
  **macOS**. Both are fully supported.
- A terminal, and permission to install software (`sudo` on Linux,
  Homebrew on macOS).
- Roughly half an hour for the first setup, plus the time to download a
  model.
- For the AI brain: a **local AI model server** — oMLX on macOS, Ollama on
  Linux by default, or vLLM / SGLang on a Linux GPU box. Kastellan does not
  ship a model; it talks to one you run. The default models want a machine
  with **about 32 GiB of RAM or more** (Step 4).

**You do *not* need:** an NVIDIA GPU specifically, a cloud account, or any
particular vendor's product. Kastellan is vendor-neutral by design.

---

## Step 1 — Get the code and build it

Kastellan is built with Rust. Install the Rust toolchain (from
[rustup.rs](https://rustup.rs)), then make it available in your shell:

```sh
source "$HOME/.cargo/env"
```

Get the source (clone the repository), then from the project's top folder
build everything with the project's release script:

```sh
bash scripts/build-release.sh
```

This compiles the core agent and all the tool workers into
`target/release/` — which is exactly where the installer in Step 5 picks
them up. The first build takes a while; later builds are faster.

> **Always use the script, never a plain `cargo build --release`.** The
> chat (Matrix) worker needs an extra build option that a plain release
> build leaves out, and a plain build run *after* the script silently
> replaces the working chat worker with one that refuses to start. If you
> ever do run a plain release build, run the script again afterwards. The
> script prints a fingerprint of the chat worker at the end so you can see
> that it is the right one.

You can optionally run the test suite (it is long):

```sh
cargo test --workspace
```

If you see `[SKIP]` lines in its output (visible with
`cargo test --workspace -- --nocapture`), those tests did **not** run — on
Linux, usually because Step 3 hasn't been done yet. A green result made of
skips proves nothing about containment.

(If you just want to experiment from the source tree without installing, a
plain `cargo build --workspace` produces the same programs under
`target/debug/` — see the "run it straight from the build" note in Step 5.)

---

## Step 2 — Install the local database (PostgreSQL)

Kastellan keeps its memory and its audit log in a local PostgreSQL 18
database, with the `pgvector` extension, that runs entirely on your machine.

**On Linux (Ubuntu):** a script installs the right version and extension for
you:

```sh
scripts/linux/install-postgres.sh
```

This installs the PostgreSQL **binaries** only — it does **not** start a
shared, system-wide database. Kastellan runs its *own* private database
instance, against its own data folder, reachable only over a local socket
(no network port at all).

**On macOS:** install PostgreSQL 18 with `pgvector`, either from
Postgres.app or Homebrew, e.g.:

```sh
brew install postgresql@18 pgvector
```

You only need the binaries here — you do **not** create or start a database
by hand. The one-command install in Step 5 initialises Kastellan's private
cluster, locks it down (dedicated restricted account, local-socket-only, peer
authentication), and migrates it to the right schema for you. If the
installer can't find your PostgreSQL binaries automatically (it looks in the
usual Homebrew and Ubuntu locations; Postgres.app is not one of them), point
it at them with `--pg-bin-dir`.

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
> create its cells: the agent refuses to start its workers, and (to fail
> safely) the sandbox tests **skip** rather than run. A "green" test result
> with skipped sandbox tests means *the containment was never actually
> exercised* — a false sense of safety. After running the command above, the
> sandbox is real and the tests genuinely confine their workers. If you are
> on Linux, do this before trusting the agent with anything.

**On macOS** there is nothing to do here — the system's `sandbox-exec` works
out of the box.

(The optional micro-VM cells need their own one-time setup; see "Stronger
cells: micro-VMs" below. You don't need them to get started.)

---

## Step 4 — Point Kastellan at your AI model

Kastellan talks to an OpenAI-style local model server — you tell it where
that server is and which model to use. The one-command install (Step 5)
defaults to a different server per platform:

| Platform | Default server | Default chat model | Default embedding model | RAM the default chat model needs |
|---|---|---|---|---|
| **macOS** | oMLX, `http://127.0.0.1:8000` | `Qwen3.8-27B-8bit` | `embeddinggemma-300m-bf16` | **~33 GiB** |
| **Linux** | Ollama, `http://127.0.0.1:11434` | `gemma4:26b-a4b-it-q8_0` | `embeddinggemma` | **~32 GiB** |

Neither server ships with Kastellan. On macOS, install
[oMLX](https://github.com/madroidmaq/omlx) and start it on `:8000`; on Linux,
install [Ollama](https://ollama.com).

On **Linux** (with a local Ollama) the installer pulls the default models for
you. On **macOS** it cannot — oMLX manages its own model library, so add the
two models from oMLX's admin UI first; the installer prints a note rather
than attempting a download it has no way to perform. (`--no-start` skips the
pull on both, since it only lays down files without bringing anything up.)

The **memory check runs on every platform** and is fail-closed: if the model
you asked for cannot fit in this machine's RAM (its estimated size plus 20%
headroom and 2 GiB for the operating system), the install stops and tells
you what it needs rather than handing you a system that dies at its first
task. The last column above is what the defaults ask for — on a smaller
machine, choose a smaller model:

```sh
# macOS: an MLX repo id — a -4bit variant, or fewer parameters
kastellan-cli install --llm-url "http://127.0.0.1:8000" --llm-model "Qwen3.8-27B-4bit"

# Linux: an Ollama registry tag
kastellan-cli install --llm-url "http://127.0.0.1:11434" --llm-model "gemma4:9b"
```

Model names are **not** interchangeable between the two: oMLX takes MLX repo
ids (`Qwen3.8-27B-8bit`), Ollama takes registry tags (`gemma4:26b-a4b-it-q8_0`).
Because of that, `--llm-url` **must** be given together with `--llm-model`
(and usually `--embedding-model`) — passing the URL alone is rejected,
rather than silently pairing your endpoint with a model name it has never
heard of.

On a Linux GPU host running vLLM or SGLang, the model server usually listens
on port `8000`; pass that URL with the model name it serves.

Frontier (cloud) models are deliberately *not* reachable yet — that path is
gated behind a future policy layer and is not built. Kastellan runs
local-only.

---

## Step 5 — Install and run (one command)

This is the step that ties everything together. From the repository root
(so the installer can find the prompt and seed files), run:

```sh
./target/release/kastellan-cli install
```

(adding the `--llm-…` flags from Step 4 if you are not using the defaults).
In one go this:

- checks the chosen models fit in memory and, on Linux with Ollama, pulls
  them;
- copies the built programs into `~/.local/lib/kastellan/` and their assets
  into `~/.local/share/kastellan/`, and puts a link to `kastellan-cli` in
  `~/.local/bin/` so you can call it by name (it warns you if that folder is
  missing from your `PATH`, or comes after the system folders there);
- initialises Kastellan's own private PostgreSQL database (idempotent — safe
  to re-run);
- writes a configuration file at `~/.config/kastellan/kastellan.env`
  (readable only by you) — **regenerated from the command-line flags every
  time you install** (see "Your settings" below);
- installs the service-supervisor units (`systemd --user` on Linux, `launchd`
  on macOS) so the agent starts on login — and, on Linux, at boot even if
  nobody logs in — and **restarts itself with sensible backoff if it
  crashes**;
- starts everything and waits until both the database and the daemon report
  healthy before returning.

The full set of install flags:

```
kastellan-cli install [--llm-model <name>] [--llm-url <url>] [--embedding-model <name>]
                      [--pg-bin-dir <dir>] [--from <built-bin-dir>]
                      [--matrix-homeserver-url <url> --matrix-user <@bot:server>]
                      [--no-start]
```

`--from` names the folder holding the built programs (by default, the folder
the `kastellan-cli` you ran lives in); `--no-start` installs everything but
starts nothing.

No root (`sudo`) is needed — everything lives under your own user account,
which is exactly the security boundary Kastellan is built around. Running
under the supervisor is also what switches on the strongest defaults, such as
**forced egress routing** (Chapter 3, Threat 2).

**Check that it is healthy** at any time:

```sh
systemctl --user status kastellan.target          # Linux
launchctl print gui/$UID/kastellan-core           # macOS
kastellan-cli secret list                         # both — proves the CLI reaches the database
```

The daemon's own log is in `~/.local/state/kastellan/` (`kastellan-core.out`
and `kastellan-core.err`; on Linux also `journalctl --user -u kastellan-core`).

> **Prefer to run it straight from the build?** For quick experiments you can
> skip the install entirely and launch the daemon directly from a debug build
> with `./target/debug/kastellan`. It performs the same fail-safe startup
> (checking the database, connecting under its restricted account, starting
> the audit-log mirror) and shuts down cleanly when stopped — but it isn't
> supervised or started on login, and forced egress routing is **off** unless
> you set `KASTELLAN_EGRESS_FORCE_ROUTING=1` yourself (the daemon says so at
> startup). In that mode you configure it with ordinary `KASTELLAN_*`
> environment variables (for example `KASTELLAN_LLM_LOCAL_URL` /
> `KASTELLAN_LLM_LOCAL_MODEL`) instead of the settings files.

---

## Your settings: `kastellan.env` and `kastellan.env.local`

There are two settings files side by side in `~/.config/kastellan/`, and
which one you edit matters:

- **`kastellan.env`** belongs to the installer. It is rewritten from the
  install flags every time you run `install`, so anything you add there is
  lost on the next upgrade. (If an install would drop or change a key you
  added, it warns you and keeps a copy as `kastellan.env.bak`.)
- **`kastellan.env.local`** is yours. The installer never writes it, and its
  values **win** over the generated file. Every setting this chapter tells you
  to add goes here. Give it the same private permissions: `chmod 600`.

Write one `KEY=value` per line, with comments on their own line — not
`export KEY=value`, and not a `#` comment after a value (it becomes part of
the value). Paste with care: a non-breaking space copied from a web page
makes a line silently declare nothing.

How a change takes effect differs by platform:

| Platform | After editing `kastellan.env.local` |
|---|---|
| **Linux** | `systemctl --user restart kastellan-core` |
| **macOS** | re-run `kastellan-cli install` **with the same flags you used originally** — launchd bakes the values in at install time, and a bare `install` would also reset your model choices and drop the Matrix settings |

To confirm your settings actually arrived, look for the `operator overlay`
lines: one printed by `install`, and one in the daemon's log at startup that
names any key that did *not* reach the running agent.

Two settings worth knowing about from the start:

- `KASTELLAN_TIMEZONE=Europe/Berlin` (any IANA zone name) — the time zone the
  agent uses for "today"; unset means the machine's own.
- `KASTELLAN_PLANNER_MODE=classic` or `notebook` — how the agent works
  through a task. `classic` (the default) keeps one growing plan; `notebook`
  has the agent write its own step list, work one step per turn and keep
  notes as it goes, which keeps long tasks manageable. Any other value stops
  the daemon at startup, naming the setting.

The complete reference, including the exact file grammar, is
[Operator environment settings](../../deploy/operator-env.md).

---

## Step 6 — Set up the chat channel (Matrix)

Day to day, you talk to Kastellan over **Matrix** — a self-hosted,
end-to-end-encrypted chat that only you and the agent share. This channel
**works end to end today**: a message from you runs through the full agent —
memory, plan, CASSANDRA review, sandboxed workers, audit log — and comes back
as a reply.

**6a. A homeserver and two accounts.** Setting up your own small Matrix
homeserver is its own task; the project includes a full guide and helper
scripts:

- The setup guide: [`docs/deploy/matrix-homeserver.md`](../../deploy/matrix-homeserver.md)
- Helper script: `scripts/matrix/setup-conduwuit.sh` (sets up a small,
  single-user homeserver with federation turned off)

You need two accounts on it: **yours**, and one for the **agent** (the
"bot", e.g. `@kastellan:your-homeserver`). Use any Matrix app (Element, for
example) for your own.

**6b. Give Kastellan the bot's password and log it in once.** With the
database running (after a first `install`), store the bot's password as a
secret — you are prompted for it, and it is never echoed or logged — then run
the one-time login, which also sets up the bot's encryption keys:

```sh
kastellan-cli secret put matrix_kastellan_password
kastellan-cli matrix probe --homeserver "https://your-homeserver" \
                           --user "@kastellan:your-homeserver"
```

`matrix probe` prints the bot's identity when the login worked. (Its built-in
defaults point at the project's own server, so always pass your
`--homeserver` and `--user`.) After that the agent reuses the saved login;
you do not need the password again for normal restarts and upgrades.

**6c. Say who you are.** Add your own account to
`~/.config/kastellan/kastellan.env.local`, so the bot only accepts
invitations and messages from you (comma-separated if more than one):

```sh
KASTELLAN_MATRIX_PEERS=@you:your-homeserver
```

**6d. Switch the channel on.** Re-run the installer with the homeserver and
the bot account:

```sh
kastellan-cli install \
  --matrix-homeserver-url "https://your-homeserver" \
  --matrix-user "@kastellan:your-homeserver"
```

(These two flags must always be given together. Pass them again on every
later `install`, or the channel switches off — see "Upgrading" below.)

**6e. Pair.** A new chat partner must **pair** before the agent will listen
to them. Invite the bot into a direct (two-person) room, then issue a code
from the command line and send it — just the code, nothing else — to the
bot from your account:

```sh
kastellan-cli pair issue                 # prints a single-use code, valid 10 minutes
kastellan-cli pair list                  # shows active pairings
kastellan-cli pair revoke matrix @you:your-homeserver   # removes one
```

(`pair issue` also takes `--ttl-mins <n>` for a different lifetime and
`--label <text>` to note who it was for.) No one can message the agent unless
you've explicitly paired them in this way — unpaired messages are dropped and
never even reach the agent.

---

## Step 7 — Give it a task

**Over Matrix,** just write to the bot in your paired room. Replies come back
in the same room; a reply that couldn't be delivered while the channel was
down is sent when it comes back.

**From the command line,** hand it a task directly:

```sh
kastellan-cli ask "Summarise the latest news on <topic>"
```

The agent will plan the task, run it through CASSANDRA, carry out the
approved steps in sandboxed workers, and print the result — exactly the flow
described in Chapter 2. Press Ctrl-C to cancel the task. Two options:

- `--long` puts the task in the long-running lane, so it doesn't hold up
  quick ones (`--fast`, the default, is for quick tasks).
- `--classification-floor Personal` (or `ClinicalConfidential`, or `Secret`;
  the default is `Public`) tells CASSANDRA how sensitive the task's data is,
  which raises the bar it applies. Kastellan also guesses a floor from the
  wording of your request; stating one yourself is the safer habit for
  sensitive tasks.

**Keeping an eye on tasks:**

```sh
kastellan-cli tasks list                 # recent tasks (filter with --lane, --state, -n 20)
kastellan-cli tasks status <id>          # one task in detail
kastellan-cli tasks tail <id>            # print one task's audit entries
kastellan-cli tasks cancel <id>          # stop a task
kastellan-cli tasks correct <id> "<text>"   # steer a running notebook-mode task
```

`tasks correct` adds a correction the agent sees on its next turn and that
the reviewer checks every later step against; it only has an effect in
`notebook` planner mode (the command warns you when it may not).
`tasks fail <id>` marks a running task as crashed by hand, for one that is
stuck.

---

## Step 8 — See exactly what it did (the audit log)

Everything the agent does is recorded. To watch the audit log live, like a
rolling feed:

```sh
kastellan-cli audit tail                         # follow new entries
kastellan-cli audit tail --from-start            # replay everything, then follow
kastellan-cli audit tail --from-start --no-follow   # print everything and exit
```

This shows every tool the agent ran, every AI call, every block CASSANDRA,
the injection screen or the egress proxy imposed, and every message in and
out — with secrets redacted. The viewer reads the on-disk mirror in
`~/.local/state/kastellan/`, so it works even without a live database
connection. **When in doubt about what the agent is doing, this is the place
to look.**

---

## Answering the agent's questions

When a review decides that a human must decide (Chapter 3, "When a human
must decide"), the task pauses and the question arrives in your Matrix room
with a short code. Answer by sending exactly the command and the code:

```
/approve 7f3a9c2e1b
/deny 7f3a9c2e1b
```

Anything extra in the message (a "thanks!", a quote) makes it unreadable,
and the bot tells you the right shape. You can also answer from the command
line:

```sh
kastellan-cli inbox list                      # open questions
kastellan-cli inbox show <ask-id>             # one question in full
kastellan-cli inbox resolve <ask-id> approve  # or: deny   [--note "<why>"]
```

An unanswered question expires after 24 hours and the task stops.

---

## Turning tools on and off

Several workers are **opt-in** and stay off until you explicitly enable
them, because they widen what the agent can do. Others are present but can
reach nothing until you list what they may reach. The switches go in
`~/.config/kastellan/kastellan.env.local`, then apply them as described in
"Your settings" above.

| Tool | How it is switched on |
|---|---|
| **python-exec** — run Python | `KASTELLAN_PYTHON_EXEC_ENABLE=1` |
| **browser-driver** — headless browsing | `KASTELLAN_BROWSER_DRIVER_ENABLE=1`, its Python environment (`scripts/workers/browser-driver/install.sh`), and an allowlist (below) |
| **web-search** | `KASTELLAN_WEB_SEARCH_ENDPOINT` — the address of a SearxNG search engine; `scripts/web-search/setup-searxng.sh` starts one locally and prints the setting |
| **web-research** | uses the same SearxNG (`KASTELLAN_WEB_RESEARCH_ENDPOINT`), and reads only from the sites you allowlist for it |
| **web-fetch** | present by default, but fetches only from sites you allowlist |
| **shell-exec** | present by default, but runs only the commands you allowlist |
| **mail** — read your local mail archive | `KASTELLAN_MAIL_ENDPOINT` + `KASTELLAN_MAIL_TOKEN_FILE` (below) |
| **gliner-relex** — pick out people, places and relationships | `KASTELLAN_GLINER_RELEX_ENABLE=1` and its environment (`scripts/workers/gliner-relex/install.sh`) |

**Allowlists** are kept in the database, not in a settings file, and are
managed with the command line:

```sh
kastellan-cli tools allowlist add web-fetch example.com
kastellan-cli tools allowlist add web-fetch .example.org        # leading dot = any subdomain
kastellan-cli tools allowlist add shell-exec /usr/bin/ls        # commands by full path
kastellan-cli tools allowlist list
kastellan-cli tools allowlist remove web-fetch example.com
```

The same works for `web-research` and `browser-driver`. Every change is
recorded in the audit log. The daemon reads allowlists when it starts, so
restart it afterwards (Linux: `systemctl --user restart kastellan-core`;
macOS: `launchctl kickstart -k gui/$UID/kastellan-core`).

The principle throughout: **a tool can do nothing you haven't switched on,
and a network tool can reach nowhere you haven't listed.** Start narrow and
widen deliberately.

### The mail tool

The mail tool searches and reads a [localmail](https://github.com/hherb/localmail)
archive — strictly read-only, and only the mail accounts you grant it. It
needs a current localmail (API version 1.6 or newer; older versions are
refused by name). In short:

1. In localmail, mint a dedicated **API key** (not a login token, which
   expires after 30 days) and grant it the accounts the agent may read.
2. Save the key in a private file, e.g. `~/.config/kastellan/mail-token`
   (`chmod 600`).
3. In `kastellan.env.local`:

   ```sh
   KASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443
   KASTELLAN_MAIL_TOKEN_FILE=/home/you/.config/kastellan/mail-token
   ```

   For a localmail on the same machine, use `http://127.0.0.1:<port>` — the
   literal address, not `localhost`. The tool may reach exactly that address
   and nothing else. If your localmail uses a home-made (self-signed)
   certificate, it also needs `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` — the
   installer writes a commented explanation of it into `kastellan.env`.

The full walkthrough is [`docs/workers/mail.md`](../../workers/mail.md).

### The inbound email channel

Separately from the mail *tool*, you can let the agent receive gated
instructions **by email**. It is off until you set all five
`KASTELLAN_EMAIL_*` settings (`_ENDPOINT`, `_SUBSCRIPTION`, `_ADDRESS`,
`_TOKEN_FILE`, `_AUTHSERV_ID`); a partial set leaves the channel off and says
so loudly in the log, while everything else keeps running. Then pair your
address — there is no pairing by email itself:

```sh
kastellan-cli pair issue-token --channel email --peer you@example.org
```

The printed token must appear in the body of **every** message you send the
agent, and messages must be plain text (or include a plain-text part).
`KASTELLAN_EMAIL_AUTHSERV_ID` must match, exactly, the name your own mail
server writes into its authentication stamp — get it wrong and every message
is silently refused. The installer writes a commented guide to all of this
into `kastellan.env`; copy the lines you need into `kastellan.env.local`.
Remember that the channel is inbound only: replies are not sent by email yet.

### Stronger cells: micro-VMs

The workers that face the most hostile input can run inside a throwaway
virtual machine instead of an ordinary sandbox (Chapter 3). This is opt-in
per worker:

- **Linux (Firecracker):** run `sudo scripts/linux/install-firecracker-vsock.sh`
  once, install Firecracker with `scripts/workers/microvm/install-firecracker.sh`,
  build the images with `bash scripts/workers/microvm/rebuild-all-rootfs.sh`
  (after `bash scripts/build-release.sh`, and again after every rebuild),
  then set `KASTELLAN_<WORKER>_USE_MICROVM=1` — for `PYTHON_EXEC`,
  `WEB_FETCH`, `WEB_SEARCH`, `WEB_RESEARCH`, `BROWSER_DRIVER` or `MATRIX`.
- **macOS (Apple `container`):** `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1` or
  `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`, with Apple's `container` tool
  installed and the worker's image built.

Without the setup, the micro-VM backend refuses to start and the worker
stays in its ordinary sandbox. The details are in the developer manual,
[Chapter 7 — Sandboxing](../../devel/manual/07-sandboxing.md).

---

## Optional: the guard model

You can give the injection screen a second opinion: a small dedicated safety
model (Shieldstral) that judges each tool result the pattern screen let
through (Chapter 3, Threat 1). It is served by `llama-server` from
llama.cpp, normally on the same machine. In `kastellan.env.local`:

```sh
KASTELLAN_LLM_GUARD_URL=http://127.0.0.1:8081/v1
KASTELLAN_LLM_GUARD_MODEL=shieldstral
KASTELLAN_LLM_GUARD_TAU=0.79552656
# optional: let the supervisor run the guard server for you
KASTELLAN_GUARD_SERVER_BIN=/home/you/.local/bin/llama-server
KASTELLAN_GUARD_SERVER_MODEL=/home/you/models/shieldstral/Shieldstral-1.0-3B-Q8_0.gguf
```

`KASTELLAN_LLM_GUARD_TAU` is the decision threshold and has no default; the
value above is the calibrated one from the operator settings reference.
With the two `KASTELLAN_GUARD_SERVER_*` lines, re-running `install` adds a
supervised `kastellan-guard` service that starts before the agent and comes
back after a reboot; stop any guard server you started by hand first.

Be aware that a configured guard is taken seriously: if its settings are
wrong, or it is still unreachable after about a minute, **the daemon refuses
to start** and says why. That is deliberate — a guard you believe is on must
not be silently off. Remove the lines to go back to the pattern screen alone.
Details: [Operator environment settings](../../deploy/operator-env.md),
"A supervised guard backend".

---

## Reviewing what the agent learns

Kastellan keeps a few kinds of memory that you can inspect and prune.

**Standing notes** (always in the agent's context) — yours to add and remove:

```sh
kastellan-cli memory l1 add "I prefer answers in metric units."
kastellan-cli memory l1 list             # what is in context now (--all for every note)
kastellan-cli memory l1 remove <id>
```

**Skills** the agent crystallised from tasks that went well start out
untrusted. Read one before trusting it, then move it up the ladder:

```sh
kastellan-cli memory l3 list
kastellan-cli memory l3 show <id>        # read the skill in full first
kastellan-cli memory l3 approve <id>     # you may run it; the agent may suggest it
kastellan-cli memory l3 pin <id>         # the agent may use it on its own
kastellan-cli memory l3 revoke <id>      # back to untrusted
kastellan-cli memory l3 remove <id>
kastellan-cli memory l3 run <id> [--arg name=value]   # dry run; add --execute to really run it
```

**People, places and relationships** that the extraction worker finds are
**quarantined** until you approve them:

```sh
kastellan-cli entities list --state quarantined
kastellan-cli entities show <id>
kastellan-cli entities approve <id> [<id>...]
kastellan-cli entities reject <id> [<id>...]
kastellan-cli entities merge --keep <id> --drop <id>[,<id>...]   # duplicates
kastellan-cli relations show <entity-id>                          # its connections
```

The vocabulary of kinds the extractor may use is yours too
(`kastellan-cli entities kinds add|remove|list`,
`kastellan-cli relations kinds add|remove|list`); the agent cannot widen it
on its own.

---

## Secrets

Secrets (passwords, API keys) are stored encrypted, and the agent only ever
handles a reference to them:

```sh
kastellan-cli secret put <name>          # prompts for the value; never echoed
kastellan-cli secret list                # names and dates only, never values
kastellan-cli secret delete <name>
```

When piping a value in (`printf '%s' "$VALUE" | kastellan-cli secret put <name> --raw`),
`--raw` stores the bytes exactly as given.

---

## Upgrading

**On Linux,** one script takes a supervised install to the latest `main`: it
switches the checkout to `main`, pulls, rebuilds with
`scripts/build-release.sh`, re-installs (carrying your Matrix settings over),
restarts, and checks that the services and the Matrix channel came back:

```sh
scripts/upgrade_from_git.sh
```

If an upgrade of the Matrix library makes the saved chat login unusable (the
log says the Matrix worker failed to start or log in), run it with
`--relogin`, which logs the bot in afresh from the stored password; then
verify the bot once more in your Matrix app.

**On macOS** (and by hand on Linux): pull the new code, run
`bash scripts/build-release.sh`, then re-run `kastellan-cli install` from the
repository root **with the same flags as before** — including the two
`--matrix-…` flags, or the chat channel is switched off. Re-running install is
a clean upgrade: it stops the services, swaps in the new programs, and starts
them again.

---

## Uninstalling

```sh
kastellan-cli uninstall            # remove the services and the CLI link; keep programs, data and secrets
kastellan-cli uninstall --purge    # also delete the programs, database, secrets, settings and logs
```

`--purge` asks you to type `purge` to confirm, and it deletes
`kastellan.env.local` too — copy anything you want to keep first.

---

## Command reference

Every `kastellan-cli` subcommand, in one place. `kastellan-cli help` prints
a usage summary with most of the options (not yet every subcommand below).

| Command | What it does |
|---|---|
| `install …` / `uninstall [--purge]` | Install or upgrade the supervised agent; remove it (Step 5, "Uninstalling") |
| `ask "<task>" [--fast\|--long] [--classification-floor <class>]` | Give the agent a task and wait for the answer |
| `tasks list\|status\|tail\|cancel\|correct\|fail` | Watch and manage tasks |
| `inbox list\|show\|resolve` | Answer the agent's questions |
| `audit tail [--from-start] [--no-follow] [--state-dir <dir>]` | Read the audit log |
| `pair issue\|issue-token\|list\|revoke` | Manage who may talk to the agent |
| `matrix probe …` | One-time Matrix login, and a connection check |
| `secret put\|list\|delete` | Manage stored secrets |
| `tools allowlist add\|remove\|list` | What each tool may reach or run |
| `memory l1 add\|list\|remove\|reembed` | Standing notes (`reembed` fills in missing search vectors, e.g. after the embedding server was down) |
| `memory l3 list\|show\|approve\|pin\|revoke\|remove\|run` | Review and trust skills |
| `entities list\|show\|approve\|reject\|merge\|reembed` and `entities kinds …` | Review extracted people, places and things |
| `relations show <id>` and `relations kinds …` | Inspect relationships; manage their vocabulary |
| `guard calibrate` / `guard capture` | Tuning tools for the guard model (advanced) |
| `observation replay` | Re-run captured plans through the reviewer (advanced) |

---

## A sensible first-run checklist

1. ✅ Built the project with `bash scripts/build-release.sh` (and, optionally,
   ran the tests without unexplained `[SKIP]` lines).
2. ✅ PostgreSQL 18 with pgvector installed.
3. ✅ **Linux only:** ran the AppArmor sandbox script (Step 3) — don't skip.
4. ✅ Decided on your local AI model and checked it fits (Step 4).
5. ✅ Ran `kastellan-cli install` and saw both services come up healthy (Step 5).
6. ✅ Put your own settings in `kastellan.env.local`, never in `kastellan.env`.
7. ✅ Paired yourself over Matrix, or ran a task from the CLI (Steps 6–7).
8. ✅ Watched that task appear in the audit log (Step 8).
9. ✅ Only then enabled extra tools and allowlists, one at a time.

---

## Where to go next

- **Something not working?** The daemon log in `~/.local/state/kastellan/`,
  the per-component scripts under `scripts/` and the engineering notes under
  `docs/devel/` are the detailed fallback.
- **Every setting, precisely?**
  [Operator environment settings](../../deploy/operator-env.md).
- **Want the precise security guarantees and their current limits?**
  [`docs/threat-model.md`](../../threat-model.md) is the authoritative,
  technical version of Chapter 3.
- **Curious about the internals?** The
  [developer manual](../../devel/manual/index.md) lives under
  `docs/devel/manual/`.

That's the whole loop: build it, lock down the sandbox, install it in one
command, point it at a model, give it work, and watch the audit log.
Everything Kastellan does, it does within walls it cannot move — and you can
always read back exactly what happened.
