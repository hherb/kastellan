# 10 — Your first contribution

A step-by-step guide for going from idea to merged PR.

---

## Step 1 — Read the handover first

Before touching any code, read `docs/devel/handovers/HANDOVER.md`. It tells
you what is currently in progress, what is green, what is deliberately
stubbed, and what the next planned item is. Skim `docs/devel/ROADMAP.md` for
the long-range view. Working on something that conflicts with in-progress work
is avoidable.

The handover names PRs and issues, not branch names or commit hashes, because
a merge would make those stale. Two commands give you the live state:

```sh
git log --oneline -1 origin/main     # the current tip of main
gh pr list --state open              # what is in flight
gh issue list --state open           # the open issues — the live answer
```

---

## Step 2 — Find or open an issue

Every non-trivial change should have a GitHub issue. Look for an existing one
before opening a new one; issues are labelled by `area:*`, which makes them
easier to search. If your change is small (a typo fix, a one-line
correction), a PR without an issue is fine.

---

## Step 3 — Create a branch

Branch from `main`. Use a descriptive name that follows the existing pattern:

```sh
git checkout main
git pull
git checkout -b feat/my-feature-name
# or
git checkout -b fix/<issue-number>-short-description
# or
git checkout -b refactor/what-is-being-refactored
```

---

## Step 4 — Write tests first

This codebase follows TDD ordering: write the test, confirm it fails, then
write the code that makes it pass. Reviewers will notice if tests were added
as an afterthought.

For pure functions, add unit tests in the module's `#[cfg(test)] mod tests`
(inline, or in its sibling `tests.rs` — see [chapter 9](./09-rust-patterns.md)).

For database or sandbox behaviour, add an integration test in the crate's
`tests/` directory using the helpers from `tests-common` (a per-test Postgres
cluster, the `[SKIP]`/`[E2E]` line builders, the REQUIRE knobs). A test that
can skip must say so with a `[SKIP]` line and should be covered by a REQUIRE
knob, so a skip can be turned into a failure.

---

## Step 5 — Write the code, in small slices

Keep changes focused. A PR that does three things at once is three times
harder to review and three times more likely to be asked to split. The
project's history is a long run of small slices, each shippable on its own:
prefer a slice that lands one piece behind a default-off setting over one PR
that lands everything.

Check the hard constraints from [chapter 8](./08-hard-constraints.md) before
committing. If your change adds a dependency, verify its license. If your
change touches a platform-specific path, implement the counterpart on the
other OS. If a file you need to grow is already near the 500-line cap, split
it first in a separate movement-only commit.

---

## Step 6 — Check your work

**Tests.**

```sh
cargo test --workspace -- --nocapture
```

All tests should pass. `--nocapture` matters: `[SKIP]` lines go to stderr,
which is hidden for passing tests without it. On Linux, confirm the sandbox
tests actually ran rather than skipping (see
[chapter 5](./05-build-test-run.md#what-skip-lines-mean)).

**Clippy, with warnings as errors.** CI runs this and the tree is
warning-clean; keep it so:

```sh
cargo clippy --workspace --all-targets -- -D warnings
```

To force a genuinely cold run, use a dedicated target directory — **never
`touch` source files**, which makes the micro-VM image freshness gates report
a fresh image as stale:

```sh
CARGO_TARGET_DIR=$HOME/.cargo-clippy-<topic> cargo clippy --workspace --all-targets -- -D warnings
```

Exit 0 alone does not prove a full pass; count the `Checking kastellan` lines
(one per workspace crate, 27 today).

**Gated tiers, as evidence.** Many suites skip-and-pass when their fixture is
missing (Postgres, bwrap, KVM, a model). If your change touches one of those
tiers, run its profile of the gate script, which turns skips into failures and
checks that tests really ran:

```sh
bash scripts/run-e2e-gate.sh --list         # the profiles and the knobs each sets
bash scripts/run-e2e-gate.sh guard-tier     # e.g. Postgres + sandbox + guard tier
```

It keeps the whole log under `~/.local/state/kastellan/gate-logs/` — cite it
in your PR.

**Both platforms.** A Mac never compiles Linux-only code and vice versa. If
you can only test on one, say so in the PR.

**Release builds** go through `bash scripts/build-release.sh` only, and only
matter if your change touches what the micro-VM images or the deploy read from
`target/release/`. Do not hand-run `cargo build --release -p …`.

---

## Step 7 — Write a clear commit message

Each commit should explain *why* the change was made, not just *what* changed.
The "what" is visible in the diff. The subject line follows the tree's
`type(scope): summary` style, with the issue in brackets.

Good:
```
fix(sandbox): refuse a policy path that contains `..` (#NNN)

`is_absolute()` alone let `/scratch/../../etc/ssl` through, and bwrap
binds what a path resolves to, not what it names. Reject `..`
components up front so a future caller passing an untrusted path
cannot reach outside the intended share.
```

Not useful:
```
update sandbox policy
```

**Watch for closing keywords.** GitHub closes an issue when a merged PR body,
a commit message — or the squash commit's message — contains a keyword
(`close`, `closes`, `closed`, `fix`, `fixes`, `fixed`, `resolve`, `resolves`,
`resolved`) followed by an issue number. That includes a *forward reference*
such as "a later slice fixes #NNN", which closes that issue now. Before merging,
grep the PR body and every commit message with the regex the handover uses:

```sh
grep -En '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'
```

Write "part of #NNN" or "see #NNN" for an issue the PR does not finish.

---

## Step 8 — Open the pull request

There is no PR template; the description should include:

1. **What this PR does** — one paragraph.
2. **Why it is needed** — link to the issue.
3. **How to test it** — which test command covers the new behaviour, and
   which gate profiles you ran (with the log path).
4. **Platform coverage** — does it work on both Linux and macOS? Which host
   did you test on?
5. **Test count delta** — e.g. "workspace 998 → 1002 (+4)".

### What CI runs

`main` is protected: a PR cannot merge until the three jobs of the
`linux-check` workflow (`.github/workflows/linux-check.yml`) pass on an
x86_64 Ubuntu runner:

| Job | What it checks |
|-----|----------------|
| `cargo check + clippy (linux)` | `cargo check` and `cargo clippy -D warnings` over the whole workspace, all targets, `--locked`; plus the **hermetic** test suites (no Postgres, bwrap, KVM or network) whose guards must fire on a PR — `tests-common`, the mail worker, the worker prelude, shell-exec, python-exec, the install-plan and router-config defaults, the guard tier and the `kastellan-cli` binary's own tests |
| `uv lock --check (gliner-relex)` | the Python worker's `pyproject.toml` and `uv.lock` agree, plus its pure (no-torch) guard tests |
| `cargo check + clippy (matrix worker, live-matrix)` | the Matrix worker built, linted and tested with its `live-matrix` feature, which the default workspace build leaves out |

CI does **not** run the full suite: Postgres-, bwrap- and KVM-backed tests run
on the operators' hosts. That is why step 3 of your PR description matters.

Two other workflows exist: `docs.yml` builds and publishes this manual
(`scripts/site/check-manual.sh` verifies it — run it if you edit the manual)
on pushes to `main`, and `release.yml` publishes the workspace to crates.io
when a `v*` tag is pushed.

---

## What reviewers look for

- **Hard constraints** (chapter 8) — no exceptions.
- **Test coverage** — new behaviour should have a failing test that the code
  makes pass.
- **No false greens** — a test that can skip must announce it, and a claim
  that a gated tier passed needs the gate log, not just exit code 0.
- **File-size cap** — files over 500 LOC should be flagged and ideally split.
- **Audit log** — any tool call, secret access, or state transition that
  matters to the security story should write an audit row.
- **Cross-platform** — platform-specific paths need counterparts.
- **Comment quality** — comments explain *why*, not *what*. No multi-paragraph
  docstrings for obvious code.

---

## Keep the handover current

The handover convention is in `docs/devel/handovers/README.md`: read
`HANDOVER.md` at the start of a working session, and update it at the end.
When your change ships, record it in `HANDOVER.md` (what shipped, the test
count delta, open follow-up issues, the gate that actually ran and on which
host) and tick or add the matching item in `docs/devel/ROADMAP.md`. Commit the
two files **together**, with a `docs(handover): …` message, as part of your
PR or in a follow-up PR. Update the load-bearing fields (date, test counts)
before the prose, so they are right even if you run out of time.

The next contributor will read the handover to understand the current state,
so a stale number there misleads them silently.

---

## A note on patience

This project is security-first. Reviewers are conservative about changes that
touch the sandbox layer, the dispatcher chokepoint, the audit log, or the
CASSANDRA pipeline. Expect careful review of those areas. The conservative
posture is intentional, not personal.
