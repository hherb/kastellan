# Observation-phase fixture captures

This directory is the dataset infrastructure for the CASSANDRA
observation phase (spec §9). Each fixture is one "real-ish" prompt the
agent might receive; each capture is a frozen JSON snapshot of what the
live agent did with that prompt against a specific local LLM baseline.

## Layout

```
tests/observation/
├── README.md                              (this file)
├── fixtures/<id>/prompt.md                # H1 = summary; body = prompt
├── fixtures/<id>/meta.toml                # category, principle, notes
└── captures/<id>/<date>_<model_slug>.json # never overwritten
```

## Running a capture

The capture orchestrator is an `#[ignore]`-flagged integration test
(`core/tests/observation_capture.rs`). It needs a **real local LLM
running** before it starts — there is no skip-as-pass for this path;
the test fails loudly if the LLM is unreachable.

1. Start your local LLM. The orchestrator's default expectation is
   Ollama on macOS / vLLM on Linux at the standard local OpenAI-compat
   port. Override either with env vars before invoking:

   ```sh
   export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:11434/v1
   export KASTELLAN_LLM_LOCAL_MODEL='gemma4:26b-a4b-it-q8_0'
   ```

2. Build the workspace once so the daemon, CLI, and worker binaries
   exist at the expected paths:

   ```sh
   source "$HOME/.cargo/env"
   cargo build --workspace
   ```

3. Run the orchestrator:

   ```sh
   cargo test -p kastellan-core --test observation_capture -- --ignored --nocapture
   ```

4. Captures land under `tests/observation/captures/<id>/`. **The
   orchestrator refuses to overwrite an existing capture file** — if
   you want to recapture against an updated model, change the date or
   model env var first.

## Dry-run mode

Set `KASTELLAN_OBSERVATION_DRY_RUN=1` to walk the fixture tree, parse
each `prompt.md` + `meta.toml`, and print the work plan without
dialing the LLM or writing files. Useful for adding a new fixture and
verifying the meta parses.

## Adding a new fixture

```sh
mkdir tests/observation/fixtures/<new-id>
$EDITOR tests/observation/fixtures/<new-id>/prompt.md
$EDITOR tests/observation/fixtures/<new-id>/meta.toml
KASTELLAN_OBSERVATION_DRY_RUN=1 cargo test -p kastellan-core \
  --test observation_capture -- --ignored --nocapture
```

## Capture format

`capture.json` schema is documented in
[`docs/superpowers/specs/2026-05-13-observation-phase-captures-design.md`](../../docs/superpowers/specs/2026-05-13-observation-phase-captures-design.md).
The wire shape is pinned by Rust unit tests in
`core/src/observation/capture.rs::tests`.
