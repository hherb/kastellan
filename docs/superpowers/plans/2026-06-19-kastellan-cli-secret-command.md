# `kastellan-cli secret` Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an audited operator CLI (`kastellan-cli secret put|list|delete`) over `db::secrets`, so the `@kastellan` Matrix password (and future secrets) can be stored on the daemon host, encrypted with the same `OsKeyringProvider` the daemon uses.

**Architecture:** Reusable logic lives in the core library (`kastellan_core::secrets::admin`) so a PG-gated integration test can drive it with a `MapKeyProvider`; the CLI module `core/src/bin/kastellan-cli/secret.rs` is a thin wrapper that acquires stdin / OS keyring / pool and prints. Mirrors the existing `pair.rs` command exactly.

**Tech Stack:** Rust, sqlx/Postgres, `kastellan_db::secrets` (AES-256-GCM + OS keyring), `kastellan_db::audit`, `thiserror`, `rpassword` (new dep, for the silent TTY prompt).

## Global Constraints

- AGPL-3.0 project; AGPL-compatible deps only. New dep `rpassword` (Apache-2.0/MIT) is compatible.
- `extra_aad = None` on every `db::secrets::put`/`get` call here — matches the Vault's `materialize(.., None)` convention; a non-None AAD would make daemon bootstrap fail.
- Never accept the secret value via argv; never print/audit plaintext (or its fingerprint) — name + key_id only.
- `put`/`delete` write via `connect_admin_pool`; `list` reads via `connect_runtime_pool` (mirrors `pair.rs`).
- Keep each file < 500 LOC. Toolchain rustc 1.96; `cargo clippy --workspace --all-targets -D warnings` must stay clean after every task.
- PG-gated integration tests skip-as-pass without PG; verify live with `KASTELLAN_PG_BIN_DIR=<dir> cargo test -p kastellan-core --test secret_cli_e2e` (Mac PG 18) or on the DGX.

---

### Task 1: Library `kastellan_core::secrets::admin` (store/remove + audit)

**Files:**
- Create: `core/src/secrets/admin.rs`
- Modify: `core/src/secrets/mod.rs` (add `pub mod admin;`)
- Test: `core/tests/secret_cli_e2e.rs` (create; PG-gated)

**Interfaces:**
- Consumes: `kastellan_db::secrets::{put, get, list, delete, KeyProvider}`, `kastellan_db::audit::insert`, `kastellan_db::DbError`.
- Produces (used by Task 2 + the e2e):
  - `pub enum Outcome { Created, Updated }` (derives `Debug, Clone, Copy, PartialEq, Eq`)
  - `pub enum AdminError` (`thiserror`)
  - `pub async fn store_secret(pool: &sqlx::PgPool, key_provider: &dyn KeyProvider, name: &str, value: &[u8]) -> Result<Outcome, AdminError>`
  - `pub async fn remove_secret(pool: &sqlx::PgPool, name: &str) -> Result<bool, AdminError>`

- [ ] **Step 1: Write the failing e2e test**

Create `core/tests/secret_cli_e2e.rs`:

```rust
//! Integration tests for the `kastellan-cli secret` logic
//! (`kastellan_core::secrets::admin`). Mirrors `secret_vault_e2e.rs`:
//! per-test PG cluster via tests_common, real audit_log, MapKeyProvider
//! (no real OS keyring). Skip-as-pass on hosts without PG; on this Mac
//! set `KASTELLAN_PG_BIN_DIR` to run live.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::secrets::admin::{remove_secret, store_secret, Outcome};
use kastellan_core::secrets::Vault;
use kastellan_db::secrets::{MapKeyProvider, KEY_LEN};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};
use sqlx::Row;

const TEST_KEY_ID: &str = "test-keyring";

fn test_key_provider() -> MapKeyProvider {
    MapKeyProvider::new(TEST_KEY_ID, [42u8; KEY_LEN])
}

async fn probe_and_pool(spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "secret-cli-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(spec)
        .await
        .expect("connect runtime pool")
}

#[tokio::test(flavor = "multi_thread")]
async fn store_list_delete_roundtrip_with_clean_audit() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "seccli-1",
        "seccli-1-log",
        &format!("kastellan-test-seccli-1-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();

    // 1. store -> Created; round-trips through db::secrets::get + Vault.
    let o = store_secret(&pool, &kp, "matrix_pw", b"hunter2-token")
        .await
        .expect("store");
    assert_eq!(o, Outcome::Created);
    let got = kastellan_db::secrets::get(&pool, &kp, "matrix_pw", None)
        .await
        .expect("get");
    assert_eq!(got.as_slice(), b"hunter2-token");
    let r = Vault::new()
        .materialize(&pool, &kp, "matrix_pw", "test")
        .await
        .expect("materialize");
    assert!(r.as_str().starts_with("secret://"));

    // 2. store same name again -> Updated.
    let o2 = store_secret(&pool, &kp, "matrix_pw", b"hunter2-rotated")
        .await
        .expect("store2");
    assert_eq!(o2, Outcome::Updated);

    // 3. list includes it with a non-empty key_id.
    let rows = kastellan_db::secrets::list(&pool).await.expect("list");
    assert!(rows.iter().any(|s| s.name == "matrix_pw" && !s.key_id.is_empty()));

    // 4. every secret.put audit row is metadata-only (name + key_id, NO plaintext).
    let put_rows = sqlx::query(
        "SELECT payload::text AS p FROM audit_log WHERE actor='cli' AND action='secret.put'",
    )
    .fetch_all(&pool)
    .await
    .expect("audit put query");
    assert_eq!(put_rows.len(), 2, "one secret.put per store");
    for row in &put_rows {
        let p: String = row.try_get("p").unwrap();
        assert!(p.contains("matrix_pw"), "payload names the secret");
        assert!(p.contains(TEST_KEY_ID), "payload carries key_id");
        assert!(!p.contains("hunter2"), "payload MUST NOT contain plaintext");
    }

    // 5. delete -> true, then gone, then false; one secret.deleted row.
    assert!(remove_secret(&pool, "matrix_pw").await.expect("rm"));
    assert!(kastellan_db::secrets::get(&pool, &kp, "matrix_pw", None)
        .await
        .is_err());
    assert!(!remove_secret(&pool, "matrix_pw").await.expect("rm2"));
    let del_rows = sqlx::query(
        "SELECT payload::text AS p FROM audit_log WHERE actor='cli' AND action='secret.deleted'",
    )
    .fetch_all(&pool)
    .await
    .expect("audit del query");
    assert_eq!(del_rows.len(), 1);
}
```

- [ ] **Step 2: Run the test to verify it fails (compile error: `admin` missing)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test secret_cli_e2e 2>&1 | tail -20`
Expected: FAIL — `unresolved import kastellan_core::secrets::admin` (the module doesn't exist yet).

- [ ] **Step 3: Create the `admin` module**

Create `core/src/secrets/admin.rs`:

```rust
//! Operator-facing write path for `db::secrets`, used by the
//! `kastellan-cli secret` command. Kept in the library (not the CLI
//! binary) so the PG integration test can drive it with a
//! `MapKeyProvider` instead of the real OS keyring.
//!
//! All calls use `extra_aad = None` to match the Vault's
//! `materialize(.., None)` convention — a non-None AAD here would make
//! the daemon's bootstrap materialize fail. Audit rows are
//! metadata-only (name + key_id); the plaintext never appears.

use sqlx::PgPool;

use kastellan_db::secrets::KeyProvider;

/// Whether a `store_secret` created a new row or updated an existing one.
/// The label is best-effort (existence is checked before the upsert,
/// which is itself atomic) — purely for the operator-facing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Updated,
}

/// Errors from the secret admin path.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("secret admin: {0}")]
    Secrets(#[from] kastellan_db::secrets::SecretsError),

    /// Audit write failed. No `#[from]`: `DbError` is the crate-wide
    /// error for `kastellan_db`; an explicit map keeps a future DbError
    /// from being swallowed silently (mirrors `VaultError::Audit`).
    #[error("secret admin: audit insert failed: {0}")]
    Audit(kastellan_db::DbError),
}

/// UPSERT a named secret, then write a metadata-only `secret.put` audit
/// row. Returns whether the row was created or updated.
pub async fn store_secret(
    pool: &PgPool,
    key_provider: &dyn KeyProvider,
    name: &str,
    value: &[u8],
) -> Result<Outcome, AdminError> {
    // Existence pre-check for the created/updated label. `list` is
    // metadata-only and cheap (single-user server, few secrets).
    let existed = kastellan_db::secrets::list(pool)
        .await?
        .iter()
        .any(|s| s.name == name);

    kastellan_db::secrets::put(pool, key_provider, name, value, None).await?;

    let key_id = key_provider.current_id();
    kastellan_db::audit::insert(
        pool,
        "cli",
        "secret.put",
        serde_json::json!({ "name": name, "key_id": key_id }),
    )
    .await
    .map_err(AdminError::Audit)?;

    Ok(if existed {
        Outcome::Updated
    } else {
        Outcome::Created
    })
}

/// Delete a named secret. Writes a `secret.deleted` audit row only when
/// a row was actually removed. Returns whether anything was deleted.
pub async fn remove_secret(pool: &PgPool, name: &str) -> Result<bool, AdminError> {
    let deleted = kastellan_db::secrets::delete(pool, name).await?;
    if deleted {
        kastellan_db::audit::insert(
            pool,
            "cli",
            "secret.deleted",
            serde_json::json!({ "name": name }),
        )
        .await
        .map_err(AdminError::Audit)?;
    }
    Ok(deleted)
}
```

- [ ] **Step 4: Wire the module into the secrets facade**

Modify `core/src/secrets/mod.rs` — add alongside the existing `pub mod vault;` lines:

```rust
pub mod admin;
```

- [ ] **Step 5: Run the test to verify it passes (live PG) or skips (no PG)**

Run (Mac, live PG): `source "$HOME/.cargo/env" && KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" cargo test -p kastellan-core --test secret_cli_e2e -- --nocapture`
Expected: PASS (`store_list_delete_roundtrip_with_clean_audit ... ok`). Without `KASTELLAN_PG_BIN_DIR` the test returns early (skip-as-pass) and still shows `ok`.

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: clean.

```bash
git add core/src/secrets/admin.rs core/src/secrets/mod.rs core/tests/secret_cli_e2e.rs
git commit -m "feat(secrets): admin store/remove for the secret CLI + e2e"
```

---

### Task 2: CLI `secret` command (thin wrapper + pure helpers)

**Files:**
- Create: `core/src/bin/kastellan-cli/secret.rs`
- Modify: `core/src/bin/kastellan-cli/main.rs` (add `mod secret;`, dispatch arm, usage text)
- Modify: `core/Cargo.toml` (add `rpassword` dep)

**Interfaces:**
- Consumes: `kastellan_core::secrets::admin::{store_secret, remove_secret, Outcome}`, `kastellan_db::secrets::{list, OsKeyringProvider}`, `kastellan_db::pool::{connect_admin_pool, connect_runtime_pool}`, `crate::common::{resolve_connect_spec, with_runtime}`.
- Produces: `pub(crate) fn run(args: &[String]) -> ExitCode`; pure helpers `read_secret_value`, `parse_put_args`.

- [ ] **Step 1: Add the `rpassword` dependency**

Modify `core/Cargo.toml` — under `[dependencies]`, add (Apache-2.0/MIT; AGPL-compatible):

```toml
rpassword = "7"
```

- [ ] **Step 2: Write the failing unit tests for the pure helpers**

Create `core/src/bin/kastellan-cli/secret.rs` with ONLY the helpers + tests for now:

```rust
//! `secret {put, list, delete}` — operator management of `db::secrets`
//! (Matrix Phase D Task 5 slice 0). Thin wrapper over
//! `kastellan_core::secrets::admin`; see
//! docs/superpowers/specs/2026-06-19-kastellan-cli-secret-command-design.md.

use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use kastellan_core::secrets::admin::{remove_secret, store_secret, Outcome};
use kastellan_db::pool::{connect_admin_pool, connect_runtime_pool};

use crate::common::{resolve_connect_spec, with_runtime};

/// Turn raw stdin bytes into the secret value. Unless `keep_raw`, strips
/// exactly one trailing `\n` (and a preceding `\r`) so `echo pw |` and
/// `printf %s pw |` both store the same bytes. Empty result is rejected.
pub(crate) fn read_secret_value(raw: &[u8], keep_raw: bool) -> Result<Vec<u8>, String> {
    let mut bytes = raw.to_vec();
    if !keep_raw && bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.is_empty() {
        return Err("empty secret value (nothing on stdin)".to_string());
    }
    Ok(bytes)
}

/// Parse `secret put <name> [--raw]` → `(name, keep_raw)`.
pub(crate) fn parse_put_args(args: &[String]) -> Result<(String, bool), String> {
    let mut name: Option<String> = None;
    let mut keep_raw = false;
    for a in args {
        match a.as_str() {
            "--raw" => {
                if keep_raw {
                    return Err("--raw given twice".to_string());
                }
                keep_raw = true;
            }
            s if s.starts_with("--") => return Err(format!("unknown flag {s}")),
            s => {
                if name.is_some() {
                    return Err(format!("unexpected argument {s}"));
                }
                name = Some(s.to_string());
            }
        }
    }
    let name = name.ok_or("put requires <name>")?;
    Ok((name, keep_raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_value_strips_one_trailing_newline() {
        assert_eq!(read_secret_value(b"hunter2\n", false).unwrap(), b"hunter2");
        assert_eq!(read_secret_value(b"hunter2\r\n", false).unwrap(), b"hunter2");
        assert_eq!(read_secret_value(b"hunter2", false).unwrap(), b"hunter2");
        // only ONE newline stripped
        assert_eq!(read_secret_value(b"hunter2\n\n", false).unwrap(), b"hunter2\n");
    }

    #[test]
    fn read_value_raw_keeps_exact_bytes() {
        assert_eq!(read_secret_value(b"hunter2\n", true).unwrap(), b"hunter2\n");
        assert_eq!(read_secret_value(b"a\nb\n", true).unwrap(), b"a\nb\n");
    }

    #[test]
    fn read_value_rejects_empty() {
        assert!(read_secret_value(b"", false).is_err());
        assert!(read_secret_value(b"\n", false).is_err()); // strips to empty
        assert!(read_secret_value(b"", true).is_err());
    }

    #[test]
    fn parse_put_name_and_raw() {
        assert_eq!(parse_put_args(&["s".into()]).unwrap(), ("s".to_string(), false));
        assert_eq!(
            parse_put_args(&["s".into(), "--raw".into()]).unwrap(),
            ("s".to_string(), true)
        );
        assert_eq!(
            parse_put_args(&["--raw".into(), "s".into()]).unwrap(),
            ("s".to_string(), true)
        );
    }

    #[test]
    fn parse_put_rejects_bad_args() {
        assert!(parse_put_args(&[]).is_err()); // missing name
        assert!(parse_put_args(&["--bogus".into()]).is_err());
        assert!(parse_put_args(&["a".into(), "b".into()]).is_err()); // two names
        assert!(parse_put_args(&["a".into(), "--raw".into(), "--raw".into()]).is_err());
    }
}
```

- [ ] **Step 3: Run the helper tests to verify they fail (module not yet declared)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --bin kastellan-cli secret:: 2>&1 | tail -20`
Expected: FAIL — `file not included in module tree` / unresolved, because `main.rs` has no `mod secret;` yet (added in Step 5).

- [ ] **Step 4: Append the `run` dispatch + command wrappers to `secret.rs`**

Add to `core/src/bin/kastellan-cli/secret.rs` (above the `#[cfg(test)] mod tests` block):

```rust
pub(crate) fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("put") => with_runtime("secret put", secret_put(&args[1..])),
        Some("list") => with_runtime("secret list", secret_list(&args[1..])),
        Some("delete") => with_runtime("secret delete", secret_delete(&args[1..])),
        _ => {
            eprintln!("usage: kastellan-cli secret <put|list|delete> ...");
            ExitCode::from(2)
        }
    }
}

async fn secret_put(args: &[String]) -> ExitCode {
    let (name, keep_raw) = match parse_put_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}\nusage: kastellan-cli secret put <name> [--raw]");
            return ExitCode::from(2);
        }
    };

    // Read the value: silent prompt on a TTY, raw stdin when piped.
    let value: Vec<u8> = if std::io::stdin().is_terminal() {
        match rpassword::prompt_password(format!("Value for secret {name:?}: ")) {
            Ok(s) => s.into_bytes(),
            Err(e) => {
                eprintln!("read secret: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        let mut raw = Vec::new();
        if let Err(e) = std::io::stdin().read_to_end(&mut raw) {
            eprintln!("read stdin: {e}");
            return ExitCode::from(1);
        }
        match read_secret_value(&raw, keep_raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_admin_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let kp = match kastellan_db::secrets::OsKeyringProvider::ensure_initialized() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("keyring: {e}");
            return ExitCode::from(1);
        }
    };

    match store_secret(&pool, &kp, &name, &value).await {
        Ok(Outcome::Created) => {
            println!("stored {name} (created)");
            ExitCode::from(0)
        }
        Ok(Outcome::Updated) => {
            println!("stored {name} (updated)");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("secret put: {e}");
            ExitCode::from(1)
        }
    }
}

async fn secret_list(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("usage: kastellan-cli secret list");
        return ExitCode::from(2);
    }
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match kastellan_db::secrets::list(&pool).await {
        Ok(rows) => {
            if rows.is_empty() {
                println!("(no secrets)");
                return ExitCode::from(0);
            }
            for s in rows {
                println!("{}\t{}\t{}\t{}", s.name, s.key_id, s.created_at, s.updated_at);
            }
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("secret list: {e}");
            ExitCode::from(1)
        }
    }
}

async fn secret_delete(args: &[String]) -> ExitCode {
    let name = match args {
        [n] => n.clone(),
        _ => {
            eprintln!("usage: kastellan-cli secret delete <name>");
            return ExitCode::from(2);
        }
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_admin_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    match remove_secret(&pool, &name).await {
        Ok(true) => {
            println!("deleted {name}");
            ExitCode::from(0)
        }
        Ok(false) => {
            println!("no such secret {name}");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("secret delete: {e}");
            ExitCode::from(1)
        }
    }
}
```

- [ ] **Step 5: Declare the module + dispatch in `main.rs`**

In `core/src/bin/kastellan-cli/main.rs`: add `mod secret;` beside the other `mod` lines (e.g. after `mod pair;`), and add a dispatch arm beside `"pair" => pair::run(&args[2..]),`:

```rust
"secret"      => secret::run(&args[2..]),
```

Also add usage lines to the top-of-file `//!` doc block and the printed help text, next to the `pair` entries:

```
    kastellan-cli secret put    <name> [--raw]   # value read from stdin
    kastellan-cli secret list
    kastellan-cli secret delete <name>
```

- [ ] **Step 6: Run the helper unit tests — verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --bin kastellan-cli secret:: -- --nocapture`
Expected: PASS (`read_value_strips_one_trailing_newline`, `read_value_raw_keeps_exact_bytes`, `read_value_rejects_empty`, `parse_put_name_and_raw`, `parse_put_rejects_bad_args` — 5 ok).

- [ ] **Step 7: Build + clippy the whole workspace**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bin kastellan-cli && cargo clippy --workspace --all-targets -- -D warnings`
Expected: builds; clippy clean.

- [ ] **Step 8: Commit**

```bash
git add core/src/bin/kastellan-cli/secret.rs core/src/bin/kastellan-cli/main.rs core/Cargo.toml Cargo.lock
git commit -m "feat(cli): kastellan-cli secret put|list|delete"
```

---

### Task 3: Verify end-to-end + record

**Files:** none (verification + handover).

- [ ] **Step 1: Full workspace test + clippy (Mac, live PG)**

Run:
```bash
source "$HOME/.cargo/env"
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test secret_cli_e2e -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: e2e PASS (live), clippy clean. (Optionally re-run the full suite on the DGX.)

- [ ] **Step 2: Manual smoke (on the daemon host — the DGX, as the daemon's OS user)**

```bash
printf %s '<the @kastellan password>' | kastellan-cli secret put matrix_kastellan_password
kastellan-cli secret list      # shows matrix_kastellan_password + key_id, no value
```
Expected: `stored matrix_kastellan_password (created)`, then a list row. (This is the operational step that stores the Matrix bot password; it must run on the DGX so the daemon's keyring can decrypt it.)

- [ ] **Step 3: Update HANDOVER/ROADMAP**

Note in `docs/devel/handovers/HANDOVER.md` that Task 5 slice 0 (the `secret` CLI) shipped, and that the next slice is the matrix-worker spawn reading `matrix_kastellan_password` into `KASTELLAN_MATRIX_PASSWORD`. Commit.

---

## Self-Review

- **Spec coverage:** surface put/list/delete ✓ (Task 2); no plaintext readback ✓ (no `get` command); stdin + strip-one-newline + `--raw` ✓ (`read_secret_value`, Task 2 Step 2); TTY silent prompt ✓ (`rpassword`, Task 2 Step 4); `OsKeyringProvider` + `extra_aad=None` ✓ (admin.rs, Task 1); admin pool for writes / runtime for list ✓ (Task 2); audit metadata-only ✓ (admin.rs + e2e assertion, Tasks 1); lib placement for testability ✓ (Task 1); unit + PG-gated e2e ✓. Optional REVOKE migration explicitly deferred (spec §"Optional follow-up") — not a task.
- **Placeholder scan:** none — every step has concrete code/commands.
- **Type consistency:** `store_secret`/`remove_secret`/`Outcome`/`AdminError` signatures identical in Task 1 (definition), the Task 1 e2e, and Task 2 (consumer). `read_secret_value(&[u8], bool) -> Result<Vec<u8>, String>` and `parse_put_args(&[String]) -> Result<(String, bool), String>` consistent between definition and use.
