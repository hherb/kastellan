# Entry-kind-aware `tool_allowlists` validation + NetScreen backstop — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close #459 residual #3 by making `tool_allowlists` validate domain-kind entries (web-fetch/web-research/browser-driver) distinctly from argv0-kind entries (shell-exec) — unblocking domain rows and rejecting the `localhost:8888` footgun at the source — plus a NetScreen backstop.

**Architecture:** Four layers, source → backstop. (1) A pure `EntryKind` + `validate_domain` in the `db` crate; (2) a migration `0021` union-branch CHECK admitting argv0 paths, domains, and bracketed IPv6; (3) a `WorkerManifest::allowlist_kind()` classification the CLI uses to pick the validator; (4) a `NetScreen` predicate that treats a malformed double-port entry as statically dead.

**Tech Stack:** Rust (rustc 1.96.0), `sqlx` (runtime queries — no compile-time `query!`, no `.sqlx`/`DATABASE_URL` needed), Postgres 18, `std::net::Ipv6Addr` (std — no new dependency).

**Spec:** `docs/superpowers/specs/2026-07-18-tool-allowlists-entry-kind-validation-design.md`

## Global Constraints

- **No new dependency.** Hand-rolled domain validator; IPv6 via `std::net::Ipv6Addr`. AGPL-compatible deps only (no new dep at all here).
- **Cross-platform (Linux + macOS first-class).** Pure Rust + one SQL migration; no OS-specific code. Only the live-PG tests are host-gated.
- **Migrations are immutable once applied.** Add a **new** file `0021_…`; never edit `0009`. `sqlx::migrate!("./migrations")` auto-discovers it — no `MIGRATOR` Rust change.
- **TDD; all tests pass before commit.** `cargo clippy --workspace --all-targets -D warnings` clean at every commit.
- **Files under ~500 lines where feasible.** `db/src/tool_allowlists.rs` is ~309 lines today; re-`wc -l` after Task 1 and lift `#[cfg(test)] mod tests` into `tool_allowlists/tests.rs` if it crosses 500.
- **argv0-kind validation is UNCHANGED.** Same acceptance set, same behaviour for `shell-exec`.
- **Cargo needs the env sourced** in non-interactive shells: `source "$HOME/.cargo/env"` before any `cargo` command.
- **Live-PG tests:** run individually on the Mac via the session-local `KASTELLAN_PG_BIN_DIR` override (a full-workspace live-PG run flakes at PG bring-up), or on the DGX. **The DGX (native aarch64, real PG) is the authoritative gate for the migration/CHECK + CLI e2e** (Tasks 5–6).

---

## File Structure

- `db/src/tool_allowlists.rs` — **modify**: add `EntryKind`, `InvalidDomain`, `validate_domain`, `validate_entry`; add `kind` param to `add`/`remove` (Tasks 1, 3).
- `db/migrations/0021_tool_allowlists_domain_entries.sql` — **create**: union-branch CHECK (Task 5).
- `core/src/worker_manifest.rs` — **modify**: `WorkerManifest::allowlist_kind()` default method (Task 2).
- `core/src/workers/shell_exec.rs` — **modify**: `allowlist_kind() → Argv0` (Task 2).
- `core/src/workers/{web_fetch,web_research,browser_driver}.rs` — **modify**: `allowlist_kind() → Domain` (Task 2).
- `core/src/registry_build.rs` — **modify**: `allowlist_kind_for_tool` helper + test (Task 2).
- `core/src/cli_audit/registry.rs` — **modify**: `tools_allowlist_{add,remove}_and_audit` take + forward `kind` (Task 3).
- `core/src/bin/kastellan-cli/tools_allowlist.rs` — **modify**: resolve kind, add `InvalidDomain` error arm, usage text (Task 3).
- `core/src/workers/endpoint_guard.rs` — **modify**: `host_is_malformed` + wire into `screen_net_allowlist` (Task 4).
- `core/tests/tool_allowlists_check_e2e.rs` — **create**: raw-SQL CHECK behaviour (Task 5, DGX-gated).
- `core/tests/cli_tools_allowlist_e2e.rs` — **modify**: kind-aware CLI add e2e (Task 6, DGX-gated).

---

## Task 1: DB — `EntryKind` + pure `validate_domain` + `validate_entry`

**Files:**
- Modify: `db/src/tool_allowlists.rs` (add enum, error variant, two pure fns, tests)

**Interfaces:**
- Consumes: existing `ToolAllowlistError`, `validate_argv0`.
- Produces:
  - `pub enum EntryKind { Argv0, Domain }` (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`)
  - `pub fn validate_domain(entry: &str) -> Result<(), ToolAllowlistError>`
  - `pub fn validate_entry(kind: EntryKind, entry: &str) -> Result<(), ToolAllowlistError>`
  - `ToolAllowlistError::InvalidDomain`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `db/src/tool_allowlists.rs`:

```rust
#[test]
fn validate_domain_accepts_domains_wildcards_ipv4_and_bracketed_ipv6() {
    for ok in [
        "example.org",
        "api.example.org",
        ".example.org",        // wildcard
        "example.org.",        // FQDN trailing dot
        "a-b.example.org",     // hyphen inside a label
        "203.0.113.5",         // bare IPv4
        "[::1]",               // IPv6 loopback (bracketed)
        "[2606:4700:4700::1111]",
        "[fd12:3456::1]",      // ULA
    ] {
        validate_domain(ok).unwrap_or_else(|e| panic!("{ok} should be valid: {e}"));
    }
}

#[test]
fn validate_domain_rejects_ports_schemes_paths_and_malformed() {
    for bad in [
        "",
        "localhost:8888",       // embedded port — the #459 residual-#3 footgun
        "http://example.org",   // scheme
        "example.org/search",   // path
        "user@example.org",     // userinfo
        "a..b",                 // empty label
        "-a.example.org",       // leading hyphen
        "a-.example.org",       // trailing hyphen
        "::1",                  // unbracketed IPv6
        "[not-ipv6]",           // brackets but not an IPv6 addr
        "exa mple.org",         // whitespace
        "foo\tbar",             // control char
    ] {
        assert!(
            matches!(validate_domain(bad), Err(ToolAllowlistError::InvalidDomain)),
            "{bad:?} should be InvalidDomain"
        );
    }
}

#[test]
fn validate_entry_dispatches_by_kind() {
    validate_entry(EntryKind::Argv0, "/bin/echo").unwrap();
    assert!(matches!(
        validate_entry(EntryKind::Argv0, "example.org"),
        Err(ToolAllowlistError::InvalidArgv0)
    ));
    validate_entry(EntryKind::Domain, "example.org").unwrap();
    assert!(matches!(
        validate_entry(EntryKind::Domain, "localhost:8888"),
        Err(ToolAllowlistError::InvalidDomain)
    ));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-db validate_domain 2>&1 | tail -20
```

Expected: FAIL to **compile** (`cannot find type EntryKind`, `cannot find function validate_domain`, `no variant InvalidDomain`).

- [ ] **Step 3: Add the enum, error variant, and two pure functions**

At the top of `db/src/tool_allowlists.rs`, add the std import next to the existing `use`s:

```rust
use std::net::Ipv6Addr;
```

Add the new error variant inside `enum ToolAllowlistError` (after `Argv0HasDotDot`):

```rust
    #[error("allowlist entry is not a valid host/domain; expected a bare domain \
             (example.org), a wildcard (.example.org), a bare IPv4, or a bracketed \
             IPv6 literal ([::1]) — no scheme, port, path, '@', or whitespace")]
    InvalidDomain,
```

Add the enum after `MAX_TOOL_NAME_LEN`:

```rust
/// Which shape an entry in `tool_allowlists` takes for a given tool. A tool is
/// entirely one kind or the other — it is a function of the tool, never mixed:
/// `shell-exec` stores argv0 exec paths; the web workers store domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Absolute `argv[0]` exec path — validated by [`validate_argv0`].
    Argv0,
    /// Host / domain allowlist entry — validated by [`validate_domain`].
    Domain,
}
```

Add the two functions after `validate_argv0`:

```rust
/// Validate a domain-kind allowlist entry: a bare domain (`example.org`), a
/// wildcard (`.example.org`), a bare IPv4 (`203.0.113.5`), or a **bracketed**
/// IPv6 literal (`[::1]`). Rejects anything carrying a scheme, embedded port,
/// path, userinfo (`@`), or whitespace — so `localhost:8888` (the #459
/// residual-#3 footgun) is rejected here at the source, before it can become
/// the dead net entry `localhost:8888:443`.
///
/// Brackets are REQUIRED for IPv6 so the downstream `host:443` mapping
/// (`allowlist_to_net_entries`) yields a valid `[::1]:443` and the bracket-aware
/// `host_of_entry` strips it back cleanly. A bare `::1` is rejected.
///
/// Hand-rolled (no `url`/idna dependency — IPv6 via `std::net::Ipv6Addr`), LDH
/// label rules, matching the style of [`validate_argv0`]. The SQL CHECK in
/// migration `0021` is a coarser shape backstop; this is the authoritative gate.
pub fn validate_domain(entry: &str) -> Result<(), ToolAllowlistError> {
    if entry.is_empty() {
        return Err(ToolAllowlistError::InvalidDomain);
    }
    // No control chars, whitespace, or NUL anywhere (bytes 0x00..=0x20 and DEL).
    if entry.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(ToolAllowlistError::InvalidDomain);
    }
    // Bracketed IPv6 literal: the inner text must parse as an Ipv6Addr.
    if let Some(inner) = entry.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return match inner.parse::<Ipv6Addr>() {
            Ok(_) => Ok(()),
            Err(_) => Err(ToolAllowlistError::InvalidDomain),
        };
    }
    // Domain / IPv4 branch. Strip one optional wildcard leading dot and one
    // optional FQDN trailing dot, then validate LDH labels.
    let host = entry.strip_prefix('.').unwrap_or(entry);
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return Err(ToolAllowlistError::InvalidDomain);
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(ToolAllowlistError::InvalidDomain);
        }
        if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(ToolAllowlistError::InvalidDomain);
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ToolAllowlistError::InvalidDomain);
        }
    }
    Ok(())
}

/// Dispatch to the right validator for a tool's [`EntryKind`]. `add`/`remove`
/// call this so the DB layer applies argv0 rules to argv0 tools and domain
/// rules to domain tools.
pub fn validate_entry(kind: EntryKind, entry: &str) -> Result<(), ToolAllowlistError> {
    match kind {
        EntryKind::Argv0 => validate_argv0(entry),
        EntryKind::Domain => validate_domain(entry),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-db tool_allowlists 2>&1 | tail -20
```

Expected: PASS (all `validate_domain`/`validate_entry` tests + the unchanged `validate_argv0`/`validate_tool_name` tests).

- [ ] **Step 5: Check file size + clippy, then commit**

```sh
source "$HOME/.cargo/env"
wc -l db/src/tool_allowlists.rs   # if > 500, lift `mod tests` to tool_allowlists/tests.rs
cargo clippy -p kastellan-db --all-targets -- -D warnings 2>&1 | tail -5
git add db/src/tool_allowlists.rs
git commit -m "feat(db): EntryKind + pure validate_domain/validate_entry for tool_allowlists (#459)"
```

If `wc -l` reports > 500, before committing: create `db/src/tool_allowlists/tests.rs` with the `#[cfg(test)]` module contents, replace the inline module in `tool_allowlists.rs` with `#[cfg(test)] mod tests;`, rename `tool_allowlists.rs` → `tool_allowlists/mod.rs` (or keep the file and add `#[path]`), re-run `cargo test -p kastellan-db`, and `git add` both files.

---

## Task 2: Core — `WorkerManifest::allowlist_kind()` + `allowlist_kind_for_tool`

**Files:**
- Modify: `core/src/worker_manifest.rs` (default trait method)
- Modify: `core/src/workers/shell_exec.rs` (`Argv0`)
- Modify: `core/src/workers/web_fetch.rs`, `core/src/workers/web_research.rs`, `core/src/workers/browser_driver.rs` (`Domain`)
- Modify: `core/src/registry_build.rs` (helper + test)

**Interfaces:**
- Consumes: `kastellan_db::tool_allowlists::EntryKind` (Task 1); existing `WORKER_MANIFESTS`, `WorkerManifest::allowlist_tool`.
- Produces:
  - `WorkerManifest::allowlist_kind(&self) -> Option<kastellan_db::tool_allowlists::EntryKind>` (default `None`)
  - `pub fn allowlist_kind_for_tool(name: &str) -> Option<EntryKind>` in `registry_build`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `core/src/registry_build.rs`:

```rust
#[test]
fn allowlist_kind_for_tool_maps_argv0_and_domain_tools() {
    use kastellan_db::tool_allowlists::EntryKind;
    assert_eq!(allowlist_kind_for_tool("shell-exec"), Some(EntryKind::Argv0));
    assert_eq!(allowlist_kind_for_tool("web-fetch"), Some(EntryKind::Domain));
    assert_eq!(allowlist_kind_for_tool("web-research"), Some(EntryKind::Domain));
    assert_eq!(allowlist_kind_for_tool("browser-driver"), Some(EntryKind::Domain));
    // A worker with no allowlist, and an unknown name, both map to None.
    assert_eq!(allowlist_kind_for_tool("python-exec"), None);
    assert_eq!(allowlist_kind_for_tool("nonexistent-tool"), None);
}
```

- [ ] **Step 2: Run the test to verify it fails**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core allowlist_kind_for_tool 2>&1 | tail -20
```

Expected: FAIL to compile (`cannot find function allowlist_kind_for_tool`).

- [ ] **Step 3: Add the trait method, the four impls, and the helper**

In `core/src/worker_manifest.rs`, add the defaulted method to `trait WorkerManifest`, right after `allowlist_tool`:

```rust
    /// The shape of this worker's `tool_allowlists` entries, when it declares
    /// an allowlist. `None` ⇒ no allowlist (the default). Drives which
    /// validator the CLI applies on `add`/`remove` (argv0 paths vs domains).
    fn allowlist_kind(&self) -> Option<kastellan_db::tool_allowlists::EntryKind> {
        None
    }
```

In `core/src/workers/shell_exec.rs`, add to `impl WorkerManifest for ShellExecManifest`, next to `allowlist_tool`:

```rust
    fn allowlist_kind(&self) -> Option<kastellan_db::tool_allowlists::EntryKind> {
        Some(kastellan_db::tool_allowlists::EntryKind::Argv0)
    }
```

In each of `core/src/workers/web_fetch.rs`, `core/src/workers/web_research.rs`, `core/src/workers/browser_driver.rs`, add to the respective `impl WorkerManifest`, next to `allowlist_tool`:

```rust
    fn allowlist_kind(&self) -> Option<kastellan_db::tool_allowlists::EntryKind> {
        Some(kastellan_db::tool_allowlists::EntryKind::Domain)
    }
```

In `core/src/registry_build.rs`, add the helper after the `WORKER_MANIFESTS` static (add `use kastellan_db::tool_allowlists::EntryKind;` at the top of the file if not already imported):

```rust
/// The kind of `tool_allowlists` entry a tool uses, discovered by scanning the
/// static manifest list. `None` for a tool that declares no allowlist or an
/// unrecognized name — the CLI treats `None` as the argv0 default, preserving
/// today's behaviour. Pure.
pub fn allowlist_kind_for_tool(name: &str) -> Option<EntryKind> {
    WORKER_MANIFESTS
        .iter()
        .find(|m| m.allowlist_tool() == Some(name))
        .and_then(|m| m.allowlist_kind())
}
```

- [ ] **Step 4: Run the test to verify it passes**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core allowlist_kind_for_tool 2>&1 | tail -20
```

Expected: PASS.

- [ ] **Step 5: clippy + commit**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
git add core/src/worker_manifest.rs core/src/workers/shell_exec.rs \
  core/src/workers/web_fetch.rs core/src/workers/web_research.rs \
  core/src/workers/browser_driver.rs core/src/registry_build.rs
git commit -m "feat(core): WorkerManifest::allowlist_kind + allowlist_kind_for_tool (#459)"
```

---

## Task 3: Wire the kind through `add`/`remove` → cli_audit → CLI

**Files:**
- Modify: `db/src/tool_allowlists.rs` (`add`/`remove` gain `kind: EntryKind`)
- Modify: `core/src/cli_audit/registry.rs` (`tools_allowlist_{add,remove}_and_audit` gain + forward `kind`)
- Modify: `core/src/bin/kastellan-cli/tools_allowlist.rs` (resolve kind, `InvalidDomain` arm, usage text)
- Modify: `db/tests/postgres_e2e.rs` (**9 call sites, lines ~1408–1531** — `add`/`remove` in
  `tool_allowlists_round_trip_and_grant_shape` + the validation-rejection test; all argv0-kind,
  so each gains `EntryKind::Argv0`. Discovered during Task 1's DGX run — this file was missed by
  the original caller sweep.)

**Interfaces:**
- Consumes: `EntryKind`, `validate_entry` (Task 1); `allowlist_kind_for_tool` (Task 2).
- Produces:
  - `db::add(pool, tool, kind: EntryKind, entry, created_by)` and `db::remove(pool, tool, kind: EntryKind, entry)`
  - `cli_audit::tools_allowlist_add_and_audit(pool, tool, kind: EntryKind, argv0)` and `…remove_and_audit(pool, tool, kind, argv0)`

This is a wiring task: its behaviour is covered by `validate_entry` (Task 1) and the live e2e (Task 6). The gate is compile + clippy + no regression of the existing shell-exec CLI e2e.

- [ ] **Step 1: Change the DB `add`/`remove` to take + apply the kind**

In `db/src/tool_allowlists.rs`, replace the `add` signature/validation and `remove` signature/validation. `add` becomes:

```rust
pub async fn add(
    pool: &PgPool,
    tool: &str,
    kind: EntryKind,
    argv0: &str,
    created_by: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_entry(kind, argv0)?;
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
```

`remove` becomes (same change — take `kind`, call `validate_entry`):

```rust
pub async fn remove(
    pool: &PgPool,
    tool: &str,
    kind: EntryKind,
    argv0: &str,
) -> Result<bool, ToolAllowlistError> {
    validate_tool_name(tool)?;
    validate_entry(kind, argv0)?;
    let rows = sqlx::query(
        "DELETE FROM tool_allowlists WHERE tool = $1 AND argv0 = $2",
    )
    .bind(tool)
    .bind(argv0)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() == 1)
}
```

- [ ] **Step 2: Forward the kind through the cli_audit wrappers**

In `core/src/cli_audit/registry.rs`, add `use kastellan_db::tool_allowlists::EntryKind;` and update both wrappers. `tools_allowlist_add_and_audit`:

```rust
pub async fn tools_allowlist_add_and_audit(
    pool: &PgPool,
    tool: &str,
    kind: EntryKind,
    argv0: &str,
) -> Result<bool, kastellan_db::tool_allowlists::ToolAllowlistError> {
    let inserted = kastellan_db::tool_allowlists::add(pool, tool, kind, argv0, CLI_AUDIT_ACTOR).await?;
    // ... audit block unchanged (payload stays {tool, argv0}) ...
```

`tools_allowlist_remove_and_audit`:

```rust
pub async fn tools_allowlist_remove_and_audit(
    pool: &PgPool,
    tool: &str,
    kind: EntryKind,
    argv0: &str,
) -> Result<bool, kastellan_db::tool_allowlists::ToolAllowlistError> {
    let removed = kastellan_db::tool_allowlists::remove(pool, tool, kind, argv0).await?;
    // ... audit block unchanged ...
```

(Leave the audit payload as `{tool, argv0}` — the kind is derivable from the tool, no payload change.)

- [ ] **Step 3: Resolve the kind + surface `InvalidDomain` in the CLI**

In `core/src/bin/kastellan-cli/tools_allowlist.rs`, in `tools_allowlist_add`, after the `(tool, argv0)` destructure and before the DB call, resolve the kind and pass it; also add `InvalidDomain` to the exit-2 arm. Replace the call site:

```rust
    let kind = kastellan_core::registry_build::allowlist_kind_for_tool(&tool)
        .unwrap_or(kastellan_db::tool_allowlists::EntryKind::Argv0);

    match tools_allowlist_add_and_audit(&pool, &tool, kind, &argv0).await {
        Ok(true)  => { println!("added {tool} {argv0}"); ExitCode::from(0) }
        Ok(false) => { println!("already present"); ExitCode::from(0) }
        Err(e @ (kastellan_db::tool_allowlists::ToolAllowlistError::InvalidArgv0
            | kastellan_db::tool_allowlists::ToolAllowlistError::InvalidToolName
            | kastellan_db::tool_allowlists::ToolAllowlistError::InvalidDomain
            | kastellan_db::tool_allowlists::ToolAllowlistError::Argv0HasNul
            | kastellan_db::tool_allowlists::ToolAllowlistError::Argv0HasDotDot)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e) => { eprintln!("{e}"); ExitCode::from(1) }
    }
```

Do the identical change in `tools_allowlist_remove` (resolve `kind`, pass to `tools_allowlist_remove_and_audit`, add `InvalidDomain` to its exit-2 arm).

Update the two usage strings from `add <tool> <argv0>` / `remove <tool> <argv0>` to `add <tool> <argv0|domain>` / `remove <tool> <argv0|domain>`.

- [ ] **Step 4: Build, clippy, and confirm no regression**

```sh
source "$HOME/.cargo/env"
cargo build -p kastellan-db -p kastellan-core 2>&1 | tail -10
cargo clippy -p kastellan-db -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: builds clean, clippy clean. (Live add/remove behaviour is re-verified in Task 6.)

- [ ] **Step 5: Commit**

```sh
git add db/src/tool_allowlists.rs core/src/cli_audit/registry.rs \
  core/src/bin/kastellan-cli/tools_allowlist.rs
git commit -m "feat(core): thread EntryKind through allowlist add/remove + CLI (#459)"
```

---

## Task 4: NetScreen backstop — malformed double-port entry is dead

**Files:**
- Modify: `core/src/workers/endpoint_guard.rs` (`host_is_malformed` + `screen_net_allowlist` wiring + message + tests)

**Interfaces:**
- Consumes: existing `host_of_entry`, `host_is_localhost_name`, `NetScreen`.
- Produces: `fn host_is_malformed(host: &str) -> bool` (private); an entry counts as dead if localhost-name **or** malformed.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `core/src/workers/endpoint_guard.rs`:

```rust
#[test]
fn host_is_malformed_flags_nonbracketed_embedded_colon() {
    // The residual-#3 double-port shape: `localhost:8888:443` → host_of_entry
    // strips the trailing :443 → `localhost:8888`, which still carries a colon.
    assert!(host_is_malformed("localhost:8888"));
    assert!(host_is_malformed("evil.example.org:1234"));
    // Well-formed hosts and bracketed IPv6 are NOT malformed.
    assert!(!host_is_malformed("localhost"));
    assert!(!host_is_malformed("example.org"));
    assert!(!host_is_malformed("127.0.0.1"));
    assert!(!host_is_malformed("[::1]"));
    assert!(!host_is_malformed("[2606:4700:4700::1111]"));
}

#[test]
fn screen_flags_double_port_entry_as_dead() {
    // A single all-dead malformed entry ⇒ Refuse.
    let entries = vec!["localhost:8888:443".to_string()];
    assert!(matches!(
        screen_net_allowlist("web-fetch", &entries, true),
        NetScreen::Refuse { .. }
    ));
    // A live host alongside a malformed one ⇒ Warn naming the dead entry.
    let mixed = vec!["docs.example.org:443".to_string(), "localhost:8888:443".to_string()];
    match screen_net_allowlist("web-fetch", &mixed, true) {
        NetScreen::Warn { dead } => assert_eq!(dead, vec!["localhost:8888:443".to_string()]),
        other => panic!("expected Warn, got {other:?}"),
    }
    // Not force-routed ⇒ Ok (unchanged).
    assert!(matches!(
        screen_net_allowlist("web-fetch", &entries, false),
        NetScreen::Ok
    ));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core -- endpoint_guard 2>&1 | tail -20
```

Expected: FAIL to compile (`cannot find function host_is_malformed`) and/or the double-port entry currently classifies `Ok` (not flagged).

- [ ] **Step 3: Add `host_is_malformed` and fold it into the dead filter**

In `core/src/workers/endpoint_guard.rs`, add the predicate near `host_of_entry`:

```rust
/// A `Net::Allowlist` entry host is malformed when — after `host_of_entry`
/// has stripped one trailing `:<port>` — a **non-bracketed** host still holds a
/// colon. A well-formed host never carries a bare colon (IPv6 is bracketed and
/// handled by `host_of_entry`), so this catches the residual-#3 double-port
/// shape `localhost:8888:443` (→ host `localhost:8888`). Such an entry is
/// statically dead: the proxy cannot dial a host with an embedded port.
fn host_is_malformed(host: &str) -> bool {
    !host.starts_with('[') && host.contains(':')
}
```

In `screen_net_allowlist`, widen the dead filter so an entry counts as dead when its host is a localhost-name **or** malformed:

```rust
    let dead: Vec<String> = entries
        .iter()
        .filter(|e| {
            let host = host_of_entry(e);
            host_is_localhost_name(host) || host_is_malformed(host)
        })
        .cloned()
        .collect();
```

Update the `Refuse { detail }` message to name the malformed case — change the phrase `uses a `localhost` name` to `uses a `localhost` name or an embedded port` in the `format!`.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core -- endpoint_guard 2>&1 | tail -20
```

Expected: PASS (the new tests + all existing `endpoint_guard` tests, incl. `ip_literals_are_not_flagged…`, `localhost_names_are_flagged`, `host_of_entry_strips_only_a_trailing_digit_port`).

- [ ] **Step 5: clippy + commit**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
git add core/src/workers/endpoint_guard.rs
git commit -m "feat(core): NetScreen backstop for malformed double-port allowlist entries (#459)"
```

---

## Task 5: Migration 0021 — CHECK for both entry kinds (DGX-gated)

> **SUPERSEDED during execution — do not use the union-branch SQL below.** The
> union-branch CHECK silently dropped the `0009` guarantee that shell-exec
> entries are absolute (a bare `echo` matches the *domain* branch), which the
> pre-existing `postgres_e2e` assertion caught in the full DGX gate. Shipped
> instead: a `kind` column (`DEFAULT 'argv0'`) with a `CASE`-on-kind CHECK, so
> SQL never needs to know a tool name and adding a tool costs no migration. See
> spec §9 (Revision) and `db/migrations/0021_tool_allowlists_domain_entries.sql`
> for the authoritative version.

**Files:**
- Create: `db/migrations/0021_tool_allowlists_domain_entries.sql`
- Create: `core/tests/tool_allowlists_check_e2e.rs` (raw-SQL CHECK behaviour, live PG)

**Interfaces:**
- Consumes: the `tool_allowlists` table (migration 0009).
- Produces: a table CHECK that accepts argv0 paths, bare/wildcard domains + IPv4, and bracketed IPv6 — and rejects `localhost:8888`.

- [ ] **Step 1: Write the migration**

Create `db/migrations/0021_tool_allowlists_domain_entries.sql`:

```sql
-- Phase 1 — tool_allowlists holds two entry kinds (#459 residual #3).
--
-- The `0009` CHECK required every argv0 to be an absolute path (`argv0 LIKE
-- '/%'`). That is correct for `shell-exec` (argv[0] exec paths) but rejects the
-- DOMAIN entries `web-fetch`/`web-research`/`browser-driver` store — so domain
-- allowlist rows were uninsertable. Replace the argv0-only CHECK with a
-- union-branch CHECK that admits either shape and still rejects malformed rows
-- (a port-bearing `localhost:8888` fails every branch — it has a colon and no
-- leading slash). The Rust per-kind validators (`db::tool_allowlists`) remain
-- the authoritative, more-precise gate; this CHECK is the shared bypass-guard.
--
-- The `0009` argv0 CHECK is an inline *unnamed* constraint (Postgres
-- auto-generated its name), so it is dropped by finding the CHECK whose
-- definition mentions `argv0 LIKE`. The separate `octet_length(tool) > 0` CHECK
-- is untouched.

DO $$
DECLARE
    c_name text;
BEGIN
    SELECT conname INTO c_name
    FROM pg_constraint
    WHERE conrelid = 'tool_allowlists'::regclass
      AND contype = 'c'
      AND pg_get_constraintdef(oid) LIKE '%argv0 LIKE%';
    IF c_name IS NOT NULL THEN
        EXECUTE format('ALTER TABLE tool_allowlists DROP CONSTRAINT %I', c_name);
    END IF;
END $$;

ALTER TABLE tool_allowlists ADD CONSTRAINT tool_allowlists_entry_shape CHECK (
    octet_length(argv0) > 0
    AND argv0 !~ '(^|/)\.\.(/|$)'          -- no '..' segment (both kinds)
    AND (
        argv0 LIKE '/%'                    -- argv0-kind: absolute path
        OR argv0 ~ '^\.?[A-Za-z0-9.-]+$'   -- domain-kind: bare/wildcard host / IPv4
        OR argv0 ~ '^\[[0-9A-Fa-f:]+\]$'   -- domain-kind: bracketed IPv6 literal
    )
);
```

> **No Rust change:** `sqlx::migrate!("./migrations")` in `db/src/lib.rs` globs the directory at build time, so `MIGRATOR` picks up `0021` automatically. `tool_allowlists.rs` uses runtime `sqlx::query` (not `query!`), so there is no `.sqlx`/`DATABASE_URL` to regenerate.

- [ ] **Step 2: Write the failing CHECK-behaviour test**

Create `core/tests/tool_allowlists_check_e2e.rs`. This uses **raw SQL** to bypass the Rust validator and pin the SQL CHECK directly (the migration is applied by `probe::run`):

```rust
//! Live-PG pin for the migration-0021 union-branch CHECK on `tool_allowlists`.
//! Raw INSERTs (bypassing the Rust validators) confirm the SQL layer accepts
//! both entry kinds and rejects the #459-residual-#3 port-bearing row.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_db::pool::connect_runtime_pool;
use kastellan_db::probe::run as probe_run;
use kastellan_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, unique_suffix};

async fn raw_insert(pool: &sqlx::PgPool, tool: &str, argv0: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tool_allowlists (tool, argv0, created_by)
         VALUES ($1, $2, 'test') ON CONFLICT (tool, argv0) DO NOTHING",
    )
    .bind(tool)
    .bind(argv0)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0021_check_accepts_both_kinds_and_rejects_malformed() {
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ta-chk-d",
        "ta-chk-l",
        &format!("kastellan-postgres-tool-allowlists-check-e2e-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "tool_allowlists_check_e2e"}))
        .await.expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    // Accepted: argv0 path, bare domain, wildcard, IPv4, bracketed IPv6.
    for ok in ["/bin/echo", "example.org", ".example.org", "203.0.113.5", "[::1]"] {
        raw_insert(&pool, "web-fetch", ok).await
            .unwrap_or_else(|e| panic!("{ok} should satisfy the CHECK: {e}"));
    }
    // Rejected by the CHECK: port-bearing, scheme, path, unbracketed IPv6.
    for bad in ["localhost:8888", "http://x", "x/y", "::1"] {
        assert!(raw_insert(&pool, "web-fetch", bad).await.is_err(),
            "{bad} should violate the CHECK");
    }
}
```

- [ ] **Step 3: Run the test (Mac session-local override, or defer to DGX)**

On the Mac (Postgres.app), run individually with the session-local `KASTELLAN_PG_BIN_DIR`:

```sh
source "$HOME/.cargo/env"
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test tool_allowlists_check_e2e -- --nocapture 2>&1 | tail -30
```

Expected: PASS. If PG is unavailable (`pg_bin_dir_or_skip` returns None), the test SKIPs — run it on the DGX instead (Step 5).

- [ ] **Step 4: clippy + commit**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --test tool_allowlists_check_e2e -- -D warnings 2>&1 | tail -5
git add db/migrations/0021_tool_allowlists_domain_entries.sql core/tests/tool_allowlists_check_e2e.rs
git commit -m "feat(db): migration 0021 union-branch CHECK for domain allowlist entries (#459)"
```

- [ ] **Step 5: DGX gate (authoritative for the CHECK)**

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  cargo test -p kastellan-core --test tool_allowlists_check_e2e -- --nocapture' 2>&1 | tail -30
```

Expected: PASS on native Linux + live PG. (Full-workspace DGX gate runs at the end — Task 7.)

---

## Task 6: CLI e2e — kind-aware domain add (DGX-gated)

**Files:**
- Modify: `core/tests/cli_tools_allowlist_e2e.rs` (add a domain-tool subtest)

**Interfaces:**
- Consumes: the real `kastellan-cli` binary, the migration 0021 CHECK, the CLI kind resolution (Task 3).

- [ ] **Step 1: Write the failing subtest**

Add a new `#[tokio::test]` to `core/tests/cli_tools_allowlist_e2e.rs`, mirroring the existing harness (`bring_up_pg_cluster` → `probe_run` → `connect_runtime_pool` → `cli_binary` + `cli_env`):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_add_domain_tool_accepts_domain_rejects_port_bearing() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "ta-dom-d", "ta-dom-l",
        &format!("kastellan-postgres-cli-tools-allowlist-domain-e2e-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_tools_allowlist_domain_e2e"}))
        .await.expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("runtime pool");
    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // A domain is accepted for a domain-kind tool.
    let ok = Command::new(&bin)
        .args(["tools", "allowlist", "add", "web-fetch", "example.org"])
        .env_clear().envs(env.clone()).output().expect("spawn add domain");
    assert!(ok.status.success(), "add domain exit: {:?}, stderr: {}",
        ok.status, String::from_utf8_lossy(&ok.stderr));

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT argv0 FROM tool_allowlists WHERE tool = $1 ORDER BY argv0")
        .bind("web-fetch").fetch_all(&pool).await.unwrap();
    assert_eq!(rows, vec![("example.org".to_string(),)]);

    // A port-bearing row is rejected (exit 2, InvalidDomain), no row landing.
    let bad = Command::new(&bin)
        .args(["tools", "allowlist", "add", "web-fetch", "localhost:8888"])
        .env_clear().envs(env.clone()).output().expect("spawn add bad domain");
    assert_eq!(bad.status.code(), Some(2), "stderr: {}",
        String::from_utf8_lossy(&bad.stderr));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("host/domain"),
        "stderr was: {}", String::from_utf8_lossy(&bad.stderr));

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tool_allowlists WHERE tool = 'web-fetch'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1, "the rejected row must not have landed");
}
```

- [ ] **Step 2: Run the subtest (Mac override, or DGX)**

```sh
source "$HOME/.cargo/env"
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test cli_tools_allowlist_e2e \
  cli_add_domain_tool_accepts_domain_rejects_port_bearing -- --nocapture 2>&1 | tail -30
```

Expected: PASS. (SKIPs if PG/supervisor unavailable — run on the DGX.)

- [ ] **Step 3: clippy + commit**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --test cli_tools_allowlist_e2e -- -D warnings 2>&1 | tail -5
git add core/tests/cli_tools_allowlist_e2e.rs
git commit -m "test(core): CLI e2e for kind-aware domain allowlist add (#459)"
```

- [ ] **Step 4: DGX gate for the CLI e2e**

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  cargo test -p kastellan-core --test cli_tools_allowlist_e2e -- --nocapture' 2>&1 | tail -30
```

Expected: PASS (both the existing shell-exec round-trip and the new domain subtest).

---

## Task 7: Full verification + docs + PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md` (session-end updates)

- [ ] **Step 1: Mac full check**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -5
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
cargo test -p kastellan-db 2>&1 | tail -5
cargo test -p kastellan-core --lib -- workers::endpoint_guard registry_build 2>&1 | tail -10
```

Expected: build + clippy clean; db + the touched core lib tests green.

- [ ] **Step 2: DGX full-workspace gate (authoritative)**

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && git fetch && \
  git checkout feat/459-tool-allowlists-entry-kind-validation && git pull && \
  cargo build --workspace && \
  cargo test --workspace 2>&1 | tail -15 && \
  cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'
```

Expected: `cargo test --workspace` = **2572 + the new tests** passed / 0 failed / 47 ignored (the added tests: Task 1 unit ×3, Task 2 unit ×1, Task 4 unit ×2, Task 5 e2e ×1, Task 6 e2e ×1 — net a small positive delta), clippy clean. Record the exact count as the new baseline. (Push the branch first if the DGX pulls from origin.)

- [ ] **Step 3: Update HANDOVER + ROADMAP**

- HANDOVER header: new "✅ MERGED/READY" entry for #459 residual #3 (four layers, new DGX baseline, spec/plan paths). Update "Last updated", "Current state", "Next TODO" (residual #3 → done; #459 fully closed), toolchain baseline count.
- ROADMAP: add a `[x]` sub-line under the #459 entry for residual #3.
- Crate map: note `db::tool_allowlists` now carries `EntryKind`/`validate_domain`.

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): #459 residual #3 shipped — entry-kind tool_allowlists validation"
```

- [ ] **Step 4: Push + open PR**

```sh
git push -u origin feat/459-tool-allowlists-entry-kind-validation
gh pr create --base main --title "Entry-kind-aware tool_allowlists validation + NetScreen backstop (#459 residual #3)" \
  --body "Closes #459 residual #3. See docs/superpowers/specs/2026-07-18-tool-allowlists-entry-kind-validation-design.md.

Root cause: tool_allowlists conflated argv0-paths (shell-exec) and domains (web-*) under one /-prefix validator + CHECK, so domain rows were uninsertable and the localhost:8888 → localhost:8888:443 blind spot's input was unreachable.

Four layers: (1) DB EntryKind + hand-rolled validate_domain (bare/wildcard domains, IPv4, bracketed IPv6 via std::net::Ipv6Addr; rejects embedded port/scheme/path); (2) migration 0021 union-branch CHECK; (3) WorkerManifest::allowlist_kind() + CLI kind dispatch; (4) NetScreen double-port backstop.

Verified: Mac cargo test + clippy -D warnings clean; DGX full-workspace <NEW BASELINE> + clippy clean (migration/CHECK + CLI e2e authoritative there).

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

Link the PR to #459 (it stays open only for residual #3 → this closes it) and paste the DGX baseline count into the body.

---

## Self-Review (completed by plan author)

**Spec coverage:** §3 layer 1 → Task 1; layer 2 → Task 5; layer 3 → Tasks 2–3; layer 4 → Task 4. §4 testing: pure suites (Tasks 1,2,4), live-PG CHECK (Task 5), CLI e2e (Task 6), verification gates (Task 7). §5 cross-platform / §6 file-size / §8 decisions all reflected in Global Constraints + Task 1 Step 5. No spec requirement without a task.

**Type consistency:** `EntryKind { Argv0, Domain }`, `validate_domain`, `validate_entry`, `allowlist_kind`, `allowlist_kind_for_tool`, `host_is_malformed` used identically across tasks. `add`/`remove` signatures gain `kind: EntryKind` in Task 3 and every caller (the two cli_audit wrappers → the two CLI fns) is updated in the same task — no dangling caller.

**Placeholder scan:** every code/command step carries concrete content; `<NEW BASELINE>` in Task 7 is a value to be filled from the DGX run output, not omitted plan content.
