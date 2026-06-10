# L3 `run` registry-divergence diagnostic Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `kastellan-cli memory l3 run <id>` refuses because a needed tool is absent from its in-process registry rebuild, print an actionable hint that distinguishes "your env var is unset (the daemon has it)" from "the tool is genuinely unknown" — resolving the confusing failure mode in issue #179.

**Architecture:** A pure classifier `diagnose_registry_divergence` in `core/src/memory/l3_invoke.rs` compares three tool-name sets (needed / live / daemon-snapshot) and returns a `Vec<RegistryDivergence>`; its `Display` renders each into an operator hint. The CLI `memory_l3_run` handler calls it on the existing `InvokeReport::Refused` arm — fetching the daemon snapshot via the existing `latest_registry_tools` helper — and prints any hints to stderr. No change to security posture or to what is runnable; the diagnostic is advisory output only.

**Tech Stack:** Rust, `std::collections::BTreeSet`, `thiserror`/`std::fmt::Display`, `sqlx` (only via the existing snapshot helper), `cargo test`.

---

## File Structure

- **Modify** `core/src/memory/l3_invoke.rs` — add `RegistryDivergence` enum + `Display` impl + `diagnose_registry_divergence` pure fn. (~390 → ~440 LOC, under cap.)
- **Modify** `core/src/memory/l3_invoke/tests.rs` — add unit tests for the classifier + `Display`.
- **Modify** `core/src/bin/kastellan-cli/memory_l3.rs` — call the classifier in the `InvokeReport::Refused` arm; update the operator-prerequisite doc comment.
- **Modify** `docs/devel/ROADMAP.md` — sub-note on line 165 (autonomous door subsumes #179's structural remainder).
- **Modify** `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — session-end state update.

---

## Task 1: Pure classifier `diagnose_registry_divergence` + `RegistryDivergence`

**Files:**
- Modify: `core/src/memory/l3_invoke.rs`
- Test: `core/src/memory/l3_invoke/tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `core/src/memory/l3_invoke/tests.rs`. First confirm the existing `use super::*;` / import style at the top of that file and match it; these tests reference `super::{diagnose_registry_divergence, RegistryDivergence}` and `std::collections::BTreeSet`. Add a small local helper to build a set from `&[&str]`.

```rust
// --- issue #179: registry-divergence diagnostic --------------------------

use std::collections::BTreeSet;

use super::{diagnose_registry_divergence, RegistryDivergence};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn diagnose_missing_in_snapshot_is_env_hint() {
    // needed shell-exec is absent from the live rebuild but present in the
    // daemon snapshot ⇒ classic "env var unset" cliff.
    let needed = set(&["shell-exec"]);
    let live = set(&[]); // operator shell lacked KASTELLAN_SHELL_EXEC_BIN
    let snapshot = set(&["shell-exec"]);
    let got = diagnose_registry_divergence(&needed, &live, Some(&snapshot));
    assert_eq!(
        got,
        vec![RegistryDivergence::MissingLocallyButInSnapshot { tool: "shell-exec".into() }]
    );
}

#[test]
fn diagnose_unknown_everywhere() {
    // needed tool absent from BOTH live and a present snapshot ⇒ genuinely
    // unknown, not an env problem.
    let needed = set(&["ghost-tool"]);
    let live = set(&["shell-exec"]);
    let snapshot = set(&["shell-exec"]);
    let got = diagnose_registry_divergence(&needed, &live, Some(&snapshot));
    assert_eq!(
        got,
        vec![RegistryDivergence::UnknownEverywhere { tool: "ghost-tool".into() }]
    );
}

#[test]
fn diagnose_no_snapshot() {
    // needed tool absent from live and the daemon has never recorded a
    // snapshot ⇒ can't confirm, but flag it.
    let needed = set(&["shell-exec"]);
    let live = set(&[]);
    let got = diagnose_registry_divergence(&needed, &live, None);
    assert_eq!(
        got,
        vec![RegistryDivergence::MissingLocallyNoSnapshot { tool: "shell-exec".into() }]
    );
}

#[test]
fn diagnose_all_present_is_empty() {
    // every needed tool is in the live registry ⇒ no hint (stay silent on
    // refusals that are not about missing tools).
    let needed = set(&["shell-exec", "gliner-relex"]);
    let live = set(&["shell-exec", "gliner-relex"]);
    let snapshot = set(&["shell-exec"]);
    let got = diagnose_registry_divergence(&needed, &live, Some(&snapshot));
    assert!(got.is_empty());
}

#[test]
fn diagnose_multiple_tools_deterministic_order() {
    // two missing tools of different classes; output follows sorted needed
    // iteration order (BTreeSet) regardless of insertion.
    let needed = set(&["zeta-tool", "alpha-tool"]);
    let live = set(&[]);
    let snapshot = set(&["alpha-tool"]); // alpha in snapshot, zeta nowhere
    let got = diagnose_registry_divergence(&needed, &live, Some(&snapshot));
    assert_eq!(
        got,
        vec![
            RegistryDivergence::MissingLocallyButInSnapshot { tool: "alpha-tool".into() },
            RegistryDivergence::UnknownEverywhere { tool: "zeta-tool".into() },
        ]
    );
}

#[test]
fn display_renders_actionable_hint_naming_the_tool() {
    let variants = [
        RegistryDivergence::MissingLocallyButInSnapshot { tool: "shell-exec".into() },
        RegistryDivergence::MissingLocallyNoSnapshot { tool: "shell-exec".into() },
        RegistryDivergence::UnknownEverywhere { tool: "shell-exec".into() },
    ];
    for v in &variants {
        let rendered = v.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("shell-exec"), "hint must name the tool: {rendered}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib memory::l3_invoke::tests::diagnose 2>&1 | tail -20`
Expected: compile error — `cannot find function diagnose_registry_divergence` / `cannot find type RegistryDivergence` in `super`.

- [ ] **Step 3: Implement the enum, `Display`, and the classifier**

In `core/src/memory/l3_invoke.rs`, after the `prepare_invocation` function (before `run_steps`), add:

```rust
/// Why a tool a skill needs is absent from the live in-process registry,
/// classified by comparing the live set against the daemon's recorded
/// `registry.loaded` snapshot. Drives the operator-facing hint on the
/// `memory l3 run` refusal path (issue #179). Advisory only — it changes
/// nothing about what is or isn't runnable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryDivergence {
    /// In the daemon's snapshot but missing from the live rebuild — almost
    /// always an unset env var (e.g. `KASTELLAN_SHELL_EXEC_BIN`) in the
    /// operator's shell. THIS is the #179 usability cliff.
    MissingLocallyButInSnapshot { tool: String },
    /// Missing locally and no daemon snapshot exists to compare against —
    /// likely an env problem, but unconfirmable (has the daemon ever run?).
    MissingLocallyNoSnapshot { tool: String },
    /// In neither the live registry nor the snapshot — a genuinely unknown
    /// tool, not an environment problem (the legitimate refusal).
    UnknownEverywhere { tool: String },
}

impl std::fmt::Display for RegistryDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryDivergence::MissingLocallyButInSnapshot { tool } => write!(
                f,
                "'{tool}' is registered by the daemon but missing from your \
                 environment — is the tool's env var (e.g. KASTELLAN_SHELL_EXEC_BIN) \
                 set? Run with the same environment the daemon uses."
            ),
            RegistryDivergence::MissingLocallyNoSnapshot { tool } => write!(
                f,
                "'{tool}' is missing from your environment and no daemon registry \
                 snapshot exists to compare against (has the daemon run at least once?)."
            ),
            RegistryDivergence::UnknownEverywhere { tool } => write!(
                f,
                "'{tool}' is unknown to both your environment and the daemon's last \
                 snapshot — the skill references a tool that is no longer registered."
            ),
        }
    }
}

/// Classify every tool the skill NEEDS that is absent from the live registry,
/// using the daemon's recorded `registry.loaded` snapshot to distinguish an
/// unset-env cliff from a genuinely unknown tool (issue #179).
///
/// Returns empty when every needed tool is present locally — so the caller
/// stays silent on refusals that are not about missing tools (trust,
/// `secret://`, arg errors). `snapshot_tools == None` means the daemon has
/// never recorded a snapshot. Output order is deterministic (sorted, by the
/// `BTreeSet` iteration of `needed_tools`).
pub fn diagnose_registry_divergence(
    needed_tools: &BTreeSet<String>,
    live_tools: &BTreeSet<String>,
    snapshot_tools: Option<&BTreeSet<String>>,
) -> Vec<RegistryDivergence> {
    needed_tools
        .iter()
        .filter(|t| !live_tools.contains(*t))
        .map(|tool| match snapshot_tools {
            Some(snap) if snap.contains(tool) => {
                RegistryDivergence::MissingLocallyButInSnapshot { tool: tool.clone() }
            }
            Some(_) => RegistryDivergence::UnknownEverywhere { tool: tool.clone() },
            None => RegistryDivergence::MissingLocallyNoSnapshot { tool: tool.clone() },
        })
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib memory::l3_invoke::tests 2>&1 | tail -20`
Expected: all `l3_invoke::tests` pass, including the 6 new `diagnose_*` / `display_*` tests; existing `l3_invoke` unit tests still green.

- [ ] **Step 5: Clippy the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -15`
Expected: exit 0, no warnings.

- [ ] **Step 6: Commit**

```bash
git add core/src/memory/l3_invoke.rs core/src/memory/l3_invoke/tests.rs
git commit -m "$(cat <<'EOF'
feat(l3,#179): pure diagnose_registry_divergence classifier

Compares needed/live/daemon-snapshot tool sets to distinguish an unset-env
cliff (MissingLocallyButInSnapshot) from a genuinely unknown tool
(UnknownEverywhere) and a no-snapshot case (MissingLocallyNoSnapshot).
Display renders each into an actionable operator hint. Advisory only.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Wire the diagnostic into the `memory l3 run` refusal path

**Files:**
- Modify: `core/src/bin/kastellan-cli/memory_l3.rs` (the `InvokeReport::Refused` arm, currently ~line 430; and the doc comment at ~line 316)

- [ ] **Step 1: Add the diagnostic call to the `Refused` arm**

In `memory_l3_run`, locate the `match report` block. Replace the `InvokeReport::Refused` arm:

```rust
        InvokeReport::Refused { reasons } => {
            eprintln!("REFUSED to run skill '{}' (#{id}):", template.name);
            for r in &reasons { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
```

with one that adds the divergence hints. This uses `live_tools` (already in scope at `memory_l3.rs:399`), the `template`, and the existing `latest_registry_tools(&pool)` helper:

```rust
        InvokeReport::Refused { reasons } => {
            eprintln!("REFUSED to run skill '{}' (#{id}):", template.name);
            for r in &reasons { eprintln!("  - {r}"); }

            // Issue #179: when a refusal is (partly) about a tool missing from
            // this CLI's in-process registry rebuild, explain *why* the local
            // view differs from the daemon's. The snapshot read is best-effort
            // — a diagnostic-only DB error must never change the exit path.
            let needed: BTreeSet<String> =
                template.steps.iter().map(|s| s.tool.clone()).collect();
            let snapshot = latest_registry_tools(&pool).await.ok().flatten();
            let hints = kastellan_core::memory::l3_invoke::diagnose_registry_divergence(
                &needed, &live_tools, snapshot.as_ref(),
            );
            for h in &hints {
                eprintln!("  hint: {h}");
            }

            ExitCode::from(1)
        }
```

Note: `BTreeSet` is already imported in `memory_l3_run` (`use std::collections::BTreeSet;` at the top of the fn). `latest_registry_tools` is a sibling fn in this file. If the compiler reports `live_tools` was moved (it is built at line ~399 and `registry` is consumed into the dispatcher under `--execute`), confirm `live_tools` is a `BTreeSet<String>` owned separately from `registry` — it is collected from `registry.entries()` into its own set, so it remains available. No change needed there.

- [ ] **Step 2: Update the operator-prerequisite doc comment**

Find the doc comment block on `memory_l3_run` (the `## Operator-environment prerequisite (fail-safe)` section, ~line 316). Update its final sentences to point at the new hint and the long-term direction. Replace:

```rust
/// operator must invoke `run` with the same tool-registry env the daemon uses.
/// Parity with the snapshot used by `approve` is tracked in issue #179 (the
/// daemon-snapshot-vs-live tradeoff is a deliberate design question).
```

with:

```rust
/// operator must invoke `run` with the same tool-registry env the daemon uses.
/// When this bites, the refusal now prints a `hint:` line (via
/// `diagnose_registry_divergence`) distinguishing an unset-env cliff from a
/// genuinely unknown tool. The structural fix — moving execution into the
/// daemon so there is a single registry (issue #179, Opt 3) — is folded into
/// the autonomous-invocation slice (ROADMAP line 165), which builds that path.
```

- [ ] **Step 3: Build the CLI binary**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bin kastellan-cli 2>&1 | tail -15`
Expected: compiles clean (no `moved value` / unused-import errors).

- [ ] **Step 4: Clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -15`
Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/bin/kastellan-cli/memory_l3.rs
git commit -m "$(cat <<'EOF'
feat(l3,#179): print registry-divergence hint on memory l3 run refusal

On InvokeReport::Refused, classify needed-vs-live-vs-daemon-snapshot tools
and print an actionable `hint:` line so an unset KASTELLAN_SHELL_EXEC_BIN
no longer reads as a cryptic "tool not in registry". Snapshot read is
best-effort — never changes the exit path. Doc comment updated to point at
the hint + the Opt-3 long-term direction (ROADMAP line 165).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Extend the live-PG e2e to assert the hint (optional but recommended)

**Files:**
- Modify: `core/tests/cli_memory_l3_run_e2e.rs` (the unknown-tool-refuses scenario)

> This task requires a live Postgres (Postgres.app v18). If PG is unavailable the suite skips-as-pass; run it once PG is up. See `~/.claude/.../memory/postgres-app-bin-paths.md` for the session-local `KASTELLAN_PG_BIN_DIR` override.

- [ ] **Step 1: Read the existing unknown-tool scenario**

Run: `grep -n "unknown\|not in registry\|REFUSED\|stderr\|fn " core/tests/cli_memory_l3_run_e2e.rs | head -40`
Identify the test that approves a skill, then makes its tool unregistered at run time and asserts a refusal. Note how it captures the CLI's stderr (the harness pattern — `Command` output, `String::from_utf8`).

- [ ] **Step 2: Add a stderr assertion for the hint**

In that scenario, after the existing assertion that the run was refused, add an assertion that the stderr contains the hint substring. Match the test's existing stderr variable name; the assertion is:

```rust
    // Issue #179: the refusal should carry an actionable divergence hint,
    // not just the bare "tool ... not in registry" reason.
    assert!(
        stderr.contains("hint:"),
        "expected a registry-divergence hint on the refusal; stderr was:\n{stderr}"
    );
```

If the scenario unregisters the tool by removing it from BOTH the live rebuild and the snapshot, the hint will be `UnknownEverywhere` (contains the tool name + "no longer registered"). If it only drops it from the live rebuild (snapshot still has it), it will be `MissingLocallyButInSnapshot`. Assert on the stable `"hint:"` prefix (present for every non-empty divergence) rather than variant-specific wording, to stay robust to the scenario's exact setup.

- [ ] **Step 3: Run the e2e against live PG**

Run (adjust the bin dir per the memory note):
```sh
source "$HOME/.cargo/env"
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test cli_memory_l3_run_e2e -- --nocapture 2>&1 | tail -30
```
Expected: all scenarios pass (5/5), zero `[SKIP]` lines, including the augmented unknown-tool case.

- [ ] **Step 4: Commit**

```bash
git add core/tests/cli_memory_l3_run_e2e.rs
git commit -m "$(cat <<'EOF'
test(l3,#179): assert run refusal carries a registry-divergence hint

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Decision record — ROADMAP sub-note + #179 comment

**Files:**
- Modify: `docs/devel/ROADMAP.md` (line 165, the autonomous-door item)

- [ ] **Step 1: Add the Opt-3 sub-note to ROADMAP line 165**

Open `docs/devel/ROADMAP.md`, find the autonomous-door bullet (line 165: `- [ ] **L3 skill invocation — the AUTONOMOUS door ...`). Append to the end of that bullet's text:

```
 Also subsumes issue #179's structural remainder: building daemon-side execution gives a single tool registry, so the operator `run` CLI is rerouted to a thin daemon IPC trigger and the in-process registry rebuild (with its env-divergence cliff) is retired. The interim divergence diagnostic shipped 2026-06-03 (spec `docs/superpowers/specs/2026-06-03-l3-run-registry-divergence-diagnostic.md`).
```

- [ ] **Step 2: Add the #179 tracking comment via gh**

Run (records that the interim shipped and the issue is re-scoped to Opt 3):
```sh
gh issue comment 179 --body "Resolved the usability harm via the interim diagnostic (Approach C, Opt 2): \`memory l3 run\` refusals now print an actionable \`hint:\` line via a pure \`diagnose_registry_divergence\` classifier, distinguishing an unset-env cliff from a genuinely unknown tool. Shipped on branch \`fix/issue-179-run-registry-divergence-diagnostic\`.

Re-scoping this issue to the **structural fix (Opt 3)**: moving execution into the daemon so there is a single tool registry and the operator \`run\` CLI becomes a thin IPC trigger. That is folded into the autonomous-invocation slice (ROADMAP line 165), which has to build the daemon-side execution path anyway. Spec: \`docs/superpowers/specs/2026-06-03-l3-run-registry-divergence-diagnostic.md\`."
```
Expected: comment posted (issue stays OPEN).

- [ ] **Step 3: Commit the ROADMAP note**

```bash
git add docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(roadmap,#179): note autonomous door subsumes Opt-3 structural fix

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Full-workspace verification + session-end handover update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace test + clippy + doc-links**

Run:
```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -15
cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc -p kastellan-core --no-deps --document-private-items 2>&1 | grep -c "unresolved" || true
```
Expected: `cargo test --workspace` green (baseline 1276 + 6 new unit tests = 1282, +1 if the e2e assertion counts as a new test — it does not add a test fn, so 1282; the e2e count is unchanged). Clippy exit 0. Doc-links unresolved count = `main`'s 21 (zero new).

- [ ] **Step 2: Update HANDOVER.md**

Update the "Last updated" header, "Currently on", and "Last commit on `main`" to describe the shipped #179 diagnostic on branch `fix/issue-179-run-registry-divergence-diagnostic`. Record: what shipped (pure classifier + CLI hint), the Opt-3 deferral to the autonomous door, verification numbers, and that #179 stays OPEN re-scoped to Opt 3. Note `docs/essay-medium-draft.md` stays untracked.

- [ ] **Step 3: Tick the ROADMAP**

Add a `- [x]` entry under the L3 arc recording the interim #179 diagnostic shipped (date, branch, spec/plan paths, verification).

- [ ] **Step 4: Commit the handover**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): #179 interim diagnostic shipped

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 5: Push + open PR (operator-gated)**

Do NOT push or open the PR without operator confirmation. When confirmed:
```sh
git push -u origin fix/issue-179-run-registry-divergence-diagnostic
gh pr create --fill --base main
```

---

## Notes for the implementer

- **TDD order is load-bearing:** Task 1 writes the classifier test RED before the implementation. Don't skip the "verify it fails" step.
- **The classifier is pure** — no I/O, no `async`, no DB. All DB access (the snapshot fetch) stays in the CLI handler via the existing `latest_registry_tools` helper. Keep it that way (rule #1: pure functions in reusable modules).
- **Best-effort snapshot read:** `latest_registry_tools(&pool).await.ok().flatten()` deliberately swallows a DB error into `None` — a diagnostic must never change the command's exit path. `None` is then classified as `MissingLocallyNoSnapshot`, which is the honest message in that case.
- **Don't `git add -A`** — stage only the named files each commit; `docs/essay-medium-draft.md` and any `.claude/*.lock` must stay untracked.
- **File-size cap:** after Task 1, `wc -l core/src/memory/l3_invoke.rs` should be ~440 — under 500. If it somehow exceeds, the classifier is a clean candidate to move into a `l3_invoke/diagnose.rs` sibling (re-exported), but this is not expected.
