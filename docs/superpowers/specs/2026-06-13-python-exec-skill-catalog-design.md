# python-exec skill catalog — slice 1 design

**Date:** 2026-06-13
**Status:** approved (brainstorm), pending implementation plan
**ROADMAP:** Phase 4 — "Skill catalog (named/persisted Python skills) with optional human-approve gate" (ROADMAP:228)
**Precedent:** the L3 templated-skill arc (`core/src/memory/l3_*`) and the `python-exec` worker (PR #267 + #270)

---

## 1. Summary

A **Python skill** is a named, persisted, **agent-authored** snippet of Python that the agent
has already run successfully via the `python.exec` worker, promoted through the **same trust
lifecycle** as L3 templated skills, and re-invoked **verbatim** through `python.exec`.

The payload differs from a templated skill (opaque Python source vs a structured tool-call
template `Vec<L3TemplateStep>`); **everything else reuses the existing L3 arc** — the
`SkillTrust` enum, the `memories` layer-3 storage, canonical-SHA-256 dedup, the operator
approve/pin/revoke CLI, the daemon-side `l3_run` reroute, the `<skills>` planner surfacing, and
the audit shape.

This slice deliberately keeps the strongest security property attainable: **what the operator
approves (by SHA-256) is byte-for-byte what executes**, and **the python-exec jail
(`Net::Deny`, scratch-only FS, cpu/mem/wall caps, `WorkerStrict` seccomp) is the containment
boundary** — not static analysis of the code.

### Locked design decisions (from brainstorm 2026-06-13)

| Fork | Decision | Rationale |
| ---- | -------- | --------- |
| Authoring path | **Agent-authored** (mirror L3 crystallise) | Maximum reuse of the crystallise→approve→pin→invoke machinery; code provenance is the agent, grounded by prior execution. |
| Parameters | **None — verbatim code, SHA-256-bound** | Preserves "approve exactly this": the approved code == the code that runs. No injection surface. Params are a clean later slice once `python.exec` grows a structured arg channel. |
| Storage | **Reuse layer-3 `memories` row + `kind` discriminator** | Reuses `set_skill_trust`, `load_layer_by_trust`, dedup, and the trust enum verbatim. No migration. |
| Approval gate (machine-checkable) | **L3 carry-over only**: structural caps + `secret://` scan + SHA-256 binding | The jail is the security boundary; the human reading the source is the real gate. No brittle static analysis / import allowlist. |

---

## 2. Lifecycle (reused verbatim from L3)

```
agent runs python.exec ──► crystallise ──────► operator reads source ──► approve / pin ──► invoke
   (this task)             l3py_crystallise     memory l3 show            set_skill_trust    operator CLI run
                           trust=untrusted                               (kind-agnostic)     or agent (pinned only)
                           kind=python                                                       │
                                                                                             └─► python.exec (verbatim code)
```

Same five stations as the templated arc: **crystallise → approve → pin → invoke → surface**. The
following are **shared, not duplicated**:

- `SkillTrust { Untrusted | UserApproved | Pinned }` + `from_metadata_str` (fail-safe) +
  `as_str` — `core/src/memory/l3_approval.rs`.
- `set_skill_trust(pool, id, trust_str)` — `db/src/memories/write.rs` (operates on
  `metadata->>'trust'` regardless of kind).
- `load_layer_by_trust(pool, layer, trusts, cap)` — `db/src/memories/search.rs` (returns both
  kinds; callers branch on `kind`).
- `is_runnable` (UserApproved | Pinned) / `is_autonomously_invocable` (Pinned only) gates.
- The daemon-side `l3_run` task reroute (`core/src/scheduler/l3_run.rs`) — the operator→daemon
  command channel over the Postgres `tasks` queue.

---

## 3. Storage & the `kind` discriminator

Same `memories` table, `layer = 3` (the Skill layer). **No migration.** A new `metadata.kind`
field distinguishes the two flavours:

- `metadata.kind` **absent ⇒ `"templated"`** (backward-compatible default; existing L3 rows are
  untouched and continue to parse as templated).
- `metadata.kind == "python"` ⇒ a Python skill row.

A Python skill row:

```json
{
  "source": "agent_raised",
  "task_id": 42,
  "trust": "untrusted",
  "kind": "python",
  "body_sha256": "<sha256 of canonical {name,description,code} JSON, lowercase hex>",
  "created_at": "<RFC3339>",
  "python": {
    "name": "<snake_case, 1–64 bytes>",
    "description": "<1–512 bytes, no newline/control>",
    "code": "<verbatim Python source, 1..=CODE_CAP bytes, valid UTF-8>"
  }
}
```

- The **`body` column = the description** (lexical-searchable + human-listable), exactly as
  templated skills store it.
- **Code lives in `metadata.python.code`** — never surfaced to the planner, printed to the
  operator only on `memory l3 show`.
- **Dedup** = canonical-JSON SHA-256 (object keys sorted at every depth) over the
  `PythonSkillCandidate`, EXISTS-checked against `metadata->>'body_sha256'` before insert — the
  same mechanism as `crystallise_l3`.
- **`CODE_CAP` = 64 KiB** (well under the worker's 256 KiB `python.exec` code limit; a tunable
  constant). A skill larger than this is rejected at crystallise with a structural reason.

---

## 4. Rust modules

### 4.1 New candidate type

`core/src/cassandra/types.rs`, beside `L3SkillCandidate`:

```rust
/// An agent-authored Python skill awaiting crystallisation.
///
/// `code` is stored and later executed *verbatim* — no placeholder substitution,
/// no params. The SHA-256 of the canonical {name, description, code} JSON is the
/// dedup key and the approval binding (see §4.4).
pub struct PythonSkillCandidate {
    pub name: String,        // snake_case, 1..=64 bytes
    pub description: String, // 1..=512 bytes, no newline / control chars
    pub code: String,        // verbatim Python, 1..=CODE_CAP bytes, valid UTF-8
}
```

### 4.2 Code organization — parallel modules sharing pure helpers

Three focused sibling modules under `core/src/memory/`, mirroring the templated layout and
**reusing** the shared trust/db/RejectReason helpers rather than generalizing the `l3_*` files
in place (keeps each file focused, well under the 500-LOC cap, and keeps `kind` branches out of
the templated path):

- **`l3py_crystallise.rs`** — the writer.
- **`l3py_approval.rs`** — the pure approval evaluator.
- **`l3py_invoke.rs`** — the pure invocation gate + operator/agent orchestration (split into
  `pure`/`operator`/`agent` siblings if it approaches cap, mirroring `l3_invoke/`).

`l3_surface.rs` gains an **additive** `kind`-aware branch (no new module).

### 4.3 Crystallise (`l3py_crystallise.rs`)

- `validate_python_skill(c: &PythonSkillCandidate) -> Result<PythonSkillCandidate, L3PyError>` —
  **pure**: snake_case name (≤64 B), description (1–512 B, no newline/control), code
  (1..=`CODE_CAP` B, valid UTF-8), and a defensive `secret://` literal scan over the code.
- `compute_python_sha256(c: &PythonSkillCandidate) -> String` — **pure**: SHA-256 over canonical
  JSON (sorted keys), lowercase hex.
- `crystallise_python_skill(pool, candidate, task_id) -> Result<L3WriteOutcome, L3PyError>` —
  validate → compute SHA → EXISTS-check by `body_sha256` → insert at `layer=3`,
  `trust="untrusted"`, `kind="python"` via the existing `insert_memory_at_layer`.

**Trigger / grounding:** a new `Plan.python_skill: Option<PythonSkillCandidate>` directive the
scheduler's `drain_lane` validates and crystallises, **grounded by `dispatch_count >= 1`** (the
agent must have actually dispatched a tool this task — the direct mirror of the L3 grounding
gate). *(Slice-2 refinement, noted not built: tighten the gate to "the agent ran `python.exec`
successfully this task" specifically, which requires per-tool dispatch tracking.)*

### 4.4 Approval (`l3py_approval.rs`)

- `evaluate_python_approval(c: &PythonSkillCandidate) -> ApprovalDecision` — **pure, no
  live-registry dependency** (the templated path's tool-existence check is moot — a Python skill
  dispatches no tools; its entire capability ceiling is the python-exec jail). Re-runs structural
  validation + the `secret://` scan over the code.
- Reuses the shared `ApprovalDecision { Approve | Reject { reasons } }` and `RejectReason`. Adds
  a code-appropriate `RejectReason::CodeSecretRef { offset: usize, found: String }` arm (the
  templated `SecretRefPresent { step, .. }` keys on step index, which Python code has none of).
- Trust flip on `approve`/`pin`: the existing `set_skill_trust` (kind-agnostic) — no change.

### 4.5 Invoke (`l3py_invoke.rs`)

- `prepare_python_invocation(c, stored_trust, stored_sha256) -> Result<String, InvokeRefusal>` —
  **pure decision gate**:
  1. trust is runnable (`UserApproved | Pinned`), else refuse;
  2. re-run `evaluate_python_approval(c)` (structural + secret re-scan), else refuse with reasons;
  3. **re-compute `compute_python_sha256(c)` and confirm it equals `stored_sha256`** — refuse on
     drift (the TOCTOU close for code: the approved bytes are the executed bytes);
  4. return the verbatim `code` string.
- **Operator path** (`l3py_invoke::operator`): dispatches **one** `python.exec` step
  (`{ "code": <verbatim> }`) through the existing `ToolHostStepDispatcher`; refusals audited
  regardless of `--execute`; dry-run returns the code that *would* run; audit rows mirror L3.
- **Agent-autonomous path** (`l3py_invoke::agent`): extend the existing `Plan.invoke_skill`
  directive so `expand_for_agent` resolves a **pinned** Python skill by name → a single
  `python.exec` step. Only `Pinned` is autonomously invocable; CASSANDRA review applies on the
  agent path exactly as for templated skills.

### 4.6 Surfacing (`l3_surface.rs`, additive)

- `SurfacedSkill` gains a `kind` field. A Python skill surfaces as `name` + `description` +
  `invocable` — **no params, and the code is never shown to the planner** (it invokes by name).
- `load_l3_skills_for_prompt` already trust-filters via `load_layer_by_trust`; it gains a
  `kind`-aware projection (templated → existing `parse_surfaced_skill`; python → a thin
  `name/description/invocable` projection). Row/byte caps unchanged.

---

## 5. CLI surface

Reuse the `memory l3` subcommands, made kind-aware, plus **one new command**:

- **`memory l3 show <id>`** *(new)* — prints the **full source** of a Python skill (or the
  template, for a templated skill). **Security-critical:** the operator must read the code before
  approving; that human read *is* the gate.
- `list` — gains a `kind` column + code byte-size for Python rows.
- `approve` / `pin` / `revoke` / `remove` — **kind-agnostic** (operate on trust/row), unchanged.
- `run <id> [--arg…ignored] [--execute]` — **kind-aware**: a Python skill queues an
  `l3_run`-style task the **daemon** executes against its live registry. The daemon must have
  `python-exec` enabled (`KASTELLAN_PYTHON_EXEC_ENABLE=1`); if not registered, **fail closed with
  a clear error** (never silently no-op). Dry-run by default.

**Audit:** reuse the L3 action constants (`l3.crystallised`, `l3.approve`, `l3.pin`, `l3.revoke`,
`l3.remove`, `l3.invoke_rejected`, `l3.invoked`, `l3.invoke_outcome`) with `kind:"python"` added
to the payload — one coherent skill-lifecycle stream for the `kastellan-cli audit tail` consumer.

---

## 6. Security analysis

- **Containment unchanged.** A Python skill runs in the exact same python-exec jail as any
  ad-hoc `python.exec` call: `Net::Deny`, `fs_write=[]` (scratch = the jail's ephemeral `/tmp`
  tmpfs), `cpu 10 s` / `mem 512 MiB` / `wall 30 s`, `Profile::WorkerStrict`, `SingleUse`. Worst
  case for a malicious/buggy skill is bounded CPU/mem until the policy kills it; it cannot reach
  the network, the host FS, or any other worker. This is **within the threat-model invariant** —
  no new reachable surface.
- **Approve == execute.** The SHA-256 binding (§4.5 step 3) guarantees the bytes the operator
  read and approved are the bytes that run. A stored row's code is immutable (trust flips touch
  only `metadata.trust` via `jsonb_set`); the re-hash is belt-and-suspenders against any
  out-of-band tampering and mirrors the L3 "re-validate against live state at invoke" discipline.
- **No secret embedding.** The `secret://` scan at both crystallise and invoke rejects any skill
  whose source contains a `secret://` literal (which would not resolve inside python-exec anyway —
  secret substitution is a core-side input concern — but the scan is defense-in-depth and matches
  the templated gate).
- **No autonomous run without pinning.** Surfacing shows only `UserApproved | Pinned`; only
  `Pinned` is autonomously invocable by the agent, and that path still passes CASSANDRA review.
  An `Untrusted` freshly-crystallised skill is inert until an operator acts.
- **Fail-closed daemon run.** If `python-exec` is not enabled in the daemon, `run` errors
  explicitly rather than silently doing nothing.

---

## 7. Scope boundaries

**Explicitly deferred (NOT slice 1):**

- **Runtime params** — needs a `python.exec` structured arg channel (a python-exec slice-2);
  until then skills are verbatim/param-less.
- **Operator-authored `register` CLI** — this slice is agent-authored only.
- **Richer per-trust capability ceilings** (ROADMAP:229) — the python-exec jail *is* the ceiling
  today; per-level (workers/net/fs) ceilings are a separate Phase-4 item.
- **macOS writable scratch** — an orthogonal python-exec slice-1 gap (Seatbelt deny-default).
- **Tighter grounding gate** — "python.exec ran successfully this task" specifically (vs
  `dispatch_count >= 1`); needs per-tool dispatch tracking.

---

## 8. Testing (TDD)

**Pure unit tests** (no I/O):

- `validate_python_skill`: name (non-snake_case reject, >64 B reject), description (newline/control
  reject, >512 B reject), code (empty reject, >`CODE_CAP` reject, invalid-UTF-8 handled),
  `secret://`-in-code reject.
- `compute_python_sha256`: determinism + key-order independence + sensitivity to each field.
- `evaluate_python_approval`: Approve on clean skill; Reject with `CodeSecretRef` on embedded
  `secret://`; Reject on structural violation.
- `prepare_python_invocation`: refuse on `Untrusted`; refuse with reasons on dirty re-validation;
  **refuse on SHA drift**; return verbatim code on the happy path for `UserApproved` and `Pinned`.

**DB tests** (PG-required, skip-as-pass):

- `crystallise_python_skill`: insert at layer 3 / `kind=python` / `trust=untrusted`; dedup on
  re-crystallise of identical code.
- `set_skill_trust` flips a Python row (kind-agnostic).
- `load_layer_by_trust` returns Python rows alongside templated; surfacing projection parses a
  Python row (and a malformed one is skipped, fail-safe).

**E2E** (PG + sandbox gated, skip-as-pass; mirrors `cli_memory_l3_run_daemon_e2e`):

- `cli_memory_l3py_run_daemon_e2e`: crystallise a Python skill → operator `approve` → operator
  `run --execute` against the live daemon registry → real-jail `python.exec` round-trip returns
  the expected stdout. Pin the `env_clear()` + no-`KASTELLAN_PYTHON_EXEC_BIN`-leak invariant (the
  #179-style regression pin). No-daemon → cancels & errors.

---

## 9. Build sequence (for the implementation plan)

1. `PythonSkillCandidate` type + `CODE_CAP` constant + canonical-JSON SHA helper (pure, tests).
2. `l3py_crystallise.rs` (pure validate + crystallise writer + dedup; DB test).
3. `Plan.python_skill` directive + `drain_lane` crystallise wiring (grounding gate).
4. `l3py_approval.rs` (pure evaluator + `CodeSecretRef` arm; unit tests).
5. `l3py_invoke.rs` pure gate (`prepare_python_invocation` + SHA-drift refuse; unit tests).
6. `l3_surface.rs` kind-aware projection (`SurfacedSkill.kind`; unit tests).
7. CLI: `memory l3 show` + kind-aware `list`/`run`; daemon `l3_run` Python branch (fail-closed).
8. Agent-autonomous `Plan.invoke_skill` Python resolution (`expand_for_agent`).
9. `cli_memory_l3py_run_daemon_e2e` + audit `kind` field.

Each step is TDD (test first), each file under 500 LOC, all `cargo test --workspace` +
`clippy --workspace --all-targets -D warnings` green before commit.
