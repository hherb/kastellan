# Entry-kind-aware `tool_allowlists` validation + NetScreen backstop (#459 residual #3)

**Date:** 2026-07-18
**Issue:** [#459](https://github.com/hherb/kastellan/issues/459) residual #3
**Status:** Design approved; ready for implementation planning.

---

## 1. Problem

The `tool_allowlists` table (migration `0009`) is the operator source-of-truth
for two *semantically different* kinds of allowlist entry, stored in one
`argv0` column and validated as if there were only one kind:

- **argv0-kind** — the `shell-exec` worker: each entry is an absolute
  `argv[0]` exec path (e.g. `/bin/echo`). Validated by
  [`validate_argv0`](../../../db/src/tool_allowlists.rs) (must start with `/`,
  no NUL, no `..` segment) and the SQL `CHECK (argv0 LIKE '/%' AND …)`.
- **domain-kind** — the `web-fetch`, `web-research`, and `browser-driver`
  workers: each entry is a **host / domain** (e.g. `example.org`,
  `.example.org` wildcard). `web-fetch`/`web-research` map it to a
  `Net::Allowlist` entry via `allowlist_to_net_entries` → `format!("{host}:443")`;
  `browser-driver` uses the DB rows **verbatim**.

Because the only validator and the only SQL CHECK are argv0-shaped, **a domain
row cannot be inserted at all**: `kastellan-cli tools allowlist add web-fetch
example.org` fails `InvalidArgv0`, and even a raw superuser `INSERT` is rejected
by the `argv0 LIKE '/%'` CHECK. Integration tests only work because they pass
domain vectors *directly* to the entry builders, bypassing the DB.

Two consequences:

1. **Domain allowlists are effectively unpopulatable in production.** Since
   `web_research::research::hit_allowed` treats an empty allowlist as
   "allow nothing", a daemon-driven web-fetch/web-research cannot fetch any
   operator-configured content host.
2. **Residual #3 is a symptom of the same gap.** The originally-filed blind
   spot — a `tool_allowlists` row like `localhost:8888` maps to the net entry
   `localhost:8888:443`, whose `host_of_entry` strips the trailing `:443` and
   yields `localhost:8888` (not a `localhost` *NAME*), so it escapes the #459
   `NetScreen` and registers dead — describes a row the DB cannot currently
   produce. The mapping bug is real, but its input is currently unreachable.

The real fix therefore both **unblocks domain allowlists** and **closes
residual #3 at the source**: a domain-kind validator rejects `localhost:8888`
(it carries an embedded port), so the malformed net entry is never built.

This spec implements the defense-in-depth ("real fix + backstop") option: an
entry-kind-aware validator + a per-kind SQL CHECK as the source fix, plus a
`NetScreen` hardening as a backstop for any row that ever reaches the registry
by another path.

## 2. Goals / non-goals

**Goals**

- Domain-kind allowlist rows (`example.org`, `.example.org`, bare IPv4, and a
  **bracketed IPv6 literal** `[::1]`) are insertable via
  `kastellan-cli tools allowlist add <domain-tool> <host>`.
- A malformed domain row carrying a port/scheme/path/userinfo/whitespace
  (`localhost:8888`, `http://x`, `x/y`, `a@b`) is **rejected at both the Rust
  validator and the SQL CHECK** for domain-kind tools.
- argv0-kind (`shell-exec`) validation is **unchanged** — same acceptance set,
  same SQL guarantee (`/`-absolute, no `..`).
- The generic `NetScreen` (#459 slice 1) also refuses/warns on a malformed
  double-port entry (`localhost:8888:443`) — backstop for any row that bypasses
  layers 1–2.
- Pure functions, TDD, junior-readable inline docs.

**Non-goals**

- No new table, no schema split (`tool_allowlists` stays the one table; the
  separate-table option D was considered and declined for #459's scope).
- No `kind` column (the union-branch CHECK avoids denormalizing a value that is
  a function of `tool`).
- No change to how workers *consume* the allowlist (`allowlist_to_net_entries`,
  `hit_allowed`, etc. are untouched).

## 3. Design

Four coordinated layers, source → backstop.

### Layer 1 — DB domain validator (`db/src/tool_allowlists.rs`)

Add an entry-kind enum and a pure domain validator; make `add`/`remove` kind-aware.

```rust
/// Which shape an entry in `tool_allowlists` takes for a given tool. A tool is
/// entirely one kind or the other (it is a function of the tool, never mixed):
/// `shell-exec` stores argv0 exec paths; the web workers store domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Absolute `argv[0]` exec path — validated by `validate_argv0`.
    Argv0,
    /// Host / domain allowlist entry — validated by `validate_domain`.
    Domain,
}
```

New error variant:

```rust
#[error("allowlist entry is not a valid host/domain; expected a bare domain \
         (example.org), a wildcard (.example.org), a bare IPv4, or a bracketed \
         IPv6 literal ([::1]) — no scheme, port, path, '@', or whitespace")]
InvalidDomain,
```

`validate_domain(entry)` — hand-rolled (no new `url` dependency — IPv6 uses
`std::net::Ipv6Addr` from std; matches the existing `validate_argv0` style):

- Reject empty, or any byte that is NUL / ASCII whitespace / ASCII control.
- **Bracketed IPv6 branch** — if the entry starts with `[` and ends with `]`,
  the inner text must parse as `std::net::Ipv6Addr` (`entry[1..len-1].parse()`);
  Ok on parse, else `InvalidDomain`. Brackets are **required** for IPv6 so that
  `allowlist_to_net_entries`' `format!("{host}:443")` yields a valid
  `[::1]:443` and the bracket-aware `host_of_entry` strips it back to `[::1]`.
  A bare (unbracketed) `::1` is rejected (it would map to the ambiguous
  `::1:443`).
- **Domain / IPv4 branch** (LDH-label rules) — otherwise:
  - Strip **one** optional leading `.` (wildcard form) and **one** optional
    trailing `.` (FQDN form). A leading `..` (empty first label) is rejected by
    the label loop below.
  - Reject total length > 253 bytes (after the optional dot strips).
  - Split the remainder on `.`; require ≥1 label; each label: non-empty,
    ≤ 63 bytes, every byte in `[A-Za-z0-9-]`, not starting or ending with `-`.
  - Bare IPv4 (`203.0.113.5`) passes (all-digit labels are valid LDH). A
    legitimate content host; the proxy's carve-out dials a literal loopback.

Accepts: `example.org`, `.example.org`, `api.example.org`, `203.0.113.5`,
`[::1]`, `[2606:4700:4700::1111]`, `[fd12:3456::1]`.
Rejects: `localhost:8888` (`:` in a non-bracketed entry), `http://x` (`:`,`/`),
`x/y` (`/`), `a@b` (`@`), `a..b` (empty label), `-a.b`/`a-.b` (hyphen edge),
`::1` (unbracketed IPv6), `[not-ipv6]` (fails `Ipv6Addr::parse`), whitespace.

Dispatcher + kind-aware I/O:

```rust
pub fn validate_entry(kind: EntryKind, entry: &str) -> Result<(), ToolAllowlistError> {
    match kind {
        EntryKind::Argv0 => validate_argv0(entry),
        EntryKind::Domain => validate_domain(entry),
    }
}

pub async fn add(pool, tool, kind: EntryKind, entry, created_by) -> Result<bool, _> {
    validate_tool_name(tool)?;
    validate_entry(kind, entry)?;
    // INSERT … ON CONFLICT DO NOTHING (unchanged SQL)
}
pub async fn remove(pool, tool, kind: EntryKind, entry) -> Result<bool, _> { … }
```

`validate_argv0`, `list_for_tool`, `list_for_tool_full`, `list_all`,
`AllowlistEntry` are **unchanged** (reads are kind-agnostic — the registry only
needs the string; the mapping is applied later by each worker's entry builder).

### Layer 2 — Migration `0021` union-branch CHECK

`0021_tool_allowlists_domain_entries.sql`: drop the argv0-only CHECK and add a
union-branch CHECK that admits either shape and rejects malformed rows for both.

```sql
-- The `0009` argv0 CHECK is an inline *unnamed* constraint, so Postgres
-- auto-generated its name (`tool_allowlists_check`, `_check1`, …) — not stable
-- to hardcode. Drop it deterministically by finding the CHECK whose definition
-- mentions `argv0 LIKE`, then add a *named* union-branch replacement.
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
        OR argv0 ~ '^\.?[A-Za-z0-9.-]+$'   -- domain-kind: bare/wildcard host / IPv4, no ':' '/' '@' ws
        OR argv0 ~ '^\[[0-9A-Fa-f:]+\]$'   -- domain-kind: bracketed IPv6 literal
    )
);
```

Notes:

- The `octet_length(tool) > 0` CHECK from `0009` is a **separate** constraint and
  stays untouched — only the argv0 CHECK is replaced.
- The SQL CHECK is a *shape* backstop, not a full validator: e.g. an empty-label
  `a..b` slips the domain branch (`.` and letters are in the class) — the Rust
  `validate_domain` rejects it on the authoritative path, and the resulting net
  entry would be a dead non-localhost host anyway. This asymmetry is acceptable
  by design (Rust authoritative, SQL a coarse bypass-guard).
- `localhost:8888` fails **all three** branches (no leading `/`; the `:` is outside
  the domain character class) → rejected at SQL. Double defense-in-depth with
  the Rust validator.
- Existing `shell-exec` rows satisfy the argv0 branch unchanged → no backfill,
  no data migration.
- The Rust per-kind validators remain the authoritative, more-precise gate
  (label length, hyphen placement, 253-byte cap); the SQL CHECK is the shared
  backstop for callers that bypass them (the runtime role has direct INSERT).
- Migrations are immutable once applied — this is a **new** file, `0009` is not
  edited. `MIGRATOR` range in `db/src/lib.rs` extends to `0021`.

### Layer 3 — Core tool→kind classification

`WorkerManifest` gains a defaulted method beside `allowlist_tool`:

```rust
/// The shape of this worker's `tool_allowlists` entries, when it declares an
/// allowlist. `None` ⇒ no allowlist (the default). Drives which validator the
/// CLI applies on `add`/`remove`.
fn allowlist_kind(&self) -> Option<kastellan_db::tool_allowlists::EntryKind> {
    None
}
```

- `ShellExecManifest` → `Some(EntryKind::Argv0)`.
- `WebFetchManifest`, `WebResearchManifest`, `BrowserDriverManifest` →
  `Some(EntryKind::Domain)`.

Pure helper in `registry_build` (next to `WORKER_MANIFESTS`):

```rust
/// Kind of `tool_allowlists` entry a tool uses, by scanning the static manifest
/// list. `None` for a tool that declares no allowlist or is unknown.
pub fn allowlist_kind_for_tool(name: &str) -> Option<EntryKind> {
    WORKER_MANIFESTS.iter()
        .find(|m| m.allowlist_tool() == Some(name))
        .and_then(|m| m.allowlist_kind())
}
```

CLI `tools_allowlist_{add,remove}` ([tools_allowlist.rs](../../../core/src/bin/kastellan-cli/tools_allowlist.rs))
resolve the kind and thread it down:

```rust
let kind = allowlist_kind_for_tool(&tool).unwrap_or(EntryKind::Argv0);
```

`unwrap_or(EntryKind::Argv0)` preserves today's behaviour for an unknown/other
tool name (every add is argv0-validated today). `InvalidDomain` is added to the
CLI's exit-2 user-error match arm alongside the existing argv0 variants.
`cli_audit::tools_allowlist_{add,remove}_and_audit` signatures gain the
`kind: EntryKind` parameter and forward it to `db::…::{add,remove}`.

### Layer 4 — NetScreen backstop (`core/src/workers/endpoint_guard.rs`)

Harden the generic screen so a **malformed double-port** entry is treated as
statically dead, independent of the localhost-name check:

- `host_of_entry` is unchanged (still strips one trailing `:<digits>`), but a
  new predicate `host_is_malformed(host) -> bool` returns true when the host
  component (non-bracketed) still contains a `:` — a well-formed `Net::Allowlist`
  host never carries a bare colon (IPv6 is bracketed and handled separately).
- `screen_net_allowlist` counts an entry as **dead** if it is a localhost-name
  **or** its host is malformed. All-dead ⇒ `Refuse`, proper subset ⇒ `Warn`
  (existing severity policy). The `Refuse`/`Warn` messages gain a clause naming
  malformed entries ("… uses a `localhost` name or an embedded port …").

This closes `localhost:8888:443` (host `localhost:8888` has a bare colon → dead)
as a backstop even though layers 1–2 mean such a row can no longer be inserted.

## 4. Testing (TDD)

**Mac-runnable (pure Rust):**

- `db::tool_allowlists`: `validate_domain` accept/reject table (the cases in
  §3.1, incl. IPv6 — `[::1]`/`[2606:4700:4700::1111]` accepted, bare `::1` and
  `[not-ipv6]` rejected), `validate_entry` dispatch, `InvalidDomain` surfaced.
  Existing `validate_argv0` tests unchanged and still green.
- `registry_build::allowlist_kind_for_tool` mapping (shell-exec→Argv0, the
  three web tools→Domain, unknown→None); per-manifest `allowlist_kind()`.
- `endpoint_guard`: `host_is_malformed` unit cases; `screen_net_allowlist` with
  a double-port entry (`localhost:8888:443`) → `Refuse` alone / `Warn` in a
  mixed list; existing localhost-name and literal-IP tests unchanged.

**Live-PG-gated (DGX):**

- Migration/CHECK integration test (`db` or `core/tests`, live PG): domain rows
  (`example.org`, `.example.org`, `[::1]`) INSERT; `localhost:8888`, `http://x`,
  `x/y`, `::1` (unbracketed) are rejected by the CHECK; an existing `/bin/echo`
  argv0 row still INSERTs.
- CLI e2e (extend `core/tests/cli_tools_allowlist_e2e.rs`): `tools allowlist add
  web-fetch example.org` succeeds and lists; `add web-fetch localhost:8888`
  exits 2 with the domain error; `add shell-exec /bin/echo` still works.

**Verification gates:**

- Mac: `cargo test -p kastellan-db -p kastellan-core` (the pure suites) +
  `cargo clippy --workspace --all-targets -D warnings`.
- DGX (native aarch64, live PG 18): `cargo test --workspace` (new baseline =
  2572 + the added tests) + `clippy --workspace --all-targets -D warnings`. The
  migration/CHECK + CLI e2e are only meaningful against real Postgres, so the
  DGX gate is authoritative for layer 2.

## 5. Cross-platform / invariants

- All pure Rust + one SQL migration — **no OS-specific code**, so both Linux and
  macOS are covered identically; only the live-PG tests are DGX-gated (macOS PG
  can run them via the session-local-override pattern if desired).
- AGPL-compatible: no new dependency (hand-rolled validator; no `url` in `db`).
- No worker gains an "unsandboxed" path; no threat-model invariant touched — this
  is operator-config validation, strictly narrowing what can be stored.

## 6. File-size watch (Item 9b)

`db/src/tool_allowlists.rs` is ~309 lines today; the domain validator + its test
table will push it up. If it crosses ~500, lift the `#[cfg(test)] mod tests`
into `tool_allowlists/tests.rs` (pub-use-preserving, the standard test-lift) —
decide during implementation once the real line count is known.

## 7. Out of scope / follow-ups

- Strict IPv4 octet-range validation (`999.999.999.999` passes the LDH domain
  branch as a syntactically-valid hostname; it is a dead host, not a security
  issue — the proxy owns real resolution). Not worth special-casing.
- Rejecting `add` for a genuinely unknown tool name (today any charset-valid
  tool name is accepted; kept as-is for back-compat — a typo-guard is a separate,
  optional hardening).
- The separate-table refactor (option D) — declined for #459's scope.

## 8. Resolved decisions

- **Scope:** real fix + backstop (validator + migration + CLI + NetScreen), not
  a guard-only patch — the guard alone would defend a state the DB cannot reach.
- **Migration shape:** union-branch CHECK (no `kind` column) — keeps argv0's
  absolute-path SQL guarantee, adds a precise domain-shape SQL guarantee, rejects
  `localhost:8888` at SQL, and avoids denormalizing a value derivable from `tool`.
- **Domain validator:** hand-rolled LDH rules, no new `db` dependency.
- **IPv6 from the start:** bracketed IPv6 literals (`[::1]`) are accepted via
  `std::net::Ipv6Addr` (still no new dep) — the allowlist is IPv6-ready rather
  than deferring it. Brackets required so the `{host}:443` append stays valid.
