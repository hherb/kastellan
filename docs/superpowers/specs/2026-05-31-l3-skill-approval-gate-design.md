# L3 skill trust enum + approval gate — slice 1 of the invocation arc

**Date:** 2026-05-31
**Status:** Design, ready for plan.
**Branch:** `feat/l3-skill-approval-gate`
**Roadmap item:** 10(b) — "L3 invocation + Skill trust enum" (the *gate*, not the *door*).

## Pre-reqs (all shipped)

- **L3 writer** (spec [`2026-05-31-l3-skill-crystallisation-design.md`](2026-05-31-l3-skill-crystallisation-design.md), PR #173, merged at `6eb966e`). It populates `MemoryLayer::Skill` (L3) rows whose `metadata` is `{source, task_id, trust:"untrusted", body_sha256, created_at, template}`, where `template` is the full normalised [`L3SkillCandidate`](../../../core/src/cassandra/types.rs). `trust:"untrusted"` was written as an explicit forward-compat placeholder for *this* slice.
- **`validate_l3_skill`** ([`core/src/memory/l3_crystallise.rs`](../../../core/src/memory/l3_crystallise.rs)) — the structural + `{{placeholder}}` closed-world + reserved-tag + caps validator. Reused here for defense-in-depth re-validation.
- **Secret-ref vocabulary** — `secrets::REF_PREFIX = "secret://"` ([`core/src/secrets/mod.rs:38`](../../../core/src/secrets/mod.rs#L38)); the canonical ref format is `secret://<8-hex>`.
- **`registry.loaded` audit row** — the daemon writes one `actor='core' action='registry.loaded'` row at every startup with payload `{ "tools": [{name, binary, allowlist_len, allowlist_sha256}, …] }` ([`core/src/main.rs`](../../../core/src/main.rs) `write_registry_loaded_row`). This is the daemon's own authoritative record of which tools it registered on this host.
- **Approve/reject audit precedent** — the entity-quarantine review already ships `ACTION_ENTITIES_APPROVED`/`ACTION_ENTITIES_REJECTED` ([`core/src/scheduler/audit.rs`](../../../core/src/scheduler/audit.rs)); this slice mirrors that naming one domain over.
- **CLI audit precedent** — `cli_audit::l3_remove_and_audit` ([`core/src/cli_audit.rs`](../../../core/src/cli_audit.rs)) is the "mutate + best-effort audit" template the new helpers copy.

## Why now

The L3 writer shipped storage but left every crystallised skill `trust:"untrusted"` and **non-executable** — there is no path that promotes a skill out of untrusted, and (deliberately) no path that runs one. The ROADMAP sequences the *door* (invocation) behind the *gate* (a trust enum + an approval control), because invocation executes agent-authored tool-call sequences and is the single largest new attack surface in the memory system.

This slice ships the **gate, not the door**:

- A typed `SkillTrust` enum (`Untrusted | UserApproved | Pinned`) replacing the bare `trust` string at the read boundary.
- A **pure** approval gate that, given a skill template and the set of tools the daemon actually registered, decides Approve / Reject-with-reasons. It rejects any skill that (a) is structurally invalid, (b) carries a baked-in `secret://` reference, or (c) names a tool the live daemon did not register.
- An operator surface — `kastellan-cli memory l3 approve <id>` / `revoke <id>` — that runs the gate, flips the stored trust, and emits a typed audit row (including for *rejected* approvals, so the security trail captures every operator attempt).

Crucially, **nothing executes a skill in this slice.** `UserApproved` and `Pinned` have no behavioural consequence yet — no code reads `trust` to decide whether to run anything. The gate is built and verified before the door exists, exactly as the writer-only slice built storage before recall.

## Scope

In scope (this slice):

- **New module** [`core/src/memory/l3_approval.rs`](../../../core/src/memory/l3_approval.rs) — `SkillTrust` enum, `ApprovalDecision`/`RejectReason`, the pure `evaluate_approval` gate, and two pure helpers (`scan_secret_refs`, `extract_tool_names`). No I/O. Kept separate from the writer (`l3_crystallise.rs`, 467 LOC) so both modules stay focused and under the 500-LOC cap.
- **New db helper** `db::memories::set_skill_trust(pool, id, trust: &str) -> Result<bool, DbError>` ([`db/src/memories/write.rs`](../../../db/src/memories/write.rs)) — layer-guarded `UPDATE … WHERE id = $1 AND layer = 3`. db takes a `&str` (the `db` crate sits below `core` and cannot depend on the `core`-owned `SkillTrust` enum).
- **Three new audit action constants** in [`core/src/scheduler/audit.rs`](../../../core/src/scheduler/audit.rs): `ACTION_L3_APPROVED = "l3.approved"`, `ACTION_L3_APPROVE_REJECTED = "l3.approve_rejected"`, `ACTION_L3_REVOKED = "l3.revoked"` — plus pure payload builders.
- **Two new `cli_audit` helpers** — `l3_approve_and_audit`, `l3_revoke_and_audit` (mirror `l3_remove_and_audit`).
- **CLI** [`core/src/bin/kastellan-cli/memory_l3.rs`](../../../core/src/bin/kastellan-cli/memory_l3.rs) gains `approve` + `revoke` subcommands; `list` output reads the typed `SkillTrust` instead of the raw string.

Out of scope (named follow-ups at the end):

- **Invocation / execution.** No path substitutes parameters and runs a skill's steps. That is the next slice (the *door*), and it also needs recall surfacing.
- **Recall surfacing (item 10c).** No `<skills>` prompt block; the planner still cannot see L3 rows.
- **The `pin` command** (`UserApproved → Pinned`). `Pinned` is defined in the enum for forward-compat but no command produces it — like `trust:"untrusted"` was inert until this slice, `Pinned` is inert until invocation gives the tiers meaning.
- **Method-existence validation.** The `ToolRegistry` indexes *tools*, not *methods* (a worker returns `METHOD_NOT_FOUND` at dispatch). Method validity is therefore inherently a dispatch-time concern; the gate validates tool existence only.
- **Live-registry approval inside the daemon.** Approval runs in the CLI against the *recorded* registry snapshot (see "Where the tool check lives"). The live re-check happens at invocation time (next slice; TOCTOU defense).

## Where the tool check lives (the central design tension)

The `ToolRegistry` is a **daemon-runtime artifact**: [`build_tool_registry`](../../../core/src/main.rs) constructs it at startup from environment (`KASTELLAN_SHELL_EXEC_BIN`) plus DB allowlists, reflecting what is actually deployed on *this* host. The CLI binary never runs that bring-up, and the DB exposes only per-tool allowlists (no "list all tools", and env-gated tools like `gliner-relex` are absent from the allowlist table). So the CLI cannot reconstruct the live registry directly.

The faithful, DB-backed source the CLI *can* read is the **latest `registry.loaded` audit row** — the daemon's own list of the tool names it registered. The gate sources `known_tools` from that snapshot.

**Fail-closed on a missing snapshot.** If no `registry.loaded` row exists (the daemon has never booted, or the row was truncated), the gate cannot verify tool existence and **rejects** with `NoRegistrySnapshot` ("start the daemon once so the registry is recorded"). For a security gate, "cannot verify ⇒ do not approve" is the safe default. This is acceptable because the snapshot check is **defense-in-depth, not load-bearing**: invocation (next slice) re-validates every tool against the *live* registry at dispatch time, which is the real enforcement point (a tool could be removed between approval and invocation — classic TOCTOU). Approval-time tool checking just catches obvious mistakes early.

## The `SkillTrust` enum

```rust
// core/src/memory/l3_approval.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillTrust { Untrusted, UserApproved, Pinned }

impl SkillTrust {
    /// Metadata-string form: "untrusted" / "user_approved" / "pinned".
    pub fn as_str(self) -> &'static str { … }

    /// TOTAL parse from a metadata string. Fail-safe: any unknown or
    /// absent value maps to `Untrusted` (an unrecognised trust marker
    /// must never read as trusted).
    pub fn from_metadata_str(s: &str) -> SkillTrust { … }
}
```

The enum is *not* `Serialize`/`Deserialize`-derived: it is converted via `as_str` / `from_metadata_str` so the fail-safe parse is explicit and total (serde would error or need a custom impl on an unknown string; we want a silent, safe downgrade to `Untrusted`). Existing L3 rows carrying `"untrusted"` parse unchanged; rows with a corrupted/missing marker read as `Untrusted`.

## The approval gate (pure)

```rust
pub enum RejectReason {
    StructuralInvalid(String),               // validate_l3_skill re-run failed
    SecretRefPresent { step: usize, found: String },
    UnknownTool { tool: String },
    NoRegistrySnapshot,                      // known_tools could not be established
}
// NOTE: `NoRegistrySnapshot` is constructed by the CLI orchestration when
// no `registry.loaded` row exists — it is NOT emitted by `evaluate_approval`,
// which only ever sees a (possibly empty) `known_tools` set. It lives in this
// enum so every rejection renders through one uniform `RejectReason` path.

pub enum ApprovalDecision {
    Approve,
    Reject { reasons: Vec<RejectReason> },
}

/// Decide whether a stored skill template may be promoted to
/// `UserApproved`. PURE — no I/O. `known_tools` is the set of tool
/// names the live daemon registered (from the latest `registry.loaded`
/// snapshot). An EMPTY set is treated as fail-closed: every step tool
/// becomes `UnknownTool` (callers signal "no snapshot" via the
/// `NoRegistrySnapshot` path before calling, OR by passing the empty
/// set — both reject).
pub fn evaluate_approval(
    template: &L3SkillCandidate,
    known_tools: &BTreeSet<String>,
) -> ApprovalDecision
```

The gate collects **all** reasons (no short-circuit) so the operator sees every problem in one pass:

1. **Structural re-validation.** Re-run `validate_l3_skill(template)`. The stored template *should* already be valid, but a row could have been hand-edited in SQL or written by an older validator. On `Err`, push `StructuralInvalid(msg)` and stop (the remaining checks assume a well-formed template).
2. **Secret-ref scan.** For every step, walk `step.parameters` and flag any string that begins with `secrets::REF_PREFIX` ("secret://"). A baked-in vault ref is *always* wrong in a reusable skill: refs are per-task and TTL-scoped, so the ref is dead on reuse, and surfacing one cross-task is a leak vector. Push `SecretRefPresent { step, found }` for each.
3. **Tool existence.** For every `step.tool ∉ known_tools`, push `UnknownTool { tool }`.

`Approve` iff no reasons; otherwise `Reject { reasons }`.

### Pure helpers (both unit-tested standalone)

```rust
/// Recursively collect every string leaf that begins with REF_PREFIX.
/// Mirrors the writer's `collect_placeholders` walker (objects + arrays;
/// not object keys).
fn scan_secret_refs(v: &serde_json::Value, out: &mut Vec<String>)

/// Extract the set of tool names from a `registry.loaded` audit payload
/// `{ "tools": [ {"name": "...", …}, … ] }`. Missing/!array/!object-rows
/// are skipped; a payload with no usable names yields an empty set
/// (which the CLI maps to `NoRegistrySnapshot`).
pub fn extract_tool_names(payload: &serde_json::Value) -> BTreeSet<String>
```

## The db helper

```rust
// db/src/memories/write.rs
/// Flip a layer-3 row's metadata `trust` field. Layer-guarded so an
/// L0/L1/L2 id (or a non-existent id) is a no-op. Returns true iff a
/// row was updated.
pub async fn set_skill_trust(
    pool: &PgPool,
    id: i64,
    trust: &str,
) -> Result<bool, DbError> {
    // UPDATE memories
    //   SET metadata = jsonb_set(metadata, '{trust}', to_jsonb($2::text), true)
    //   WHERE id = $1 AND layer = 3
    // → rows_affected() == 1
}
```

`jsonb_set` mutates only the `trust` key; `template`, `source`, `body_sha256`, `created_at`, `task_id` are untouched. No new migration (the `metadata` column already exists). An UPDATE does not fire the `deleted_memories` AFTER-DELETE trigger; the `l3.approved`/`l3.revoked` audit row is the trust-change journal.

## Audit-row contract

| Actor | Action | Payload keys | When |
|---|---|---|---|
| `cli` | `l3.approved` | `{memory_id, skill_name, body_sha256, tools:[…]}` | `memory l3 approve` — gate returns `Approve`; trust set to `user_approved`. `tools` is the template's distinct step tools. |
| `cli` | `l3.approve_rejected` | `{memory_id, skill_name?, body_sha256?, reasons:[…]}` | `memory l3 approve` — gate returns `Reject`; trust unchanged; CLI exits non-zero. `reasons` are rendered `RejectReason` strings. `skill_name`/`body_sha256` are absent if the row/template could not be parsed. |
| `cli` | `l3.revoked` | `{memory_id, updated}` | `memory l3 revoke` — trust set to `untrusted`, no gate. `updated` is the `set_skill_trust` bool. |

Auditing the **rejected** path is a deliberate departure from the writer's "validation failures don't audit" rule: that rule governed the *silent agent-raised* path; an operator explicitly attempting to approve a skill that carries a `secret://` ref is a security-relevant event and belongs in the trail. Payload builders are pure functions in `scheduler::audit`, unit-tested for shape.

## Data flow

```
operator: kastellan-cli memory l3 approve 42
  └─ db::memories::fetch_by_ids(pool, &[42]) → Memory (reject if layer != 3 / absent)
  └─ parse memory.metadata["template"] → L3SkillCandidate     (reject ⇒ l3.approve_rejected, StructuralInvalid)
  └─ SELECT payload FROM audit_log
       WHERE actor='core' AND action='registry.loaded'
       ORDER BY id DESC LIMIT 1                                (none ⇒ Reject{NoRegistrySnapshot})
  └─ extract_tool_names(payload) → known_tools
  └─ evaluate_approval(&template, &known_tools)                [PURE]
       ├─ Approve → db::set_skill_trust(42, "user_approved")
       │            cli_audit::…  audit cli/l3.approved {memory_id, skill_name, body_sha256, tools}
       │            print "approved skill '<name>' (#42)"      ; exit 0
       └─ Reject  → audit cli/l3.approve_rejected {memory_id, …, reasons}
                    print reasons to stderr                    ; exit 1
                    (trust UNCHANGED — no db write)

operator: kastellan-cli memory l3 revoke 42
  └─ db::set_skill_trust(42, "untrusted")  (no gate; downgrade is always safe)
  └─ cli_audit::…  audit cli/l3.revoked {memory_id, updated}
  └─ print result                                              ; exit 0

operator: kastellan-cli memory l3 list
  └─ list_l3(pool) → rows; render SkillTrust::from_metadata_str(metadata.trust)
```

## Files touched

NEW (2):
- `core/src/memory/l3_approval.rs` — `SkillTrust` + gate + helpers + module-internal unit tests.
- This spec + the plan that follows it.

MODIFIED (~8):
- `core/src/memory/mod.rs` — `pub mod l3_approval;`.
- `core/tests/cli_memory_l3_e2e.rs` — the existing CLI e2e file (from the writer slice) gains approve/revoke scenarios.
- `db/src/memories/write.rs` — `set_skill_trust` + (parent `memories.rs` re-export so `db::memories::set_skill_trust` resolves, matching the existing write-helper re-exports).
- `core/src/scheduler/audit.rs` — three action constants + payload builders + unit tests.
- `core/src/cli_audit.rs` — `l3_approve_and_audit`, `l3_revoke_and_audit`.
- `core/src/bin/kastellan-cli/memory_l3.rs` — `approve` + `revoke` subcommands; typed-trust in `list`.
- `core/src/bin/kastellan-cli/main.rs` — dispatch wiring for the two new subcommands (if the router is centralised there rather than in `memory_l3.rs`).
- `db/tests/postgres_e2e.rs` — `set_skill_trust` flip + layer-guard cases.

DOCS (2): `HANDOVER.md` + `ROADMAP.md` session-end update.

No new migration. No new `db/` table. The `metadata` JSONB column and `fetch_by_ids` already exist.

## Test budget

Estimate **+22 to +28**, workspace 1177 → ~1199–1205.

- ~10–13 unit (`l3_approval.rs::tests`): `SkillTrust` round-trip (each variant `as_str`↔`from_metadata_str`) + fail-safe unknown/empty → `Untrusted`; `scan_secret_refs` (finds nested in object + array, ignores plain strings, ignores a `secret://`-prefixed *object key*); `extract_tool_names` (happy, missing `tools`, non-array, entry without `name`, empty → empty set); `evaluate_approval` — clean Approve, single secret-ref Reject, multi-step secret-ref (one reason per occurrence), unknown-tool Reject, empty-`known_tools` ⇒ all tools unknown, structural-invalid short-circuits, **multi-reason accumulation** (secret ref + unknown tool together).
- ~3 unit (`scheduler::audit::tests`): the three payload builders' key-sets.
- ~2–3 db (`postgres_e2e`): `set_skill_trust` flips trust + returns `true`; layer-guard (an L1 id → `false`, row untouched); non-existent id → `false`.
- ~4–5 CLI e2e (`cli_memory_l3_e2e`): approve happy (seed L3 + a `registry.loaded` row naming the template's tool → trust becomes `user_approved` + one `l3.approved` row); approve rejected on a baked-in `secret://` ref (trust unchanged + `l3.approve_rejected` row + non-zero exit); approve fail-closed with no `registry.loaded` row (`NoRegistrySnapshot`, trust unchanged); revoke (user_approved → untrusted + `l3.revoked`); approve unknown-tool (registry snapshot omits the tool → reject).

## Risk surface

- **Snapshot staleness / drift.** The `registry.loaded` snapshot can lag the live registry (a tool added/removed since the last daemon boot). Mitigated: the snapshot check is defense-in-depth; invocation re-validates against the live registry (next slice). A stale snapshot can only cause a *false reject* (tool registered live but not in the snapshot) — fail-closed, never a false approve. Operators re-run after a daemon restart.
- **Approval ≠ safe-to-run.** Approving flips a metadata marker; it does not execute anything this slice. The marker's only future consumer (invocation) is gated behind its own live re-validation. No execution risk is introduced now.
- **Secret-ref scan completeness.** The scan matches the `secret://` literal prefix on string leaves. A secret *value* pasted as plaintext (not a ref) is undetectable here — same limitation the writer documented. The vault stores refs, not plaintext, so a well-behaved trajectory only ever carries refs; plaintext-secret detection remains a named, separate hardening item.
- **Re-approving an already-approved row.** `approve` re-runs the gate and re-sets `user_approved` (idempotent). No special-case; re-running the gate is harmless and reconfirms the skill still passes against the current snapshot.
- **No UNIQUE/locking on the trust flip.** Two concurrent `approve`/`revoke` on the same id race to a last-writer-wins trust value; both audit. Cost is a benign racing marker, resolved by re-running. Not worth a row lock for an operator-driven CLI.

## Open questions for the implementer

None blocking. The design commits on:
- `SkillTrust` as a hand-converted (not serde-derived) enum with a total, fail-safe `from_metadata_str` (unknown ⇒ `Untrusted`).
- The gate is **pure** and registry-parameterised; the CLI supplies `known_tools` from the latest `registry.loaded` snapshot and rejects fail-closed when none exists.
- Tool existence only (no method validation — the registry has no method index).
- `approve` + `revoke` commands; `Pinned` defined but command-less; no `pin`, no `add`, no execution.
- Rejected approvals **are** audited (`l3.approve_rejected`).
- db `set_skill_trust` takes a `&str` (layer-guarded, no `core` dependency leaking down).

If any of these turn out wrong during implementation, file the correction inline.

## Self-review checklist (done before commit)

- [x] No placeholders / TBD / TODO in body text.
- [x] Audit-row bump described concretely (three new actions, pure builders) and the rejected-path departure from the writer's no-audit rule is justified.
- [x] File-touch list cross-checked against the precedent (`l3_crystallise.rs`, `cli_audit::l3_remove_and_audit`, `bin/kastellan-cli/memory_l3.rs`, `audit.rs` `ACTION_ENTITIES_APPROVED` pair, `db/memories/write.rs` re-export pattern).
- [x] No new `db/` table or migration claimed — `metadata` column + `fetch_by_ids` confirmed already shipped.
- [x] Central tension (registry is daemon-only) stated, resolved (snapshot + fail-closed), and the resolution's safety (false-reject only, never false-approve; live re-check at invocation) is explicit.
- [x] Writer-only/gate-only boundary explicit: nothing executes; `UserApproved`/`Pinned` are inert this slice.
- [x] Scope check: ~22–28 tests + 1 new pure module (~250–350 LOC) + 1 db helper + 3 audit constants + 2 CLI subcommands is one session, sized like the writer slice.
- [x] Cross-references use the `path#Lline` clickable-link shape.
