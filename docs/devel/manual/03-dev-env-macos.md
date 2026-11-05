# 3 — Dev environment: macOS

Tested on macOS 14 Sonoma and 15 Sequoia, Apple Silicon (M-series) and Intel.
macOS 26 (Tahoe) adds Apple's `container` CLI for the opt-in micro-VM
backend; that is optional and not required for basic development.

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
rustc --version    # 1.78+ required; the dev box + CI track current stable
```

---

## Step 4 — Install Postgres 18

```sh
brew install postgresql@18 pgvector
```

Homebrew installs Postgres but does not start it, and you do not need the
Homebrew service running: the test suite spins up its own throwaway cluster
per test from these binaries (found under
`/opt/homebrew/opt/postgresql@18/bin` or `/usr/local/opt/postgresql@18/bin`).
`pgvector` is required — the first migration runs `CREATE EXTENSION vector`.

**Postgres.app** works as a runtime database (pass its `bin/` to
`kastellan-cli install --pg-bin-dir`), but it is deliberately not
auto-detected by the tests, because its slower launchd bring-up overshoots the
test fixture's wait. If it is your only install, point the tests at it with
`export KASTELLAN_PG_BIN_DIR=/Applications/Postgres.app/Contents/Versions/18/bin`.

You only need your own per-user cluster when you want to run the daemon by
hand. Initialise it with:

```sh
cargo run -p kastellan-db --bin kastellan-db-init
```

This creates `~/.local/share/kastellan/pg/data` (Unix socket only, peer
auth) and is idempotent. It does not start the server — see
[chapter 5](./05-build-test-run.md#running-the-daemon).

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

Healthy output on macOS is `0 failed` across every crate, with a number of
`ignored` tests. The exact pass count grows commit by commit (see the latest
`HANDOVER.md`). Ignored tests need the Apple `container` CLI, a real GLiNER
model, a live mail archive or a Linux-only feature (Firecracker); none is
required for normal development. Read [chapter 5](./05-build-test-run.md)
before trusting a green run — a `[SKIP]` line means a test checked nothing.

---

## Optional: Apple `container` micro-VM backend

On macOS 26 (Tahoe) and later, Apple's `container` CLI runs each worker in
its own lightweight Linux VM (Virtualization.framework) — the macOS
counterpart of the Linux Firecracker backend. It is opt-in per worker and
today covers `python-exec` and `gliner-relex`.

```sh
brew install container
container system start --enable-kernel-install

scripts/workers/python-exec/build-image.sh     # kastellan/python-exec:dev
scripts/workers/gliner-relex/build-image.sh    # kastellan/gliner-relex:dev (~3 GB of wheels)
```

Then switch a worker over with `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1`
(together with `KASTELLAN_PYTHON_EXEC_ENABLE=1`) or
`KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`. The image tag can be overridden
with `KASTELLAN_PYTHON_EXEC_IMAGE` / `KASTELLAN_GLINER_RELEX_IMAGE`.

The python-exec image is cross-built from your host cargo cache, so build
the workspace once first. The container tests compare the image's build time
against its sources and refuse a **stale** image — any change to
`Cargo.lock` counts, so rebuild the image after a dependency bump. Run that
tier as evidence with `bash scripts/run-e2e-gate.sh container` (see
[chapter 5](./05-build-test-run.md#gated-e2e-tiers-skip-as-pass-and-the-gate-script)).

---

## Optional: Python workers

The two Python workers live outside the Cargo workspace. Each has an
idempotent setup script, run from the repo root:

```sh
scripts/workers/gliner-relex/install.sh     # needs `uv` and the Hugging Face CLI (`hf`)
scripts/workers/browser-driver/install.sh   # needs python3
```

`gliner-relex` is set up with [`uv`](https://docs.astral.sh/uv/) and
downloads its model weights (~1.3 GB). `browser-driver` deliberately uses the
system `python3 -m venv` instead of `uv` (a uv venv points at an interpreter
outside the venv, which the jail cannot read) and stages Chromium inside the
venv. Without them, the tests that drive the real workers `[SKIP]`.

---

## Optional: Local LLM

Most tests run against a scripted fake LLM and need no model. To run the
daemon (or the few live-LLM tests) you need an OpenAI-compatible endpoint. On
macOS the default is **oMLX** on `:8000` — it serves MLX-quantised models
through the same OpenAI-compatible surface and is materially faster than
Ollama on Apple silicon, the gap widening with model size.

oMLX is a separate application you install yourself (it is not fetched by
Kastellan's installer, and it manages its own model library): get it from
<https://github.com/madroidmaq/omlx>, start its server on `:8000`, then add
the two models below from its admin UI.

> **Memory:** `Qwen3.8-27B-8bit` is a 27-billion-parameter model at 8 bits —
> ~29 GB of weights, and the installer's rule wants **~33 GiB** of unified
> memory for it (weights + 20% headroom + a 2 GiB OS reserve). It does not fit
> a 16, 24, or 32 GB Mac. Pick a smaller MLX repo id there — `Qwen3.8-27B-4bit`
> needs ~18 GiB — and pass it as `KASTELLAN_LLM_LOCAL_MODEL`.

```sh
export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:8000/v1
export KASTELLAN_LLM_LOCAL_MODEL=Qwen3.8-27B-8bit
export KASTELLAN_LLM_EMBEDDING_MODEL=embeddinggemma-300m-bf16
```

Only the **URL** here is a router default — leave it unset and you get the
same value. The two model names are **not**: they are the *installer's*
defaults, which reach the daemon through the generated `kastellan.env`, and a
dev checkout running `cargo test` never loads that file. With them unset the
router falls back to the placeholders `local-default` and `embedding-default`,
which oMLX will reject. Export all three.

### The fallback runtime: llama.cpp

oMLX is the default, not the only option. **llama.cpp's `llama-server` is the
designated fallback on macOS** for anything oMLX cannot serve — it also speaks
OpenAI-compat, so it needs no code, only a URL. Point the router at it with an
explicit `KASTELLAN_LLM_LOCAL_URL` (llama.cpp has no conventional port, which
is why it is not a default anywhere).

The concrete gap driving this today is **token logprobs**. oMLX does not
return them: `logprobs`/`top_logprobs` are absent from `/v1/chat/completions`
entirely, and while `top_logprobs` *is* declared on `/v1/responses` it is
accepted and ignored — no response schema in oMLX's OpenAPI document emits
them. Measured against the live server on 2026-08-15; re-check when oMLX
updates, since a declared-but-inert parameter suggests the wiring is partly
there.

Anything needing a **calibrated score** rather than a bare token therefore
cannot use oMLX. The live example is the Phase 5 model-based guard tier
(Shieldstral), whose whole design rests on renormalising the `yes`/`no`
logprobs into a confidence band — see
[`docs/superpowers/specs/2026-08-13-shieldstral-guard-model-feasibility-study.md`](../../superpowers/specs/2026-08-13-shieldstral-guard-model-feasibility-study.md).
That is the fallback's first real customer — in a supervised install the
guard's `llama-server` runs as the `kastellan-guard` unit of
`kastellan.target`, started before the core, once you name the server binary
and model file (`KASTELLAN_GUARD_SERVER_BIN` / `KASTELLAN_GUARD_SERVER_MODEL`
alongside `KASTELLAN_LLM_GUARD_URL` / `KASTELLAN_LLM_GUARD_MODEL`). It has
also been **measured**:
llama.cpp returns 20 logprob alternatives with both `yes` and `no` present on
every call, and covers the multimodal half too. Reproduce with
`sh scripts/eval/run-shieldstral-llamacpp.sh` — the script's exit status is
the measurement's verdict, so a re-run on other weights or a reworded policy
prompt fails loudly rather than printing a table and exiting 0.

Ollama also still works, and remains the Linux install default:

```sh
brew install ollama
ollama serve &                       # runs in background
ollama pull gemma4:26b-a4b-it-q8_0   # the installer's Ollama default (or any chat model that fits)
ollama pull embeddinggemma
export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:11434/v1
export KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0
export KASTELLAN_LLM_EMBEDDING_MODEL=embeddinggemma
export KASTELLAN_LLM_THINKING_SWITCH=reasoning_effort
```

The last line matters: Ollama silently ignores the router's default way of
switching a model's "thinking" off, and only honours `reasoning_effort`.

Most unit and integration tests use a mock HTTP server (a scripted fake LLM)
and do not require a real model; only a handful of `#[ignore]`d end-to-end
tests make live LLM calls.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `sandbox-exec: Operation not permitted` | SIP or permissions issue | Check System Preferences → Privacy & Security |
| `[SKIP]` lines in Postgres tests | Postgres binaries not found | Re-run step 4, or set `KASTELLAN_PG_BIN_DIR` |
| `[SKIP]` naming a worker binary | Fresh checkout, nothing built yet | `cargo build --workspace` |
| Container tests refuse a stale image | Image older than its sources | Re-run the worker's `build-image.sh` |
| `container` service errors | Service not started | `container system start --enable-kernel-install` |
| `command not found: cargo` | Rust env not sourced | `source "$HOME/.cargo/env"` |
| Seatbelt `[SKIP]` in sandbox tests | Sandbox probe failed | Run `cargo test -p kastellan-sandbox -- --nocapture` to see why |
