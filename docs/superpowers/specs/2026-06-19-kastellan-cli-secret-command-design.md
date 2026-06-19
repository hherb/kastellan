# Design — `kastellan-cli secret` operator command

**Date:** 2026-06-19
**Status:** approved (brainstorm), pre-implementation
**Context:** First slice of Matrix Phase D Task 5. The agent (`@kastellan`) logs
into the live homeserver `matrix.kastellan.dev` with a password the matrix worker
reads at spawn. That password must live in `db::secrets` (AES-256-GCM at rest +
OS keyring), but **there is no operator-facing way to write a secret today** —
`kastellan-cli` has `pair`, `tools allowlist`, `tasks`, … but no `secret`. This
spec adds that command. (Reading the secret into `KASTELLAN_MATRIX_PASSWORD` at
matrix-worker spawn is the *next* Task 5 slice, out of scope here.)

## Goal

A minimal, audited operator CLI over `db::secrets` so an operator can store,
inventory, and remove named secrets on the daemon host — encrypted with the same
`OsKeyringProvider` the daemon uses, so what is stored is decryptable by
`Vault::materialize` at runtime.

Non-goals: plaintext readback (deliberately omitted), rotation policy, secret
references in tool params (already exists via `SecretRef`/`Vault`), key rotation.

## Command surface

```
kastellan-cli secret put <name> [--raw]   # value read from stdin (never argv)
kastellan-cli secret list                 # metadata only
kastellan-cli secret delete <name>
```

- **`put <name> [--raw]`**
  - `name` validated by `db::secrets::validate_name` (reused; rejects bad names).
  - Value read from **stdin**, never from argv (no process-list / shell-history
    exposure). If stdin is a TTY, prompt silently (no echo); if piped, read all
    bytes.
  - Trailing-newline handling via a pure helper
    `read_secret_value(raw: &[u8], keep_raw: bool) -> Result<Vec<u8>, String>`:
    strips **exactly one** trailing `\n` (and a preceding `\r` if present) unless
    `--raw`; rejects an empty result. This prevents the newline-class login break
    (`echo pw |` vs `printf %s pw |` both DTRT; `--raw` preserves exact bytes for
    a secret that genuinely ends in a newline).
  - UPSERT (rotation-friendly — `db::secrets::put` is `ON CONFLICT DO UPDATE`).
    Prints `stored <name> (created)` or `(updated)` by checking existence first.
  - `extra_aad = None` — matches the Vault's `get(..., None)` convention; a
    non-None AAD here would make the daemon's bootstrap `materialize` fail.
- **`list`** — prints `name  key_id  created_at  updated_at` from
  `db::secrets::list()` (excludes ciphertext/nonce/AAD by construction). Prints
  `(no secrets)` when empty.
- **`delete <name>`** — `db::secrets::delete`; prints `deleted <name>` or
  `no such secret <name>` (exit 0 either way; absence is not an error).

## Crypto / pool / role

- **Key provider:** `OsKeyringProvider::ensure_initialized()` — identical to
  `core/src/main.rs`. Fail-closed with a clear message if the keyring is
  locked/unreachable. Threaded as `&dyn KeyProvider` into the `put` async fn so
  tests can inject a `MapKeyProvider` (the real keyring is flaky/prompting in CI,
  per `key_provider.rs`).
- **Pools (mirrors `pair.rs`):** `put`/`delete` → `connect_admin_pool` (operator
  writes); `list` → `connect_runtime_pool` (SELECT-only). No new migration.
- **AAD:** `None` (see above).

## Audit

`put`/`delete` write one metadata-only row via
`db::audit::insert(&pool, "cli", action, payload)`:
- `secret.put` → `{ "name": <name>, "key_id": <key_id> }`
- `secret.deleted` → `{ "name": <name> }`

**Never** the plaintext, and **not** the value fingerprint — name + key_id only.
`list` does not audit (read-only, like `pair list`).

## Module structure

Two pieces — reusable logic in the core **library**, a thin **CLI** wrapper —
because a `core/tests/` integration test cannot import a binary-private module.

**Library: `kastellan_core::secrets::admin`** (new `core/src/secrets/admin.rs`,
beside the existing `secrets::vault`). DB + crypto + audit, parameterized so the
PG e2e can call it with a `MapKeyProvider`:
- `async fn store_secret(pool: &PgPool, key_provider: &dyn KeyProvider, name: &str, value: &[u8]) -> Result<Outcome, AdminError>`
  — `Outcome` is `Created | Updated` (existence checked before the upsert; label
  is best-effort, the upsert itself is atomic). Validates the name, calls
  `db::secrets::put(.., name, value, None)`, then writes the `secret.put` audit
  row (`{name, key_id}`, `key_id = key_provider.current_id()`).
- `async fn remove_secret(pool: &PgPool, name: &str) -> Result<bool, AdminError>`
  — `db::secrets::delete`; on a real delete, writes the `secret.deleted` audit
  row (`{name}`).
- (`list` is a plain `db::secrets::list()` call — no wrapper needed.)

**CLI: `core/src/bin/kastellan-cli/secret.rs`** mirroring `pair.rs` — thin:
- `pub(crate) fn run(args) -> ExitCode` dispatching `put|list|delete` via
  `common::with_runtime`.
- `put`: admin pool + `OsKeyringProvider::ensure_initialized()`, reads stdin via
  `read_secret_value`, calls `admin::store_secret`, prints outcome.
- `delete`: admin pool, `admin::remove_secret`, prints.
- `list`: runtime pool, `db::secrets::list()`, prints table.
- Pure helpers unit-tested in-module (like `pair.rs`'s `parse_issue_args`):
  `read_secret_value`, `parse_put_args` → `(name, keep_raw)`.
- `main.rs`: `mod secret;` + `"secret" => secret::run(&args[2..])` + usage text.

Both files target < 300 LOC (well under the 500 cap).

## Testing (TDD)

**Unit (no PG, no keyring):**
- `read_secret_value`: strips one `\n`; strips `\r\n`; `--raw` keeps exact bytes
  incl. trailing `\n`; empty (and newline-only without `--raw`) → error.
- `parse_put_args`: `<name>`; `<name> --raw`; `--raw <name>`; missing name →
  error; unknown flag → error.

**e2e (new `core/tests/secret_cli_e2e.rs`, PG-gated, skip-as-pass without PG):**
Calls the library fns `kastellan_core::secrets::admin::{store_secret,
remove_secret}` with a `MapKeyProvider` + the test pool, proving the round-trip
the CLI performs:
1. `store_secret` → `Created`; `db::secrets::get(.., None)` returns the exact
   bytes; `Vault::materialize` succeeds with the name.
2. second `store_secret` (same name) → `Updated`.
3. `db::secrets::list()` includes the name with non-empty `key_id`.
4. `remove_secret` → `true`; `get` now `NotFound`; second `remove_secret` →
   `false`.
5. exactly one `secret.put` audit row exists, its payload has `name`+`key_id`,
   and **no** row's payload contains the plaintext bytes.

## Verification

`cargo test -p kastellan-core` (unit + e2e, the latter live on the DGX / a
PG-backed Mac run), `cargo clippy --workspace --all-targets -D warnings`.
Manual: on the DGX, `printf %s '<pw>' | kastellan-cli secret put matrix_kastellan_password`
then `kastellan-cli secret list`.

## Optional follow-up (noted, not built here)

A migration `REVOKE INSERT, UPDATE, DELETE ON secrets FROM <runtime role>`
(keeping SELECT for the Vault) — defense-in-depth so a daemon/agent compromise
cannot write or delete secrets; only the operator can, via `connect_admin_pool`.
Mirrors migration 0018's least-privilege approach for the pairing tables.
