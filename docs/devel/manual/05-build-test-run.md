# 5 — Build, test, and run

---

## Before any command

Cargo is not on the default PATH in non-interactive shells. Always source the
Rust environment first, or add this line to your shell profile:

```sh
source "$HOME/.cargo/env"
```

---

## Building

```sh
# Build everything
cargo build --workspace

# Build only one crate (faster when iterating on a specific piece)
cargo build -p hhagent-sandbox
cargo build -p hhagent-core
```

Warnings are expected for a few pre-existing issues in `db/src/probe.rs` and
`hhagent-protocol`. Do not introduce new warnings.

---

## Running the test suite

```sh
# All tests in all crates
cargo test --workspace

# All tests, with stdout/stderr shown (useful for diagnosing [SKIP] messages)
cargo test --workspace -- --nocapture

# One crate
cargo test -p hhagent-sandbox

# One integration test file
cargo test -p hhagent-sandbox --test linux_smoke

# One test by name substring
cargo test -p hhagent-sandbox argv_starts_with_bwrap
```

### What "3 ignored" means

The test suite typically reports a small number of ignored tests. These are
tests that require hardware or services that may not be present:

- Apple `container` CLI (macOS Tahoe+ only)
- A real GLiNER/ReLeX ML model
- A real frontier LLM endpoint

Ignored tests are not failures. They skip gracefully and do not affect the
green/red status of a CI run.

### What `[SKIP]` lines mean

Unlike `ignored`, `[SKIP]` lines appear in test output when a sandbox test
detects that `bwrap` or `sandbox-exec` is not functional on the current host.
**A green run with `[SKIP]` lines is a false positive** — the tests passed
without actually testing containment.

To check whether real sandboxing is active:

```sh
cargo test -p hhagent-sandbox -- --nocapture 2>&1 | grep -E 'SKIP|ok'
```

If you see `[SKIP]` on Linux, re-run the AppArmor setup from
[chapter 2](./02-dev-env-linux.md#step-3).

---

## Running the daemon

The agent core binary is built at `target/debug/hhagent`:

```sh
./target/debug/hhagent
```

It starts, runs migrations (if needed), and then blocks waiting for tasks.
Exit with Ctrl-C.

The CLI for interacting with a running daemon:

```sh
./target/debug/hhagent-cli --help
./target/debug/hhagent-cli audit tail
./target/debug/hhagent-cli tasks list
./target/debug/hhagent-cli ask "Summarise the last 5 emails from Alice"
```

---

## Useful environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `HHAGENT_LLM_LOCAL_URL` | `http://127.0.0.1:8000/v1` (Linux) / `:11434/v1` (macOS) | Local LLM endpoint |
| `HHAGENT_LLM_LOCAL_MODEL` | `""` | Model name to pass to the local LLM |
| `HHAGENT_SHELL_EXEC_BIN` | (auto-detected) | Path to `hhagent-worker-shell-exec` binary |
| `HHAGENT_SHELL_EXEC_ALLOWLIST` | `""` | Colon-separated list of allowed shell commands |
| `HHAGENT_STATE_DIR` | `~/.local/state/hhagent` | Audit JSONL output directory |

---

## Python worker tests

The `gliner-relex` worker is Python. Its tests use `uv` (a fast Python package manager):

```sh
cd workers/gliner-relex
uv run pytest
```

These tests are independent of the Rust test suite. They run against the
Python source directly without spawning a Rust process.
