# Opaque Secret References — Slice 1 Design

**Status:** approved 2026-05-28 (brainstorming session).
**Closes:** HANDOVER Item 31 ("Opaque secret references — `secret://<8-hex>`").
**Companion:** [`docs/superpowers/specs/2026-05-28-worker-output-prompt-injection-guard-design.md`](2026-05-28-worker-output-prompt-injection-guard-design.md) — the just-shipped sibling chokepoint extension (Item 30).
**Author:** Claude (Opus 4.7) for hherb.

## 1. Goal & Non-goals

### 1.1 Goal

Plaintext secrets (API tokens, IMAP passwords, signing keys) must never appear in:

- the LLM's conversation history (the planner sees only opaque references);
- the `audit_log` payload of any `actor='policy'` row;
- any future operator UI replaying transcripts.

Planner-visible references have the shape `secret://<8-hex>` (e.g. `secret://abc12345`). Core substitutes refs → plaintext at the `core::tool_host::dispatch` chokepoint, immediately before the JSON-RPC envelope is handed to the worker process.

### 1.2 Threat model

**In scope:**

- A compromised LLM (frontier or local) reading the transcript.
- An attacker who later reads `audit_log`.
- A future operator UI replaying transcripts.
- A worker echoing back its `payload.req` argv into stdout (the precedent established by `printf` in `injection_guard_e2e`).

**Out of scope:**

- A compromised daemon process (it holds the plaintext by definition).
- A compromised OS keyring (`db::secrets` already depends on it).
- Side channels: RAM dump, swap, debugger attach.

### 1.3 Non-goals (Slice 1 only)

These are deliberately deferred to later slices to keep the slice scope tight; each is tracked in §9 below.

- Operator-on-demand `kastellan-cli secrets materialize` (needs CLI ↔ daemon IPC; Slice 2).
- Per-task vault lifetime (today: per-daemon-process with 1h TTL).
- Embedded-substring substitution (today: exact-string match only).
- Re-encrypt-with-session-key vault (today: plaintext in `Zeroizing<Vec<u8>>` in process memory).
- `secrets.materialize` as a planner-visible tool (the LLM should never *name* a secret directly).
- Binary secrets (TLS private keys, signed binary tokens — Walker rejects with `PlaintextNotUtf8`).
- Revocation (no "invalidate ref X now" surface; TTL only).
- A "planner refused to use the ref" detector — orthogonal to substitution.

## 2. Public surface

New module **`core::secrets`** (sibling to `core::cassandra`, `core::memory`, etc.). Public surface:

```rust
// core/src/secrets/mod.rs

/// An opaque pointer into the in-process Vault. Constructed only by
/// `Vault::materialize`; the planner never sees the inner bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretRef(String);  // canonical form: "secret://<8 lowercase hex>"

impl SecretRef {
    /// The full `secret://<8-hex>` string. Safe to embed in transcripts:
    /// it reveals nothing without an active Vault.
    pub fn as_str(&self) -> &str;

    /// SHA-256 of `self.as_str()`. Audit rows carry this, not the ref
    /// itself, so an operator with audit-log read can correlate without
    /// being able to redeem.
    pub fn ref_hash(&self) -> String;  // 64-char lowercase hex
}

/// The per-daemon-process secret materialization cache.
pub struct Vault { /* opaque */ }

impl Vault {
    /// Construct with [`DEFAULT_TTL`] (1 h).
    pub fn new() -> Self;

    /// Construct with a custom TTL (for tests).
    pub fn with_ttl(ttl: Duration) -> Self;

    /// Decrypt `name` via `db::secrets::get`, stash the plaintext keyed
    /// by a fresh ref, write the `policy / secret.materialized` audit
    /// row, and return the ref. Caller must hold a runtime-role pool.
    pub async fn materialize(
        &self,
        pool: &sqlx::PgPool,
        key_provider: &dyn kastellan_db::secrets::KeyProvider,
        name: &str,
        actor: &str,                  // who is asking ("core:bootstrap", etc.)
    ) -> Result<SecretRef, VaultError>;

    /// Sync redemption. Returns the discrimination between Hit / Expired
    /// / NotFound so the chokepoint can write the correct `reason`
    /// field on a failed-redemption audit row. Expired entries are
    /// lazily dropped on each `redeem` call (zeroed via Zeroizing::Drop).
    pub fn redeem(&self, r: &SecretRef) -> RedeemResult;
}

pub enum RedeemResult {
    Hit(Zeroizing<Vec<u8>>),
    Expired,
    NotFound,
}

pub enum VaultError { /* see §5 */ }

pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);
pub const REF_PREFIX: &str = "secret://";
pub const REF_HEX_LEN: usize = 8;  // 4 random bytes via OsRng
```

**Ref construction.** `Vault::materialize` generates 4 random bytes via `aes_gcm::aead::OsRng` (already a dep) and formats `secret://{:08x}`. 4 bytes = ~4.3 billion namespace; under TTL=1h a collision needs ~65 K live refs per process to hit a 1% birthday probability — comfortably outside any expected workload. The constant `REF_HEX_LEN` makes a future widening one line.

**Why `materialize` takes `actor: &str`.** The `policy / secret.materialized` audit row carries the actor verbatim. Slice 1's only caller is the daemon's bootstrap path (`actor = "core:bootstrap"`); Slice 2's CLI will pass `actor = "cli:operator"`. Forcing the caller to name itself avoids an "unknown" actor row.

**Thread-safety.** `Vault` internally holds a `tokio::sync::RwLock<HashMap<SecretRef, Entry>>`. The whole vault is wrapped in `Arc<Vault>` by the daemon and threaded into `tool_host::dispatch` as a new parameter (one new arg, same shape as `pool` is already threaded). `Vault` is `Send + Sync` (the `RwLock` provides this).

## 3. Substitution semantics + walker contract

New helper module **`core::secrets::substitute`**:

```rust
// core/src/secrets/substitute.rs

/// One successful substitution. Emitted by the walker for each ref it
/// found and redeemed. The chokepoint translates each event into a
/// `policy / secret.redeemed` audit row.
#[derive(Debug)]
pub struct RedemptionEvent {
    pub ref_hash: String,  // SHA-256(ref.as_str()), 64-char lowercase hex
}

/// Test seam: the walker takes a `&dyn RedeemFromVault` so unit tests
/// can supply a `FakeVault` without spinning up `Vault`. Production
/// passes `&*vault` (which implements the trait inherently).
pub trait RedeemFromVault {
    fn redeem(&self, r: &SecretRef) -> RedeemResult;
}

/// Walk `value` and substitute every `Value::String` whose contents are
/// exactly a well-formed `secret://<8-hex>` ref with the redeemed
/// plaintext (interpreted as UTF-8 string). Returns one `RedemptionEvent`
/// per substitution.
pub fn substitute_refs_in_params(
    value: &mut serde_json::Value,
    vault: &dyn RedeemFromVault,
) -> Result<Vec<RedemptionEvent>, SubstituteError>;

pub enum SubstituteError {
    /// Well-formed ref but vault doesn't have it.
    MissingRef { ref_hash: String, reason: MissingReason },

    /// Vault has the ref but the plaintext is not valid UTF-8.
    /// `db::secrets` stores arbitrary bytes; binary secrets need a
    /// typed worker shape that bypasses the JSON value channel.
    PlaintextNotUtf8 { ref_hash: String },
}

#[derive(Debug, Clone, Copy)]
pub enum MissingReason {
    NotFound,   // never in vault
    Expired,    // was in vault, TTL elapsed; lazy-GC'd on this redeem
}

impl MissingReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Expired  => "expired",
        }
    }
}
```

### Walker invariants

1. **Strings only.** Numbers, booleans, nulls, array/object structural nodes are never substituted. Walks recursively into nested values.

2. **Whole-string match.** A string is substituted iff it satisfies the rule
   ```rust
   s.starts_with(REF_PREFIX)
       && s.len() == REF_PREFIX.len() + REF_HEX_LEN
       && s[REF_PREFIX.len()..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
   ```
   The lowercase-only check is belt-and-braces (refs are generated with `{:08x}` which is always lowercase) so a planner can't synthesise a casing-shifted ref to evade.

3. **Substitution preserves type.** `Value::String(ref)` becomes `Value::String(plaintext)`. The plaintext is interpreted as UTF-8 via `String::from_utf8(zeroized.to_vec())`; on failure the walker returns `PlaintextNotUtf8`. Binary secrets are out of scope for Slice 1.

4. **Zeroize discipline at the boundary.** The walker calls `vault.redeem(r)` → gets `Zeroizing<Vec<u8>>` (when Hit) → converts to `String` → assigns to `Value::String`. The original `Zeroizing` is dropped (zeroed) as soon as the `String` is constructed. The `Value::String` itself is **not** zeroized — see §9 limitation 1.

5. **Stop at first miss.** On any `MissingRef` or `PlaintextNotUtf8`, the walker returns the error immediately; `value`'s remaining nodes are not inspected. The `value` argument is left in an unspecified state — callers must not forward it to the worker on error. (In practice `dispatch` simply drops it and returns the typed error.)

6. **Exact-match only — embedded refs left alone.** A string like `"Bearer secret://abc12345"` is passed through verbatim. Minimises leak surface; Slice 2 may relax (see §9 limitation 3).

7. **No depth guard in Slice 1.** Matches the injection-guard precedent — Issue [#143](https://github.com/hherb/kastellan/issues/143) tracks the equivalent gap there; both walkers would adopt a shared depth helper if one ever lands.

### Walker test seam

A `pub(crate)` `FakeVault` test fixture implementing `RedeemFromVault` is constructed in the sibling `tests.rs` module. It carries a `HashMap<SecretRef, FakeEntry { plaintext: Vec<u8>, state: Hit | Expired | NotFound }>` so each test scenario can be staged without a real `Vault`.

## 4. Audit-row shapes

Three new `(actor, action)` pairs, all written through `kastellan_db::audit::insert`. Same posture as `injection_guard`: each insert is **best-effort** unless explicitly noted — transient DB failures are logged via `tracing::error!` but do not propagate. The materialize-time row is the **only exception**: it is hard-fail (see §5.4).

### 4.1 `policy / secret.materialized`

Written exactly once per `Vault::materialize` call.

```json
{
  "name":     "gh-token",
  "ref_hash": "<SHA-256 of `secret://abc12345`>",
  "ttl_secs": 3600,
  "actor":    "core:bootstrap"
}
```

- `name` IS in the row — it's the operator's lookup key, already enumerable by anyone with `secrets` table read.
- `ref_hash` is irreversible; lets operators correlate `materialized → redeemed → redemption_failed` across rows.
- `ttl_secs` carries the configured TTL at materialize time. If TTL ever becomes per-call, this row already records it.

### 4.2 `policy / secret.redeemed`

Written once per successful substitution. If one dispatch substitutes N refs, N rows are written. Order: redeemed rows are written **before** the tool row (matches event causality: redemption happens before the worker call).

```json
{
  "tool":     "shell-exec",
  "method":   "shell.exec",
  "ref_hash": "<SHA-256 of `secret://abc12345`>",
  "ms":       <dispatch elapsed ms>
}
```

- `ref_hash` correlates to a prior `secret.materialized` row.
- `ms` is dispatch elapsed time, NOT per-redemption (redeems are HashMap lookups at microseconds). Carried for symmetry with the existing tool row.
- Plaintext is never in this row by construction.

### 4.3 `policy / secret.redemption_failed`

Written when the walker returns any `SubstituteError`. Dispatch then returns `Err(ToolHostError::SecretRedemptionFailed)` **without calling the worker** — so no tool row is written either.

```json
{
  "tool":     "shell-exec",
  "method":   "shell.exec",
  "ref_hash": "<SHA-256 of the missing ref>",
  "reason":   "not_found",
  "ms":       <elapsed ms incl. failed walk>
}
```

- `reason` is a closed set: `"not_found"` | `"expired"` | `"plaintext_not_utf8"`. New variants must update `MissingReason::as_str()` plus this spec.

### 4.4 Ordering invariant

| Walk outcome | Rows written (in order) | Worker called? |
|---|---|---|
| No refs in params | `tool:<n> / <method>` | yes |
| Refs present, all redeem | `policy / secret.redeemed` × N, then `tool:<n> / <method>` | yes (substituted params) |
| Refs present, one missing | `policy / secret.redemption_failed` (only) | **no** |

This ordering is a deliberate choice and pinned by integration tests in §7.2.

## 5. Error variants + fail-closed posture

### 5.1 `core::secrets::VaultError` (materialize-time)

```rust
pub enum VaultError {
    #[error("vault: secret lookup failed: {0}")]
    Secrets(#[from] kastellan_db::secrets::SecretsError),

    /// Hard-fail on audit write — see §5.4. Wraps the existing
    /// `kastellan_db::DbError` returned by `audit::insert` (the audit
    /// module shares the crate-level DbError; there is no dedicated
    /// AuditError type).
    #[error("vault: audit row insert failed during materialize: {0}")]
    Audit(#[from] kastellan_db::DbError),

    #[error("vault: materialized plaintext is empty")]
    EmptyPlaintext,
}
```

### 5.2 `core::secrets::SubstituteError` (substitution-time)

Defined in §3 above. Two variants: `MissingRef { ref_hash, reason }`, `PlaintextNotUtf8 { ref_hash }`.

### 5.3 `ToolHostError::SecretRedemptionFailed` (new variant on existing enum)

```rust
pub enum ToolHostError {
    Sandbox(...),       // existing
    Io(...),            // existing
    Protocol(...),      // existing

    /// Substitution failed before the worker call. The dispatch's
    /// audit-row side-effect (`policy / secret.redemption_failed`)
    /// happened before this error was returned.
    #[error("tool_host: secret redemption failed: {0}")]
    SecretRedemptionFailed(#[from] crate::secrets::SubstituteError),
}
```

`ToolHostError` is **not** currently `#[non_exhaustive]` (verified at spec-write time against `core/src/tool_host.rs:21`). This slice adds the attribute — first new variant since the enum stabilised in Option M (2026-05-10). Out-of-crate match arms will need an `_ => ...` fallback (or an explicit new arm); the sealed module-private constructor pattern is preserved.

### 5.4 Fail-closed posture summary

- **Materialize fails on audit error** (§4.1 row write failure → `VaultError::Audit`). A materialized ref without an audit row is a posture violation, not a transient logging glitch.
- **Redeem is sync and infallible.** Returns `RedeemResult::{Hit | Expired | NotFound}`; the walker translates `Expired`/`NotFound` into `SubstituteError::MissingRef`.
- **Walker stops at first miss.** Worker is not called; tool row is not written; one `policy / secret.redemption_failed` row.
- **Walker stops at first UTF-8 failure** with the same shape, `reason: "plaintext_not_utf8"`.
- **Successful redemption is best-effort on `secret.redeemed`** (parallel to the tool row). The plaintext is already substituted into params; turning the dispatch into an error because the audit log was unreachable would be worse than missing one row.

This creates an **asymmetry: materialize-time audit is hard-fail; redeem-time audit is best-effort.** Documented inline at each call site. The reason: at materialize time we can refuse without observable consequence (the operator's bootstrap fails, they fix it); at redeem time we'd be killing a tool call mid-flight after the plaintext is already in process memory.

## 6. Lifecycle (bootstrap → dispatch → drop)

### 6.1 Bootstrap (daemon startup)

`core/src/main.rs`. New step inserted after `connect_runtime_pool` and before `spawn_scheduler`:

```rust
let vault = Arc::new(core::secrets::Vault::new());

// KASTELLAN_BOOTSTRAP_SECRETS = "name1,name2,name3"
// Each must exist in the `secrets` table; missing ones fail bring-up.
if let Some(names) = std::env::var("KASTELLAN_BOOTSTRAP_SECRETS").ok() {
    let key_provider = kastellan_db::secrets::OsKeyringProvider::ensure_initialized()?;
    for name in names.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let secret_ref = vault.materialize(&pool, &key_provider, name, "core:bootstrap").await?;
        tracing::info!(name = %name, ref_hash = %secret_ref.ref_hash(), "secret materialized at bootstrap");
        // The ref string itself is NOT logged — only the hash. Test
        // fixtures reconstruct refs via their own Vault::materialize
        // calls; see §7.
    }
}

let scheduler_handle = spawn_scheduler(pool.clone(), registry, vault.clone(), ...);
```

The vault is dropped when the daemon process exits; `Drop` walks the inner `HashMap` and explicitly drops every `Zeroizing<Vec<u8>>` (auto-zero). Process exit on SIGKILL bypasses this — acceptable per threat model.

### 6.2 Dispatch (per-call)

Current `core::tool_host::dispatch` signature:

```rust
pub async fn dispatch(
    pool: &sqlx::PgPool,
    worker: &mut SupervisedWorker,
    tool: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ToolHostError>
```

Becomes:

```rust
pub async fn dispatch(
    pool: &sqlx::PgPool,
    vault: &crate::secrets::Vault,        // NEW
    worker: &mut SupervisedWorker,
    tool: &str,
    method: &str,
    mut params: serde_json::Value,        // NOTE: now `mut`
) -> Result<serde_json::Value, ToolHostError>
```

Body shape (new substitution block inserted before `worker.call`):

```rust
let started = Instant::now();

// ── NEW: substitution chokepoint ──
let redemption_events = match substitute_refs_in_params(&mut params, vault) {
    Ok(events) => events,
    Err(e) => {
        // Fail-closed: write `secret.redemption_failed`, return typed error.
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (ref_hash, reason) = match &e {
            SubstituteError::MissingRef { ref_hash, reason } => (ref_hash.clone(), reason.as_str()),
            SubstituteError::PlaintextNotUtf8 { ref_hash }   => (ref_hash.clone(), "plaintext_not_utf8"),
        };
        let payload = serde_json::json!({
            "tool": tool, "method": method, "ref_hash": ref_hash,
            "reason": reason, "ms": elapsed_ms,
        });
        if let Err(audit_err) =
            kastellan_db::audit::insert(pool, "policy", "secret.redemption_failed", payload).await
        {
            tracing::error!(tool = %tool, error = %audit_err, "secret.redemption_failed audit insert failed");
        }
        return Err(ToolHostError::SecretRedemptionFailed(e));
    }
};

// Snapshot the (now-substituted) request for the tool row.
// IMPORTANT: this snapshot contains plaintext. The tool row's
// `payload.req` field is allowed to carry it (precedent set by
// injection-guard slice 1 commit 45627fd: the privacy invariant
// is scoped to `actor='policy'` rows only).
let req_for_audit = params.clone();

// ── EXISTING: worker.call ──
let cmd = WorkerCommand::new(method, params);
let call_result = tokio::task::block_in_place(|| worker.call(cmd));
let elapsed_ms = started.elapsed().as_millis() as u64;

// ── NEW: emit `secret.redeemed` rows BEFORE the tool row. ──
for event in &redemption_events {
    let payload = serde_json::json!({
        "tool": tool, "method": method, "ref_hash": event.ref_hash, "ms": elapsed_ms,
    });
    if let Err(e) = kastellan_db::audit::insert(pool, "policy", "secret.redeemed", payload).await {
        tracing::error!(tool = %tool, ref_hash = %event.ref_hash, error = %e, "secret.redeemed audit insert failed");
    }
}

// ── EXISTING: injection guard + tool row + dispatch return (unchanged) ──
```

**Two key facts:**

1. **Substitution happens BEFORE the injection guard.** The injection guard screens *results*; substitution screens *params*. Orthogonal screens at opposite ends of the dispatch.
2. **`payload.req` in the tool row carries the substituted plaintext.** Precedent established by injection-guard's commit `45627fd`. The privacy invariant (no plaintext in any `policy / *` row) is unchanged.

### 6.3 Drop (lifecycle exit)

`Vault::Drop` walks the inner `HashMap` and drops every `Zeroizing<Vec<u8>>` (auto-zero). Refs themselves are non-secret strings and need no special handling. On graceful daemon shutdown (`SIGTERM` caught by `tokio::signal::unix`), the `Arc<Vault>` refcount hits zero and `Drop` runs.

### 6.4 TTL lazy-GC

Every `Vault::redeem` call:

1. Acquires the read lock.
2. Looks up the ref. **Cache miss → release lock, return `NotFound`.**
3. If found AND `Instant::now() < entry.expires_at` → return `Hit(Zeroizing::new(entry.plaintext.clone()))`. The clone is unavoidable (interior storage is shared by ref-count; cloning gives the caller exclusive ownership). The clone is dropped after substitution.
4. If found AND `Instant::now() >= entry.expires_at` → drop the read lock, acquire the write lock, remove the entry (its `Zeroizing` drops → zero), return `Expired`.

(In step 3, the small race between read-lock release and write-lock acquire in step 4 is benign: at worst the GC happens during a redundant subsequent redeem.)

## 7. Test seam

PG is fully available on this Mac (Postgres.app v18 on :5532, v16 on :5432, both running). With `KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/"` exported, the full integration suite runs locally with no source-tree edits. **Slice 1 is implemented and verified with PG live** — including the 7 new integration tests.

### 7.1 Pure unit tests (no PG, no keyring)

Three buckets in sibling `tests.rs` modules (Rust 2018 module resolution, matching Item 30's pattern):

- **`core::secrets::tests`** (in `secrets/tests.rs`): `SecretRef::as_str` / `ref_hash` round-trip; constants pinned (`DEFAULT_TTL`, `REF_PREFIX`, `REF_HEX_LEN`); `Vault::new` defaults to `DEFAULT_TTL`.
- **`core::secrets::vault::tests`** (in `secrets/vault/tests.rs`): Vault lifecycle (insert via `pub(crate)` test-only `_test_insert` helper; redeem returns Hit; sleep past TTL; redeem returns Expired; second redeem returns NotFound, proving lazy GC).
- **`core::secrets::substitute::tests`** (in `secrets/substitute/tests.rs`): Walker against `FakeVault`. Cases: top-level ref, nested in Object, nested in Array, embedded substring left alone, uppercase hex left alone, wrong length (7 / 9 hex) left alone, missing → `MissingRef(NotFound)`, expired → `MissingRef(Expired)`, non-UTF-8 → `PlaintextNotUtf8`, empty containers (Object/Array/Null/Number/Bool) no-op.

### 7.2 Integration tests (PG live; run during implementation)

**`core/tests/secret_vault_e2e.rs`** — new file, 7 tests:

| # | Test | What it pins |
|---|------|--------------|
| 1 | `materialize_writes_audit_row_and_returns_ref` | Stage secret via `db::secrets::put` with `MapKeyProvider`. `Vault::materialize` returns `Ok(SecretRef)`. Exactly one `policy / secret.materialized` row carrying `{name="X", ref_hash=<hash>, ttl_secs=3600, actor="test"}`. |
| 2 | `materialize_fails_when_secret_missing` | No secret staged. Materialize returns `Err(VaultError::Secrets(SecretsError::NotFound))`. No `secret.materialized` row written. |
| 3 | `redeem_returns_plaintext_within_ttl` | Materialize; `Vault::redeem(&ref)` returns `RedeemResult::Hit` with original plaintext bytes. |
| 4 | `redeem_returns_expired_past_ttl` | `Vault::with_ttl(Duration::from_millis(100))`; materialize; `tokio::time::sleep(Duration::from_millis(150)).await`; `redeem` returns `Expired`; second `redeem` returns `NotFound`. |
| 5 | `dispatch_substitutes_and_writes_redeemed_row` | Real PG + real shell-exec + real Vault. Stage secret "X", materialize, build `params = {"argv": ["printf", "%s\\n", "<ref-string>"]}`, dispatch. Worker stdout = plaintext. Audit log: exactly 1× `policy/secret.materialized` + 1× `policy/secret.redeemed` (ref_hash matches) + 1× `tool:shell-exec/shell.exec`. |
| 6 | `dispatch_fails_closed_on_missing_ref` | Synthetic never-materialized ref `secret://00000000`; dispatch returns `Err(ToolHostError::SecretRedemptionFailed)`. Audit log has exactly 1 row: `policy/secret.redemption_failed` with `reason="not_found"`. **No tool row written.** |
| 7 | **Privacy invariant** | Original plaintext byte-string (unique marker `"SECRET_LEAK_MARKER_xyz789"`) MUST NOT appear as a substring of any `WHERE actor='policy'` row's serialized payload. Positive-presence assertion: `rows.is_empty()` for `secret.redeemed` ALSO fails — catches a regression where the chokepoint stops emitting. |

If implementation budget allows, an 8th case: `dispatch_substitutes_multiple_refs_in_one_params`. Else the pure-unit walker tests cover that path.

### 7.3 Run posture during implementation

```sh
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/"
cargo test --workspace
```

Every commit must show all tests green. The 7 new integration tests are not allowed to skip-as-pass during the slice's implementation — they must run. Skip-as-pass is the posture for hosts that don't have the env var set (CI, other contributors); on this Mac it's irrelevant.

### 7.4 Deliberately NOT tested in Slice 1

- **OS keyring round-trip** (`OsKeyringProvider::ensure_initialized()`). Same posture as `db::secrets` — manual smoke only. Headless CI cannot drive libsecret without prompting.
- **Bootstrap-from-env in `main.rs`.** Functional units (`Vault::materialize`) are tested directly; the env-driven daemon bring-up is verified manually on first daemon restart. A `supervisor_e2e`-shaped test could be added later if the env path becomes production-primary; today it's a test-fixture path.

## 8. File layout

### New files

| Path | Purpose | Approx LOC |
|---|---|---|
| `core/src/secrets/mod.rs` | Public surface re-exports + `mod vault; mod substitute;` declarations. | ~80 |
| `core/src/secrets/vault.rs` | `Vault` impl: `RwLock<HashMap<SecretRef, Entry>>`, `Entry { plaintext: Zeroizing<Vec<u8>>, expires_at: Instant }`, `new`/`with_ttl`/`materialize`/`redeem`/Drop. Inherent `impl RedeemFromVault for Vault`. | ~200 |
| `core/src/secrets/substitute.rs` | `RedeemFromVault` trait + `substitute_refs_in_params` walker + `RedemptionEvent` + `SubstituteError` + `MissingReason`. | ~150 |
| `core/src/secrets/tests.rs` | Module-level pure unit tests. | ~80 |
| `core/src/secrets/vault/tests.rs` | Vault lifecycle tests. | ~150 |
| `core/src/secrets/substitute/tests.rs` | Walker tests with `FakeVault`. | ~250 |
| `core/tests/secret_vault_e2e.rs` | 7 integration tests from §7.2. | ~450 |

### Modified files

| Path | Change | Net LOC |
|---|---|---|
| `core/src/lib.rs` | `pub mod secrets;` declaration. | +1 |
| `core/src/tool_host.rs` | Add `vault: &Vault` parameter to `dispatch`; insert substitution block before `worker.call`; emit `secret.redeemed` rows; new `ToolHostError::SecretRedemptionFailed` variant. | ~+70 LOC. Currently 767 LOC (263 over the 500-LOC cap); after this slice ~837 LOC (337 over). |
| `core/src/main.rs` | Construct `Vault`; read `KASTELLAN_BOOTSTRAP_SECRETS`; materialize loop; pass to `spawn_scheduler`. | ~+30 |
| `core/src/scheduler/tool_dispatch.rs` | `ToolHostStepDispatcher` carries `Arc<Vault>`; new constructor param; forward to `tool_host::dispatch`. | ~+15 |
| `core/src/scheduler/runner.rs` (or wherever `spawn_scheduler` lives) | Accept `Arc<Vault>`; thread through to `ToolHostStepDispatcher`. | ~+5 |

### No new migrations

`db::secrets` schema is unchanged; `audit_log` is unchanged. The three new `(actor, action)` pairs are wire-shape additions, not column additions.

### Cargo.toml

`sha2`, `zeroize`, `tokio::sync` are all already in `kastellan-core`'s deps. `aes_gcm::aead::OsRng` (re-exported from `db::secrets`'s dep `aes-gcm`) is available transitively but for cleanliness Slice 1 will add `rand = { workspace = true, features = ["std"] }` to `core/Cargo.toml` and use `rand::RngCore` directly. **Action item during Task 1: confirm `rand` is already in `Cargo.lock` via transitive deps and add the direct dep if needed.**

### File-size watch

- `tool_host.rs` 767 → ~837 LOC (337 over the 500-LOC cap). Pre-existing tech-debt extended; defer the sibling-lift refactor until both Item 30 AND Item 31 are merged so the lift can collapse screening + substitution into a `tool_host/chokepoint.rs` seam.
- Largest new file in `secrets/` is `vault.rs` at ~200 LOC — well under cap.
- Integration test file at ~450 LOC is borderline; if it crowds 500 LOC during implementation, split per fixture-class (materialize vs dispatch).

## 9. Known limitations + Slice 2 candidates

### 9.1 Known limitations of Slice 1

1. **Plaintext lives as a plain `String` in process memory between substitution and `serde_json::to_writer`.** The `Zeroizing<Vec<u8>>` is dropped after conversion; the `Value::String` itself is a regular `String` until JSON-RPC serialization completes. A panic-unwind during this window leaves plaintext in unzeroed stack frames. **Mitigation:** none in Slice 1 — the only tightening is a typed `WorkerCommand` with explicit `Zeroizing` slots, which is a major JSON-RPC envelope refactor. Threat model already excludes RAM dump.

2. **`payload.req` in the tool audit row carries the substituted plaintext.** Precedent from injection-guard slice 1 (`45627fd`): the privacy invariant is scoped to `actor='policy'` rows only. The tool row legitimately receives plaintext because the worker legitimately received it. Operators who don't want this either don't pass refs to tool calls (defeats the slice) or accept the trade-off.

3. **Exact-string substitution only.** Embedded substrings (`"Bearer secret://..."`) pass through verbatim. Slice 2 may relax with explicit object-shape opt-in.

4. **No walker depth guard.** Matches the injection-guard gap tracked as [#143](https://github.com/hherb/kastellan/issues/143). A shared depth helper would close both walkers at once.

5. **Per-process (not per-task) vault scope.** A ref materialized for task A is redeemable in task B until TTL expiry. Per-task scoping needs `Vault` task-lifetime + dispatch-side task-ID plumbing.

6. **No CLI surface.** Operators can't `kastellan-cli secrets materialize <name>`. Slice 2 builds the CLI ↔ daemon IPC.

7. **No revocation.** Once materialized, a ref is valid until TTL or daemon restart. Slice 3 if leak-incident drives need.

8. **OS keyring access at daemon-startup only.** `KASTELLAN_BOOTSTRAP_SECRETS` is the only path. Closed simultaneously with #6 in Slice 2.

9. **Binary secrets unsupported.** Walker rejects non-UTF-8 plaintext with `PlaintextNotUtf8`. A typed binary-secret channel is a separate (much bigger) slice.

10. **`tool_host.rs` 500-LOC residual grows.** 263 → ~337 over. Bundle-up sibling-lift in Slice 2.

### 9.2 Slice 2 candidates (prioritised)

| Priority | Slice | Why next |
|---|---|---|
| H | CLI ↔ daemon IPC + `kastellan-cli secrets materialize <name>` | Closes #6 + #8. Likely DB-mediated unless something else needs IPC. |
| H | `tool_host.rs` sibling-lift | The 500-LOC residual is load-bearing; collapse screening + substitution into a `chokepoint.rs` seam. |
| M | Per-task vault scoping | Principle of least privilege; blocks on empirical evidence of cross-task reuse. |
| M | Shared walker depth guard (with injection-guard) | Closes #4 + [#143](https://github.com/hherb/kastellan/issues/143). Cheap. |
| M | Frontier-router secret integration (Phase 5 prep) | The originally-motivating consumer. Demonstrates real use. |
| L | Embedded substitution with explicit opt-in | Only if a real use case (`Bearer <ref>` headers) materialises. |
| L | Binary-secret channel | Only if TLS/binary tokens become an actual consumer. |
| L | Revocation surface | Only if a leak incident hits and TTL rotation is insufficient. |

## 10. References

- HANDOVER Item 31 — original framing (`docs/devel/handovers/HANDOVER.md`, line ~4241 as of 2026-05-28).
- Item 30 design + plan — sibling chokepoint extension:
  - [`2026-05-28-worker-output-prompt-injection-guard-design.md`](2026-05-28-worker-output-prompt-injection-guard-design.md)
  - [`../plans/2026-05-28-worker-output-prompt-injection-guard-slice-1.md`](../plans/2026-05-28-worker-output-prompt-injection-guard-slice-1.md)
- Threat model — `docs/threat-model.md`.
- `db::secrets` module — `db/src/secrets.rs` (848 LOC, shipped pre-Phase-1).
- Issue [#143](https://github.com/hherb/kastellan/issues/143) — walker depth guard (filed 2026-05-28 from injection-guard `e81b079`).
- Pattern inspiration: openhuman `docs/MCP_SETUP_AGENT.md` opaque-ref pattern (GPL-3.0; re-implemented to keep AGPL-3.0 one-way compatibility ambiguity-free).
