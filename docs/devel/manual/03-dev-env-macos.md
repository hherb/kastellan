# 3 — Dev environment: macOS

Tested on macOS 14 Sonoma and 15 Sequoia, Apple Silicon (M-series) and Intel.
macOS 26 (Tahoe) adds Apple's `container` CLI for micro-VM workers; that is
optional and not required for basic development.

---

## Step 1 — Install Xcode Command Line Tools

```sh
xcode-select --install
```

This installs `clang`, `make`, and the macOS SDK headers that Rust's C
dependencies need.

---

## Step 2 — Install Homebrew (if not already installed)

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

Follow the on-screen instructions to add Homebrew to your PATH.

---

## Step 3 — Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version    # should print 1.77 or later
```

---

## Step 4 — Install Postgres

```sh
brew install postgresql@18
```

Homebrew installs Postgres but does not start it automatically. The project
manages its own per-user Postgres instance, so you do not need the Homebrew
service running. Initialise the per-user cluster:

```sh
cargo run -p kastellan-db --bin kastellan-db-init
```

This creates a cluster in `~/.local/share/kastellan/postgres/` configured for
Unix socket connections with peer auth.

---

## Step 5 — Verify sandbox-exec availability

kastellan uses `sandbox-exec` (macOS Seatbelt) to isolate worker processes.
It ships with macOS and needs no extra install. Confirm it is present:

```sh
which sandbox-exec    # should print /usr/bin/sandbox-exec
```

No AppArmor profile step is needed on macOS.

---

## Step 6 — First build

```sh
source "$HOME/.cargo/env"
cargo build --workspace
```

First build takes 2–5 minutes. Subsequent incremental builds are fast.

---

## Step 7 — Run the test suite

```sh
cargo test --workspace -- --nocapture
```

Healthy output on macOS is `0 failed` across every crate, with a small
number of `ignored` tests. The exact pass count grows commit by commit
(see the latest `HANDOVER.md`). Ignored tests need the Apple `container`
CLI (macOS Tahoe+) or a real GLiNER model; neither is required for
normal development.

---

## Optional: Local LLM for integration tests

The scheduler integration tests that call `formulate_plan` need an LLM. On
macOS the default is Ollama:

```sh
brew install ollama
ollama serve &          # runs in background
ollama pull gemma2:9b   # or any OpenAI-chat-compatible model
```

Set the environment variable that points the LLM router to Ollama:

```sh
export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:11434/v1
export KASTELLAN_LLM_LOCAL_MODEL=gemma2:9b
```

Most unit and integration tests use a mock HTTP server and do not require a
real LLM. Only the end-to-end `observation` tests and the `cli_ask_e2e` test
do live LLM calls.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `sandbox-exec: Operation not permitted` | SIP or permissions issue | Check System Preferences → Privacy & Security |
| `connection refused` in Postgres tests | DB not running | `cargo run -p kastellan-db --bin kastellan-db-init` |
| `command not found: cargo` | Rust env not sourced | `source "$HOME/.cargo/env"` |
| Seatbelt `[SKIP]` in sandbox tests | Sandbox probe failed | Run `cargo test -p kastellan-sandbox -- --nocapture` to see why |
