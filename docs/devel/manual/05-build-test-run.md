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
# Build everything (core, every worker, every helper binary)
cargo build --workspace

# Build only one crate (faster when iterating on a specific piece)
cargo build -p kastellan-sandbox
cargo build -p kastellan-core
```

Debug builds land in `target/debug/`. The core finds worker binaries
*next to its own executable*, so tests that spawn real workers need the
whole workspace built — in a fresh checkout or worktree, run
`cargo build --workspace` before `cargo test`, or those tests `[SKIP]`.

### Release builds: one script, always

```sh
bash scripts/build-release.sh
```

Never build `target/release/` by hand for anything you will install, deploy
or bake into a micro-VM image. Two reasons:

- **Cargo unifies features per invocation.** `cargo build --release -p <pkg>`
  and `cargo build --release --workspace` can produce *different bytes from
  identical source*. The installer, the deploy path and the micro-VM rootfs
  images all read `target/release/`, and the image freshness checks compare
  against it — so a hand-run narrow build leaves a binary no image was ever
  built from.
- **The Matrix worker needs a feature the workspace build leaves off.**
  `kastellan-worker-matrix` only runs when built with `live-matrix`. The
  script ends with that one sanctioned `-p kastellan-worker-matrix --features
  live-matrix` step.

That second point means the script must run **last**. A later bare
`cargo build --release --workspace` silently writes the non-featured Matrix
worker back over it — in a fraction of a second, printing nothing but
`Finished` — leaving a Matrix worker that refuses to run. If you ran any
`cargo build --release` by hand, re-run the script. It prints the Matrix
binary's digest at the end so such a flip is visible. (Debug builds are
unaffected.)

### Clippy is enforced

CI runs

```sh
cargo clippy --workspace --all-targets -- -D warnings
```

and the tree is warning-clean; keep it that way. There is no `rustfmt`
config yet — match the formatting already in the file you are editing.

Cargo caches results, so an incremental clippy run may skip crates it
already checked. To force a genuinely **cold** run, use a dedicated target
directory:

```sh
CARGO_TARGET_DIR=$HOME/.cargo-clippy-<topic> cargo clippy --workspace --all-targets -- -D warnings
```

Then count the `Checking kastellan` lines in the output: there should be
**27**, one per workspace crate. Exit 0 alone does not prove every crate was
checked.

> ⚠️ **Never `touch` source files to force a rebuild.** Some e2e tiers decide
> whether a container image or rootfs is stale by comparing its build time
> against the **mtimes** of its sources. Touching, say,
> `workers/python-exec/src/main.rs` makes every container test report the
> image stale — and under a REQUIRE knob, fail — for a file nobody changed.
> A dedicated target dir changes no file, so it cannot falsify anything.

---

## Running the test suite

```sh
# All tests in all crates
cargo test --workspace

# All tests, with stderr shown (needed to see [SKIP]/[WARN]/[E2E] lines)
cargo test --workspace -- --nocapture

# One crate
cargo test -p kastellan-sandbox

# One integration test file
cargo test -p kastellan-sandbox --test linux_smoke

# One test by name substring
cargo test -p kastellan-sandbox argv_starts_with_bwrap
```

A full run spins up many throwaway Postgres clusters and real sandboxes, so
it takes a while. The authoritative test counts for each host live in
`HANDOVER.md` (over five thousand today). CI runs `check`, clippy and a
curated set of hermetic suites; the full workspace sweep, which needs bwrap,
Postgres and so on, is run on the development hosts.

### What "ignored" means

A number of tests are marked `#[ignore]` because they need hardware or
services a normal dev box does not have: KVM + Firecracker, the Apple
`container` CLI, a real GLiNER model, a live mail archive, a live LLM. They
only run when asked for with `-- --ignored`, usually through the gate script
below. They are not failures.

### What `[SKIP]` lines mean

`[SKIP]` is different, and more dangerous. A test that needs something —
working bwrap, Postgres binaries, a built worker binary — checks for it
first and, if it is missing, prints a `[SKIP]` line and **returns early as a
pass**. The test count is identical whether it asserted anything or not, so
**a green run with `[SKIP]` lines is a false positive**.

Remember that libtest swallows the stderr of passing tests: without
`-- --nocapture` you will not see the `[SKIP]` lines at all.

```sh
cargo test -p kastellan-sandbox -- --nocapture 2>&1 | grep -E 'SKIP|ok'
```

If you see `[SKIP]` on Linux, re-run the AppArmor setup (step 3 of
[chapter 2](./02-dev-env-linux.md)).

---

## Gated e2e tiers: skip-as-pass and the gate script

Because so many suites are skip-as-pass, the project has a rule for any run
that is meant to count as **evidence**. You need two things — and one without
the other is not a gate:

1. **A REQUIRE knob**, which turns that tier's skips into failures. Each tier
   has one environment variable:

   | Knob | Tier |
   |------|------|
   | `KASTELLAN_PG_REQUIRE_E2E` | Postgres-backed tests **and** the user-level supervisor probe (one variable, because they gate the same tier) |
   | `KASTELLAN_SANDBOX_REQUIRE_E2E` | real bwrap / Seatbelt sandboxes |
   | `KASTELLAN_GUARD_REQUIRE_E2E` | the guard-tier e2e's worker-binary check |
   | `KASTELLAN_MICROVM_REQUIRE_E2E` | Firecracker (Linux) and Apple `container` (macOS) micro-VMs |
   | `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` | the real GLiNER model |
   | `KASTELLAN_MAIL_LIVE_REQUIRE_E2E` | the live localmail shape gate |

   All accept the project's flag dialect (`1|true|yes|on`); a value outside
   it prints a warning rather than silently reverting to skip.

2. **A positive control**, which fails when zero tests ran. `cargo test`
   exits 0 when a name filter matches nothing, and a knob only fires from
   *inside* a test body — so a typo in a filter gives you a green run, the
   knob set, and not one `[SKIP]` line.

Both live behind one script:

```sh
bash scripts/run-e2e-gate.sh --list          # the profiles and the knobs each sets
bash scripts/run-e2e-gate.sh guard-tier      # run one profile as evidence
```

The profiles today are `guard-tier`, `pg`, `gliner`, `microvm` (Linux),
`worker-report`, `mail-live` (run it as `scripts/mail/live-shape-gate.sh`,
which finds the credentials first), `launchd` (macOS) and `container`
(macOS). Run the script with `bash`, not `zsh`. It sets the profile's knobs,
keeps the **whole** log under `~/.local/state/kastellan/gate-logs/`, and then
checks, among other things:

- tests passed ≥ the profile's floor, and every test binary that ran printed
  one whole `test result:` line with at least one test in it;
- a **per-tier** floor of `[E2E]` lines;
- zero `[WARN]` lines, and (per profile) a cap on `[SKIP]` lines;
- an **exact** count of libtest's `ignored` and `filtered out` — so do not
  append a test-name filter after the profile name; it fails the verdict;
- no marker stranded mid-line, where the counts would miss it.

### The three markers

Any run's output can be grepped for three evidence markers:

| Marker | Meaning |
|--------|---------|
| `[SKIP]` | a test did not run its checks |
| `[WARN]` | it ran, but something about it is not what you would assume |
| `[E2E] <tier>: <detail>` | a **demanded** precondition was actually met (printed only when that tier's knob is set) |

The `[E2E]` floors are counted **per tier**, and a floor of 1 is the right
number: deleting any one tier's announcement takes its count to 0 and trips
the floor, while a single run-wide total would be satisfied by whichever tier
happens to be chattiest. If you add markers of your own, build them with
`skip_line` / `warn_line` / `e2e_line` from `kastellan-tests-common` (they
start with a newline) and print each in **one** write.

### Two traps when setting a knob by hand

- `[E2E]` and `[SKIP]` go to **stderr**, which libtest hides for passing
  tests. Without `-- --nocapture` a perfectly healthy demanded run shows
  nothing.
- `KASTELLAN_GUARD_REQUIRE_E2E` covers only the guard tier's worker-binary
  check; its other preconditions answer to the Postgres and sandbox knobs.
  The `guard-tier` profile sets all three — prefer the profile.

There is **deliberately no umbrella "demand everything" variable**: the two
development hosts can legitimately run different tiers (a Mac has no KVM),
so one flag would turn honest skips into failures and end up exported as
`=0`. A new tier gets its own `RequireKnob` beside the tests it gates, an
entry in `tests_common::require::KNOBS`, and a place in some gate profile —
`gate_script_tests` checks that the script and `KNOBS` agree in both
directions.

---

## Running the daemon

For a real, supervised system, use the installer (it builds nothing itself —
run `bash scripts/build-release.sh` first):

```sh
./target/release/kastellan-cli install     # from the repo root
```

It installs `kastellan.target` (Postgres, the core and, if configured, the
guard backend) under `systemd --user` / launchd; the
[user manual](../../user/manual/index.md) covers it in full.

For a quick dev run of the debug build, start your per-user cluster yourself
(after `kastellan-db-init` — step 4 of chapter
[2](./02-dev-env-linux.md) or [3](./03-dev-env-macos.md)) and run the daemon
**from the repo root**, so it finds `prompts/` and `seeds/`:

```sh
/usr/lib/postgresql/18/bin/postgres -D ~/.local/share/kastellan/pg/data &   # Linux path; adapt on macOS
./target/debug/kastellan
```

On boot the daemon probes the sandbox backend, waits briefly for Postgres,
creates the `kastellan` database and runs pending migrations, loads the
prompts and L0 seeds, builds its tool registry, starts any configured
channels, and then waits for tasks. Its JSON log goes to stdout; a refusal to
boot goes to stderr. Stop it with Ctrl-C.

The CLI talks to the same database:

```sh
./target/debug/kastellan-cli --help
./target/debug/kastellan-cli ask "Summarise the last 5 emails from Alice"
./target/debug/kastellan-cli tasks list
./target/debug/kastellan-cli inbox list      # questions the agent is waiting on you for
./target/debug/kastellan-cli audit tail
```

---

## Useful environment variables

A dev shell reads none of the installer's env files, so set what you need
yourself. An installed daemon reads `~/.config/kastellan/kastellan.env`
(regenerated by every install) plus your own overlay
`~/.config/kastellan/kastellan.env.local` — see
[`docs/deploy/operator-env.md`](../../deploy/operator-env.md).

| Variable | Default | Purpose |
|----------|---------|---------|
| `KASTELLAN_LLM_LOCAL_URL` | `http://127.0.0.1:8000/v1` | Local LLM endpoint (Ollama users: `http://127.0.0.1:11434/v1`) |
| `KASTELLAN_LLM_LOCAL_MODEL` | `local-default` (a placeholder — set it) | Model name for the local LLM |
| `KASTELLAN_LLM_EMBEDDING_MODEL` | `embedding-default` (a placeholder — set it) | Embedding model name |
| `KASTELLAN_LLM_THINKING_SWITCH` | `chat_template_kwargs` | Set `reasoning_effort` on Ollama |
| `KASTELLAN_LLM_TIMEOUT_MS` | `180000` | LLM request timeout |
| `KASTELLAN_LLM_GUARD_URL` / `_MODEL` | (unset ⇒ guard tier off) | The optional guard-model endpoint |
| `KASTELLAN_PLANNER_MODE` | `notebook` | Only `notebook` (or unset); `classic`, retired in #993, stops the daemon at boot (scripted notebook replies for the daemon e2es are `tests_common::scripted_llm::notebook_*_json`) |
| `KASTELLAN_PG_BIN_DIR` | (auto-detected) | Postgres `bin/` the **tests** use |
| `KASTELLAN_STATE_DIR` | `~/.local/state/kastellan` | Audit JSONL mirror directory |
| `KASTELLAN_PROMPTS_DIR` / `KASTELLAN_L0_RULES_FILE` | `prompts` / `seeds/memory/l0_meta_rules.toml` (relative to the working dir) | Where the daemon loads prompts and L0 seeds |
| `KASTELLAN_EGRESS_FORCE_ROUTING` | `1` (on) | Route networked workers through their per-worker egress proxy only. Fail-closed. |
| `KASTELLAN_WEB_SEARCH_ENDPOINT` | (unset) | SearxNG `/search` endpoint for the search workers |
| `KASTELLAN_{WEB_FETCH,WEB_SEARCH,WEB_RESEARCH,BROWSER_DRIVER}_ALLOWLIST` | (per worker) | Per-worker `host:port` egress allowlist |
| `KASTELLAN_MAIL_ENDPOINT` / `KASTELLAN_MAIL_TOKEN_FILE` | (unset) | localmail `/v1` base URL and bearer-token file for the mail worker |
| `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` | (unset) | Extra CA for the egress proxy's upstream leg, for exactly one private origin |
| `KASTELLAN_MATRIX_HOMESERVER_URL` / `KASTELLAN_MATRIX_USER` | (unset ⇒ no Matrix channel) | Enables the Matrix channel |
| `KASTELLAN_EMAIL_ENDPOINT` | (unset ⇒ no email channel) | Enables the inbound email channel; then `KASTELLAN_EMAIL_{SUBSCRIPTION,ADDRESS,AUTHSERV_ID,TOKEN_FILE}` are required |

The shell-exec worker's argv allowlist is **not** an environment variable you
set: the operator manages it in the database with
`kastellan-cli tools allowlist add|remove|list`, and the core hands it to the
worker at spawn.

### Opt-in worker enable flags

Some workers are registered only behind an explicit flag (off by default):

| Flag | Worker |
|------|--------|
| `KASTELLAN_PYTHON_EXEC_ENABLE=1` | `python-exec` |
| `KASTELLAN_BROWSER_DRIVER_ENABLE=1` | `browser-driver` |
| `KASTELLAN_GLINER_RELEX_ENABLE=1` | `gliner-relex` (entity extraction for the graph lane; also unlocks its real-model e2e tests) |

### Micro-VM backend flags (opt-in)

On **Linux**, `KASTELLAN_<WORKER>_USE_MICROVM=1` moves a worker from bwrap
into a Firecracker micro-VM. It exists for `PYTHON_EXEC`, `WEB_FETCH`,
`WEB_SEARCH`, `WEB_RESEARCH`, `BROWSER_DRIVER` and `MATRIX`; it needs the
setup in [chapter 2](./02-dev-env-linux.md) ("Optional — Firecracker micro-VM backend")
and a rootfs image built from `scripts/build-release.sh` output. Confinement
of the `firecracker` VMM process itself is on by default;
`KASTELLAN_MICROVM_CONFINE_VMM=0` opts out (for debugging only).

On **macOS**, `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1` and
`KASTELLAN_GLINER_RELEX_USE_CONTAINER=1` move those workers into an Apple
`container` micro-VM — see
[chapter 3](./03-dev-env-macos.md#optional-apple-container-micro-vm-backend).

---

## Python worker tests

The Python workers (`gliner-relex`, `browser-driver`) live outside the Cargo
workspace and have their own `pytest` suites:

```sh
cd workers/gliner-relex && uv run --frozen pytest -q
```

`--frozen` keeps uv on the committed `uv.lock` (CI checks the lock and
`pyproject.toml` agree). One test loads the real model; it skips unless the
weights are staged, and `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` turns that
skip into a failure. `browser-driver`'s tests live in
`workers/browser-driver/tests/` and need the `dev` extra (`pytest`,
`pytest-mock`) in whatever environment you run them from.

These suites test the Python source directly without a Rust process.
End-to-end tests that drive the workers from `core` under the real sandbox
live in `core/tests/` (for example `gliner_relex_e2e.rs`,
`browser_driver_e2e.rs`) and are gated behind `#[ignore]`, an enable flag,
or a REQUIRE knob, so they only run when the model or browser is installed —
`bash scripts/run-e2e-gate.sh gliner` runs the GLiNER tier as evidence.
