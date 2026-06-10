# `kastellan-db`

Postgres schema, migrations, and connection layer for the agent core.

## Migration hygiene

`kastellan-db` embeds the migration set at compile time via
`sqlx::migrate!("./migrations")` and stores applied-migration metadata in the
cluster's `_sqlx_migrations` table. sqlx fingerprints each migration by
**version number + slug + body hash**, so once a migration has shipped (i.e.
landed on `main`), the following kinds of changes will silently *pass `cargo
test`* — which always runs against a fresh cluster — but **break every
existing cluster** the next time the daemon starts:

| Mistake | What sqlx does |
| --- | --- |
| Rename `0001_init.sql` → `0001_initial.sql` | Treats the rename as a new, unapplied migration; trips a slug mismatch against the cluster's `_sqlx_migrations` row. |
| Edit an applied migration's body (even a comment) | Trips a checksum mismatch on next startup. |
| Two devs branch off the same SHA and both add `0002_*.sql` | Whoever merges first wins; the loser's PR must be force-bumped to `0003` (or whatever the next free number is) before merge. |

### Rules

1. **Migration files are append-only after merge.** Never edit a file that
   has landed on `main`. If you need a change, write the next-numbered
   migration that fixes-forward (add a column, drop an index, etc.).
2. **Numbering is dense and monotonic** (`0001_*`, `0002_*`, …, four-digit
   zero-padded). The next free number is one greater than the last file in
   `db/migrations/` on `main`.
3. **Collisions resolve by PR-merge order.** The PR that lands first keeps
   its number. The other PR's author rebases, force-bumps to the next free
   number, and re-runs the test suite against a fresh cluster.
4. **Slugs (the `_<name>` part) must not change post-ship.** Same checksum
   rule as the body.
5. **No retroactive comment edits**, even cosmetic. sqlx hashes the file
   verbatim. If a comment is wrong on a shipped migration, write a follow-up
   migration whose body is a `COMMENT ON …` statement, or update the
   project docs instead.

### Recovering an ad-hoc dev cluster that hit a checksum mismatch

If you see something like:

> `migration 0007 was previously applied but has been modified`

on a personal/dev cluster (not production), two options:

- Re-roll the cluster: drop the cluster directory and let the daemon re-run
  migrations from scratch on next start. Cheapest if there's no data worth
  keeping.
- Surgically reset the row: `DELETE FROM _sqlx_migrations WHERE version =
  <N>;` then start the daemon — sqlx will re-apply the migration. Only safe
  if the migration is idempotent (most use `CREATE … IF NOT EXISTS` /
  `CREATE OR REPLACE FUNCTION` and are fine to re-run).

Don't do either against a shared/production cluster without confirming the
state with the operator first.

See issue [#13](https://github.com/hherb/kastellan/issues/13) for the
filing context.

## Cross-role contracts

When adding a new database role that **can DELETE from `memories`**, the role
**must also be granted `INSERT` on `deleted_memories`**.

### Why

Migration [`0008_deleted_memories_audit.sql`](migrations/0008_deleted_memories_audit.sql)
installs an `AFTER DELETE` trigger on `memories` that journals the deleted
row into `deleted_memories`. The trigger function `audit_memory_delete()` is
`SECURITY INVOKER` (Postgres default) — it runs with the privileges of the
role that issued the `DELETE`.

If that role lacks `INSERT` on `deleted_memories`, the trigger fails with a
permission error and the entire `DELETE` is rolled back. The operator sees
a `permission denied for table deleted_memories` message that does not
obviously point at the audit shape.

This is the intended fail-closed behaviour: there is no path to delete a
memory without journaling it first. But it is a latent contract — easy to
forget when wiring up a new role.

### Checklist for adding a DELETE-capable role to `memories`

1. `GRANT SELECT, DELETE ON memories TO <new_role>;` — as needed by the
   caller.
2. **Also** `GRANT INSERT ON deleted_memories TO <new_role>;` — required by
   the trigger.
3. Add a regression test in `db/tests/postgres_e2e.rs` (alongside
   `runtime_role_audit_log_revoke_is_enforced` and
   `memory_delete_writes_deleted_memories_row`) that pins both grants:
   the new role can `DELETE FROM memories` and the row lands in
   `deleted_memories`.

See issue [#42](https://github.com/hherb/kastellan/issues/42) for the design
discussion.
