# L0 seed data loader — startup-time meta-rule seeding

**Date:** 2026-05-16
**Status:** Design (pre-implementation, pre-plan)
**Branch (proposed):** `feat/l0-seed-loader`
**Off:** `main` at `305941a` (HEAD after PR #72 merge)
**Pre-req shipped:** Memory layer L1 slice (`b1c63e2`) — adds the
`layer SMALLINT` column and the `seed_meta_memory` admin function.

---

## Goal

Ship the loader that turns a hand-edited TOML file of meta-rules into
L0 (`MemoryLayer::Meta`) rows in the `memories` table, idempotently on
every daemon restart. Also ship the paired read-side helper
`load_l0_active(pool, cap_rows, cap_bytes)` so the future
prompt-assembler slice has a single source of truth for the "active L0
rule set" without re-deriving the dedup-by-rule-id rule from scratch.

The slice deliberately does not ship: prompt-assembler wiring, an L0
admin CLI, automatic file-change watching, embeddings on L0 rows, or
tag-based filtering. Those are later slices, each gated behind this
loader existing.

## Why now, why this small

The L1 slice (2026-05-15) landed the storage primitive but left two
follow-ups in its "Follow-ups this slice unlocks" section. L0 seeding
is the first of those follow-ups and the cheapest move that unblocks
the prompt-assembler slice — which is the first consumer of *both*
`load_l0_active` and `load_l1`. Shipping the loader + reader together
mirrors the L1 slice's "storage primitive shipped ahead of consumer"
posture: the next slice (prompt assembler) can compose
`load_l0_active` + `load_l1` without designing them.

L0 rules are agent safety constraints — e.g. "never `rm -rf`", "if you
just refused on constitutional grounds, do not re-enter the action
loop." They go into *every* system prompt at the highest priority, so
losing or duplicating them is more harmful than mis-recalling an
ordinary L2 fact. That posture justifies a fail-closed loader and an
append-only ledger discipline (matching `agent_prompts`).

## Locked design decisions

1. **Versioning model: insert-only ledger.** Each rule has a stable
   `id` in the source file. On daemon start, the loader computes
   `SHA256(body)` for each rule and queries
   `SELECT 1 FROM memories WHERE layer = 0
        AND metadata->>'l0_rule_id' = $1
        AND metadata->>'body_sha256' = $2 LIMIT 1`.
   If the row exists → skip (counter: `unchanged_skipped`). If it
   doesn't → call `seed_meta_memory(...)` with the rule body +
   metadata (counter: `new_rows_written`). Old versions of edited
   rules stay in the table forever for audit; the read-side helper
   dedups at query time.
   Rejected: diff-based replace (DELETE noise on rule removal);
   wipe-and-reseed (N audit rows on every restart even when nothing
   changed).

2. **Source format: single TOML file** at
   `seeds/memory/l0_meta_rules.toml` (default, overridable via
   `HHAGENT_L0_RULES_FILE`). Each rule is a `[[rule]]` table with
   `id`, `body`, and optional `tags`.
   Rejected: one-file-per-rule (high per-file overhead for short
   one-sentence rules); YAML (indentation traps on plain-text bodies);
   markdown frontmatter (parser cost without offsetting benefit at
   this scale).
   Adds `toml = "0.8"` direct dep to `core/Cargo.toml`
   (Apache-2.0 + MIT — AGPL-compatible). Already in the dep graph
   transitively; we declare it honestly.

3. **No embeddings on L0 rows.** L0 rules never go through semantic
   recall — they're pinned into every prompt unconditionally. Saving
   the embedding round-trip at seed time also avoids the failure mode
   "seed succeeded for some rules but embedding failed for others",
   which would leave a partial L0 set in the DB.

4. **Fail-closed on a malformed file, soft-skip on missing file.**
   If the env var is unset *and* the default path does not exist, the
   loader logs `info!("no L0 rules file found, skipping seed")` and
   returns an empty `L0SeedReport`. Daemon comes up. If the file
   exists but is malformed (TOML parse error, missing required field,
   duplicate `id`, oversized `body`), the loader returns `Err` and
   the daemon refuses to start — matches `probe::run` posture.
   Rationale: silently coming up with stale L0 rules is more
   dangerous than refusing to come up.

5. **Ship a starter TOML in-tree** at `seeds/memory/l0_meta_rules.toml`
   with two illustrative rules. Operator can edit / delete / add
   rules; the file is operator-owned thereafter. The starter rules
   are themselves defensible defaults (recursive-delete safety,
   refusal-stickiness), so a fresh install comes up with a non-empty
   L0 set.

## Source-file shape

The starter file shipped in-tree at `seeds/memory/l0_meta_rules.toml`:

```toml
# L0 meta-rules / hard constraints loaded into every system prompt at
# the highest priority. Edit + commit + daemon restart to update.
# The loader is idempotent on (rule_id, body_sha256); old versions of
# edited rules stay in the database for audit.
#
# Each rule needs:
#   id     stable identifier, [a-z0-9_]+, unique within this file
#   body   the rule text the agent reads; <= 1024 bytes, one sentence
#   tags   optional array of strings; not used at load time today,
#          reserved for future filtering

[[rule]]
id = "never_rm_rf"
body = "Never invoke 'rm -rf' or any equivalent recursive destructive command without explicit operator confirmation. If a task plan calls for one, stop and ask."
tags = ["safety", "filesystem"]

[[rule]]
id = "refusal_is_terminal"
body = "If the constitutional reviewer or your own plan emits a refusal, do not re-enter the action loop on the same task. Surface the refusal and stop."
tags = ["safety", "constitutional"]
```

### Validation rules (enforced by `parse_l0_rules`)

- `id` matches `^[a-z0-9_]+$`; non-empty; ≤ 64 bytes.
- `id` unique within the file (duplicate is a hard error, not a
  warning — the loader cannot pick a winner).
- `body` non-empty after trimming; ≤ 1024 bytes (UTF-8 byte count).
- `tags` optional; each tag is a non-empty UTF-8 string. An empty
  `tags` array is allowed.
- Unknown top-level keys (anything other than `rule`) → error
  (catches `[rules]` typos).
- Unknown keys inside a `[[rule]]` table → error (catches `tag` vs
  `tags` typos).

The 1024-byte body cap is deliberately generous — most rules will be
one sentence (<200 bytes). The cap is here to catch a future operator
pasting a paragraph of prose by accident.

## Storage shape (per rule)

Each rule maps to one row in `memories` via `seed_meta_memory`:

| Column        | Value                                            |
| ------------- | ------------------------------------------------ |
| `id`          | server-assigned bigserial                        |
| `body`        | the rule body verbatim                           |
| `metadata`    | `{"l0_rule_id": "<id>", "body_sha256": "<hex>", "tags": [...], "source_path": "seeds/memory/l0_meta_rules.toml"}` |
| `embedding`   | NULL                                             |
| `layer`       | 0 (`MemoryLayer::Meta`)                          |
| `created_at`  | server-assigned `now()`                          |

The `body_sha256` field is the operator-visible drift signal: if a
rule's body changes, the next daemon start inserts a *new* row
(carrying the new sha256), and the old row remains for audit.
`load_l0_active` returns the newest version per `l0_rule_id`.

`source_path` records which seed file the row came from. Today there
is one file; tomorrow there may be more (e.g. organisation-scoped
overlay files). The field is the future-proofing seam.

## Public surface

In `core/src/memory/l0_seed.rs` (new module):

```rust
/// A single parsed L0 rule, ready to seed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L0Rule {
    pub id: String,
    pub body: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Error)]
pub enum L0Error {
    #[error("toml parse error in {path}: {source}")]
    TomlParse { path: PathBuf, source: toml::de::Error },
    #[error("validation error in {path}: {detail}")]
    Validation { path: PathBuf, detail: String },
    #[error("io error reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("db error: {0}")]
    Db(#[from] hhagent_db::DbError),
}

/// Operator-visible summary of one seed run.
#[derive(Clone, Debug, Default)]
pub struct L0SeedReport {
    pub rules_loaded: usize,        // rules parsed from source
    pub new_rows_written: usize,    // (rule_id, sha256) not yet in DB
    pub unchanged_skipped: usize,   // (rule_id, sha256) already in DB
    pub source_path: PathBuf,       // file the rules came from
    pub source_sha256: String,      // SHA-256 of the file content
}

/// Pure: parse the TOML string into a validated rule list. The
/// `source_path` is only used for diagnostic message construction.
pub fn parse_l0_rules(
    source_path: &Path,
    toml_str: &str,
) -> Result<Vec<L0Rule>, L0Error>;

/// Async DB: seed the given rules into `memories`, idempotent on
/// `(l0_rule_id, body_sha256)`. Does NOT read the filesystem.
/// `source_path` is recorded into `metadata.source_path` per row and
/// returned in the report.
pub async fn seed_l0_from_rules(
    pool: &PgPool,
    source_path: &Path,
    source_sha256: &str,
    rules: &[L0Rule],
) -> Result<L0SeedReport, L0Error>;

/// Convenience: read + parse + seed. Reads `path` as UTF-8 TOML,
/// computes its SHA-256, parses, and delegates to `seed_l0_from_rules`.
pub async fn seed_l0_from_file(
    pool: &PgPool,
    path: &Path,
) -> Result<L0SeedReport, L0Error>;

/// L0_DEFAULT_CAP_ROWS = 64 (twice L1; L0 should still be tiny).
pub const L0_DEFAULT_CAP_ROWS: usize = 64;

/// L0_DEFAULT_CAP_BYTES = 8192 (~2K tokens; twice L1 since L0 is the
/// highest-priority always-pinned layer).
pub const L0_DEFAULT_CAP_BYTES: usize = 8192;

/// Read-side helper: returns the currently-active L0 rule set —
/// newest version per `l0_rule_id` — newest-first, bounded by the
/// two caps. The future prompt assembler is the intended consumer.
///
/// Oversize single body (`row.body.len() > cap_bytes`) is dropped
/// silently with `tracing::warn!` carrying the rule id, matching the
/// `load_l1` post-review precedent. `cap_rows = 0` or `cap_bytes = 0`
/// is a fast-path `Ok(vec![])`.
pub async fn load_l0_active(
    pool: &PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<Memory>, DbError>;

/// Convenience wrapper pinning the two default caps so a caller
/// cannot accidentally fat-finger `cap_rows = 0` or `cap_bytes = 0`.
pub async fn load_l0_active_default(
    pool: &PgPool,
) -> Result<Vec<Memory>, DbError>;
```

### Dedup query for `load_l0_active`

```sql
SELECT DISTINCT ON (metadata->>'l0_rule_id')
       id, body, metadata, layer, created_at
  FROM memories
 WHERE layer = 0
   AND metadata ? 'l0_rule_id'
 ORDER BY metadata->>'l0_rule_id', created_at DESC, id DESC
 LIMIT $1
```

(The `Memory` struct has no `embedding` field, so the SELECT
deliberately omits the column. Including `embedding::text` would
cost PG a pgvector→text encoding per L0 row for bytes that would
be discarded.)

`metadata ? 'l0_rule_id'` is the safety check against legacy L0 rows
written by a future hand-fixup (or test) that lacked the rule_id key
— they're excluded from the active set rather than crashing the
dedup. The two caps are then applied in Rust: outer wrapper enforces
`cap_bytes` after the SQL limit applies `cap_rows`, mirroring
`load_l1`.

The `LIMIT $1` uses the same `limit_as_i64` helper from
`db::memories` as `load_layer` so the saturating-cast behaviour is
consistent.

## Audit row on seed completion

After the loader returns `Ok`, `main.rs` writes one row:

```
actor   = "core"
action  = "l0.seeded"
payload = {
    "rules_loaded": <usize>,
    "new_rows_written": <usize>,
    "unchanged_skipped": <usize>,
    "source_path": "seeds/memory/l0_meta_rules.toml",
    "source_sha256": "<hex>"
}
```

One row per daemon startup. Operator-visible breadcrumb that the
loader ran and what it produced. The `source_sha256` lets an operator
compare two restarts and tell "loader ran against the same file" from
"loader ran against an edited file". Matches the `registry.loaded`
audit-row precedent from the tool-allowlist slice.

When the env var is unset and the default file is missing, no audit
row is written (the loader bailed before doing any work; nothing to
record).

## Wire-in in `core/src/main.rs`

Slotted right after `pool` is available, parallel to
`load_prompts_from_dir`, before lane runners spawn:

```rust
let l0_path = std::env::var("HHAGENT_L0_RULES_FILE")
    .map(PathBuf::from)
    .unwrap_or_else(|_| default_l0_rules_path()); // seeds/memory/l0_meta_rules.toml

if l0_path.exists() {
    let report = memory::l0_seed::seed_l0_from_file(&pool, &l0_path).await?;
    write_l0_seeded_audit_row(&pool, &report).await?;
    tracing::info!(
        rules = report.rules_loaded,
        new = report.new_rows_written,
        unchanged = report.unchanged_skipped,
        "L0 seed loader completed",
    );
} else {
    tracing::info!(path = ?l0_path, "no L0 rules file found, skipping seed");
}
```

The `default_l0_rules_path()` helper resolves
`seeds/memory/l0_meta_rules.toml` relative to the daemon's working
directory (matching the `HHAGENT_PROMPTS_DIR` pattern). Production
deployment via the existing supervisor spec already sets `WorkingDir`
to the install root.

## Tests (TDD ordered)

### Unit tests in `core/src/memory/l0_seed.rs::tests`

| # | Name | Asserts |
|---|------|---------|
| 1 | `parse_valid_minimal_one_rule` | one `[[rule]]` block with required fields → `Ok(vec![rule])` |
| 2 | `parse_valid_multi_rule_preserves_order` | rules returned in source-file order |
| 3 | `parse_rejects_missing_id` | `Err(Validation)` with diagnostic |
| 4 | `parse_rejects_missing_body` | ditto |
| 5 | `parse_rejects_empty_body` | trimmed-empty body is invalid |
| 6 | `parse_rejects_oversize_body` | 1025-byte body fails; 1024-byte body passes |
| 7 | `parse_rejects_duplicate_id` | two `[[rule]]` blocks with same `id` → Err |
| 8 | `parse_rejects_bad_id_charset` | `Id-With-Dashes` and `UPPER` and `with space` all rejected |
| 9 | `parse_rejects_unknown_top_level_key` | `[rules]` instead of `[[rule]]` → Err |
| 10 | `parse_rejects_unknown_rule_key` | `tag = [...]` (typo for `tags`) → Err |
| 11 | `parse_empty_file_is_ok` | empty input → `Ok(vec![])` |
| 12 | `parse_tags_optional_and_default_empty` | missing `tags` → empty vec |
| 13 | `build_l0_metadata_pins_key_set` | helper builds the metadata JSON with exactly the documented 4 keys |
| 14 | `compute_body_sha256_is_stable` | same body → same hex; differs on whitespace change |
| 15 | `l0_default_caps_pin` | `L0_DEFAULT_CAP_ROWS == 64`, `L0_DEFAULT_CAP_BYTES == 8192` |

### DB integration tests in `core/tests/memory_l0_seed_e2e.rs`

Per-test PG cluster via `hhagent_tests_common::bring_up_pg_cluster`,
same pattern as `memory_layers_e2e.rs`.

The wire-in helper that writes the `actor='core' action='l0.seeded'`
audit row lives in `core/src/main.rs` and is exercised end-to-end by
`supervisor_e2e` on daemon bring-up. We do not add a dedicated
shape-pin test for the audit row in this slice — the `audit::insert`
+ `truncate_payload` round-trip is already pinned in `db` integration
tests, and a per-test PG cluster spin-up just to assert one extra
audit row's keys would not earn its ~2 s cost.

| # | Name | Asserts |
|---|------|---------|
| 1 | `seed_from_rules_writes_new_rows` | 2 rules in fresh DB → `new_rows_written = 2`, `unchanged_skipped = 0`; both rows exist with `layer = 0` and the expected metadata keys |
| 2 | `seed_from_rules_is_idempotent_on_unchanged_input` | seed once (2 rules); seed same input again → `new_rows_written = 0`, `unchanged_skipped = 2`; exactly 2 rows in DB |
| 3 | `seed_from_rules_writes_new_row_on_edited_body` | seed once; edit one rule's body; re-seed → `new_rows_written = 1`, `unchanged_skipped = 1`; exactly 3 rows in DB (the edited rule has both old + new) |
| 4 | `seed_from_file_reads_parses_and_seeds` | write the starter TOML to a tempdir; call `seed_l0_from_file` → exactly 2 rules seeded |
| 5 | `seed_from_file_fails_closed_on_malformed_toml` | write a TOML file with a malformed `[[rule]]` → `seed_l0_from_file` returns `Err(TomlParse)`; no rows in DB |
| 6 | `load_l0_active_returns_newest_per_rule_id` | seed twice with an edited body; `load_l0_active(64, 8192)` returns 1 row (the edited rule), and the row body matches the new version |
| 7 | `load_l0_active_respects_cap_rows` | 3 rules seeded; `load_l0_active(2, 8192)` returns 2 |
| 8 | `load_l0_active_oversize_body_dropped_silently` | seed a rule with a 600-byte body and another with a 100-byte body; `load_l0_active(64, 500)` returns the 100-byte one (the 600-byte one is over the strict-`>`-cap-on-cumulative check) |
| 9 | `load_l0_active_excludes_legacy_l0_rows_without_rule_id` | manually `seed_meta_memory` a row with metadata `{}` (no `l0_rule_id`); `load_l0_active` returns 0 rows (the `metadata ? 'l0_rule_id'` clause filters it out) |

### What's NOT tested in this slice

- **Audit-row payload shape end-to-end** — covered indirectly by the
  existing `audit::insert` + `audit::truncate_payload` round-trip
  tests in `db`. A dedicated shape-pin would require its own per-test
  PG cluster spin-up for one extra row assertion; not worth it.
- **Daemon startup wire-in via `supervisor_e2e`** — out of scope.
  The supervisor smoke test already exercises bring-up; adding an L0
  seed assertion to it would couple two slices' test plans. If a
  future operator reports "L0 didn't load on startup", we add it then.
- **TOML escape sequences inside rule bodies** — `toml` crate covers
  this; we don't re-test the library.

Estimated test count delta: **+15 unit + +9 DB integration = +24**,
moving the workspace count 607 → 631.

## Implementation order (per-task TDD)

1. **Task 1.** Add `toml = "0.8"` to `core/Cargo.toml`. Sanity-build
   the workspace.
2. **Task 2.** Write the 15 unit tests in `l0_seed::tests` (RED —
   they fail with "module not found"). Implement `L0Rule`,
   `L0Error`, `parse_l0_rules`, `compute_body_sha256`,
   `build_l0_metadata`, `L0_DEFAULT_CAP_ROWS`, `L0_DEFAULT_CAP_BYTES`.
   Tests go GREEN.
3. **Task 3.** Write the 9 DB integration tests in
   `memory_l0_seed_e2e.rs` (RED — they fail with "no such function").
   Implement `seed_l0_from_rules`, `seed_l0_from_file`,
   `load_l0_active`, `load_l0_active_default`. Tests go GREEN.
4. **Task 4.** Wire `l0_seed::seed_l0_from_file` into
   `core/src/main.rs` after the prompts loader. Write the
   `actor='core' action='l0.seeded'` audit row. Default-path
   resolver `default_l0_rules_path()` (cwd-relative
   `seeds/memory/l0_meta_rules.toml`). Add the env-var override
   `HHAGENT_L0_RULES_FILE`.
5. **Task 5.** Ship the starter TOML at
   `seeds/memory/l0_meta_rules.toml` with the two example rules.
   Add a brief operator-facing README.md in `seeds/memory/`
   explaining the edit + restart cadence.
6. **Task 6.** Update HANDOVER.md + ROADMAP.md.

Each task is one commit. Workspace stays green between commits.

## Open questions / parking lot

- **Per-org overlay files** (e.g.
  `seeds/memory/l0_meta_rules.{site}.toml` merged into the base set).
  Not in scope. The `source_path` metadata field is the seam for it.
- **Tag-based filtering at load time** (only rules tagged
  `enabled-in-prod` go into the prompt). Not in scope. Tags are
  stored for future ops queries.
- **Hot-reload on file change** (inotify / FSEvents). Not in scope.
  Operator restarts the daemon to pick up edits — same cadence as
  `agent_prompts`.
- **L0 admin CLI** (`hhagent-cli l0 list/diff/lint`). Out of scope.
  The TOML file is the source of truth; if observation phase shows
  it's not enough, add later.
- **Length budget vs `L0_DEFAULT_CAP_BYTES`** — at 8 KiB the active
  L0 set could hold ~32 average rules. If real-world L0 sets push
  past that, either raise the cap or build the L1-promotion
  heuristic so noisy rules graduate downward. Re-evaluate after the
  prompt-assembler slice has a real token budget to respect.

## Follow-ups this slice unlocks (separate specs)

- **Prompt-assembler `llm_router::build_system_prompt`.** First
  joint consumer of `load_l0_active` + `load_l1`. Concatenates
  `[L0 rules]` + `[L1 index]` + `[task]` + `[recall(query)]`,
  enforces a global token cap by dropping in priority order
  L4 → L2 → L3 → L1 → L0. Pre-req: this slice (already on `main`
  for L1; this slice for L0).
- **L0 admin CLI** — `hhagent-cli l0 {list, diff <file>, lint <file>}`
  for operator workflows beyond "edit and restart". Pre-req: this
  slice. Filed if observation surfaces a need.
- **Per-org overlay files** — `seeds/memory/l0_meta_rules.toml` +
  `seeds/memory/l0_meta_rules.<site>.toml` merged at load time.
  Pre-req: this slice + a real second-site use case.
