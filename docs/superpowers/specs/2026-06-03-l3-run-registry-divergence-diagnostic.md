# L3 `run` registry-divergence diagnostic + Opt-3 decision record (issue #179)

**Date:** 2026-06-03
**Status:** design approved, pre-implementation
**Issue:** [#179](https://github.com/hherb/hhagent/issues/179) — *memory l3 run: live registry rebuilt from operator env diverges from daemon's `registry.loaded` snapshot (approve/run parity)*
**Roadmap:** interim slice of the L3 arc; the structural fix is folded into ROADMAP line 165 (the autonomous door).

---

## Problem

`hhagent-cli memory l3 run <id>` and `hhagent-cli memory l3 approve <id>` disagree about *which tools exist*:

| Path | Source of "which tools exist" | Where |
| ---- | ----------------------------- | ----- |
| `memory l3 approve` | daemon's recorded `registry.loaded` **snapshot** (a DB audit row) | `latest_registry_tools` → `evaluate_approval` |
| `memory l3 run` | **in-process rebuild from the operator's shell env** (`HHAGENT_SHELL_EXEC_BIN`, the gliner-relex env, …) | `registry_build::build_tool_registry` |

`build_tool_registry` only registers `shell-exec` when `HHAGENT_SHELL_EXEC_BIN` resolves to a file *in the CLI's own environment* (`core/src/registry_build.rs:105`). Run `memory l3 run` from a plain shell that lacks the daemon's unit env → empty/reduced registry → a legitimately-approved skill is **refused** with a cryptic `tool 'shell-exec' not in registry`.

- **Not a security issue.** It fails *safe*: the worst outcome is a refusal, and under `--execute` the sandbox still contains anything that does run. No skill runs that shouldn't.
- **It is a usability cliff,** and the failure message is confusing — it names a tool that "should" be registered without explaining why the local view differs.

## Key insight (reframes the issue's three options)

The env-dependence is **intrinsic to the "CLI spawns the worker in-process" model**, not just a validation-source choice:

- **Validation** consumes `live_tools` — a `BTreeSet<String>` of names (`memory_l3.rs:399`).
- **Execution** (`--execute`) moves the *full* `ToolRegistry` into `ToolHostStepDispatcher` (`memory_l3.rs:413`). That registry carries the real binary path + allowlist needed to **spawn** the worker, not just names.

Therefore:

- **Issue Option 1** (validate `run` against the daemon snapshot) only fixes the *dry-run* refusal. Under `--execute` the CLI process still spawns the worker itself, so it still needs the daemon's env to build a *spawnable* registry. Worse, it introduces a validate-OK-then-dispatch-fail (`UNKNOWN_TOOL`) skew. **Rejected.**
- **Issue Option 3** (execution moves into the daemon; `run` becomes a thin IPC trigger) is the *only* option that actually removes the env-dependence — and it is exactly the daemon-side execution path the **autonomous door** must build inside the inner loop. **This is the correct long-term fix, deferred to that slice.**
- **Issue Option 2** (rebuild live, but make divergence loud and specific) directly fixes the harm the issue actually names — the confusing failure mode — at near-zero cost, without changing the security posture. **This is the interim fix shipped here.**

**Decision (Approach C):** ship Option 2's diagnostic now; record Option 3 as the long-term direction the autonomous-door slice delivers.

## Non-goals

- No change to the security posture: the live re-validation (TOCTOU close via `evaluate_approval`) and the sandboxed `tool_host::dispatch` chokepoint are untouched.
- No change to what is or isn't runnable. The diagnostic is purely advisory output on the existing refusal path.
- No daemon-side execution path, no IPC surface, no `Plan`-schema change — those belong to the autonomous door (ROADMAP line 165).

---

## Design

### Component 1 — pure classifier (`core/src/memory/l3_invoke.rs`)

A pure, unit-testable function comparing three name-sets, with a typed result whose `Display` renders an actionable operator hint.

```rust
/// Why a tool a skill needs is absent from the live in-process registry,
/// classified by comparing the live set against the daemon's recorded
/// `registry.loaded` snapshot. Drives the operator-facing hint on the
/// `memory l3 run` refusal path (issue #179); advisory only — it changes
/// nothing about what is or isn't runnable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryDivergence {
    /// In the daemon's snapshot but missing from the live rebuild — almost
    /// always an unset env var (e.g. `HHAGENT_SHELL_EXEC_BIN`) in the
    /// operator's shell. THIS is the #179 usability cliff.
    MissingLocallyButInSnapshot { tool: String },
    /// Missing locally and no daemon snapshot exists to compare against —
    /// likely an env problem, but unconfirmable (has the daemon ever run?).
    MissingLocallyNoSnapshot { tool: String },
    /// In neither the live registry nor the snapshot — a genuinely unknown
    /// tool, not an environment problem (the legitimate refusal).
    UnknownEverywhere { tool: String },
}

/// Classify every tool the skill NEEDS that is absent from the live registry.
/// Returns empty when every needed tool is present locally — so the caller
/// stays silent on refusals that are not about missing tools (trust,
/// `secret://`, arg errors).
///
/// `snapshot_tools == None` means the daemon has never recorded a
/// `registry.loaded` row.
pub fn diagnose_registry_divergence(
    needed_tools: &BTreeSet<String>,
    live_tools: &BTreeSet<String>,
    snapshot_tools: Option<&BTreeSet<String>>,
) -> Vec<RegistryDivergence>;
```

**Classification rule** — for each `tool` in `needed_tools` that is *not* in `live_tools`:

| snapshot state | variant |
| -------------- | ------- |
| `Some(s)` and `s` contains `tool` | `MissingLocallyButInSnapshot` |
| `Some(s)` and `s` lacks `tool`    | `UnknownEverywhere` |
| `None`                            | `MissingLocallyNoSnapshot` |

Output order is deterministic (iteration over the sorted `BTreeSet`).

**`Display`** renders each variant into a single actionable line, e.g.:

- `MissingLocallyButInSnapshot { tool }` → `'{tool}' is registered by the daemon but missing from your environment — is the tool's env var (e.g. HHAGENT_SHELL_EXEC_BIN) set? Run with the same environment the daemon uses.`
- `MissingLocallyNoSnapshot { tool }` → `'{tool}' is missing from your environment and no daemon registry snapshot exists to compare against (has the daemon run at least once?).`
- `UnknownEverywhere { tool }` → `'{tool}' is unknown to both your environment and the daemon's last snapshot — the skill references a tool that is no longer registered.`

(Exact wording is finalised in implementation; the variants and their *meaning* are the contract.)

### Component 2 — CLI integration (`core/src/bin/hhagent-cli/memory_l3.rs`)

In `memory_l3_run`'s `InvokeReport::Refused` arm (currently `memory_l3.rs:430`), after printing the existing refusal reasons:

1. Build `needed_tools: BTreeSet<String>` from `template.steps[].tool`.
2. Fetch `snapshot_tools` via the existing `latest_registry_tools(&pool)` helper (`Ok(Some(set))` / `Ok(None)` / on `Err`, skip the diagnostic — never fail the command on a diagnostic-only DB read).
3. Call `diagnose_registry_divergence(&needed_tools, &live_tools, snapshot.as_ref())`.
4. If non-empty, print the hints to **stderr** under a `hint:` heading (consistent with how refusal reasons are already printed to stderr).

`live_tools` is already in scope at this point (`memory_l3.rs:399`). The classifier's independence from `invoke_l3`'s reason strings means: a non-tool refusal (e.g. trust) with all tools present locally yields an empty hint list → no extra output; no coupling to reason-string formats.

The existing operator-prerequisite doc comment on `memory_l3_run` (`memory_l3.rs:316`) is updated to point at the new diagnostic and to note the #179 long-term direction.

### Component 3 — decision record (docs only, no code)

- This spec records the Option-3 decision (above).
- **ROADMAP line 165** (the autonomous door) gains a sub-note: *delivering daemon-side execution (the single execution path) subsumes #179's structural remainder; the operator `run` CLI is rerouted to a daemon IPC trigger at that point and the in-process rebuild path retired.*
- **Issue #179** stays **open**, re-scoped to the daemon-side single-execution-path (Opt 3) structural fix; a comment records that the interim diagnostic shipped (this slice) and links ROADMAP line 165.

---

## File-size check

`l3_invoke.rs` is 390 LOC today; the enum + classifier + `Display` add roughly 50 LOC → ~440, under the 500-LOC cap. Tests live in the existing sibling `core/src/memory/l3_invoke/tests.rs` (declared `mod tests;` at `l3_invoke.rs:390`).

## Testing (TDD)

Written **RED first**, in `core/src/memory/l3_invoke/tests.rs`:

1. `diagnose_*_missing_in_snapshot_is_env_hint` — needed ∉ live, ∈ snapshot ⇒ `MissingLocallyButInSnapshot`.
2. `diagnose_*_unknown_everywhere` — needed ∉ live, ∉ snapshot(Some) ⇒ `UnknownEverywhere`.
3. `diagnose_*_no_snapshot` — needed ∉ live, snapshot `None` ⇒ `MissingLocallyNoSnapshot`.
4. `diagnose_*_all_present_is_empty` — every needed tool ∈ live ⇒ `vec![]` (silence on non-tool refusals).
5. `diagnose_*_multiple_tools_deterministic_order` — several missing tools ⇒ stable, sorted order.
6. `display_*_renders_actionable_hint` — each variant's `Display` is non-empty and names the tool.

Optionally extend the existing live-PG e2e `core/tests/cli_memory_l3_run_e2e.rs` *unknown-tool-refuses* scenario to assert the hint text appears on stderr (nice-to-have; the refusal path itself is already covered).

## Verification gates (session-end)

- `cargo test --workspace` green (+ new unit tests).
- `cargo clippy --workspace --all-targets --locked -- -D warnings` exit 0.
- Doc-link check: unresolved count unchanged vs `main`.
- Live PG (Postgres.app v18) if the e2e is extended: `cli_memory_l3_run_e2e` green.
