# Tool Allowlist Hygiene Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the per-tool argv allowlist source-of-truth from the
`KASTELLAN_SHELL_EXEC_ALLOWLIST` env var to a dedicated Postgres table
behind the existing `kastellan_runtime` GRANT shape, with every mutation
written to `audit_log`.

**Architecture:** New table `tool_allowlists(tool, argv0, …)` with PK
on `(tool, argv0)` and a GRANT shape of SELECT/INSERT/DELETE (no UPDATE).
A new `db::tool_allowlists` module provides pure validators + async I/O.
`core::cli_audit` gains write-and-audit helpers. `kastellan-cli` gains a
`tools allowlist {add, remove, list}` subcommand tree. `build_tool_registry`
becomes async, queries the table at startup, and emits one
`actor='core' action='registry.loaded'` audit row with the SHA-256 of the
canonical-form list.

**Tech Stack:** Rust, sqlx (Postgres, runtime-tokio), `time` for
TIMESTAMPTZ, existing `kastellan-protocol` JSON-RPC, existing
`kastellan-tests-common` PgCluster harness.

**Spec:** [docs/superpowers/specs/2026-05-14-tool-allowlist-hygiene-design.md](../specs/2026-05-14-tool-allowlist-hygiene-design.md).

---

## Pre-flight

- Branch: create `feat/tool-allowlist-db` off the current `main` tip
  (`97fdf04`). Do all work on the branch; do not merge to `main`.
- Working dir: `/home/hherb/src/kastellan`. Source the cargo env once per
  shell: `source "$HOME/.cargo/env"`.
- Baseline: workspace test count is **387**. Each task that adds tests
  records the new count.

```bash
source "$HOME/.cargo/env"
git checkout -b feat/tool-allowlist-db
cargo test --workspace 2>&1 | tail -3   # expected: "test result: ok. 387 passed"
```

---

### Task 1: Migration `0009_tool_allowlists.sql`

**Files:**
- Create: `db/migrations/0009_tool_allowlists.sql`

**Rationale:** New table with a composite PK on `(tool, argv0)`,
GRANT SELECT/INSERT/DELETE to `kastellan_runtime` (no UPDATE), CHECK
constraints at the SQL layer as the structural last line of defence.
No new index — the PK covers `WHERE tool = $1`.

- [ ] **Step 1: Write the migration**

Create `db/migrations/0009_tool_allowlists.sql`:

```sql
-- Phase 1 — per-tool argv allowlist hygiene.
--
-- Source-of-truth for which absolute `argv[0]` paths each registered
-- tool worker may exec. Replaces the previous `KASTELLAN_SHELL_EXEC_ALLOWLIST`
-- env var: env-var-driven means a host restart with a typo can silently
-- widen the allowlist with no audit trail. With this table, every change
-- writes one row in `audit_log` via the chokepoint in `core::cli_audit`.
--
-- Why composite-PK on `(tool, argv0)`:
--   * Natural "one row per allowlisted path per tool" shape
--   * PK index serves the registry-build read `WHERE tool = $1`
--   * Idempotent semantics via `INSERT … ON CONFLICT DO NOTHING`
--   * Per-entry audit rows (one row per add/remove) rather than
--     whole-list replacement diffs
--
-- GRANT shape: SELECT/INSERT/DELETE for kastellan_runtime, deliberately
-- NO UPDATE. Changing an entry means DELETE + INSERT, preserving the
-- audit trail of both the old and new shapes. Mirrors audit_log's
-- append-only discipline from migration 0002, but applied as
-- "no-update" rather than "no-update-no-delete" — operators must be
-- able to retire allowlist entries.

CREATE TABLE tool_allowlists (
    tool       TEXT NOT NULL,
    argv0      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by TEXT NOT NULL,
    PRIMARY KEY (tool, argv0),
    CHECK (octet_length(tool) > 0),
    CHECK (octet_length(argv0) > 0 AND argv0 LIKE '/%')
);

GRANT SELECT, INSERT, DELETE ON tool_allowlists TO kastellan_runtime;
```

- [ ] **Step 2: Verify migration applies cleanly**

Run the existing PG-touching integration test that runs all migrations:

```bash
cargo test -p kastellan-db --test postgres_e2e \
  postgres_install_start_select_one_uninstall -- --nocapture 2>&1 | tail -5
```

Expected: `test result: ok. 1 passed`. (If PG isn't on the host the
test prints `[SKIP]` and passes as no-op — that's fine for the local
check; CI will run the real path.)

- [ ] **Step 3: Run workspace tests; expect still-green**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 387 passed` (no new tests yet, just a
migration that applies to fresh per-test clusters).

- [ ] **Step 4: Commit**

```bash
git add db/migrations/0009_tool_allowlists.sql
git commit -m "$(cat <<'EOF'
db: add 0009_tool_allowlists migration

New table for the per-tool argv allowlist source-of-truth. Composite
PK on (tool, argv0); GRANT SELECT/INSERT/DELETE to kastellan_runtime
(no UPDATE — changes are DELETE + INSERT to preserve audit trail).
CHECK constraints pin: non-empty tool name; non-empty argv0 starting
with `/`. Follow-up commits land the Rust layer + CLI subcommands.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `db::tool_allowlists` validators (RED → GREEN)

**Files:**
- Create: `db/src/tool_allowlists.rs`
- Modify: `db/src/lib.rs:30-38` (module list — add `pub mod tool_allowlists;` alphabetically)

**Rationale:** Pure validators first. No DB I/O. The validators are
the API surface every caller (CLI, future programmatic callers) trusts;
they give clear error messages where Postgres' `check_violation` would
give a cryptic SQLSTATE.

- [ ] **Step 1: Create the module skeleton with the error enum**

Create `db/src/tool_allowlists.rs`:

```rust
//! Per-tool argv allowlist storage and validators.
//!
//! The `tool_allowlists` table (migration `0009_tool_allowlists.sql`) is
//! the source-of-truth for which absolute `argv[0]` paths each registered
//! tool worker may exec. Replaces the previous
//! `KASTELLAN_SHELL_EXEC_ALLOWLIST` env-var-driven shape.
//!
//! Validators here are the user-facing gate — they produce typed errors
//! that surface as readable CLI messages. The SQL-layer CHECK constraints
//! on the table are the last-line-of-defence pin (a future caller that
//! bypassed these validators would still get rejected by Postgres).

use sqlx::PgPool;
use time::OffsetDateTime;

/// Maximum length (UTF-8 bytes) for a tool name. 64 bytes is generous
/// for the foreseeable shape of worker names (`shell-exec`, `web-fetch`,
/// `python-exec`, …) and bounds the size of audit-row payloads.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Errors that can come out of this module.
#[derive(thiserror::Error, Debug)]
pub enum ToolAllowlistError {
    #[error("tool name empty or invalid; expected ASCII alphanumeric plus '-' or '_', max {MAX_TOOL_NAME_LEN} bytes")]
    InvalidToolName,

    #[error("argv0 must be a non-empty absolute path (starting with '/')")]
    InvalidArgv0,

    #[error("argv0 contains a NUL byte")]
    Argv0HasNul,

    #[error("argv0 contains a '..' segment; pass a canonicalised path")]
    Argv0HasDotDot,

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One row in `tool_allowlists`. Returned by [`list_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowlistEntry {
    pub tool: String,
    pub argv0: String,
    pub created_at: OffsetDateTime,
    pub created_by: String,
}

/// Validate a tool name. Accepts ASCII alphanumeric plus `-` and `_`,
/// non-empty, ≤ [`MAX_TOOL_NAME_LEN`] bytes. The charset matches the
/// conservative shape used by [`crate::secrets::validate_name`] and
/// the supervisor's service-name validators — names flow through to
/// log lines and audit payloads without escaping.
pub fn validate_tool_name(name: &str) -> Result<(), ToolAllowlistError> {
    if name.is_empty() || name.len() > MAX_TOOL_NAME_LEN {
        return Err(ToolAllowlistError::InvalidToolName);
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ToolAllowlistError::InvalidToolName);
    }
    Ok(())
}

/// Validate an argv0. Must be a non-empty absolute path, contain no
/// NUL byte, and contain no `..` path segment. The filesystem is NOT
/// consulted — operators add canonicalised paths explicitly; the worker
/// itself does the exec.
pub fn validate_argv0(argv0: &str) -> Result<(), ToolAllowlistError> {
    if argv0.is_empty() || !argv0.starts_with('/') {
        return Err(ToolAllowlistError::InvalidArgv0);
    }
    if argv0.contains('\0') {
        return Err(ToolAllowlistError::Argv0HasNul);
    }
    // A literal ".." anywhere as a path *segment* (between '/'s or at
    // an end). Reject `/usr/bin/../bin/echo` but allow `/usr/bin/foo..bar`
    // (no separator on either side of the dotdot).
    for seg in argv0.split('/') {
        if seg == ".." {
            return Err(ToolAllowlistError::Argv0HasDotDot);
        }
    }
    Ok(())
}

// --- I/O layer (filled in by Task 3) ----------------------------------

/// Add one allowlist entry. Idempotent — returns `Ok(true)` if a row
/// was INSERTed, `Ok(false)` if the entry was already present.
pub async fn add(
    _pool: &PgPool,
    _tool: &str,
    _argv0: &str,
    _created_by: &str,
) -> Result<bool, ToolAllowlistError> {
    unimplemented!("Task 3 implements this")
}

/// Remove one allowlist entry. Idempotent — returns `Ok(true)` if a
/// row was deleted, `Ok(false)` if nothing matched.
pub async fn remove(
    _pool: &PgPool,
    _tool: &str,
    _argv0: &str,
) -> Result<bool, ToolAllowlistError> {
    unimplemented!("Task 3 implements this")
}

/// List the argv0 entries for one tool, ordered by argv0 ascending.
pub async fn list_for_tool(
    _pool: &PgPool,
    _tool: &str,
) -> Result<Vec<String>, ToolAllowlistError> {
    unimplemented!("Task 3 implements this")
}

/// List every entry across every tool, ordered by `(tool, argv0)`.
pub async fn list_all(_pool: &PgPool) -> Result<Vec<AllowlistEntry>, ToolAllowlistError> {
    unimplemented!("Task 3 implements this")
}

// --- Tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Filled in by Step 2.
}
```

- [ ] **Step 2: Write 6 unit tests against the validators (RED)**

Replace the empty `mod tests` block in `db/src/tool_allowlists.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_tool_name_accepts_canonical_shapes() {
        validate_tool_name("shell-exec").unwrap();
        validate_tool_name("shell_exec_v2").unwrap();
        validate_tool_name("web-fetch").unwrap();
        validate_tool_name("a").unwrap();
        validate_tool_name("ABC123").unwrap();
    }

    #[test]
    fn validate_tool_name_rejects_empty_and_oversize_and_invalid_chars() {
        assert!(matches!(
            validate_tool_name(""),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        let too_long: String = "a".repeat(MAX_TOOL_NAME_LEN + 1);
        assert!(matches!(
            validate_tool_name(&too_long),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        assert!(matches!(
            validate_tool_name("shell exec"),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        assert!(matches!(
            validate_tool_name("shell/exec"),
            Err(ToolAllowlistError::InvalidToolName)
        ));
        assert!(matches!(
            validate_tool_name("shell.exec"),
            Err(ToolAllowlistError::InvalidToolName)
        ));
    }

    #[test]
    fn validate_argv0_accepts_typical_absolute_paths() {
        validate_argv0("/usr/bin/echo").unwrap();
        validate_argv0("/bin/sh").unwrap();
        validate_argv0("/opt/kastellan/bin/web-fetch-worker").unwrap();
        validate_argv0("/").unwrap(); // odd but technically absolute
    }

    #[test]
    fn validate_argv0_rejects_relative_paths() {
        assert!(matches!(
            validate_argv0(""),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
        assert!(matches!(
            validate_argv0("echo"),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
        assert!(matches!(
            validate_argv0("./echo"),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
        assert!(matches!(
            validate_argv0("usr/bin/echo"),
            Err(ToolAllowlistError::InvalidArgv0)
        ));
    }

    #[test]
    fn validate_argv0_rejects_nul_byte() {
        assert!(matches!(
            validate_argv0("/usr/bin/echo\0"),
            Err(ToolAllowlistError::Argv0HasNul)
        ));
        assert!(matches!(
            validate_argv0("/usr/\0/echo"),
            Err(ToolAllowlistError::Argv0HasNul)
        ));
    }

    #[test]
    fn validate_argv0_rejects_dotdot_segment_but_accepts_dotdot_within_segment() {
        assert!(matches!(
            validate_argv0("/usr/bin/../bin/echo"),
            Err(ToolAllowlistError::Argv0HasDotDot)
        ));
        assert!(matches!(
            validate_argv0("/.."),
            Err(ToolAllowlistError::Argv0HasDotDot)
        ));
        // `..` *inside* a segment (no slash on either side) is fine —
        // it's a legal filename character.
        validate_argv0("/usr/bin/foo..bar").unwrap();
    }
}
```

- [ ] **Step 3: Wire the module into `db/src/lib.rs`**

Modify `db/src/lib.rs` (around line 30-38, the module list). Insert
`pub mod tool_allowlists;` alphabetically — between `tasks` and any
later module (currently after `tasks`).

Read first:

```bash
sed -n '30,42p' db/src/lib.rs
```

Then insert. The module list should end up as:

```rust
pub mod agent_prompts;
pub mod audit;
pub mod conn;
pub mod graph;
pub mod memories;
pub mod pool;
pub mod probe;
pub mod secrets;
pub mod tasks;
pub mod tool_allowlists;
```

(`tool_allowlists` slots after `tasks` alphabetically.)

- [ ] **Step 4: Run tests, expect RED on the I/O stubs but GREEN on validators**

```bash
cargo test -p kastellan-db --lib tool_allowlists -- --nocapture 2>&1 | tail -10
```

Expected: 6 unit tests pass; the build itself compiles cleanly even
though the I/O functions are `unimplemented!()` — they don't get
called at test-time.

If validators fail, fix them and re-run. The expected end state is
**6 passed**.

- [ ] **Step 5: Run workspace build to ensure no regression**

```bash
cargo build --workspace 2>&1 | tail -3
```

Expected: clean build, no warnings.

- [ ] **Step 6: Commit**

```bash
git add db/src/tool_allowlists.rs db/src/lib.rs
git commit -m "$(cat <<'EOF'
db(tool_allowlists): module skeleton + pure validators

New module kastellan_db::tool_allowlists. Public surface: ToolAllowlistError
enum, AllowlistEntry struct, MAX_TOOL_NAME_LEN const, pure validate_tool_name
and validate_argv0 helpers. Async I/O functions (add/remove/list_for_tool/
list_all) are unimplemented!() stubs filled in by the next commit. 6 unit
tests pin the validators.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `db::tool_allowlists` I/O layer (TDD via integration test)

**Files:**
- Modify: `db/src/tool_allowlists.rs` (replace the four `unimplemented!()` stubs)
- Modify: `db/tests/postgres_e2e.rs` (add `tool_allowlists_round_trip_and_grant_shape` integration test)

**Rationale:** The I/O layer is straightforward sqlx, but the GRANT shape
and CHECK constraint enforcement are best pinned end-to-end against a real
cluster. The existing `tests-common::bring_up_pg_cluster` recipe handles
the cluster boilerplate.

- [ ] **Step 1: Write the integration test (RED)**

Read the current end of `db/tests/postgres_e2e.rs` first:

```bash
tail -20 db/tests/postgres_e2e.rs
```

Append a new test at the bottom. The test brings up its own PG cluster
(via `tests-common`), runs `probe::run` to apply migrations, then
exercises `add`/`remove`/`list_for_tool`/`list_all` + the GRANT/CHECK
shape.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_allowlists_round_trip_and_grant_shape() {
    use kastellan_db::pool::connect_runtime_pool;
    use kastellan_db::probe::run as probe_run;
    use kastellan_db::tool_allowlists::{
        add, list_all, list_for_tool, remove, AllowlistEntry, ToolAllowlistError,
    };
    use kastellan_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor};

    if !skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "tool-allowlists-e2e",
        "tool-allowlists-e2e-log",
        "kastellan-postgres-tool-allowlists-e2e",
    )
    .await
    .expect("bring up PG cluster");

    probe_run(&cluster.conn_spec).await.expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // (1) Idempotent add.
    let inserted = add(&pool, "shell-exec", "/usr/bin/echo", "test").await.unwrap();
    assert!(inserted, "first add must INSERT");
    let inserted2 = add(&pool, "shell-exec", "/usr/bin/echo", "test").await.unwrap();
    assert!(!inserted2, "duplicate add must be a no-op");

    // (2) list_for_tool returns one entry.
    let v = list_for_tool(&pool, "shell-exec").await.unwrap();
    assert_eq!(v, vec!["/usr/bin/echo".to_string()]);

    // (3) A second entry under the same tool.
    let inserted3 = add(&pool, "shell-exec", "/bin/sh", "test").await.unwrap();
    assert!(inserted3);
    let v2 = list_for_tool(&pool, "shell-exec").await.unwrap();
    assert_eq!(v2, vec!["/bin/sh".to_string(), "/usr/bin/echo".to_string()],
        "list_for_tool must order argv0 ascending");

    // (4) list_all surfaces metadata.
    let all: Vec<AllowlistEntry> = list_all(&pool).await.unwrap();
    assert_eq!(all.len(), 2);
    for row in &all {
        assert_eq!(row.tool, "shell-exec");
        assert_eq!(row.created_by, "test");
    }

    // (5) Idempotent remove.
    let removed = remove(&pool, "shell-exec", "/usr/bin/echo").await.unwrap();
    assert!(removed);
    let removed2 = remove(&pool, "shell-exec", "/usr/bin/echo").await.unwrap();
    assert!(!removed2, "second remove must be a no-op");

    // (6) GRANT shape: UPDATE on tool_allowlists denied to kastellan_runtime.
    // SET ROLE explicitly in the same transaction so the test isn't
    // sensitive to pool reuse.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET ROLE kastellan_runtime")
        .execute(&mut *conn)
        .await
        .unwrap();
    let update_res = sqlx::query("UPDATE tool_allowlists SET argv0 = '/x' WHERE tool = 'shell-exec'")
        .execute(&mut *conn)
        .await;
    let err = update_res.expect_err("UPDATE on tool_allowlists must be denied");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("permission denied") || msg.contains("denied for table"),
        "unexpected error message: {msg}"
    );
    drop(conn);

    // (7) CHECK constraint: relative argv0 rejected by Postgres even
    // when the Rust validator is bypassed.
    let bad = sqlx::query("INSERT INTO tool_allowlists (tool, argv0, created_by) VALUES ('shell-exec', 'echo', 'test')")
        .execute(&pool)
        .await;
    let bad_err = bad.expect_err("relative argv0 must be CHECK-rejected");
    let bad_msg = bad_err.to_string().to_lowercase();
    assert!(
        bad_msg.contains("check") || bad_msg.contains("violates"),
        "unexpected error: {bad_msg}"
    );

    // Suppress unused-variable warning if ToolAllowlistError isn't used
    // explicitly elsewhere in the test.
    let _: ToolAllowlistError = ToolAllowlistError::InvalidArgv0;

    drop(pool);
    drop(cluster);
}
```

- [ ] **Step 2: Run integration test, expect compile-success / runtime-failure (RED)**

```bash
cargo test -p kastellan-db --test postgres_e2e \
  tool_allowlists_round_trip_and_grant_shape -- --nocapture 2>&1 | tail -15
```

Expected: build OK; the test panics on the first `add` call because
`add()` is `unimplemented!()`. If PG isn't available on the host the
test will `[SKIP]` — that's fine for local; CI will run the real path.

- [ ] **Step 3: Implement the I/O layer**

Replace the four `unimplemented!()` stubs in `db/src/tool_allowlists.rs`:

```rust
pub async fn add(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
    created_by: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_argv0(argv0)?;
    let rows = sqlx::query(
        "INSERT INTO tool_allowlists (tool, argv0, created_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (tool, argv0) DO NOTHING",
    )
    .bind(tool)
    .bind(argv0)
    .bind(created_by)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() == 1)
}

pub async fn remove(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_argv0(argv0)?;
    let rows = sqlx::query(
        "DELETE FROM tool_allowlists WHERE tool = $1 AND argv0 = $2",
    )
    .bind(tool)
    .bind(argv0)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() == 1)
}

pub async fn list_for_tool(
    pool: &PgPool,
    tool: &str,
) -> Result<Vec<String>, ToolAllowlistError> {
    validate_tool_name(tool)?;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT argv0 FROM tool_allowlists WHERE tool = $1 ORDER BY argv0 ASC",
    )
    .bind(tool)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

pub async fn list_all(pool: &PgPool) -> Result<Vec<AllowlistEntry>, ToolAllowlistError> {
    let rows: Vec<(String, String, OffsetDateTime, String)> = sqlx::query_as(
        "SELECT tool, argv0, created_at, created_by
         FROM tool_allowlists
         ORDER BY tool ASC, argv0 ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(tool, argv0, created_at, created_by)| AllowlistEntry {
            tool,
            argv0,
            created_at,
            created_by,
        })
        .collect())
}
```

- [ ] **Step 4: Run integration test, expect GREEN**

```bash
cargo test -p kastellan-db --test postgres_e2e \
  tool_allowlists_round_trip_and_grant_shape -- --nocapture 2>&1 | tail -10
```

Expected: `test result: ok. 1 passed` (or one `[SKIP]` line if PG is
unavailable, then `test result: ok. 0 passed`).

- [ ] **Step 5: Run the full workspace, expect green**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 394 passed` (387 baseline + 6 validator
unit + 1 integration = 394).

- [ ] **Step 6: Commit**

```bash
git add db/src/tool_allowlists.rs db/tests/postgres_e2e.rs
git commit -m "$(cat <<'EOF'
db(tool_allowlists): I/O layer + GRANT-shape integration test

Implements add/remove/list_for_tool/list_all using sqlx against the
0009 table. Idempotent add (ON CONFLICT DO NOTHING) and remove return
a bool indicating whether state actually changed — the caller uses this
to decide whether to emit an audit row.

New integration test pins the GRANT shape (UPDATE denied) and the SQL
CHECK constraint (relative argv0 rejected) end-to-end against a real
per-test PG cluster.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Action constants in `core/src/scheduler/audit.rs`

**Files:**
- Modify: `core/src/scheduler/audit.rs`

**Rationale:** All scheduler/core-emitted action strings live here, one
canonical source. Three new strings: `tools.allowlist.add`,
`tools.allowlist.remove`, `registry.loaded`.

- [ ] **Step 1: Find the existing `ACTION_TASK_*` const block**

```bash
grep -n "^pub const ACTION_" core/src/scheduler/audit.rs
```

Expected: several `ACTION_TASK_*` constants in alphabetical order.

- [ ] **Step 2: Insert the three new constants alphabetically**

The new constants slot before `ACTION_TASK_*` alphabetically. Add to
the const block:

```rust
/// Action string for `actor='core'` audit rows emitted at daemon
/// bring-up, summarising which tools were registered and the SHA-256
/// of each tool's loaded allowlist. Cross-restart drift detection.
pub const ACTION_REGISTRY_LOADED: &str = "registry.loaded";

/// Action string for `actor='cli'` audit rows emitted when an operator
/// adds one allowlist entry via `kastellan-cli tools allowlist add`.
pub const ACTION_TOOLS_ALLOWLIST_ADD: &str = "tools.allowlist.add";

/// Action string for `actor='cli'` audit rows emitted when an operator
/// removes one allowlist entry via `kastellan-cli tools allowlist remove`.
pub const ACTION_TOOLS_ALLOWLIST_REMOVE: &str = "tools.allowlist.remove";
```

- [ ] **Step 3: Verify the workspace still builds**

```bash
cargo build --workspace 2>&1 | tail -3
```

Expected: clean build, no warnings.

- [ ] **Step 4: Run tests; expect still-green at 394**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 394 passed`.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/audit.rs
git commit -m "$(cat <<'EOF'
core(scheduler/audit): action constants for tool-allowlist hygiene

Three new strings: ACTION_REGISTRY_LOADED ('registry.loaded'),
ACTION_TOOLS_ALLOWLIST_ADD, ACTION_TOOLS_ALLOWLIST_REMOVE. Slotted
alphabetically with the existing ACTION_TASK_* family. Wired into
cli_audit and main.rs in the next commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `cli_audit` write-and-audit helpers

**Files:**
- Modify: `core/src/cli_audit.rs`

**Rationale:** Two new helpers (`tools_allowlist_add_and_audit`,
`tools_allowlist_remove_and_audit`) follow the existing pattern from
`cancel_and_audit` and `submit_and_audit`: call the DB layer, on
success emit one `actor='cli'` audit row best-effort. Boolean return
mirrors the DB layer so the CLI can print "added" vs "already present".

- [ ] **Step 1: Find the existing helpers**

```bash
grep -n "^pub async fn\|^pub const\|^use " core/src/cli_audit.rs | head -20
```

- [ ] **Step 2: Add the new helpers at the end of the file**

Append to `core/src/cli_audit.rs` (just before the `#[cfg(test)] mod
tests` block if one exists, otherwise at end of file):

```rust
/// Add one allowlist entry and emit one `actor='cli'
/// action='tools.allowlist.add'` audit row on success.
///
/// Returns the DB-layer bool: `Ok(true)` means a row was INSERTed (and
/// an audit row was emitted, best-effort); `Ok(false)` means the entry
/// already existed and **no audit row is written** (the operator's
/// state-change intent did not materialise; logging it would confuse
/// "what was true at time T" reconstructions).
///
/// Audit-insert posture: best-effort. A transient DB failure on the
/// audit row is logged via `tracing::warn!` and swallowed; the
/// underlying `db::tool_allowlists::add` outcome propagates either way.
pub async fn tools_allowlist_add_and_audit(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
) -> Result<bool, kastellan_db::tool_allowlists::ToolAllowlistError> {
    let inserted = kastellan_db::tool_allowlists::add(pool, tool, argv0, CLI_AUDIT_ACTOR).await?;
    if inserted {
        let payload = serde_json::json!({ "tool": tool, "argv0": argv0 });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            crate::scheduler::audit::ACTION_TOOLS_ALLOWLIST_ADD,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                tool = tool,
                argv0 = argv0,
                "tools_allowlist_add_and_audit: audit insert failed"
            );
        }
    }
    Ok(inserted)
}

/// Remove one allowlist entry and emit one `actor='cli'
/// action='tools.allowlist.remove'` audit row on success.
///
/// Returns `Ok(true)` if a row was deleted (and audit row emitted
/// best-effort); `Ok(false)` if nothing matched (no audit row).
pub async fn tools_allowlist_remove_and_audit(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
) -> Result<bool, kastellan_db::tool_allowlists::ToolAllowlistError> {
    let removed = kastellan_db::tool_allowlists::remove(pool, tool, argv0).await?;
    if removed {
        let payload = serde_json::json!({ "tool": tool, "argv0": argv0 });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            crate::scheduler::audit::ACTION_TOOLS_ALLOWLIST_REMOVE,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                tool = tool,
                argv0 = argv0,
                "tools_allowlist_remove_and_audit: audit insert failed"
            );
        }
    }
    Ok(removed)
}
```

- [ ] **Step 3: Verify the workspace still builds**

```bash
cargo build --workspace 2>&1 | tail -3
```

Expected: clean build. If `serde_json` isn't in `core/Cargo.toml`'s
direct deps, check the existing `cli_audit.rs` imports — it's already
in scope per the existing `cancel_and_audit` body.

- [ ] **Step 4: Run tests, expect still-green at 394**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 394 passed` (the helpers are exercised by
Task 6's CLI e2e; no new tests in this task).

- [ ] **Step 5: Commit**

```bash
git add core/src/cli_audit.rs
git commit -m "$(cat <<'EOF'
core(cli_audit): tools_allowlist_{add,remove}_and_audit helpers

Two new producer-side helpers mirroring the cancel_and_audit /
submit_and_audit pattern: call db::tool_allowlists::{add,remove},
on a state change (Ok(true)) emit one actor='cli' audit row with the
matching action string from scheduler::audit. Audit insert is
best-effort; the DB-layer outcome is load-bearing.

Test coverage lands in the CLI e2e in the next commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: `kastellan-cli tools allowlist {add,remove,list}` + e2e test

**Files:**
- Modify: `core/src/bin/kastellan-cli.rs` (new `tools` subcommand tree)
- Create: `core/tests/cli_tools_allowlist_e2e.rs`

**Rationale:** Subprocess-level pin for the CLI surface. The test brings
up its own PG cluster, runs the new CLI subcommands as subprocesses,
and asserts both DB state and `audit_log` row shape.

The CLI parser is the existing hand-rolled style (no clap dep — matches
the `tasks` subcommand pattern).

- [ ] **Step 1: Write the integration test (RED)**

Create `core/tests/cli_tools_allowlist_e2e.rs`:

```rust
//! Subprocess-level pin for `kastellan-cli tools allowlist {add,remove,list}`.
//!
//! Each subtest runs the real CLI binary against a per-test PG cluster,
//! asserts the DB row state, the audit-row shape, and the CLI exit code
//! + stdout/stderr contract.

use std::collections::BTreeMap;
use std::process::Command;

use kastellan_db::pool::connect_runtime_pool;
use kastellan_db::probe::run as probe_run;
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, pg_bin_dir_or_skip, skip_if_no_supervisor,
};
use sqlx::Row;

/// Build the env block the CLI subprocess needs to find PG via UDS.
/// The CLI's `resolve_connect_spec` reads `KASTELLAN_DATA_DIR` and
/// builds the socket path from there (matches the daemon's resolution
/// shape — see `cli_ask_e2e::bring_up_daemon`).
fn cli_env(data_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("KASTELLAN_DATA_DIR".to_string(), data_dir.display().to_string()),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    if let Some(user) = std::env::var_os("USER") {
        env.push(("USER".to_string(), user.to_string_lossy().into_owned()));
    } else {
        // ConnectSpec::default_for needs a user for peer auth; fall back
        // to the cluster bring-up user.
        env.push(("USER".to_string(), kastellan_tests_common::current_username()));
    }
    env
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_tools_allowlist_add_remove_list_round_trip_writes_audit_rows() {
    if !skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cli-tools-allowlist-e2e",
        "cli-tools-allowlist-e2e-log",
        "kastellan-postgres-cli-tools-allowlist-e2e",
    )
    .await
    .expect("bring up PG cluster");

    probe_run(&cluster.conn_spec).await.expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- 1. `tools allowlist add` happy path ----------------------------
    let out = Command::new(&bin)
        .args(["tools", "allowlist", "add", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add");
    assert!(out.status.success(), "add exit: {:?}, stderr: {}",
        out.status, String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("added"), "stdout was: {stdout}");

    // DB row landed.
    let rows: Vec<(String,)> = sqlx::query_as("SELECT argv0 FROM tool_allowlists WHERE tool = $1 ORDER BY argv0")
        .bind("shell-exec")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows, vec![("/usr/bin/echo".to_string(),)]);

    // --- 2. Idempotent re-add ------------------------------------------
    let out2 = Command::new(&bin)
        .args(["tools", "allowlist", "add", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add #2");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("already present"), "stdout was: {stdout2}");

    // --- 3. `tools allowlist list` -------------------------------------
    let out_l = Command::new(&bin)
        .args(["tools", "allowlist", "list", "--tool", "shell-exec"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli list");
    assert!(out_l.status.success());
    let stdout_l = String::from_utf8_lossy(&out_l.stdout);
    assert!(stdout_l.contains("shell-exec"));
    assert!(stdout_l.contains("/usr/bin/echo"));

    // --- 4. `tools allowlist remove` -----------------------------------
    let out_r = Command::new(&bin)
        .args(["tools", "allowlist", "remove", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove");
    assert!(out_r.status.success());
    let stdout_r = String::from_utf8_lossy(&out_r.stdout);
    assert!(stdout_r.contains("removed"), "stdout was: {stdout_r}");
    let after: Vec<(String,)> = sqlx::query_as("SELECT argv0 FROM tool_allowlists WHERE tool = $1")
        .bind("shell-exec")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(after.is_empty());

    // --- 5. Idempotent re-remove ---------------------------------------
    let out_r2 = Command::new(&bin)
        .args(["tools", "allowlist", "remove", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove #2");
    assert!(out_r2.status.success());
    let stdout_r2 = String::from_utf8_lossy(&out_r2.stdout);
    assert!(stdout_r2.contains("not present"), "stdout was: {stdout_r2}");

    // --- 6. Validation error: relative argv0 ---------------------------
    let out_bad = Command::new(&bin)
        .args(["tools", "allowlist", "add", "shell-exec", "echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add bad");
    assert_eq!(out_bad.status.code(), Some(2), "validation error exit");
    let stderr_bad = String::from_utf8_lossy(&out_bad.stderr);
    assert!(stderr_bad.to_lowercase().contains("absolute"),
        "stderr was: {stderr_bad}");

    // --- 7. Audit multiset --------------------------------------------
    // Expected: 1 cli/tools.allowlist.add + 1 cli/tools.allowlist.remove.
    // No row for the idempotent no-ops or the validation error.
    let audit_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM audit_log WHERE actor = 'cli' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for r in &audit_rows {
        *counts.entry((r.0.clone(), r.1.clone())).or_default() += 1;
    }
    assert_eq!(
        counts.get(&("cli".to_string(), "tools.allowlist.add".to_string())),
        Some(&1)
    );
    assert_eq!(
        counts.get(&("cli".to_string(), "tools.allowlist.remove".to_string())),
        Some(&1)
    );

    // Payload spot-check: the add row's payload is `{tool, argv0}`.
    let row = sqlx::query("SELECT payload FROM audit_log WHERE actor = 'cli' AND action = 'tools.allowlist.add' LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let payload: serde_json::Value = row.get("payload");
    assert_eq!(payload["tool"], "shell-exec");
    assert_eq!(payload["argv0"], "/usr/bin/echo");

    drop(pool);
    drop(cluster);
}
```

- [ ] **Step 2: Run the test, expect compile-error RED**

```bash
cargo test -p kastellan-core --test cli_tools_allowlist_e2e 2>&1 | tail -10
```

Expected: build fails — the CLI binary doesn't know the `tools`
subcommand yet, so a later step (running it) would also fail. The
test compiles (uses only existing crates) but the CLI subprocess exits
with code 2 ("unknown subcommand: tools") and the first assertion
fails.

- [ ] **Step 3: Wire the `tools` subcommand into the CLI binary**

Read the current dispatch shape:

```bash
grep -n '^    match args\[1\]' core/src/bin/kastellan-cli.rs
```

Modify the `match` arm in `main()` (around line 47) to add a `"tools"`
arm. Add to `core/src/bin/kastellan-cli.rs` `main()`:

```rust
        "tools" => run_tools(&args[2..]),
```

(Slot it after `"tasks"` to match the help-text ordering.)

- [ ] **Step 4: Add the `run_tools` dispatcher and three subcommand handlers**

Append at the end of `core/src/bin/kastellan-cli.rs`, after the existing
`tasks_*` handlers:

```rust
// ============================================================
// `tools allowlist {add,remove,list}` subcommand tree
// ============================================================

fn run_tools(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli tools allowlist <add|remove|list> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "allowlist" => run_tools_allowlist(&args[1..]),
        other => {
            eprintln!("tools: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

fn run_tools_allowlist(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli tools allowlist <add|remove|list> ...");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tools allowlist: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match args[0].as_str() {
        "add"    => rt.block_on(tools_allowlist_add(&args[1..])),
        "remove" => rt.block_on(tools_allowlist_remove(&args[1..])),
        "list"   => rt.block_on(tools_allowlist_list(&args[1..])),
        other    => {
            eprintln!("tools allowlist: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

async fn tools_allowlist_add(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::tools_allowlist_add_and_audit;
    use kastellan_db::pool::connect_runtime_pool;

    let (tool, argv0) = match args {
        [t, a] => (t.clone(), a.clone()),
        _ => {
            eprintln!("usage: kastellan-cli tools allowlist add <tool> <argv0>");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match tools_allowlist_add_and_audit(&pool, &tool, &argv0).await {
        Ok(true)  => { println!("added {tool} {argv0}"); ExitCode::from(0) }
        Ok(false) => { println!("already present"); ExitCode::from(0) }
        Err(kastellan_db::tool_allowlists::ToolAllowlistError::InvalidArgv0) => {
            eprintln!("argv0 must be an absolute path (starting with '/')");
            ExitCode::from(2)
        }
        Err(e) => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

async fn tools_allowlist_remove(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::tools_allowlist_remove_and_audit;
    use kastellan_db::pool::connect_runtime_pool;

    let (tool, argv0) = match args {
        [t, a] => (t.clone(), a.clone()),
        _ => {
            eprintln!("usage: kastellan-cli tools allowlist remove <tool> <argv0>");
            return ExitCode::from(2);
        }
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match tools_allowlist_remove_and_audit(&pool, &tool, &argv0).await {
        Ok(true)  => { println!("removed {tool} {argv0}"); ExitCode::from(0) }
        Ok(false) => { println!("not present"); ExitCode::from(0) }
        Err(kastellan_db::tool_allowlists::ToolAllowlistError::InvalidArgv0) => {
            eprintln!("argv0 must be an absolute path (starting with '/')");
            ExitCode::from(2)
        }
        Err(e) => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

async fn tools_allowlist_list(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;
    use kastellan_db::tool_allowlists::{list_all, list_for_tool};

    let mut tool_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tool" => {
                tool_filter = args.get(i + 1).cloned();
                if tool_filter.is_none() {
                    eprintln!("--tool requires a name argument");
                    return ExitCode::from(2);
                }
                i += 2;
            }
            other => {
                eprintln!("tools allowlist list: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    if let Some(tool) = tool_filter {
        let entries = match list_for_tool(&pool, &tool).await {
            Ok(v) => v,
            Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
        };
        println!("{:<16}  {}", "TOOL", "ARGV0");
        for argv0 in entries {
            println!("{:<16}  {}", tool, argv0);
        }
    } else {
        let entries = match list_all(&pool).await {
            Ok(v) => v,
            Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
        };
        println!("{:<16}  {:<48}  {:<24}  {}",
            "TOOL", "ARGV0", "CREATED_AT", "CREATED_BY");
        for e in entries {
            println!("{:<16}  {:<48}  {:<24}  {}",
                e.tool, e.argv0, e.created_at, e.created_by);
        }
    }
    ExitCode::from(0)
}
```

- [ ] **Step 5: Update the `help_text` and the file-level docstring**

In `core/src/bin/kastellan-cli.rs`, update the `help_text()` return to
include the new subcommands. After the existing `tasks tail` line in
the help string, insert:

```text
    kastellan-cli tools allowlist add    <tool> <argv0>
    kastellan-cli tools allowlist remove <tool> <argv0>
    kastellan-cli tools allowlist list   [--tool <name>]
```

And update the file-level `//!` comment (lines 3-17) to mention the new
`tools` subcommand group between `tasks` and `audit`.

- [ ] **Step 6: Run the test, expect GREEN**

```bash
cargo test -p kastellan-core --test cli_tools_allowlist_e2e 2>&1 | tail -10
```

Expected: `test result: ok. 1 passed` (or `[SKIP]` if PG unavailable).
If it fails, read the assertion output carefully — common issues are
stdout/stderr substring mismatches (fix in handler) or audit-row
multiset (the row count must be exactly 1 add + 1 remove).

- [ ] **Step 7: Run workspace tests, expect 395 (394 + 1 new e2e)**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 395 passed`.

- [ ] **Step 8: Commit**

```bash
git add core/src/bin/kastellan-cli.rs core/tests/cli_tools_allowlist_e2e.rs
git commit -m "$(cat <<'EOF'
core(kastellan-cli): tools allowlist {add,remove,list} subcommands

Hand-rolled subcommand tree mirroring the existing tasks dispatcher
(no clap dep). Add/remove flow through cli_audit::tools_allowlist_*
helpers (emit one cli/tools.allowlist.{add,remove} audit row on
state change, none on idempotent no-op or validation error). List
is read-only — no audit row.

New integration test cli_tools_allowlist_e2e pins: add (happy + idempotent),
list, remove (happy + idempotent), validation error (relative argv0 →
exit 2), audit multiset (exactly 1 add + 1 remove row), payload
{tool, argv0} shape.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: `tests-common::seed_tool_allowlist` helper

**Files:**
- Create: `tests-common/src/allowlist.rs`
- Modify: `tests-common/src/lib.rs` (add `pub mod allowlist;` + re-export)

**Rationale:** Integration tests outside of `cli_tools_allowlist_e2e`
need to populate the allowlist before starting a daemon. A small
bulk-INSERT helper avoids spawning the CLI binary in the test setup.

- [ ] **Step 1: Create the helper module**

Create `tests-common/src/allowlist.rs`:

```rust
//! Seed `tool_allowlists` rows for integration tests.
//!
//! Tests that bring up the `kastellan` daemon and want it to see a
//! populated argv allowlist can call this between PG cluster bring-up
//! and daemon start. Bypasses the CLI binary for setup speed.

use sqlx::PgPool;

/// Bulk-INSERT one entry per `argv0` for the given `tool`. Uses
/// `created_by = "test"` so the rows are visibly test fixtures. No-op
/// on an empty `argv0s` slice.
pub async fn seed_tool_allowlist(
    pool: &PgPool,
    tool: &str,
    argv0s: &[&str],
) -> Result<(), sqlx::Error> {
    for &argv0 in argv0s {
        sqlx::query(
            "INSERT INTO tool_allowlists (tool, argv0, created_by)
             VALUES ($1, $2, 'test')
             ON CONFLICT (tool, argv0) DO NOTHING",
        )
        .bind(tool)
        .bind(argv0)
        .execute(pool)
        .await?;
    }
    Ok(())
}
```

- [ ] **Step 2: Re-export from `tests-common/src/lib.rs`**

Read first:

```bash
grep -n "^pub mod\|^pub use" tests-common/src/lib.rs
```

Add (in alphabetical position):

```rust
pub mod allowlist;
```

And the re-export (with the existing `pub use` lines):

```rust
pub use allowlist::seed_tool_allowlist;
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo build -p kastellan-tests-common 2>&1 | tail -3
cargo build --workspace 2>&1 | tail -3
```

Expected: clean builds.

- [ ] **Step 4: Run tests, expect still-green at 395**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 395 passed`.

- [ ] **Step 5: Commit**

```bash
git add tests-common/src/allowlist.rs tests-common/src/lib.rs
git commit -m "$(cat <<'EOF'
tests-common: seed_tool_allowlist helper

Bulk-INSERT helper for integration tests that need a populated argv
allowlist before daemon start. Bypasses the CLI binary; uses
created_by='test' so seeded rows are recognisable in audit-row
forensics.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Rewire `build_tool_registry` to async + DB-backed

**Files:**
- Modify: `core/src/main.rs` (the `build_tool_registry` function around line 270-308; the call site in `main`)

**Rationale:** The registry now reads its allowlist from the DB. The
function becomes `async`, takes a `&PgPool`, and emits one
`actor='core' action='registry.loaded'` audit row with the SHA-256 of
the canonical allowlist for cross-restart drift detection. The
`KASTELLAN_SHELL_EXEC_ALLOWLIST` env var is no longer read; a deprecation
WARN logs once if it's still set.

- [ ] **Step 1: Read the current `build_tool_registry` and its call site**

```bash
grep -n "build_tool_registry\|fn build_tool_registry" core/src/main.rs
```

The call site is the synchronous `build_tool_registry()` invocation
inside `main`'s `async` body — look around the line where `ToolRegistry`
is constructed.

- [ ] **Step 2: Modify `build_tool_registry` to be async + DB-backed**

Replace the existing `build_tool_registry` body in `core/src/main.rs`
with:

```rust
async fn build_tool_registry(
    pool: &sqlx::PgPool,
) -> anyhow::Result<kastellan_core::scheduler::ToolRegistry> {
    use anyhow::Context as _;
    let mut reg = kastellan_core::scheduler::ToolRegistry::new();
    let mut loaded: Vec<LoadedToolRecord> = Vec::new();

    if let Some(bin_os) = std::env::var_os("KASTELLAN_SHELL_EXEC_BIN") {
        let binary = std::path::PathBuf::from(&bin_os);
        if binary.is_file() {
            let allowlist = kastellan_db::tool_allowlists::list_for_tool(pool, "shell-exec")
                .await
                .context("loading shell-exec allowlist from DB")?;
            let entry = kastellan_core::scheduler::shell_exec_entry(binary.clone(), &allowlist);
            info!(
                tool = "shell-exec",
                binary = %binary.display(),
                allowlist_len = allowlist.len(),
                "registering tool"
            );
            loaded.push(LoadedToolRecord {
                name: "shell-exec".to_string(),
                binary: binary.display().to_string(),
                allowlist_len: allowlist.len(),
                allowlist_sha256: sha256_argv0_list(&allowlist),
            });
            reg.insert("shell-exec", entry);
        } else {
            tracing::warn!(
                binary = %binary.display(),
                "KASTELLAN_SHELL_EXEC_BIN does not point to an existing file; \
                 shell-exec NOT registered"
            );
        }
    }

    // Deprecation warning — does not block bring-up.
    if std::env::var_os("KASTELLAN_SHELL_EXEC_ALLOWLIST").is_some() {
        tracing::warn!(
            "KASTELLAN_SHELL_EXEC_ALLOWLIST is no longer honored; \
             use 'kastellan-cli tools allowlist add <tool> <argv0>' to populate the DB"
        );
    }

    // Best-effort audit row: a transient DB failure here must not
    // block daemon bring-up. The allowlist itself has already been
    // loaded successfully.
    if let Err(e) = write_registry_loaded_row(pool, &loaded).await {
        tracing::warn!(error = %e, "registry.loaded audit row insert failed");
    }

    Ok(reg)
}

/// One per-tool record carried in the `registry.loaded` audit-row
/// payload.
#[derive(serde::Serialize)]
struct LoadedToolRecord {
    name: String,
    binary: String,
    allowlist_len: usize,
    /// SHA-256 of the canonical-form allowlist:
    /// `argv0_1 || '\n' || argv0_2 || '\n' || …` where the list is
    /// lexicographically sorted and a trailing newline follows the
    /// last entry. Empty list → SHA-256 of the empty string.
    allowlist_sha256: String,
}

fn sha256_argv0_list(argv0s: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&String> = argv0s.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for argv0 in sorted {
        hasher.update(argv0.as_bytes());
        hasher.update(b"\n");
    }
    let bytes = hasher.finalize();
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

async fn write_registry_loaded_row(
    pool: &sqlx::PgPool,
    tools: &[LoadedToolRecord],
) -> Result<(), sqlx::Error> {
    let payload = serde_json::json!({ "tools": tools });
    kastellan_db::audit::insert(
        pool,
        "core",
        kastellan_core::scheduler::audit::ACTION_REGISTRY_LOADED,
        payload,
    )
    .await
    .map(|_| ())
}
```

- [ ] **Step 3: Update the call site in `main`**

Find where `build_tool_registry()` is currently called (likely after
`bring_up_database`). Change:

```rust
let tool_registry = build_tool_registry();
```

to:

```rust
let tool_registry = build_tool_registry(&pool).await?;
```

(`pool` is the `PgPool` already in scope from `bring_up_database`.)

- [ ] **Step 4: Add the `sha2` dep to `core/Cargo.toml` if not already there**

Check first:

```bash
grep "^sha2 " core/Cargo.toml || grep '^sha2 = ' core/Cargo.toml
```

If `sha2` is not in `core/Cargo.toml`'s `[dependencies]`, add it under
the existing deps:

```toml
sha2 = { workspace = true }
```

(The workspace already declares `sha2 = "0.10"` per the existing
`tests-common` usage.)

- [ ] **Step 5: Migrate `core/tests/cli_ask_e2e.rs` to use the seed helper**

Two changes:

(a) Drop the `KASTELLAN_SHELL_EXEC_ALLOWLIST` env push (the daemon no
longer reads it). Find and delete:

```bash
grep -n "KASTELLAN_SHELL_EXEC_ALLOWLIST" core/tests/cli_ask_e2e.rs
```

Delete the matching `spec.env.push((...))` 2-line block.

(b) The daemon reads the allowlist at startup as part of
`build_tool_registry`, so the seed must land **before** the daemon
boots. Since the daemon's own probe applies migrations, the test needs
to run `probe_run` explicitly first, then seed via a temporary pool,
then drop the pool before installing the supervisor service.

Both top-level tests (the happy-path test and the failure-path test)
call `cluster_for(suffix)` followed by `bring_up_daemon(suffix,
&cluster.data_dir, &mock.base_url, &user)`. Insert a seeding block
between those two calls. For the happy-path test (around line 482):

```rust
    // Apply migrations explicitly (daemon would do it idempotently,
    // but we need the schema in place before seeding the allowlist).
    {
        kastellan_db::probe::run(&cluster.conn_spec)
            .await
            .expect("probe run");
        let seed_pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("seed pool");
        kastellan_tests_common::seed_tool_allowlist(&seed_pool, "shell-exec", &[ECHO_PATH])
            .await
            .expect("seed shell-exec allowlist");
        drop(seed_pool);
    }
```

For the failure-path test (around line 665), apply the same block but
**omit** the `seed_tool_allowlist` call (empty allowlist → `/bin/cat`
is denied at the worker). The `probe::run` call still happens so
migrations are in place:

```rust
    {
        kastellan_db::probe::run(&cluster.conn_spec)
            .await
            .expect("probe run");
        // No allowlist seeding: every shell.exec call must surface
        // POLICY_DENIED, which is the failure-path's assertion target.
    }
```

Note: the happy-path test's setup function is `async`, so `.await`
is in scope. If the top-level test is `#[test]` (not `#[tokio::test]`),
wrap the seeding block in the existing `rt.block_on(async { … })`
that drives the rest of the test.

(c) Bump the audit-row multiset assertions: add `+1` for
`("core", "registry.loaded")` to both happy- and failure-path
multiset checks. Find the existing multiset assertions:

```bash
grep -n 'core".*startup\|registry.loaded' core/tests/cli_ask_e2e.rs
```

After each existing `assert_eq!(m.get(&("core".into(), "startup".into())), Some(&1), …)`
add:

```rust
        assert_eq!(m.get(&("core".into(), "registry.loaded".into())), Some(&1),
                   "expected 1× core/registry.loaded (build_tool_registry summary row); multiset = {m:?}");
```

- [ ] **Step 6: Run the migrated test**

```bash
cargo test -p kastellan-core --test cli_ask_e2e -- --nocapture 2>&1 | tail -15
```

Expected: both `cli_ask_e2e` tests pass. If they fail, the most likely
cause is the multiset assertion missed an audit row — re-read the
failure to find which `(actor, action)` count was off-by-one and
adjust.

- [ ] **Step 7: Run the full workspace**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 395 passed` (still 395 — Task 8 doesn't add
new `#[test]` functions, just migrates the existing ones).

- [ ] **Step 8: Commit**

```bash
git add core/src/main.rs core/Cargo.toml core/tests/cli_ask_e2e.rs
git commit -m "$(cat <<'EOF'
core(main): build_tool_registry reads allowlist from DB; registry.loaded row

build_tool_registry is now async, takes &PgPool, and loads the
shell-exec argv allowlist from the tool_allowlists table at startup.
KASTELLAN_SHELL_EXEC_ALLOWLIST env var is no longer honored — a one-time
WARN logs if still set. Fail-closed: DB error during the load aborts
bring-up.

Emits one actor='core' action='registry.loaded' audit row carrying
{tools: [{name, binary, allowlist_len, allowlist_sha256}]} where the
SHA-256 is over the canonical-form (lexicographically sorted, '\n'
terminated) allowlist. Cross-restart drift becomes visible at a
glance from audit_log.

cli_ask_e2e migrated to seed the allowlist via
tests-common::seed_tool_allowlist instead of an env-var push; audit
multiset assertions bumped to include the new registry.loaded row.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: Documentation refresh — HANDOVER + ROADMAP

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Rationale:** Per CLAUDE.md rule #8, update both docs at session end
to reflect what shipped.

- [ ] **Step 1: Add "Recently completed" section to HANDOVER.md**

Insert after the existing top "Recently completed" block (around line
108). Use the standard template — branch, why, shape (file list),
audit-row contract, TDD ordering, what-this-does-NOT-do, test-count
delta, files-touched list.

The new entry headline: "this session, 2026-05-14 — per-tool argv
allowlist hygiene, branch `feat/tool-allowlist-db`".

- [ ] **Step 2: Update the HANDOVER header**

Bump `Last updated`, `Last commit`, `This session's working branch` at
the top of the file.

- [ ] **Step 3: Move the "Per-tool argv allowlist hygiene" item in "Next TODO"**

Find the bullet in `## Next TODO (pick one)` (around line 1131) and
strike it out with `~~...~~` + "Shipped this session" pointer to the
new Recently-completed entry.

- [ ] **Step 4: Tick the ROADMAP item**

In `docs/devel/ROADMAP.md`, the Phase 1 follow-up tracking this work
doesn't yet exist explicitly (this task is filed as a HANDOVER-only
pickup). Add a `[x]` line under the Phase 1 audit-row follow-ups
section, immediately before the "Real ConstitutionalGuard +
DeterministicPolicy" item:

```markdown
- [x] **[follow-up] Per-tool argv allowlist hygiene** — landed
  2026-05-14 on branch `feat/tool-allowlist-db`. New migration
  `0009_tool_allowlists.sql` + `db::tool_allowlists` module +
  `cli_audit::tools_allowlist_{add,remove}_and_audit` helpers +
  `kastellan-cli tools allowlist {add,remove,list}` subcommands +
  `core::build_tool_registry` rewired to read allowlist from DB +
  `actor='core' action='registry.loaded'` audit row carrying the
  SHA-256 of the canonical-form allowlist. Test count 387 → **395**
  (+6 unit validators + 1 DB integration + 1 CLI e2e).
```

- [ ] **Step 5: Commit**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): tool-allowlist hygiene shipped

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Finishing up

- [ ] **Final workspace test pass**

```bash
cargo test --workspace 2>&1 | tail -3
```

Expected: `test result: ok. 395 passed`. No new warnings.

- [ ] **Push the branch and open a PR**

```bash
git push -u origin feat/tool-allowlist-db
gh pr create --title "feat: per-tool argv allowlist hygiene (DB-backed)" --body "$(cat <<'EOF'
## Summary
- Moves the per-tool argv allowlist source-of-truth from the
  `KASTELLAN_SHELL_EXEC_ALLOWLIST` env var to a new `tool_allowlists`
  table (migration `0009`), behind the existing `kastellan_runtime`
  GRANT shape.
- Every mutation flows through `cli_audit::tools_allowlist_*` and
  writes one `actor='cli' action='tools.allowlist.{add,remove}'`
  audit row on state change.
- Daemon bring-up emits one `actor='core' action='registry.loaded'`
  row carrying the SHA-256 of the canonical-form allowlist for
  cross-restart drift detection.

## Test plan
- [x] `cargo test --workspace` — 387 → 395 (+6 validator unit tests,
      +1 DB integration, +1 CLI e2e).
- [x] `cli_ask_e2e` migrated to `seed_tool_allowlist`; happy + failure
      paths still green with new `registry.loaded` audit row included
      in the multiset.
- [x] GRANT shape pin: `SET ROLE kastellan_runtime; UPDATE
      tool_allowlists …` denied.
- [x] SQL CHECK pin: relative `argv0` rejected by Postgres.

Spec: [docs/superpowers/specs/2026-05-14-tool-allowlist-hygiene-design.md](docs/superpowers/specs/2026-05-14-tool-allowlist-hygiene-design.md)
Plan: [docs/superpowers/plans/2026-05-14-tool-allowlist-hygiene.md](docs/superpowers/plans/2026-05-14-tool-allowlist-hygiene.md)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

(Confirm with the user before running `gh pr create`.)

---

## Spec coverage cross-check

| Spec section | Plan task |
| ------------ | --------- |
| §1 Schema (`0009_tool_allowlists.sql`) | Task 1 |
| §1 `db::tool_allowlists` validators + types | Task 2 |
| §1 `db::tool_allowlists` async I/O | Task 3 |
| §2 Action constants in `scheduler/audit.rs` | Task 4 |
| §2 `cli_audit` write-and-audit helpers | Task 5 |
| §2 `kastellan-cli tools allowlist {add,remove,list}` | Task 6 |
| §3 Daemon wiring (async `build_tool_registry`, registry.loaded, deprecation warning) | Task 8 |
| §4 Unit tests for validators | Task 2 |
| §4 DB integration test | Task 3 |
| §4 `tests-common::seed_tool_allowlist` | Task 7 |
| §4 `cli_ask_e2e` migration | Task 8 |
| §4 `cli_tools_allowlist_e2e` | Task 6 |
| Docs refresh | Task 9 |

All spec sections have a matching task. No placeholders. Type names
are consistent (`ToolAllowlistError`, `AllowlistEntry`,
`MAX_TOOL_NAME_LEN`, `tools_allowlist_{add,remove}_and_audit`,
`ACTION_TOOLS_ALLOWLIST_{ADD,REMOVE}`, `ACTION_REGISTRY_LOADED`).
