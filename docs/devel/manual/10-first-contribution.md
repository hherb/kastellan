# 10 — Your first contribution

A step-by-step guide for going from idea to merged PR.

---

## Step 1 — Read the handover first

Before touching any code, read `docs/devel/handovers/HANDOVER.md`. It tells
you what is currently in progress, what is green, and what the next planned
item is. Working on something that conflicts with in-progress work is avoidable.

---

## Step 2 — Find or open an issue

Every non-trivial change should have a GitHub issue. Look for an existing one
before opening a new one. If your change is small (a typo fix, a one-line
correction), a PR without an issue is fine.

---

## Step 3 — Create a branch

Branch from `main`. Use a descriptive name that follows the existing pattern:

```sh
git checkout main
git pull
git checkout -b feat/my-feature-name
# or
git checkout -b fix/issue-number-short-description
# or
git checkout -b refactor/what-is-being-refactored
```

---

## Step 4 — Write tests first

This codebase follows TDD ordering: write the test, confirm it fails, then
write the code that makes it pass. Reviewers will notice if tests were added
as an afterthought.

For pure functions, add unit tests in the `#[cfg(test)] mod tests` block of
the same file.

For database behaviour, add an integration test in the appropriate `tests/`
directory using the per-test Postgres cluster helpers from `tests-common`.

---

## Step 5 — Write the code

Keep changes focused. A PR that does three things at once is three times
harder to review and three times more likely to be asked to split.

Check the hard constraints from [chapter 8](./08-hard-constraints.md) before
committing. If your change adds a dependency, verify its license. If your
change touches a platform-specific path, implement or stub the counterpart.

---

## Step 6 — Run the full test suite

```sh
cargo test --workspace -- --nocapture
```

All tests should pass. No new warnings should appear. If you are on Linux,
confirm that sandbox tests are actually running (not skipping — see
[chapter 5](./05-build-test-run.md#what-skip-lines-mean)).

---

## Step 7 — Write a clear commit message

Each commit should explain *why* the change was made, not just *what* changed.
The "what" is visible in the diff.

Good:
```
feat(sandbox): add mem_mb field to SandboxPolicy

Without a memory cap, a compromised worker could allocate unbounded
memory and crash the host. This wires the existing cgroup MemoryMax
mechanism to a typed field on SandboxPolicy so callers can express
the cap once and have it enforced on both Linux and macOS.
```

Not useful:
```
update sandbox policy
```

---

## Step 8 — Open the pull request

The PR description should include:

1. **What this PR does** — one paragraph.
2. **Why it is needed** — link to the issue.
3. **How to test it** — which test command covers the new behaviour.
4. **Platform coverage** — does it work on both Linux and macOS? Which tests
   cover the other platform?
5. **Test count delta** — e.g. "workspace 998 → 1002 (+4)".

If you cannot test on both platforms, say so explicitly and explain why.
Reviewers can run the CI suite on the other platform.

---

## What reviewers look for

- **Hard constraints** (chapter 8) — no exceptions.
- **Test coverage** — new behaviour should have a failing test that the code
  makes pass.
- **File-size cap** — files over 500 LOC should be flagged and ideally split.
- **Audit log** — any tool call, secret access, or state transition that
  matters to the security story should write an audit row.
- **Cross-platform** — platform-specific paths need counterparts or stubs.
- **Comment quality** — comments explain *why*, not *what*. No multi-paragraph
  docstrings for obvious code.

---

## After the PR is merged

Update `docs/devel/handovers/HANDOVER.md` with what shipped (commit hash,
test count delta, any open follow-up issues). This is part of the convention:
the next contributor will read the handover to understand the current state.

If the change affects the roadmap (an item can be ticked off, or a new item
needs to be added), update `docs/devel/ROADMAP.md` too.

Commit the doc updates to `main` after the feature PR merges — keep the
handover current.

---

## A note on patience

This project is security-first. Reviewers are conservative about changes that
touch the sandbox layer, the dispatcher chokepoint, the audit log, or the
CASSANDRA pipeline. Expect careful review of those areas. The conservative
posture is intentional, not personal.
