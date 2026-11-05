# 2 — Dev environment: Linux (Ubuntu 24.04+)

This guide sets up a working dev environment on Ubuntu 24.04 or later. Other
Debian-based distributions work; Fedora/Arch users will need to adapt the
`apt` commands. Steps 1–6 are all you need for everyday work; the sections
after them are optional and only matter when you touch the subsystem they
name.

---

## Step 1 — Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version    # 1.78+ required; the dev box + CI track current stable
```

Cargo (the build tool) is installed alongside Rust. The `source` line is
needed in every new shell session, or you can add it to your `~/.bashrc` /
`~/.zshrc`. Non-interactive shells (scripts, `ssh host cmd`) do not read
those files, which is why the project's own scripts source it themselves.

---

## Step 2 — Install system dependencies

```sh
sudo apt update
sudo apt install -y \
    build-essential pkg-config git \
    bubblewrap \
    libssl-dev
```

`bubblewrap` (`bwrap`) is the Linux tool that puts each worker process in its
own jail. It is required to run the sandbox tests. Resource caps (CPU,
memory) come from a `systemd-run --user --scope` cgroup, so you need a normal
systemd user session — a desktop login or an `ssh` login both provide one.

---

## Step 3 — Fix the AppArmor restriction (Ubuntu 24.04+)

Ubuntu 24.04 restricts unprivileged user namespaces by default
(`kernel.apparmor_restrict_unprivileged_userns=1`). Without a workaround,
bwrap cannot create its own jail and **every sandbox test silently skips** —
a green run that tested no containment at all. Install the AppArmor profile
once:

```sh
sudo scripts/linux/install-bwrap-apparmor-profile.sh
```

This is the same per-binary profile pattern Flatpak uses
(`/etc/apparmor.d/flatpak`). You only need to do this once per machine. Other
distributions without the AppArmor user-namespace restriction do not need it.

To confirm it worked:

```sh
cargo test -p kastellan-sandbox -- --nocapture 2>&1 | grep -E 'SKIP|ok|FAILED'
```

You should see `ok` lines, not `[SKIP]` lines. If you still see `[SKIP]`, the
profile did not install correctly. For a run that *fails* instead of skipping
when bwrap is unusable, use the gate script described in
[chapter 5](./05-build-test-run.md#gated-e2e-tiers-skip-as-pass-and-the-gate-script).

---

## Step 4 — Install Postgres 18

The project ships a helper script:

```sh
sudo scripts/linux/install-postgres.sh
```

This installs PostgreSQL 18 and `pgvector` from the official PGDG apt
repository (Ubuntu's own repo ships an older major). It installs binaries
only: it does **not** run `initdb` and does not touch the system-wide
`postgresql` service apt creates. That service can keep running on port 5432;
kastellan's own cluster listens on a Unix socket only, so the two never
collide.

You do **not** need a running database to run the test suite — every test
that touches Postgres spins up its own throwaway cluster from these binaries
(found under `/usr/lib/postgresql/<version>/bin`; set `KASTELLAN_PG_BIN_DIR`
to point the tests at a different install).

You only need your own per-user cluster when you want to run the daemon by
hand. Initialise it with:

```sh
cargo run -p kastellan-db --bin kastellan-db-init
```

This creates `~/.local/share/kastellan/pg/data` (sockets in `<data>/sockets`,
`listen_addresses=''`, peer auth) and is idempotent. It does not start the
server — see [chapter 5](./05-build-test-run.md#running-the-daemon) for that,
or let `kastellan-cli install` supervise it for you.

---

## Step 5 — First build

```sh
source "$HOME/.cargo/env"
cargo build --workspace
```

The first build downloads and compiles all dependencies and takes a few
minutes. Subsequent incremental builds are fast. The build also produces
every worker binary in `target/debug/`, which is where the core and the tests
look for them — so build the workspace before running tests in a fresh
checkout or worktree.

---

## Step 6 — Run the test suite

```sh
cargo test --workspace -- --nocapture
```

Many integration tests spin up a real Postgres cluster per test, so a full
run takes a while. A healthy result is `0 failed` across every crate; the
total grows commit by commit (over five thousand tests today — the latest
`HANDOVER.md` records the current baseline). A number of tests are marked
`ignored`: they need hardware or services not present in a standard setup
(KVM + Firecracker, a real GLiNER model, a live mail archive). Read
[chapter 5](./05-build-test-run.md) before trusting a green run: `[SKIP]`
lines mean a test returned early without checking anything.

---

## Optional — Local LLM

Most tests use a mock HTTP server and need no model. To run the daemon or
the few live-LLM tests you need an OpenAI-compatible endpoint. On Linux the
installer's default is **Ollama** on `:11434`; vLLM or SGLang on `:8000` (the
router's built-in default URL) work as well, as does llama.cpp's
`llama-server`.

```sh
export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:11434/v1
export KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0     # the installer's Linux default
export KASTELLAN_LLM_EMBEDDING_MODEL=embeddinggemma
export KASTELLAN_LLM_THINKING_SWITCH=reasoning_effort      # Ollama ignores the default dialect
```

The model names are the *installer's* defaults (they reach an installed
daemon through the generated `kastellan.env`); a dev shell never loads that
file, and with them unset the router sends the placeholders `local-default`
and `embedding-default`, which a real server rejects. `kastellan-cli install`
pulls the default Ollama models for you if they are missing. See
[chapter 13](./13-llm-router.md) for the full set of router variables.

---

## Optional — Python workers

The two Python workers live outside the Cargo workspace and need their own
environments. Each has an idempotent setup script, run from the repo root:

```sh
scripts/workers/gliner-relex/install.sh     # needs `uv` and the Hugging Face CLI (`hf`)
scripts/workers/browser-driver/install.sh   # needs python3 (with venv)
```

`gliner-relex` is set up with [`uv`](https://docs.astral.sh/uv/) (`uv sync`)
and downloads the model weights (~1.3 GB) into
`~/.local/share/kastellan/workers/gliner-relex/weights`. `browser-driver`
deliberately uses the **system** `python3 -m venv` rather than `uv` (a uv venv
points at an interpreter outside the venv, which the jail cannot read),
installs from a hash-pinned `requirements.lock`, and stages Chromium inside
the venv. Without these, the tests that drive the real workers `[SKIP]`.

---

## Optional — Firecracker micro-VM backend

Most workers run under bwrap and need nothing more than the steps above. The
opt-in Firecracker backend (switched on per worker with
`KASTELLAN_<WORKER>_USE_MICROVM=1`, e.g. `KASTELLAN_PYTHON_EXEC_USE_MICROVM=1`)
needs KVM, the `firecracker` binary and a one-time privileged setup:

```sh
scripts/workers/microvm/install-firecracker.sh      # as your user, NOT sudo: pinned release into ~/.local/bin
sudo scripts/linux/install-firecracker-vsock.sh     # once per machine
```

The privileged script does three things: it grants the worker user access to
the vsock device (`/dev/vhost-vsock`, re-applied by a udev rule on every
boot), provisions `/var/lib/kastellan/microvm` as `root:<worker-group>` mode
`1775`, and installs the sha256-pinned guest kernel there as `root:root`
`0644`. Without it, `LinuxFirecracker::probe()` fails closed and the worker
stays on bwrap. `/dev/kvm` is usually already accessible; pass `--kvm` if not.

It is also a **hard prerequisite for every rootfs build**
(`scripts/workers/microvm/build-*-rootfs.sh`, or all of them at once with
`scripts/workers/microvm/rebuild-all-rootfs.sh`). The builds only *verify* the
guest kernel and never create one: an unprivileged build able to create it
could create an agent-owned kernel in a group-writable directory, silently
voiding the ownership guarantee. Re-run the installer after a pinned-kernel
bump — it is idempotent and repairs older installs. (For the non-default
per-user layout `KASTELLAN_MICROVM_DIR=~/.local/share/kastellan/microvm`,
which root does not manage and which therefore has no ownership protection,
fetch the kernel deliberately with
`scripts/workers/microvm/fetch-guest-kernel.sh <dir>`.)

Each rootfs image bakes in its own copy of the worker binary, built from
`target/release/` by `scripts/build-release.sh`; the micro-VM tests refuse to
boot an image older than its sources. The full walkthrough is the
[Linux micro-VM setup runbook](../runbooks/2026-06-26-linux-microvm-setup.md).

---

## Optional — Other local services

Only needed when you work on the matching channel or worker:

| Service | Setup |
|---------|-------|
| SearxNG (for `web-search` / `web-research`) | `scripts/web-search/setup-searxng.sh` (runs the official container on loopback) |
| A local Matrix homeserver | `scripts/matrix/setup-conduwuit.sh` (loopback conduwuit), or `scripts/matrix/dev-e2e-bootstrap.sh` for the throwaway homeserver the live Matrix e2e uses; production setup is [`docs/deploy/matrix-homeserver.md`](../../deploy/matrix-homeserver.md) |
| The guard model (Shieldstral on llama.cpp) | `sh scripts/eval/run-shieldstral-llamacpp.sh` measures it; see [chapter 3](./03-dev-env-macos.md#the-fallback-runtime-llamacpp) |

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `bwrap: setting up uid map` / `creating new namespace` errors | AppArmor restriction | Re-run step 3 |
| `[SKIP]` lines in sandbox tests | Same as above | Re-run step 3 |
| `[SKIP]` lines in Postgres tests | Postgres binaries not found | Re-run step 4, or set `KASTELLAN_PG_BIN_DIR` |
| `[SKIP]` naming a worker binary | Fresh checkout, nothing built yet | `cargo build --workspace` |
| `command not found: cargo` | Rust env not sourced | `source "$HOME/.cargo/env"` |
| Micro-VM worker silently runs under bwrap | `probe()` failed closed | Run `sudo scripts/linux/install-firecracker-vsock.sh` |
