# Per-tool argv allowlist hygiene — design

**Status:** approved, pending implementation
**Date:** 2026-05-14
**Branch (planned):** `feat/tool-allowlist-db`
**Closes:** HANDOVER "Per-tool argv allowlist hygiene" pickup
**Depends on:** Option L (`hhagent_runtime` role + GRANT shape), Option I (audit-log chokepoint), `cli_audit.rs` write-and-audit pattern (from `cancel_and_audit` / `submit_and_audit`).

## Problem

`hhagent-core::main::build_tool_registry()` reads the argv allowlist for the
`shell-exec` worker from `HHAGENT_SHELL_EXEC_ALLOWLIST` — a colon-separated
list of absolute paths. The deny-by-default posture (empty / unset → no programs
allowlisted) is correct, but the source-of-truth is a process env var:

- A host restart with a typo, an environment edit, or a unit-file change can
  silently widen the allowlist with no audit trail.
- There is no record in `audit_log` of when an entry was added, by whom, or
  what the previous allowlist looked like.
- Cross-restart drift ("the allowlist was different on yesterday's boot") is
  invisible.

Production deployment of `hhagent` requires the argv allowlist to be a
versioned, auditable source-of-truth that survives host restarts unchanged
unless explicitly mutated through an audited code path.

## Goals

1. The argv allowlist source-of-truth lives in Postgres, behind the existing
   `hhagent_runtime` GRANT shape. INSERT / DELETE on the allowlist table is
   the only path to widen or narrow it; every mutation writes one row in
   `audit_log`.
2. The daemon loads the allowlist at startup and emits one
   `actor='core' action='registry.loaded'` audit row carrying the SHA-256 of
   the loaded list, so cross-restart drift is visible at a glance even when
   the individual mutation rows are weeks old.
3. The CLI gains a `tools allowlist {add, remove, list}` subcommand surface
   that funnels every change through `cli_audit::*` and writes the matching
   `actor='cli'` row.
4. Test seam: `tests-common` exposes a `seed_tool_allowlist` helper for
   integration tests that already have a per-test PG cluster up; no extra
   bring-up cost.
5. Hard cutover. `HHAGENT_SHELL_EXEC_ALLOWLIST` is no longer read.

## Non-goals

- **Binary path migration.** `HHAGENT_SHELL_EXEC_BIN` stays as an env var.
  Binary paths are orthogonal to allowlist hygiene; one worker = one binary,
  and the binary is constrained by the project's build artifact set.
- **Multi-tenant / per-task allowlists.** The allowlist is host-global. A
  future per-task scoping is a separate feature; nothing in this design
  forecloses it.
- **Versioned snapshots.** The table is a current-truth view, not a
  history. The audit log is the history.
- **Backward-compatible env-var fallback.** No production deployment exists
  yet; no compat burden to carry.
- **Rotation / expiry.** Entries live until DELETEd.

## Design

### Section 1 — Schema & DB layer

**Migration `db/migrations/0009_tool_allowlists.sql`:**

```sql
CREATE TABLE tool_allowlists (
    tool       TEXT NOT NULL,
    argv0      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by TEXT NOT NULL,
    PRIMARY KEY (tool, argv0),
    CHECK (octet_length(tool) > 0),
    CHECK (octet_length(argv0) > 0 AND argv0 LIKE '/%')
);

GRANT SELECT, INSERT, DELETE ON tool_allowlists TO hhagent_runtime;
-- No UPDATE: changing an entry means DELETE + INSERT, preserving the
-- audit trail of both the old and new shapes.
```

Notes:

- The composite PK is sufficient for the lookup `SELECT argv0 FROM
  tool_allowlists WHERE tool = $1` (Postgres uses the PK index for the
  `tool` prefix). No extra index needed.
- The CHECK on `argv0 LIKE '/%'` is a structural pin at the SQL layer. The
  Rust-side `validate_argv0` is the user-facing validator (clearer error
  messages); the CHECK is the last line of defence against a malformed row
  reaching the table from a future caller that skipped the validator.
- `created_by` is a free-form text label (`"cli"`, `"core"`,
  `"<env-seed-bootstrap>"`, …). Not constrained at the SQL layer; the
  caller picks a string that an operator grepping `audit_log` will
  recognise.

**New module `db/src/tool_allowlists.rs` (~120 LOC + ~80 LOC tests):**

```rust
// Pure validators (no DB I/O).
pub fn validate_tool_name(name: &str) -> Result<(), ToolAllowlistError>;
pub fn validate_argv0(argv0: &str) -> Result<(), ToolAllowlistError>;

#[derive(thiserror::Error, Debug)]
pub enum ToolAllowlistError {
    #[error("tool name empty or invalid")]      InvalidToolName,
    #[error("argv0 empty or not absolute")]     InvalidArgv0,
    #[error("argv0 contains a NUL byte")]       Argv0HasNul,
    #[error("argv0 contains '..'")]             Argv0HasDotDot,
    #[error(transparent)]                       Db(#[from] sqlx::Error),
}

// Async I/O.
pub async fn add(
    pool: &PgPool, tool: &str, argv0: &str, created_by: &str,
) -> Result<bool, ToolAllowlistError>;   // true = INSERTed, false = already present

pub async fn remove(
    pool: &PgPool, tool: &str, argv0: &str,
) -> Result<bool, ToolAllowlistError>;   // true = a row was deleted

pub async fn list_for_tool(
    pool: &PgPool, tool: &str,
) -> Result<Vec<String>, ToolAllowlistError>;

#[derive(Debug, Clone)]
pub struct AllowlistEntry {
    pub tool: String, pub argv0: String,
    pub created_at: OffsetDateTime, pub created_by: String,
}

pub async fn list_all(
    pool: &PgPool,
) -> Result<Vec<AllowlistEntry>, ToolAllowlistError>;
```

Validation rules:

- `validate_tool_name`: non-empty, ASCII alphanumeric + `-`/`_`, ≤ 64 chars.
  Matches the same charset the existing `validate_service_name` /
  `slug_model` helpers accept; deliberately conservative so future tool names
  can flow through directly to log lines and audit payloads without escaping.
- `validate_argv0`: starts with `/`, non-empty, contains no NUL, contains no
  `..` segment (defends against the operator typing `/usr/bin/../bin/echo`).
  Note: we do **not** canonicalize via the filesystem — the worker spawns the
  binary itself, and `/usr/bin/../bin/echo` is intentionally rejected (a
  canonicalised entry is what the operator must add).

`add` and `remove` are infallible w.r.t. concurrent retries — `INSERT … ON
CONFLICT DO NOTHING` returns `false` on duplicate; `DELETE … RETURNING tool`
returns `Some(())` iff a row was deleted.

### Section 2 — Audit + CLI surface

**New action constants in `core/src/scheduler/audit.rs`:**

```rust
pub const ACTION_TOOLS_ALLOWLIST_ADD:    &str = "tools.allowlist.add";
pub const ACTION_TOOLS_ALLOWLIST_REMOVE: &str = "tools.allowlist.remove";
pub const ACTION_REGISTRY_LOADED:        &str = "registry.loaded";
```

Slot alphabetically. `ACTION_REGISTRY_LOADED` lives here too (single source
for every scheduler/core-emitted action string).

**Extended `core/src/cli_audit.rs`:**

```rust
pub async fn tools_allowlist_add_and_audit(
    pool: &PgPool, tool: &str, argv0: &str,
) -> Result<bool, ToolAllowlistError>;

pub async fn tools_allowlist_remove_and_audit(
    pool: &PgPool, tool: &str, argv0: &str,
) -> Result<bool, ToolAllowlistError>;
```

Both pass `created_by = CLI_AUDIT_ACTOR` (`"cli"`) to the DB layer. On a
successful mutation (`Ok(true)`), best-effort insert one
`actor='cli' action='tools.allowlist.<verb>'` row with payload
`{tool, argv0}`. On `Ok(false)` (idempotent no-op), do NOT write an audit
row — the operator's intent was to mutate, but nothing changed; an audit row
would confuse "what was the state at time T" reconstructions. The boolean
return tells the CLI whether to print `added` vs `already present`.

DB-error posture: same as `cancel_and_audit` / `submit_and_audit`. The audit
insert is best-effort (`tracing::warn!` on failure); the DB mutation's
success is the load-bearing signal.

**New `hhagent-cli tools allowlist` subcommands:**

```
hhagent-cli tools allowlist add <tool> <argv0>
hhagent-cli tools allowlist remove <tool> <argv0>
hhagent-cli tools allowlist list [--tool <name>]
```

- `add` exits 0 on success (whether INSERT or no-op) with a 1-line stdout
  message. Exit 2 on validation error.
- `remove` symmetrical.
- `list` prints a 4-column table (`TOOL  ARGV0  CREATED_AT  CREATED_BY`)
  ordered by `(tool, argv0)`. With `--tool`, filters to a single tool.
  Read-only — no audit row.

`tools` subcommand group sits next to the existing `tasks` group in
`hhagent-cli`; structure mirrors `hhagent-cli tasks list/status/cancel`.

### Section 3 — Daemon wiring

**`core/src/main.rs::build_tool_registry()` rewired:**

```rust
async fn build_tool_registry(pool: &PgPool) -> Result<ToolRegistry> {
    let mut reg = ToolRegistry::new();

    if let Some(bin_os) = std::env::var_os("HHAGENT_SHELL_EXEC_BIN") {
        let binary = PathBuf::from(&bin_os);
        if binary.is_file() {
            let allowlist =
                hhagent_db::tool_allowlists::list_for_tool(pool, "shell-exec")
                    .await
                    .context("loading shell-exec allowlist from DB")?;
            let entry = scheduler::shell_exec_entry(binary.clone(), &allowlist);
            info!(
                tool = "shell-exec", binary = %binary.display(),
                allowlist_len = allowlist.len(),
                "registering tool"
            );
            reg.insert("shell-exec", entry);
            // best-effort audit row; do not fail bring-up on this insert
            let _ = write_registry_loaded_row(pool, &[
                LoadedToolRecord {
                    name: "shell-exec",
                    binary: binary.display().to_string(),
                    allowlist_len: allowlist.len(),
                    allowlist_sha256: sha256_argv0_list(&allowlist),
                },
            ]).await;
        } else {
            tracing::warn!(
                binary = %binary.display(),
                "HHAGENT_SHELL_EXEC_BIN does not point to an existing file; \
                 shell-exec NOT registered"
            );
        }
    }

    // Deprecation warning — does not block bring-up.
    if std::env::var_os("HHAGENT_SHELL_EXEC_ALLOWLIST").is_some() {
        tracing::warn!(
            "HHAGENT_SHELL_EXEC_ALLOWLIST is no longer honored; \
             use 'hhagent-cli tools allowlist add' to populate the DB"
        );
    }

    Ok(reg)
}
```

- Becomes `async`; returns `Result`. `bring_up` in `main.rs` awaits it after
  `bring_up_database` succeeds (pool already available).
- Fail-closed on DB error: the daemon should not run with a half-loaded
  registry. The supervisor sees the non-zero exit.
- `write_registry_loaded_row(pool, tools)` writes one
  `actor='core' action='registry.loaded'` row with payload:

  ```json
  {
    "tools": [
      {"name": "shell-exec", "binary": "...", "allowlist_len": 3,
       "allowlist_sha256": "<64-hex>"}
    ]
  }
  ```

  `allowlist_sha256` is the SHA-256 of the canonical-form list:
  `argv0_1 || '\n' || argv0_2 || '\n' || …` (lexicographically sorted,
  one entry per line, terminating newline after each entry including the
  last). Same byte sequence will hash the same regardless of insertion
  order, so cross-restart comparison is meaningful. Empty list → SHA-256
  of the empty string (`e3b0c442…`), wire-distinguishable from "tool not
  registered" which produces no entry in the `tools` array at all.
- `LoadedToolRecord`, `write_registry_loaded_row`, and `sha256_argv0_list`
  are module-private helpers inside `core/src/main.rs`. No public surface.
- This row is **best-effort** — if Postgres is gone, the registry-loaded
  row write fails but the daemon still starts (we already loaded the
  allowlist successfully; the failure to record the snapshot is not
  fatal). Same posture as the chokepoint.

### Section 4 — Test plan

**Unit tests in `db/src/tool_allowlists.rs`:**

1. `validate_tool_name_accepts_shell_exec_and_shell_underscore`
2. `validate_tool_name_rejects_empty_and_oversize_and_invalid_chars`
3. `validate_argv0_accepts_typical_absolute_paths`
4. `validate_argv0_rejects_relative_paths`
5. `validate_argv0_rejects_nul_byte`
6. `validate_argv0_rejects_dotdot_segment`

**Integration test `db/tests/postgres_e2e.rs::tool_allowlists_round_trip_and_grant_shape`:**

1. `add(pool, "shell-exec", "/usr/bin/echo", "test")` returns `Ok(true)`.
2. Re-`add` of the same `(tool, argv0)` returns `Ok(false)` (idempotent).
3. `list_for_tool(pool, "shell-exec")` returns exactly `["/usr/bin/echo"]`.
4. `add` of a second `argv0` extends the list to 2 entries.
5. `list_all` returns 2 entries with `created_by="test"`.
6. `remove(pool, "shell-exec", "/usr/bin/echo")` returns `Ok(true)`.
7. Re-`remove` returns `Ok(false)`.
8. `SET ROLE hhagent_runtime; UPDATE tool_allowlists SET argv0='/x' WHERE
   tool='shell-exec'` returns `permission denied` (the structural pin for
   the missing UPDATE grant).
9. Inserting a row with a relative `argv0` via raw SQL fails the CHECK
   constraint with `check_violation`.

**New `tests-common::seed_tool_allowlist(pool, tool, argv0_list)` helper:**

```rust
pub async fn seed_tool_allowlist(
    pool: &sqlx::PgPool, tool: &str, argv0s: &[&str],
) -> anyhow::Result<()>;
```

Bulk-INSERT with `created_by="test"`. Used by every e2e test that
previously injected `HHAGENT_SHELL_EXEC_ALLOWLIST` into the `ServiceSpec.env`.

**Migration of existing tests:**

- `core/tests/cli_ask_e2e.rs`: drop `HHAGENT_SHELL_EXEC_ALLOWLIST` from the
  `ServiceSpec.env` push; call `seed_tool_allowlist(&pool, "shell-exec",
  &[ECHO_PATH])` between PG bring-up and daemon start. Happy-path seeds
  echo; failure-path seeds the empty list (so `/bin/cat` is POLICY_DENIED).
  Audit multiset bumped by `+1 core/registry.loaded` row on the happy and
  failure paths.
- `core/tests/scheduler_step_dispatch_e2e.rs`: no change. The test builds
  `ToolRegistry` directly via `shell_exec_entry()`; it never invokes
  `build_tool_registry`. The DB layer is not in its critical path.
- `core/tests/supervisor_e2e.rs`: no change. Daemon comes up with no
  registered tools (env var absent); the existing `core/startup` audit row
  assertion is sufficient.
- `core/tests/observation_capture.rs`: would need a seed call before the
  daemon starts. The orchestrator is `#[ignore]`-flagged; operator runs it
  once after seeding the allowlist via the CLI.

**New integration test `core/tests/cli_tools_allowlist_e2e.rs` (~250 LOC):**

Subprocess-level pin for the CLI surface. Brings up PG; runs `hhagent-cli
tools allowlist add/remove/list` as subprocesses; asserts:

1. `add shell-exec /usr/bin/echo` exits 0, prints `added`, DB has one row,
   one `cli/tools.allowlist.add` audit row landed with payload
   `{tool: "shell-exec", argv0: "/usr/bin/echo"}`.
2. Re-`add` exits 0, prints `already present`, DB still has one row, no
   new audit row.
3. `list --tool shell-exec` prints a 4-column table with one data row.
4. `remove shell-exec /usr/bin/echo` exits 0, prints `removed`, DB is
   empty, one `cli/tools.allowlist.remove` row landed.
5. Re-`remove` exits 0, prints `not present`, no new audit row.
6. `add shell-exec echo` (relative argv0) exits 2 with stderr matching
   "absolute path".
7. Audit multiset over the run: exactly `{cli/tools.allowlist.add ×1,
   cli/tools.allowlist.remove ×1}` (the validation-error case writes
   nothing).

## TDD ordering (per CLAUDE.md rule #2)

1. Land the migration first — `db/migrations/0009_tool_allowlists.sql`.
2. Land the validators with their unit tests RED → GREEN.
3. Land the DB layer (`add`/`remove`/`list_for_tool`/`list_all`) with the
   integration test RED → GREEN.
4. Land the `cli_audit` extensions with no tests of their own (covered by
   the CLI e2e).
5. Land the CLI subcommands; new `cli_tools_allowlist_e2e` RED → GREEN.
6. Rewire `build_tool_registry` to be async + DB-backed; migrate
   `cli_ask_e2e` to use the seed helper; both RED → GREEN.
7. Add the deprecation warning at `main.rs` start.

## Open follow-ups (filed but not in this slice)

- Binary-path source-of-truth: `HHAGENT_SHELL_EXEC_BIN` could move to a
  `tools(name, binary)` table for full hygiene. Deferred: orthogonal to
  the allowlist concern, and would only matter when a second tool exists.
- Per-task allowlist scoping: `tool_allowlists` is host-global today. A
  future column `scope TEXT NOT NULL DEFAULT 'host'` and matching CLI
  flag would allow per-task narrowing.
- TOML config bootstrap: `hhagent-cli tools allowlist import <file>`
  reading a TOML/JSON seed for ops repeatability. Easy to add when an
  operator workflow surfaces the need.
- Audit-driven rollback: a CLI subcommand `tools allowlist replay
  --since <ts>` that lists every `tools.allowlist.*` row in audit order.
  Deferred until forensics demand it.

## Files touched (planned)

- NEW `db/migrations/0009_tool_allowlists.sql`
- NEW `db/src/tool_allowlists.rs` (~200 LOC incl. tests)
- `db/src/lib.rs` — `pub mod tool_allowlists;`
- `db/tests/postgres_e2e.rs` — new `tool_allowlists_round_trip_and_grant_shape` test
- `core/src/scheduler/audit.rs` — 3 new action constants
- `core/src/cli_audit.rs` — two new helpers
- `core/src/main.rs` — `build_tool_registry` async + DB-backed,
  `registry.loaded` audit row, deprecation warning
- `core/src/bin/hhagent-cli.rs` — new `tools allowlist` subcommand tree
- NEW `core/tests/cli_tools_allowlist_e2e.rs` (~250 LOC)
- `core/tests/cli_ask_e2e.rs` — drop env var, add seed call, bump multiset
- NEW `tests-common/src/allowlist.rs` carrying `seed_tool_allowlist`;
  `tests-common/src/lib.rs` updated with `pub mod allowlist;` + root
  re-export, matching the existing per-concern file pattern
  (`skip.rs`, `guards.rs`, `pg.rs`, etc.)

## Test count delta (projected)

387 → ~399 (+~12: 6 unit validators, 1 DB integration, 4–5 in the new CLI
e2e, +1 for the deprecation-warning unit test). Existing `cli_ask_e2e`
gains a `registry.loaded` row assertion but no new `#[test]` functions.
