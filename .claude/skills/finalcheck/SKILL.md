---
name: finalcheck
description: final pre-merge pass on the current PR — review, fix everything, then drive GitHub CI to green
disable-model-invocation: false
allowed-tools: Bash(git *), Bash(gh *), Bash(cargo *), Bash(uv *), Bash(bash *), Bash(source *)
---
Run these steps in order on the PR for the current branch. Sessions here span several `git worktree`s, so
first run `pwd && git branch --show-current && git worktree list` and `cd` into the PR's worktree (they live
under `.claude/worktrees/`; a bare `cd /Users/hherb/src/kastellan` lands in the PRIMARY checkout, not yours).
Then find the PR with `gh pr view --json number,url,headRefOid`. If there is none, stop and tell me.
Cargo is not on `PATH` in non-interactive shells: prefix cargo commands with `source "$HOME/.cargo/env" &&`.

1. **Review.** Invoke the `pr-review-toolkit:review-pr` skill via the Skill tool and let it finish.
2. **Fix.** Invoke the `fixall` skill via the Skill tool on that review's findings. Merge `origin/main` first;
   it ends by committing and pushing into the PR. After the merge, grep `docs/devel/handovers/HANDOVER.md`
   for duplicated headers or two "This session" sections — parallel sessions' edits merge clean and wrong.
   Before moving on, confirm the push landed: `git fetch origin && git status -sb` must show no `ahead`, and
   the PR's `headRefOid` must equal `git rev-parse HEAD`.
3. **Drive CI to green.** Loop:
   - Watch the checks for the pushed head commit with `gh pr checks --watch --fail-fast`. Checks can take a
     minute to register, so wait if it reports none yet.
   - All green → done. Report the PR URL and the final check list. "All green" means every required
     context in the ruleset passed: `gh api repos/hherb/kastellan/rules/branches/main` (today:
     `cargo check + clippy (linux)`, `uv lock --check (gliner-relex)`,
     `cargo check + clippy (matrix worker, live-matrix)`). `branches/main/protection` returns 404 by design.
     CodeQL (`Analyze (rust|python|actions)`) is not required, but a new alert on this PR is a finding:
     read it, don't wave it through (CodeQL flags sanitisers by *name* — see memory).
   - A check failed → read the cause: `gh run list --branch "$(git branch --show-current)" --limit 5`, then
     `gh run view <run-id> --log-failed`. Reproduce it locally with the exact step the failing job runs
     (see `.github/workflows/linux-check.yml`; every cargo step there passes `--locked`):
     - `cargo check + clippy (linux)` → `cargo check --workspace --all-targets --locked`,
       `cargo clippy --workspace --all-targets --locked -- -D warnings`, then the job's hermetic
       `cargo test -p … --locked` steps (copy the failing one verbatim from the workflow)
     - `cargo check + clippy (matrix worker, live-matrix)` → the same three with
       `-p kastellan-worker-matrix --all-targets --features live-matrix --locked`
     - `uv lock --check (gliner-relex)` → in `workers/gliner-relex`: `uv lock --check --offline`, then the
       job's two `uv run --no-project --with pytest pytest …` lines
     - a CodeQL alert → `gh api repos/hherb/kastellan/code-scanning/alerts?ref=refs/pull/<n>/merge`
     - a workflow file changed → `actionlint`

     For a genuinely cold clippy run use a dedicated `CARGO_TARGET_DIR=$HOME/.cargo-clippy-<topic>`; never
     `touch` sources (it falsifies the #687 image-freshness gate). Judge each command by its **exit code**,
     never by filtered output: `cmd | tail` reports `tail`'s status. Fix the root cause, using TDD where it
     is a code bug. Confirm the fix passes locally, commit with a message naming the CI failure, push, and
     loop again.
   - **Linux-only failures** (CI is ubuntu-latest and the Mac passes locally): reproduce on the DGX over
     SSH (native Linux, see memory `dgx-native-linux-verification-over-ssh`), or cross-clippy pure-Rust
     crates for Linux on the Mac. Don't guess.
   - **A job that is CANCELLED with no log** means the runner was killed, not that a test was slow. Treat
     it as a code bug: find what killed the runner. Do not rerun it as a flake.
   - If the failure is not caused by the code (runner/infra outage, network flake, missing secret), rerun
     once with `gh run rerun <run-id> --failed`. If it fails the same way again, stop and report rather
     than changing code to work around it. The Actions API returns empty or stale data during an outage,
     so re-query before concluding a run never happened.
   - Never "fix" CI by skipping, `#[ignore]`-ing or deleting tests, by weakening lint/CI config, guards,
     REQUIRE knobs or `run-e2e-gate.sh` floors, or by adding an allowlist entry, without my explicit
     permission. Allowlist entries here are security decisions. If the same failure survives three fix
     attempts, stop and report what you tried.
4. **Not covered by CI.** CI runs only hermetic subsets, so green CI is not a full test pass. Locally, and
   include the results in the report:
   - `cargo test --workspace --no-fail-fast` with `KASTELLAN_PG_BIN_DIR` set on the Mac (see memory
     `postgres-app-bin-paths`), or ~350 PG tests skip-as-pass. Count failures across *all* binaries; never
     truncate the log.
   - If the PR touches a gated tier (sandbox, guard, micro-VM, gliner, mail), run its profile:
     `bash scripts/run-e2e-gate.sh --list`, then `bash scripts/run-e2e-gate.sh <profile>`.
   - If it touches `docs/devel/manual/` or `scripts/site/` (the docs workflow runs only on `main`):
     `scripts/site/check-manual.sh`.
