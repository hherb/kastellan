# Opaque Secret References — Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the in-process `Vault` + chokepoint substitution that lets the planner see only opaque `secret://<8-hex>` references; core substitutes refs → plaintext in `tool_host::dispatch` immediately before the worker call, and writes three new audit-row kinds (`secret.materialized`, `secret.redeemed`, `secret.redemption_failed`) — never the plaintext.

**Architecture:** New module `core::secrets` (sibling to `cassandra`, `memory`). Public surface: `Vault` (TTL'd `std::sync::RwLock<HashMap<SecretRef, Entry>>`), `SecretRef` opaque type, `RedeemResult` (Hit/Expired/NotFound), `substitute_refs_in_params` walker. Bootstrap-time materialization via `KASTELLAN_BOOTSTRAP_SECRETS` env var. Substitution wires before the just-shipped injection guard in the dispatch body; orthogonal screens at opposite ends. Per-process daemon-owned `Arc<Vault>` threaded into dispatch and scheduler as a new parameter.

**Tech Stack:** Rust workspace (kastellan-core crate). `sha2` already in workspace deps (injection-guard). `zeroize`/`aes-gcm`/`tokio::sync` already pulled in via `db::secrets` and existing deps. `rand` already in `Cargo.lock` transitively via `aes-gcm`; will add direct dep if not.

**Spec:** [docs/superpowers/specs/2026-05-28-opaque-secret-references-design.md](../specs/2026-05-28-opaque-secret-references-design.md)

---

## Pre-flight: branch + baseline

- [ ] **Step 1: Confirm green workspace baseline before any change**

```sh
source "$HOME/.cargo/env"
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/"
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1096 failed:0 ignored:3` on macOS (the baseline at session start; `main` at `c505b36`).

- [ ] **Step 2: Create the branch**

```sh
git checkout -b feat/opaque-secret-refs-slice-1
git status
```

Expected: clean working tree (the untracked `docs/essay-medium-draft.md` is the operator's draft, leave it), branch `feat/opaque-secret-refs-slice-1` checked out from `main` at `c505b36` or later.

- [ ] **Step 3: Verify direct deps exist**

The vault impl needs `rand::RngCore` (4 bytes via `OsRng`) and `sha2::Sha256`. Check what's already direct vs transitive.

```sh
grep -E '^(sha2|rand|zeroize)' core/Cargo.toml
```

Expected: `sha2` present (from injection-guard slice). `rand` and `zeroize` may be transitive only. We will add `rand` as a direct dep in Task 2 if it's missing — defer the verification until then so the failing build surfaces it.

---

## Task 1: Module skeleton, types, and const pins

**Files:**
- Create: `core/src/secrets/mod.rs`
- Create: `core/src/secrets/vault.rs` (stub)
- Create: `core/src/secrets/substitute.rs` (stub)
- Create: `core/src/secrets/tests.rs` (module-level tests)
- Modify: `core/src/lib.rs` (one-line `pub mod secrets;`)

This task gets the public surface compiling against the spec without any vault or walker logic. Stubs use `unimplemented!()`. Const pins guard against silent drift.

- [ ] **Step 1: Add `pub mod secrets;` to `core/src/lib.rs`**

Read [core/src/lib.rs](core/src/lib.rs) first to find the right insertion point — the file already declares `pub mod cassandra;`, `pub mod memory;`, etc. Insert `pub mod secrets;` alphabetically:

```rust
// (existing lines)
pub mod scheduler;
pub mod secrets;                   // NEW — Item 31 (HANDOVER) opaque secret refs slice 1
pub mod tool_host;
// (existing lines)
```

- [ ] **Step 2: Create `core/src/secrets/mod.rs`**

```rust
//! Opaque secret references and in-process materialization vault.
//!
//! Planner-visible references have the shape `secret://<8-hex>` (e.g.
//! `secret://abc12345`). Core substitutes refs → plaintext at the
//! `tool_host::dispatch` chokepoint, immediately before the JSON-RPC
//! envelope is handed to the worker process. Operators (and Slice 2's
//! CLI) materialize refs via [`Vault::materialize`]; the planner
//! never *names* a secret directly.
//!
//! ## Threat model
//!
//! Plaintext secrets must never appear in the LLM's conversation
//! history, the `audit_log` payload of any `actor='policy'` row, or
//! any future operator UI replaying transcripts. The tool row's
//! `payload.req` field IS allowed to carry plaintext (precedent set
//! by `injection_guard` slice 1, commit `45627fd`) — the privacy
//! invariant is scoped to `actor='policy'` rows only.
//!
//! See [`docs/superpowers/specs/2026-05-28-opaque-secret-references-design.md`](../../../docs/superpowers/specs/2026-05-28-opaque-secret-references-design.md)
//! for the full design.

pub mod substitute;
pub mod vault;

pub use substitute::{
    substitute_refs_in_params, MissingReason, RedeemFromVault, RedemptionEvent, SubstituteError,
};
pub use vault::{RedeemResult, SecretRef, Vault, VaultError};

use std::time::Duration;

/// Default Vault TTL — 1 hour. Bounded blast radius if a ref leaks
/// into transcript history. Construct with [`Vault::with_ttl`] to
/// override (tests use ~100 ms).
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Prefix every well-formed ref starts with.
pub const REF_PREFIX: &str = "secret://";

/// Number of lowercase-hex digits in a well-formed ref's tail.
/// 4 random bytes via `OsRng` formatted as `{:08x}`. 4-byte namespace
/// (~4.3 B) is comfortably large for one-process TTL'd refs.
pub const REF_HEX_LEN: usize = 8;

#[cfg(test)]
mod tests;
```

- [ ] **Step 3: Create `core/src/secrets/vault.rs` (stub)**

```rust
//! In-process secret materialization Vault. Holds plaintext keyed by
//! [`SecretRef`] with wall-clock TTL. See module-level docs in
//! [`super`] for the threat model.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use zeroize::Zeroizing;

use kastellan_db::secrets::KeyProvider;

use super::{DEFAULT_TTL, REF_HEX_LEN, REF_PREFIX};

/// Opaque pointer into the in-process [`Vault`]. Constructed only by
/// [`Vault::materialize`]. Safe to embed in audit logs and (eventually)
/// in transcripts: reveals nothing without an active Vault.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretRef(String);

impl SecretRef {
    /// The full `secret://<8-hex>` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// SHA-256 of [`Self::as_str`]; 64-char lowercase hex. Audit rows
    /// carry this, not the ref itself — operators with audit-log read
    /// can correlate `materialized → redeemed → redemption_failed`
    /// across rows without being able to redeem.
    pub fn ref_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.0.as_bytes());
        format!("{:x}", h.finalize())
    }

    /// `pub(crate)` constructor — only called from inside this module
    /// (and from `pub(crate)` test helpers). Keeps the only public path
    /// through [`Vault::materialize`].
    pub(crate) fn from_raw(s: String) -> Self {
        SecretRef(s)
    }
}

/// Per-daemon-process secret materialization cache. Threaded into
/// [`crate::tool_host::dispatch`] as `&Vault` (the daemon owns an
/// `Arc<Vault>` and shares it across the scheduler).
pub struct Vault {
    _ttl: Duration,
    _map: RwLock<HashMap<SecretRef, Entry>>,
}

/// Internal storage entry. Drop walks the Zeroizing and zeroes the
/// plaintext bytes automatically.
#[allow(dead_code)] // wired up in Task 2
struct Entry {
    plaintext: Zeroizing<Vec<u8>>,
    expires_at: Instant,
}

#[derive(Debug)]
pub enum RedeemResult {
    Hit(Zeroizing<Vec<u8>>),
    Expired,
    NotFound,
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault: secret lookup failed: {0}")]
    Secrets(#[from] kastellan_db::secrets::SecretsError),

    /// Hard-fail on audit write — see spec §5.4. Wraps the existing
    /// `kastellan_db::DbError` returned by `audit::insert`.
    #[error("vault: audit row insert failed during materialize: {0}")]
    Audit(kastellan_db::DbError),

    #[error("vault: materialized plaintext is empty")]
    EmptyPlaintext,
}

impl Vault {
    /// Construct with [`DEFAULT_TTL`] (1 h).
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }

    /// Construct with a custom TTL (for tests).
    pub fn with_ttl(ttl: Duration) -> Self {
        Vault {
            _ttl: ttl,
            _map: RwLock::new(HashMap::new()),
        }
    }

    /// Decrypt `name` via `db::secrets::get`, stash the plaintext keyed
    /// by a fresh ref, write the `policy / secret.materialized` audit
    /// row, and return the ref.
    pub async fn materialize(
        &self,
        _pool: &PgPool,
        _key_provider: &dyn KeyProvider,
        _name: &str,
        _actor: &str,
    ) -> Result<SecretRef, VaultError> {
        unimplemented!("Vault::materialize — filled in Task 2")
    }

    /// Sync redemption. Returns the discrimination between Hit / Expired
    /// / NotFound. Expired entries are lazily dropped on this call.
    pub fn redeem(&self, _r: &SecretRef) -> RedeemResult {
        unimplemented!("Vault::redeem — filled in Task 2")
    }
}

impl Default for Vault {
    fn default() -> Self {
        Vault::new()
    }
}

#[cfg(test)]
mod tests;

// Pin: the prefix-len + hex-len constants are referenced by both the
// walker (in `substitute.rs`) and the format string in
// `materialize`. Keeping them at the module root keeps the seam tight.
#[allow(dead_code)]
const _: () = {
    // Compile-time pin so a typo in REF_HEX_LEN trips a build error
    // here rather than at runtime via a length mismatch.
    assert!(REF_PREFIX.len() == 9, "REF_PREFIX must be 'secret://' (9 bytes)");
    assert!(REF_HEX_LEN == 8, "REF_HEX_LEN must match {:08x} format width");
};
```

- [ ] **Step 4: Create `core/src/secrets/substitute.rs` (stub)**

```rust
//! Substitution walker. Mutates `serde_json::Value` in place,
//! replacing every `Value::String` that is exactly `secret://<8-hex>`
//! with the redeemed plaintext (interpreted as UTF-8). One
//! [`RedemptionEvent`] is emitted per substitution; the dispatcher
//! translates each into a `policy / secret.redeemed` audit row.

use super::vault::{RedeemResult, SecretRef};

/// Test seam: the walker takes a `&dyn RedeemFromVault` so unit tests
/// can supply a `FakeVault` without spinning up a real [`Vault`].
/// Production passes `&*vault` (which implements the trait inherently
/// via `Vault::redeem`).
pub trait RedeemFromVault {
    fn redeem(&self, r: &SecretRef) -> RedeemResult;
}

/// One successful substitution. The chokepoint translates each event
/// into a `policy / secret.redeemed` audit row.
#[derive(Debug, Clone)]
pub struct RedemptionEvent {
    pub ref_hash: String, // SHA-256(ref.as_str()), 64-char lowercase hex
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingReason {
    NotFound,
    Expired,
}

impl MissingReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            MissingReason::NotFound => "not_found",
            MissingReason::Expired => "expired",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubstituteError {
    #[error("substitute: ref {ref_hash} missing from vault (reason: {})", reason.as_str())]
    MissingRef {
        ref_hash: String,
        reason: MissingReason,
    },

    #[error("substitute: ref {ref_hash} plaintext is not valid UTF-8")]
    PlaintextNotUtf8 { ref_hash: String },
}

/// Walk `value` and substitute every `Value::String` whose contents
/// are exactly a well-formed `secret://<8-hex>` ref with the redeemed
/// plaintext. Returns one [`RedemptionEvent`] per substitution.
///
/// Fails closed at the first miss / UTF-8 error — `value` is left in
/// an unspecified state; callers must drop it on `Err`.
pub fn substitute_refs_in_params(
    _value: &mut serde_json::Value,
    _vault: &dyn RedeemFromVault,
) -> Result<Vec<RedemptionEvent>, SubstituteError> {
    unimplemented!("substitute_refs_in_params — filled in Task 3")
}

#[cfg(test)]
mod tests;
```

- [ ] **Step 5: Create `core/src/secrets/tests.rs` (module-level tests)**

```rust
//! Module-level tests: public surface re-exports, const pins,
//! `SecretRef` round-trip. The richer Vault and Walker tests live in
//! `vault/tests.rs` and `substitute/tests.rs`.

use super::*;

#[test]
fn default_ttl_is_exactly_one_hour() {
    assert_eq!(DEFAULT_TTL, std::time::Duration::from_secs(3600));
}

#[test]
fn ref_prefix_is_secret_scheme() {
    assert_eq!(REF_PREFIX, "secret://");
}

#[test]
fn ref_hex_len_is_eight() {
    assert_eq!(REF_HEX_LEN, 8);
}

#[test]
fn secret_ref_as_str_roundtrip() {
    let r = SecretRef::from_raw("secret://deadbeef".to_string());
    assert_eq!(r.as_str(), "secret://deadbeef");
}

#[test]
fn secret_ref_hash_is_64_lowercase_hex() {
    let r = SecretRef::from_raw("secret://deadbeef".to_string());
    let h = r.ref_hash();
    assert_eq!(h.len(), 64, "SHA-256 hex must be 64 chars");
    assert!(
        h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "ref_hash must be lowercase hex: got {h:?}"
    );
}

#[test]
fn secret_ref_hash_is_stable() {
    // Same input → same output across calls.
    let r = SecretRef::from_raw("secret://aabbccdd".to_string());
    assert_eq!(r.ref_hash(), r.ref_hash());
}

#[test]
fn secret_ref_hash_distinguishes_refs() {
    let a = SecretRef::from_raw("secret://aabbccdd".to_string());
    let b = SecretRef::from_raw("secret://aabbccde".to_string());
    assert_ne!(a.ref_hash(), b.ref_hash());
}
```

- [ ] **Step 6: Create stub sibling `tests.rs` files for vault and substitute (mark explicitly stub)**

The presence of `#[cfg(test)] mod tests;` in `vault.rs` and `substitute.rs` requires the sibling test files to exist or `cargo test` fails to compile. Create them as one-line stubs that Tasks 2 and 3 will replace.

Create `core/src/secrets/vault/tests.rs`:

```rust
//! Vault lifecycle tests. Filled in Task 2.
//
// This stub keeps `#[cfg(test)] mod tests;` in vault.rs resolving.
```

Create `core/src/secrets/substitute/tests.rs`:

```rust
//! Walker tests. Filled in Task 3.
//
// This stub keeps `#[cfg(test)] mod tests;` in substitute.rs resolving.
```

- [ ] **Step 7: Build and run the module-level tests**

```sh
cargo build -p kastellan-core 2>&1 | tail -20
```

Expected: clean build (the stubs panic on call but no function exercises them yet).

```sh
cargo test -p kastellan-core secrets:: 2>&1 | grep -E "test result:" | head -5
```

Expected: `test result: ok. 7 passed; 0 failed; 0 ignored` for `core::secrets::tests`.

- [ ] **Step 8: Workspace test pin (no regression)**

```sh
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1103 failed:0 ignored:3` (1096 baseline + 7 new tests in `core::secrets::tests`). If the count differs by ±1, double-check the test bucket for typos before assuming a real regression.

- [ ] **Step 9: Commit**

```sh
git add core/src/lib.rs core/src/secrets/
git commit -m "$(cat <<'EOF'
feat(secrets): module skeleton + types + const pins

Task 1 of opaque secret references slice 1 (HANDOVER Item 31). Lays
the new `core::secrets` module with all public types as stubs that
panic on call. The 7 module-level tests pin the constants and the
SecretRef round-trip; Tasks 2 + 3 fill in Vault and the walker.

- core/src/lib.rs: declare `pub mod secrets;`
- core/src/secrets/mod.rs: public surface re-exports + 3 consts
  (DEFAULT_TTL=1h, REF_PREFIX="secret://", REF_HEX_LEN=8)
- core/src/secrets/vault.rs: SecretRef + Vault skeleton + RedeemResult
  + VaultError; materialize/redeem are unimplemented!()
- core/src/secrets/substitute.rs: RedeemFromVault trait + walker
  skeleton + SubstituteError + MissingReason; walker is unimplemented!()
- 7 unit tests in secrets/tests.rs (const pins, SecretRef
  as_str/ref_hash round-trip)

Workspace 1096 -> 1103 (+7) on macOS, all green.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `Vault` implementation

**Files:**
- Modify: `core/src/secrets/vault.rs` (replace stub bodies)
- Modify: `core/src/secrets/vault/tests.rs` (replace stub with real tests)
- Possibly modify: `core/Cargo.toml` (add `rand` direct dep if needed)

Fill in `Vault::materialize` and `Vault::redeem`. Materialize calls `db::secrets::get`, generates a fresh ref via `rand::OsRng`, writes the audit row (hard-fail), inserts into the map. Redeem reads under a sync RwLock with lazy GC on expiry.

- [ ] **Step 1: Check `rand` is available; add as direct dep if not**

```sh
grep '^rand' core/Cargo.toml || echo "MISSING — add direct dep"
```

If MISSING, edit `core/Cargo.toml` to add `rand = { workspace = true, features = ["std", "std_rng"] }` under `[dependencies]`. Then verify the workspace root provides it:

```sh
grep '^rand' Cargo.toml
```

If the workspace root also lacks it, add `rand = "0.8"` to the workspace `[workspace.dependencies]`. Run `cargo build -p kastellan-core` to confirm.

- [ ] **Step 2: Write the failing vault tests**

Replace `core/src/secrets/vault/tests.rs`:

```rust
//! Vault lifecycle tests. PG-free; uses a `pub(crate)` test helper to
//! insert entries without going through the async `materialize` path.

use std::time::Duration;

use super::*;

/// Pure test-only insert. Constructs an `Entry` with the given
/// plaintext and `now + ttl` expiry, stores under `r`. Mirrors the
/// `_test_*` inspector pattern from `worker_lifecycle::idle_timeout`.
pub(crate) fn _test_insert(vault: &Vault, r: SecretRef, plaintext: Vec<u8>) {
    let entry = Entry {
        plaintext: Zeroizing::new(plaintext),
        expires_at: Instant::now() + vault._ttl,
    };
    vault
        ._map
        .write()
        .expect("vault map poisoned")
        .insert(r, entry);
}

#[test]
fn new_uses_default_ttl() {
    let v = Vault::new();
    assert_eq!(v._ttl, super::super::DEFAULT_TTL);
}

#[test]
fn with_ttl_overrides() {
    let v = Vault::with_ttl(Duration::from_millis(250));
    assert_eq!(v._ttl, Duration::from_millis(250));
}

#[test]
fn default_constructs_with_default_ttl() {
    let v = Vault::default();
    assert_eq!(v._ttl, super::super::DEFAULT_TTL);
}

#[test]
fn redeem_hits_within_ttl() {
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = SecretRef::from_raw("secret://00000001".to_string());
    _test_insert(&v, r.clone(), b"plaintext-a".to_vec());

    match v.redeem(&r) {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-a"),
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn redeem_returns_not_found_when_absent() {
    let v = Vault::new();
    let r = SecretRef::from_raw("secret://00000002".to_string());

    match v.redeem(&r) {
        RedeemResult::NotFound => (),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn redeem_returns_expired_past_ttl_and_gcs_entry() {
    let v = Vault::with_ttl(Duration::from_millis(50));
    let r = SecretRef::from_raw("secret://00000003".to_string());
    _test_insert(&v, r.clone(), b"plaintext-b".to_vec());

    std::thread::sleep(Duration::from_millis(80));

    match v.redeem(&r) {
        RedeemResult::Expired => (),
        other => panic!("expected Expired, got {other:?}"),
    }
    // Second redeem proves the entry was lazy-GC'd on the first call.
    match v.redeem(&r) {
        RedeemResult::NotFound => (),
        other => panic!("expected NotFound after lazy GC, got {other:?}"),
    }
}

#[test]
fn redeem_returns_owned_zeroizing_clone() {
    // Caller's Zeroizing is independent of the vault's stored copy —
    // dropping it doesn't invalidate subsequent redeems within TTL.
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = SecretRef::from_raw("secret://00000004".to_string());
    _test_insert(&v, r.clone(), b"plaintext-c".to_vec());

    let first = v.redeem(&r);
    drop(first);
    match v.redeem(&r) {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-c"),
        other => panic!("expected Hit on second redeem, got {other:?}"),
    }
}

#[test]
fn vault_drop_zeroes_plaintext() {
    // Construct a vault, insert, drop, and assert via Miri-style
    // intent. We can't observe zeroing directly without unsafe RAM
    // peeking; this test is a contract pin via Zeroizing::Drop being
    // the underlying primitive. Smoke: build a vault, insert, drop —
    // no panic, no UB. Real zeroing is the responsibility of
    // Zeroizing<Vec<u8>>'s Drop impl, which is already pinned by the
    // upstream `zeroize` crate's own tests.
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = SecretRef::from_raw("secret://00000005".to_string());
    _test_insert(&v, r, b"plaintext-to-zero".to_vec());
    drop(v);
}

#[test]
fn vault_redeem_concurrent_readers_dont_block_each_other() {
    // Spawn 4 threads each redeeming the same ref 100 times. No panic,
    // no deadlock, all return Hit. Light smoke for the RwLock fast
    // path.
    let v = std::sync::Arc::new(Vault::with_ttl(Duration::from_secs(60)));
    let r = SecretRef::from_raw("secret://00000006".to_string());
    _test_insert(&v, r.clone(), b"plaintext-concurrent".to_vec());

    let mut handles = Vec::new();
    for _ in 0..4 {
        let v = v.clone();
        let r = r.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                match v.redeem(&r) {
                    RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-concurrent"),
                    other => panic!("expected Hit, got {other:?}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
}
```

Run the tests now to verify they FAIL with `unimplemented!()` panics:

```sh
cargo test -p kastellan-core secrets::vault::tests 2>&1 | tail -20
```

Expected: 8 tests, several panicking at `Vault::redeem — filled in Task 2`. `new_uses_default_ttl`, `with_ttl_overrides`, `default_constructs_with_default_ttl`, and `vault_drop_zeroes_plaintext` should PASS already (they don't call `redeem`). The 4 redeem tests should panic. Exactly 4 panics is the expected failure mode.

- [ ] **Step 3: Implement `Vault::redeem`**

Replace the `redeem` body in [core/src/secrets/vault.rs](core/src/secrets/vault.rs):

```rust
pub fn redeem(&self, r: &SecretRef) -> RedeemResult {
    let now = Instant::now();

    // Fast path: read lock, check expiry, clone on Hit.
    {
        let map = self._map.read().expect("vault map poisoned");
        match map.get(r) {
            None => return RedeemResult::NotFound,
            Some(entry) if now < entry.expires_at => {
                return RedeemResult::Hit(Zeroizing::new(entry.plaintext.to_vec()));
            }
            Some(_expired) => {
                // Fall through to slow path below.
            }
        }
    }

    // Slow path: expired entry — drop the read lock (already done by
    // scope exit), acquire write lock, remove entry. The remove zeros
    // the Zeroizing<Vec<u8>> via Drop. Subsequent redeems return
    // NotFound (which is what we test in step 2).
    {
        let mut map = self._map.write().expect("vault map poisoned");
        // Re-check under write lock in case another caller already
        // GC'd this ref in the meantime — defensive, not load-bearing
        // since concurrent expiry-races are idempotent.
        map.remove(r);
    }
    RedeemResult::Expired
}
```

Run the redeem tests:

```sh
cargo test -p kastellan-core secrets::vault::tests::redeem 2>&1 | tail -10
```

Expected: 4 PASS (`redeem_hits_within_ttl`, `redeem_returns_not_found_when_absent`, `redeem_returns_expired_past_ttl_and_gcs_entry`, `redeem_returns_owned_zeroizing_clone`).

- [ ] **Step 4: Implement `Vault::materialize`** with audit row + ref generation

Replace the `materialize` body in `vault.rs` (and add `rand::RngCore` + `Instant::now()` machinery). Insert imports at the top:

```rust
use rand::RngCore;
use serde_json::json;
```

Replace materialize:

```rust
pub async fn materialize(
    &self,
    pool: &PgPool,
    key_provider: &dyn KeyProvider,
    name: &str,
    actor: &str,
) -> Result<SecretRef, VaultError> {
    // 1. Decrypt the secret at the host boundary.
    let plaintext: Zeroizing<Vec<u8>> =
        kastellan_db::secrets::get(pool, key_provider, name, None).await?;

    if plaintext.is_empty() {
        return Err(VaultError::EmptyPlaintext);
    }

    // 2. Generate the ref: 4 random bytes via OsRng → `secret://{:08x}`.
    //    OsRng is the cryptographic RNG; collision probability is
    //    negligible at any expected workload (see spec §2).
    let mut rng = rand::rngs::OsRng;
    let mut tail = [0u8; 4];
    rng.fill_bytes(&mut tail);
    let secret_ref = SecretRef::from_raw(format!(
        "{}{:02x}{:02x}{:02x}{:02x}",
        REF_PREFIX, tail[0], tail[1], tail[2], tail[3]
    ));

    debug_assert_eq!(
        secret_ref.as_str().len(),
        REF_PREFIX.len() + REF_HEX_LEN,
        "freshly-built ref must satisfy the well-formed-ref length invariant"
    );

    // 3. Write the audit row FIRST. On failure we return Err without
    //    inserting into the vault — the spec's hard-fail-on-materialize-
    //    audit posture (§5.4) means no materialized-but-unaudited ref
    //    ever exists. Subsequent crash between this and the vault
    //    insert is acceptable: the audit row is the source of truth.
    let ref_hash = secret_ref.ref_hash();
    let ttl_secs = self._ttl.as_secs();
    let payload = json!({
        "name":     name,
        "ref_hash": ref_hash,
        "ttl_secs": ttl_secs,
        "actor":    actor,
    });
    kastellan_db::audit::insert(pool, "policy", "secret.materialized", payload)
        .await
        .map_err(VaultError::Audit)?;

    // 4. Insert into vault under the brief sync write lock. The
    //    Zeroizing<Vec<u8>> moves into the entry; on Vault::Drop or
    //    on TTL eviction, Zeroizing::Drop zeroes the plaintext bytes.
    let entry = Entry {
        plaintext: Zeroizing::new(plaintext.to_vec()),
        expires_at: Instant::now() + self._ttl,
    };
    {
        let mut map = self._map.write().expect("vault map poisoned");
        map.insert(secret_ref.clone(), entry);
    }

    Ok(secret_ref)
}
```

- [ ] **Step 5: Drop the `_test_insert` underscore guards in production code**

`Vault::_ttl` and `Vault::_map` are still named with leading underscores from Task 1's stubs. With the real `redeem` and `materialize` referencing them, the unused-warning suppression isn't needed; rename to `ttl` and `map` for readability. Drop the `#[allow(dead_code)]` on `Entry`.

Search for the renames:

```sh
grep -n '_ttl\|_map' core/src/secrets/vault.rs
```

Edit all occurrences in `vault.rs` and `vault/tests.rs`. After the rename, the test `_test_insert` helper still uses `vault.map` (one access), `vault.ttl` (one access).

- [ ] **Step 6: Run all Task 2 tests**

```sh
cargo test -p kastellan-core secrets::vault 2>&1 | tail -15
```

Expected: 9 tests, all PASS (the 8 from step 2 + the const pin compile is at module level).

- [ ] **Step 7: Workspace test pin**

```sh
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1112 failed:0 ignored:3` (1103 + 9 vault tests).

- [ ] **Step 8: Commit**

```sh
git add core/Cargo.toml Cargo.toml core/src/secrets/vault.rs core/src/secrets/vault/tests.rs
git commit -m "$(cat <<'EOF'
feat(secrets/vault): TTL'd in-process Vault with audit-hard-fail on materialize

Task 2 of opaque secret references slice 1 (HANDOVER Item 31).

Vault::materialize:
- Decrypts via db::secrets::get at the host boundary.
- Generates a fresh `secret://<8-hex>` ref via OsRng (4 random bytes).
- Writes the `policy / secret.materialized` audit row carrying
  {name, ref_hash, ttl_secs, actor}. Hard-fails on audit error
  (VaultError::Audit) — no materialized-but-unaudited ref can exist.
- Inserts (SecretRef, Entry { plaintext: Zeroizing<Vec<u8>>,
  expires_at }) under a brief sync write lock.

Vault::redeem:
- Sync RwLock fast path: present + within-TTL → Hit (cloned Zeroizing,
  caller owns; vault keeps its copy until TTL expiry).
- Absent → NotFound.
- Present but expired → drop read lock, acquire write lock, remove
  (Zeroizing::Drop zeroes), return Expired.

9 unit tests cover: TTL defaults, custom TTL, Default impl, Hit-within-
TTL, NotFound when absent, Expired-past-TTL-with-lazy-GC, owned-
Zeroizing-clone semantics, Drop smoke, concurrent-reader load.

Workspace 1103 -> 1112 (+9) on macOS, all green.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Substitution walker

**Files:**
- Modify: `core/src/secrets/substitute.rs` (replace stub body)
- Modify: `core/src/secrets/substitute/tests.rs` (replace stub with real tests)

Implement `substitute_refs_in_params` as a recursive walker. Add `RedeemFromVault` inherent impl for `Vault`. Tests use a `FakeVault` fixture.

- [ ] **Step 1: Write the failing walker tests**

Replace `core/src/secrets/substitute/tests.rs`:

```rust
//! Walker tests. Use a `FakeVault` fixture so tests are PG-free.

use std::collections::HashMap;

use serde_json::json;
use zeroize::Zeroizing;

use super::*;
use crate::secrets::vault::{RedeemResult, SecretRef};

/// Stub vault for walker tests. Each entry is either present
/// (with plaintext) or absent (NotFound) or marked Expired.
enum FakeEntry {
    Present(Vec<u8>),
    Expired,
}

struct FakeVault(HashMap<SecretRef, FakeEntry>);

impl FakeVault {
    fn new() -> Self {
        FakeVault(HashMap::new())
    }
    fn with(mut self, r: SecretRef, plaintext: &[u8]) -> Self {
        self.0.insert(r, FakeEntry::Present(plaintext.to_vec()));
        self
    }
    fn with_expired(mut self, r: SecretRef) -> Self {
        self.0.insert(r, FakeEntry::Expired);
        self
    }
}

impl RedeemFromVault for FakeVault {
    fn redeem(&self, r: &SecretRef) -> RedeemResult {
        match self.0.get(r) {
            Some(FakeEntry::Present(pt)) => RedeemResult::Hit(Zeroizing::new(pt.clone())),
            Some(FakeEntry::Expired) => RedeemResult::Expired,
            None => RedeemResult::NotFound,
        }
    }
}

fn make_ref(tail: &str) -> SecretRef {
    SecretRef::from_raw(format!("secret://{tail}"))
}

#[test]
fn top_level_ref_string_is_substituted() {
    let r = make_ref("aabbccdd");
    let vault = FakeVault::new().with(r.clone(), b"plaintext-X");

    let mut v = json!("secret://aabbccdd");
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(v, json!("plaintext-X"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].ref_hash, r.ref_hash());
}

#[test]
fn nested_ref_in_object_is_substituted() {
    let r = make_ref("11223344");
    let vault = FakeVault::new().with(r.clone(), b"PT-1");

    let mut v = json!({
        "argv": ["printf", "%s", "secret://11223344"],
        "env":  {"TOKEN": "secret://11223344"}
    });
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 2);
    for e in &events {
        assert_eq!(e.ref_hash, r.ref_hash());
    }
    // Both occurrences substituted in place.
    assert_eq!(v["argv"][2], json!("PT-1"));
    assert_eq!(v["env"]["TOKEN"], json!("PT-1"));
}

#[test]
fn nested_ref_in_array_is_substituted() {
    let r = make_ref("aabbccdd");
    let vault = FakeVault::new().with(r.clone(), b"PT-arr");

    let mut v = json!(["leave-me-alone", "secret://aabbccdd", 42]);
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 1);
    assert_eq!(v[1], json!("PT-arr"));
    assert_eq!(v[0], json!("leave-me-alone"));
    assert_eq!(v[2], json!(42));
}

#[test]
fn embedded_substring_left_alone() {
    // The spec is exact-match-only. `"Bearer secret://aabbccdd"` is NOT
    // a well-formed ref string; pass through verbatim. The vault is
    // populated with the would-be ref to prove the walker doesn't
    // even consult it on a non-exact-match string.
    let r = make_ref("aabbccdd");
    let vault = FakeVault::new().with(r, b"PT-X");

    let mut v = json!({"header": "Bearer secret://aabbccdd"});
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 0);
    assert_eq!(v["header"], json!("Bearer secret://aabbccdd"));
}

#[test]
fn uppercase_hex_left_alone() {
    // Refs are generated via `{:08x}` (always lowercase). Uppercase
    // hex is not a well-formed ref shape — pass through verbatim so
    // a planner can't synthesise a casing-shift to evade.
    let mut v = json!("secret://AABBCCDD");
    let vault = FakeVault::new();
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 0);
    assert_eq!(v, json!("secret://AABBCCDD"));
}

#[test]
fn wrong_length_hex_left_alone() {
    // 7 and 9 hex digits — pass through both.
    let vault = FakeVault::new();
    for tail in ["aabbccd", "aabbccdde", "aabbccdde0"] {
        let mut v = json!(format!("secret://{tail}"));
        let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
        assert_eq!(events.len(), 0, "tail {tail} should not match");
    }
}

#[test]
fn missing_ref_returns_missing_ref_with_not_found_reason() {
    let r = make_ref("dead0001");
    let vault = FakeVault::new();
    let mut v = json!("secret://dead0001");

    let err = substitute_refs_in_params(&mut v, &vault).expect_err("must fail closed");
    match err {
        SubstituteError::MissingRef { ref_hash, reason } => {
            assert_eq!(ref_hash, r.ref_hash());
            assert_eq!(reason, MissingReason::NotFound);
        }
        other => panic!("expected MissingRef(NotFound), got {other:?}"),
    }
}

#[test]
fn expired_ref_returns_missing_ref_with_expired_reason() {
    let r = make_ref("dead0002");
    let vault = FakeVault::new().with_expired(r.clone());
    let mut v = json!("secret://dead0002");

    let err = substitute_refs_in_params(&mut v, &vault).expect_err("must fail closed");
    match err {
        SubstituteError::MissingRef { ref_hash, reason } => {
            assert_eq!(ref_hash, r.ref_hash());
            assert_eq!(reason, MissingReason::Expired);
        }
        other => panic!("expected MissingRef(Expired), got {other:?}"),
    }
}

#[test]
fn non_utf8_plaintext_returns_plaintext_not_utf8_error() {
    let r = make_ref("00ff00ff");
    // 0xFF is not valid UTF-8 by itself.
    let vault = FakeVault::new().with(r.clone(), &[0xFF, 0xFE, 0xFD]);
    let mut v = json!("secret://00ff00ff");

    let err = substitute_refs_in_params(&mut v, &vault).expect_err("must reject binary");
    match err {
        SubstituteError::PlaintextNotUtf8 { ref_hash } => {
            assert_eq!(ref_hash, r.ref_hash());
        }
        other => panic!("expected PlaintextNotUtf8, got {other:?}"),
    }
}

#[test]
fn empty_object_is_no_op() {
    let vault = FakeVault::new();
    let mut v = json!({});
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
    assert_eq!(events.len(), 0);
    assert_eq!(v, json!({}));
}

#[test]
fn empty_array_is_no_op() {
    let vault = FakeVault::new();
    let mut v = json!([]);
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
    assert_eq!(events.len(), 0);
    assert_eq!(v, json!([]));
}

#[test]
fn null_number_bool_are_no_ops() {
    let vault = FakeVault::new();
    for mut v in [json!(null), json!(42), json!(3.14), json!(true), json!(false)] {
        let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
        assert_eq!(events.len(), 0);
    }
}

#[test]
fn non_ref_string_is_no_op() {
    let vault = FakeVault::new();
    let mut v = json!("just some unrelated text");
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
    assert_eq!(events.len(), 0);
    assert_eq!(v, json!("just some unrelated text"));
}

#[test]
fn multiple_distinct_refs_in_one_value_all_substituted() {
    let a = make_ref("11111111");
    let b = make_ref("22222222");
    let vault = FakeVault::new()
        .with(a.clone(), b"PT-a")
        .with(b.clone(), b"PT-b");

    let mut v = json!({"left": "secret://11111111", "right": "secret://22222222"});
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 2);
    assert_eq!(v["left"], json!("PT-a"));
    assert_eq!(v["right"], json!("PT-b"));
}
```

Run the tests now to verify they FAIL with `unimplemented!()`:

```sh
cargo test -p kastellan-core secrets::substitute::tests 2>&1 | tail -10
```

Expected: 14 test panics at `substitute_refs_in_params — filled in Task 3`.

- [ ] **Step 2: Implement `is_well_formed_ref` helper** (module-private, pure)

Add to `core/src/secrets/substitute.rs` above the `substitute_refs_in_params` definition:

```rust
use super::{REF_HEX_LEN, REF_PREFIX};

/// True iff `s` is exactly `secret://` + 8 lowercase hex chars and
/// nothing else. The lowercase-only check is belt-and-braces (refs
/// are generated with `{:08x}` which is always lowercase) so a
/// planner can't synthesise a casing-shifted ref to evade.
fn is_well_formed_ref(s: &str) -> bool {
    if s.len() != REF_PREFIX.len() + REF_HEX_LEN {
        return false;
    }
    if !s.starts_with(REF_PREFIX) {
        return false;
    }
    s[REF_PREFIX.len()..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
```

- [ ] **Step 3: Implement the recursive walker**

Replace the `substitute_refs_in_params` body:

```rust
pub fn substitute_refs_in_params(
    value: &mut serde_json::Value,
    vault: &dyn RedeemFromVault,
) -> Result<Vec<RedemptionEvent>, SubstituteError> {
    let mut events = Vec::new();
    walk(value, vault, &mut events)?;
    Ok(events)
}

fn walk(
    value: &mut serde_json::Value,
    vault: &dyn RedeemFromVault,
    events: &mut Vec<RedemptionEvent>,
) -> Result<(), SubstituteError> {
    match value {
        serde_json::Value::String(s) => {
            if !is_well_formed_ref(s) {
                return Ok(());
            }
            // Construct the SecretRef directly from the well-formed string.
            let secret_ref = SecretRef::from_raw(s.clone());
            let ref_hash = secret_ref.ref_hash();
            match vault.redeem(&secret_ref) {
                RedeemResult::Hit(pt) => {
                    // Convert plaintext to UTF-8 String; reject on
                    // invalid UTF-8 (binary secrets are out of scope).
                    let plaintext = String::from_utf8(pt.to_vec()).map_err(|_| {
                        SubstituteError::PlaintextNotUtf8 {
                            ref_hash: ref_hash.clone(),
                        }
                    })?;
                    *s = plaintext;
                    events.push(RedemptionEvent { ref_hash });
                    // pt drops here (Zeroizing zeroes its bytes); the
                    // new `s` is a regular String — see spec §9
                    // limitation 1 (known and accepted).
                    Ok(())
                }
                RedeemResult::Expired => Err(SubstituteError::MissingRef {
                    ref_hash,
                    reason: MissingReason::Expired,
                }),
                RedeemResult::NotFound => Err(SubstituteError::MissingRef {
                    ref_hash,
                    reason: MissingReason::NotFound,
                }),
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                walk(item, vault, events)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (_key, val) in map.iter_mut() {
                walk(val, vault, events)?;
            }
            Ok(())
        }
        // Number, Bool, Null — structurally cannot contain refs.
        _ => Ok(()),
    }
}
```

- [ ] **Step 4: Add inherent `impl RedeemFromVault for Vault`**

Add to `core/src/secrets/vault.rs` (anywhere after the `Vault` impl block):

```rust
impl super::substitute::RedeemFromVault for Vault {
    fn redeem(&self, r: &SecretRef) -> RedeemResult {
        Vault::redeem(self, r)
    }
}
```

This lets the chokepoint call `substitute_refs_in_params(&mut params, &*vault)` with the production `Vault` directly.

- [ ] **Step 5: Run all walker tests**

```sh
cargo test -p kastellan-core secrets::substitute 2>&1 | tail -10
```

Expected: 14 PASS.

- [ ] **Step 6: Workspace test pin**

```sh
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1126 failed:0 ignored:3` (1112 + 14 walker tests).

- [ ] **Step 7: Commit**

```sh
git add core/src/secrets/substitute.rs core/src/secrets/substitute/tests.rs core/src/secrets/vault.rs
git commit -m "$(cat <<'EOF'
feat(secrets/substitute): exact-match walker with FakeVault test seam

Task 3 of opaque secret references slice 1 (HANDOVER Item 31).

substitute_refs_in_params recursively walks a serde_json::Value and
replaces every Value::String that is *exactly* `secret://<8-hex>` with
the redeemed plaintext (UTF-8). Stops at the first miss or UTF-8
failure (fail-closed). Embedded substrings, uppercase hex, and
wrong-length tails are passed through verbatim — exact-match only.

The new RedeemFromVault trait is the pure-test seam: walker takes
&dyn RedeemFromVault, FakeVault test fixture implements it without
PG or keyring. Vault gets an inherent impl so production passes
&*vault directly.

is_well_formed_ref(s: &str) -> bool is the structural predicate;
mirrors the spec §3 invariants exactly.

14 unit tests cover: top-level / nested-object / nested-array
substitution, embedded-substring left alone, uppercase-hex left alone,
3 wrong-length tails left alone, MissingRef(NotFound),
MissingRef(Expired), PlaintextNotUtf8, empty-container no-ops,
no-ref-string no-op, multiple distinct refs.

Workspace 1112 -> 1126 (+14) on macOS, all green.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire into `tool_host::dispatch` + scheduler plumbing + main.rs + 8 integration tests

**Files:**
- Modify: `core/src/tool_host.rs` (new param + substitution block + new error variant)
- Modify: `core/src/scheduler/tool_dispatch.rs` (carry `Arc<Vault>`)
- Modify: `core/src/scheduler/runner.rs` (accept `Arc<Vault>` in `spawn_scheduler`)
- Modify: `core/src/main.rs` (bootstrap `KASTELLAN_BOOTSTRAP_SECRETS`)
- Create: `core/tests/secret_vault_e2e.rs` (8 integration tests)
- Modify: `core/src/main.rs` callers if `spawn_scheduler`'s arity changed elsewhere

The largest task. We make the dispatch signature change first (compile errors localize the call sites), thread the `Arc<Vault>` through, then write the integration tests last so they exercise the full chain end-to-end.

- [ ] **Step 1: Add `ToolHostError::SecretRedemptionFailed` + `#[non_exhaustive]`**

Read [core/src/tool_host.rs](core/src/tool_host.rs) lines 21-35 first to find the enum. Apply:

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]   // NEW — Item 31. First variant addition since Option M (2026-05-10).
pub enum ToolHostError {
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] ClientError),

    /// NEW — Item 31. Substitution failed before the worker call.
    /// The dispatch's audit-row side-effect
    /// (`policy / secret.redemption_failed`) happened before this
    /// error was returned. Scheduler should treat this like
    /// POLICY_DENIED — task step fails fast, no retry budget burned.
    #[error("tool_host: secret redemption failed: {0}")]
    SecretRedemptionFailed(#[from] crate::secrets::SubstituteError),
}
```

- [ ] **Step 2: Build and surface all match-on-ToolHostError sites**

```sh
cargo build -p kastellan-core 2>&1 | grep -E "non-exhaustive|missing match arm" | head -10
```

Expected: the `#[non_exhaustive]` change may surface match arms across the workspace that need an `_ =>` fallback. Check `core/src/scheduler/tool_dispatch.rs::map_dispatch_result` first — it handles every existing variant explicitly:

```sh
grep -n "ToolHostError::" core/src/ -r 2>&1 | head -10
```

If `map_dispatch_result` needs updating, add an arm:

```rust
ToolHostError::SecretRedemptionFailed(_) => StepOutcome::Err {
    code: "POLICY_DENIED".to_string(),
},
```

(Same code as the existing injection-guard / argv-policy denial path — scheduler treats it as a policy denial.)

- [ ] **Step 3: Modify `tool_host::dispatch` signature + body**

Read the current dispatch (lines 156-260) and edit. The signature gains `vault` and `params` becomes `mut params`:

```rust
pub async fn dispatch(
    pool: &sqlx::PgPool,
    vault: &crate::secrets::Vault,        // NEW
    worker: &mut SupervisedWorker,
    tool: &str,
    method: &str,
    mut params: serde_json::Value,        // NOTE: now `mut`
) -> Result<serde_json::Value, ToolHostError> {
```

Insert the substitution block at the very top of the body, before the existing `let started = Instant::now();`:

```rust
    let started = Instant::now();

    // ── Secret-ref substitution (Item 31, slice 1). ──
    //
    // Walk `params` and substitute every exact-match `secret://<8-hex>`
    // string with the redeemed plaintext. Fail-closed: any miss or
    // UTF-8 failure stops the dispatch before `worker.call` and
    // emits `policy / secret.redemption_failed`; the worker is not
    // called and no `tool:<n>` row is written.
    //
    // Redemption events are saved for emission AFTER `worker.call`
    // (so the elapsed_ms field is the dispatch elapsed time, not the
    // pre-call elapsed time).
    let redemption_events = match crate::secrets::substitute_refs_in_params(&mut params, vault) {
        Ok(events) => events,
        Err(e) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let (ref_hash, reason) = match &e {
                crate::secrets::SubstituteError::MissingRef { ref_hash, reason } => {
                    (ref_hash.clone(), reason.as_str())
                }
                crate::secrets::SubstituteError::PlaintextNotUtf8 { ref_hash } => {
                    (ref_hash.clone(), "plaintext_not_utf8")
                }
            };
            let payload = serde_json::json!({
                "tool":     tool,
                "method":   method,
                "ref_hash": ref_hash,
                "reason":   reason,
                "ms":       elapsed_ms,
            });
            if let Err(audit_err) =
                kastellan_db::audit::insert(pool, "policy", "secret.redemption_failed", payload).await
            {
                tracing::error!(
                    tool = %tool,
                    method = %method,
                    error = %audit_err,
                    "secret.redemption_failed audit insert failed"
                );
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
```

**Important:** the existing line `let req_for_audit = params.clone();` lower down must be deleted — we already snapshotted above. Search for it:

```sh
grep -n "req_for_audit" core/src/tool_host.rs
```

There should be exactly two occurrences after this edit: the one we wrote, and any reference inside the audit-payload builder.

Now insert the `secret.redeemed` rows BEFORE the existing tool row's audit insert. Find the existing block:

```rust
    if let Err(audit_err) =
        kastellan_db::audit::insert(pool, &actor, method, audit_payload).await
    {
```

Insert immediately above it:

```rust
    // ── Emit `secret.redeemed` audit rows (one per substitution). ──
    //
    // Best-effort: a transient audit insert failure is logged but
    // does not propagate. The plaintext is already substituted into
    // params and the worker already ran; turning the dispatch into
    // an error because the audit log was unreachable would be worse
    // than missing rows. (Materialize-time audit IS hard-fail; see
    // Vault::materialize and spec §5.4 for the asymmetry rationale.)
    for event in &redemption_events {
        let payload = serde_json::json!({
            "tool":     tool,
            "method":   method,
            "ref_hash": event.ref_hash,
            "ms":       elapsed_ms,
        });
        if let Err(e) =
            kastellan_db::audit::insert(pool, "policy", "secret.redeemed", payload).await
        {
            tracing::error!(
                tool = %tool,
                ref_hash = %event.ref_hash,
                error = %e,
                "secret.redeemed audit insert failed"
            );
        }
    }
```

- [ ] **Step 4: Update the dispatch docstring**

Find the existing "Audit-log shape" section (around lines 103-122) and replace:

```rust
/// ## Audit-log shape
///
/// **One to many rows per call.** The standard happy path writes one
/// `'tool:<name>'` row. Two additional row kinds:
///
/// * `policy / secret.redeemed` — emitted once per `secret://<8-hex>`
///   ref that was substituted from `params` (Item 31). Carries
///   `{tool, method, ref_hash, ms}`; never the plaintext.
/// * `policy / injection.blocked` — emitted when the prompt-injection
///   guard blocks a worker result (Item 30). Carries SHA-256 + length
///   + score + class codes; never the raw scanned body.
///
/// On a substitution miss the chokepoint writes exactly one row,
/// `policy / secret.redemption_failed`, and returns
/// `ToolHostError::SecretRedemptionFailed`. The tool row is NOT
/// written (the worker was not called).
```

(Keep the rest of the docstring as-is; only the "Audit-log shape" subsection changes.)

- [ ] **Step 5: Update `ToolHostStepDispatcher` to carry `Arc<Vault>`**

Read [core/src/scheduler/tool_dispatch.rs](core/src/scheduler/tool_dispatch.rs) — find `ToolHostStepDispatcher`'s struct, constructor, and `dispatch_step` method. Apply:

```rust
use std::sync::Arc;
use crate::secrets::Vault;

pub struct ToolHostStepDispatcher {
    pool: sqlx::PgPool,
    vault: Arc<Vault>,                   // NEW
    registry: Arc<ToolRegistry>,
    // ... existing fields ...
}

impl ToolHostStepDispatcher {
    pub fn new(
        pool: sqlx::PgPool,
        vault: Arc<Vault>,               // NEW (insert after `pool`)
        registry: Arc<ToolRegistry>,
        // ... existing args ...
    ) -> Self {
        Self {
            pool,
            vault,
            registry,
            // ...
        }
    }
}
```

Inside `dispatch_step`, find the existing `crate::tool_host::dispatch(...)` call and insert `&self.vault` after `&self.pool`:

```rust
let call_outcome = crate::tool_host::dispatch(
    &self.pool,
    &self.vault,             // NEW
    &mut worker,
    tool_name,
    method,
    params,
)
.await;
```

- [ ] **Step 6: Update `spawn_scheduler` signature to accept `Arc<Vault>`**

Read [core/src/scheduler/runner.rs](core/src/scheduler/runner.rs) and find `spawn_scheduler`. Add `vault: Arc<Vault>` (with `use` import). Thread through to `ToolHostStepDispatcher::new`.

Then update every caller. Find them:

```sh
grep -rn "spawn_scheduler" core/src/ core/tests/ 2>&1 | head -10
```

Each caller (`main.rs`, e2e tests like `scheduler_inner_loop_e2e`, `cli_ask_e2e`) needs to construct an `Arc<Vault>` and pass it. For test callers, an empty `Arc::new(Vault::new())` is the right default (no bootstrap secrets, no substitution will fire because no params include refs).

- [ ] **Step 7: Wire `KASTELLAN_BOOTSTRAP_SECRETS` into `main.rs`**

Read [core/src/main.rs](core/src/main.rs) and find the sequence around `connect_runtime_pool` / `spawn_mirror` / `spawn_scheduler`. Insert:

```rust
// ── Bootstrap secret materialization vault (Item 31, slice 1). ──
//
// KASTELLAN_BOOTSTRAP_SECRETS = "name1,name2,name3" — comma-separated
// names that must each exist in the `secrets` table. Missing names
// fail bring-up (fail-closed: a configured-but-missing secret is
// operator error). The ref string itself is NOT logged — only the
// ref_hash. Test fixtures reconstruct refs via their own
// Vault::materialize calls.
let vault = std::sync::Arc::new(kastellan_core::secrets::Vault::new());
if let Ok(names_csv) = std::env::var("KASTELLAN_BOOTSTRAP_SECRETS") {
    let key_provider = kastellan_db::secrets::OsKeyringProvider::ensure_initialized()?;
    for name in names_csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let secret_ref = vault
            .materialize(&pool, &key_provider, name, "core:bootstrap")
            .await?;
        tracing::info!(
            name = %name,
            ref_hash = %secret_ref.ref_hash(),
            "secret materialized at bootstrap"
        );
    }
}

// (existing) Pass vault into spawn_scheduler:
let scheduler_handle = spawn_scheduler(pool.clone(), vault.clone(), /* existing args */).await?;
```

The exact insertion point depends on where `pool` is bound in `main.rs`; the vault construction should be after `pool` exists but before `spawn_scheduler` is called.

- [ ] **Step 8: Compile-check the workspace**

```sh
cargo build --workspace 2>&1 | tail -20
```

Expected: clean build. If there are missing match arms or signature mismatches at scheduler-test call sites, fix them by passing `Arc::new(Vault::new())` as the new `vault` arg.

- [ ] **Step 9: Run the existing test suite to check nothing regressed**

```sh
cargo test --workspace --no-run 2>&1 | tail -10
```

Expected: every test compiles. Then:

```sh
cargo test -p kastellan-core --lib secrets:: 2>&1 | tail -5
```

Expected: 30 PASS (7 mod + 9 vault + 14 substitute). Now the full workspace:

```sh
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1126 failed:0 ignored:3` (no integration tests added yet; the +30 from secrets:: are already counted from Task 1-3 commits).

- [ ] **Step 10: Create `core/tests/secret_vault_e2e.rs`**

This is the load-bearing end-to-end pin. Build the file in one go. It mirrors `injection_guard_e2e.rs` structure: bring up per-test PG cluster + sandbox + supervisor + shell-exec worker, exercise dispatch through the real chokepoint.

```rust
//! End-to-end integration tests for opaque secret references (Item 31).
//!
//! Mirrors `injection_guard_e2e.rs` shape: per-test PG cluster via
//! tests_common, real shell-exec worker, real sandbox, real audit log.
//! Skip-as-pass on hosts without PG/supervisor/sandbox/worker; on this
//! Mac set `KASTELLAN_PG_BIN_DIR` to run live.

use std::sync::Arc;
use std::time::Duration;

use kastellan_core::secrets::{
    substitute_refs_in_params, MissingReason, RedeemFromVault, RedemptionEvent, SecretRef,
    SubstituteError, Vault, VaultError,
};
use kastellan_core::tool_host::{self, ToolHostError, WorkerSpec};
use kastellan_db::secrets::{KeyProvider, MapKeyProvider, SecretsError, KEY_LEN};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, sandbox_factory_or_skip,
    shell_exec_binary_or_skip, skip_if_no_supervisor, unique_suffix,
};
use serde_json::json;
use sqlx::Row;

const TEST_KEY_ID: &str = "test-keyring";

fn test_key_provider() -> MapKeyProvider {
    MapKeyProvider::new(TEST_KEY_ID, [42u8; KEY_LEN])
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_writes_audit_row_and_returns_ref() {
    let Some(bin_dir) = pg_bin_dir_or_skip("materialize_writes_audit_row_and_returns_ref") else { return; };
    skip_if_no_supervisor("materialize_writes_audit_row_and_returns_ref");

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;

    let kp = test_key_provider();
    kastellan_db::secrets::put(&pool, &kp, "test-secret-X", b"plaintext-XYZ", None)
        .await
        .expect("put");

    let vault = Vault::new();
    let secret_ref = vault
        .materialize(&pool, &kp, "test-secret-X", "test")
        .await
        .expect("materialize");

    assert!(
        secret_ref.as_str().starts_with("secret://"),
        "ref must begin with secret:// prefix, got {}",
        secret_ref.as_str()
    );
    assert_eq!(
        secret_ref.as_str().len(),
        "secret://".len() + 8,
        "ref must be 'secret://' + 8 hex chars"
    );

    let rows: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT actor, action, payload FROM audit_log WHERE actor = 'policy' AND action = 'secret.materialized'",
    )
    .fetch_all(&pool)
    .await
    .expect("query");

    assert_eq!(rows.len(), 1, "exactly one secret.materialized row");

    let payload: serde_json::Value = rows[0].try_get("payload").expect("payload");
    assert_eq!(payload["name"], json!("test-secret-X"));
    assert_eq!(payload["ref_hash"], json!(secret_ref.ref_hash()));
    assert_eq!(payload["ttl_secs"], json!(3600));
    assert_eq!(payload["actor"], json!("test"));
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_fails_when_secret_missing() {
    let Some(bin_dir) = pg_bin_dir_or_skip("materialize_fails_when_secret_missing") else { return; };
    skip_if_no_supervisor("materialize_fails_when_secret_missing");

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;
    let kp = test_key_provider();

    let vault = Vault::new();
    let err = vault
        .materialize(&pool, &kp, "no-such-secret", "test")
        .await
        .expect_err("must fail");

    match err {
        VaultError::Secrets(SecretsError::NotFound(name)) => {
            assert_eq!(name, "no-such-secret");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }

    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor = 'policy' AND action = 'secret.materialized'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(row_count, 0, "no audit row written on materialize failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn redeem_returns_plaintext_within_ttl() {
    let Some(bin_dir) = pg_bin_dir_or_skip("redeem_returns_plaintext_within_ttl") else { return; };
    skip_if_no_supervisor("redeem_returns_plaintext_within_ttl");

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;
    let kp = test_key_provider();
    kastellan_db::secrets::put(&pool, &kp, "X", b"plaintext-abc", None).await.unwrap();

    let vault = Vault::new();
    let secret_ref = vault.materialize(&pool, &kp, "X", "test").await.unwrap();

    let result = <Vault as RedeemFromVault>::redeem(&vault, &secret_ref);
    use kastellan_core::secrets::RedeemResult;
    match result {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-abc"),
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn redeem_returns_expired_past_ttl() {
    let Some(bin_dir) = pg_bin_dir_or_skip("redeem_returns_expired_past_ttl") else { return; };
    skip_if_no_supervisor("redeem_returns_expired_past_ttl");

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;
    let kp = test_key_provider();
    kastellan_db::secrets::put(&pool, &kp, "X", b"plaintext-exp", None).await.unwrap();

    let vault = Vault::with_ttl(Duration::from_millis(100));
    let secret_ref = vault.materialize(&pool, &kp, "X", "test").await.unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    use kastellan_core::secrets::RedeemResult;
    match <Vault as RedeemFromVault>::redeem(&vault, &secret_ref) {
        RedeemResult::Expired => (),
        other => panic!("expected Expired, got {other:?}"),
    }
    match <Vault as RedeemFromVault>::redeem(&vault, &secret_ref) {
        RedeemResult::NotFound => (),
        other => panic!("expected NotFound after lazy GC, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_substitutes_and_writes_redeemed_row() {
    let Some(bin_dir) = pg_bin_dir_or_skip("dispatch_substitutes_and_writes_redeemed_row") else { return; };
    skip_if_no_supervisor("dispatch_substitutes_and_writes_redeemed_row");
    let Some(sandbox_backend) = sandbox_factory_or_skip("dispatch_substitutes_and_writes_redeemed_row") else { return; };
    let Some(worker_bin) = shell_exec_binary_or_skip("dispatch_substitutes_and_writes_redeemed_row") else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;
    let kp = test_key_provider();

    // The plaintext we want the worker to receive — a unique marker so
    // the privacy-invariant test (test 7) can search the audit log
    // for it.
    let marker = "SECRET_LEAK_MARKER_xyz789";
    kastellan_db::secrets::put(&pool, &kp, "marker-secret", marker.as_bytes(), None)
        .await
        .unwrap();

    let vault = Arc::new(Vault::new());
    let secret_ref = vault
        .materialize(&pool, &kp, "marker-secret", "test")
        .await
        .unwrap();

    // Build a shell-exec worker policy that allows /usr/bin/printf so
    // the worker can echo our substituted plaintext to stdout.
    let printf_bin = "/usr/bin/printf";
    let policy = kastellan_tests_common::worker_strict_policy_allow(printf_bin);
    let spec = WorkerSpec {
        policy: &policy,
        program: std::path::Path::new(&worker_bin),
        args: &[printf_bin.to_string()],
    };
    let mut worker = tool_host::spawn_worker(&*sandbox_backend, spec)
        .expect("spawn shell-exec");

    let params = json!({
        "argv": [printf_bin, "%s\n", secret_ref.as_str()],
    });

    let result = tool_host::dispatch(&pool, &vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect("dispatch");

    let stdout = result["stdout"].as_str().expect("stdout");
    assert!(
        stdout.contains(marker),
        "worker stdout should contain substituted plaintext: got {stdout:?}"
    );

    // Audit log: 1 materialize + 1 redeemed + 1 tool row (3 in addition
    // to the bring-up rows that probe::run writes).
    let materialize_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='policy' AND action='secret.materialized'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(materialize_count, 1);

    let redeemed_rows: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT payload FROM audit_log WHERE actor='policy' AND action='secret.redeemed'",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(redeemed_rows.len(), 1);
    let p: serde_json::Value = redeemed_rows[0].try_get("payload").unwrap();
    assert_eq!(p["tool"], json!("shell-exec"));
    assert_eq!(p["method"], json!("shell.exec"));
    assert_eq!(p["ref_hash"], json!(secret_ref.ref_hash()));

    let tool_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='tool:shell-exec'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(tool_count, 1, "exactly one tool:shell-exec row");
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_fails_closed_on_missing_ref() {
    let Some(bin_dir) = pg_bin_dir_or_skip("dispatch_fails_closed_on_missing_ref") else { return; };
    skip_if_no_supervisor("dispatch_fails_closed_on_missing_ref");
    let Some(sandbox_backend) = sandbox_factory_or_skip("dispatch_fails_closed_on_missing_ref") else { return; };
    let Some(worker_bin) = shell_exec_binary_or_skip("dispatch_fails_closed_on_missing_ref") else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;

    // Empty vault — no refs materialized.
    let vault = Arc::new(Vault::new());

    let printf_bin = "/usr/bin/printf";
    let policy = kastellan_tests_common::worker_strict_policy_allow(printf_bin);
    let spec = WorkerSpec {
        policy: &policy,
        program: std::path::Path::new(&worker_bin),
        args: &[printf_bin.to_string()],
    };
    let mut worker = tool_host::spawn_worker(&*sandbox_backend, spec).unwrap();

    let synthetic_ref = "secret://00000000";
    let params = json!({"argv": [printf_bin, "%s\n", synthetic_ref]});

    let err = tool_host::dispatch(&pool, &vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect_err("dispatch must fail");

    match err {
        ToolHostError::SecretRedemptionFailed(SubstituteError::MissingRef { reason, .. }) => {
            assert_eq!(reason, MissingReason::NotFound);
        }
        other => panic!("expected SecretRedemptionFailed(MissingRef(NotFound)), got {other:?}"),
    }

    // Exactly one row: redemption_failed. No tool:shell-exec row.
    let failed_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='policy' AND action='secret.redemption_failed'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(failed_count, 1);

    let tool_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='tool:shell-exec'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(tool_count, 0, "no tool row when fail-closed");

    let failed_payload: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT payload FROM audit_log WHERE actor='policy' AND action='secret.redemption_failed'",
    ).fetch_all(&pool).await.unwrap();
    let p: serde_json::Value = failed_payload[0].try_get("payload").unwrap();
    assert_eq!(p["reason"], json!("not_found"));
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_rows_contain_no_substring_of_redeemed_plaintext() {
    // Privacy invariant. Mirrors injection-guard's
    // `policy_audit_row_contains_no_substring_of_blocked_body` pin
    // from commit 45627fd. The plaintext marker MUST NOT appear in
    // any `actor='policy'` row's serialized payload. Positive-
    // presence assertion: rows.is_empty() for secret.redeemed ALSO
    // fails — catches a regression where the chokepoint stops
    // emitting.
    let Some(bin_dir) = pg_bin_dir_or_skip("policy_rows_contain_no_substring_of_redeemed_plaintext") else { return; };
    skip_if_no_supervisor("policy_rows_contain_no_substring_of_redeemed_plaintext");
    let Some(sandbox_backend) = sandbox_factory_or_skip("policy_rows_contain_no_substring_of_redeemed_plaintext") else { return; };
    let Some(worker_bin) = shell_exec_binary_or_skip("policy_rows_contain_no_substring_of_redeemed_plaintext") else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;
    let kp = test_key_provider();

    let marker = "SECRET_LEAK_MARKER_xyz789";
    kastellan_db::secrets::put(&pool, &kp, "marker-secret", marker.as_bytes(), None).await.unwrap();

    let vault = Arc::new(Vault::new());
    let secret_ref = vault.materialize(&pool, &kp, "marker-secret", "test").await.unwrap();

    let printf_bin = "/usr/bin/printf";
    let policy = kastellan_tests_common::worker_strict_policy_allow(printf_bin);
    let spec = WorkerSpec {
        policy: &policy,
        program: std::path::Path::new(&worker_bin),
        args: &[printf_bin.to_string()],
    };
    let mut worker = tool_host::spawn_worker(&*sandbox_backend, spec).unwrap();
    let params = json!({"argv": [printf_bin, "%s\n", secret_ref.as_str()]});
    let _ = tool_host::dispatch(&pool, &vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect("dispatch");

    let policy_rows: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT payload FROM audit_log WHERE actor='policy'",
    ).fetch_all(&pool).await.unwrap();

    let redeemed_only: Vec<&sqlx::postgres::PgRow> = policy_rows
        .iter()
        .filter(|r| {
            let action: String = r.try_get("action").unwrap_or_default();
            action == "secret.redeemed"
        })
        .collect();
    assert!(
        !redeemed_only.is_empty(),
        "positive-presence assertion: at least one secret.redeemed row must exist"
    );

    for row in &policy_rows {
        let p: serde_json::Value = row.try_get("payload").unwrap();
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            !s.contains(marker),
            "privacy invariant violated — policy row payload contains the plaintext: {s}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_substitutes_multiple_refs_in_one_params() {
    let Some(bin_dir) = pg_bin_dir_or_skip("dispatch_substitutes_multiple_refs_in_one_params") else { return; };
    skip_if_no_supervisor("dispatch_substitutes_multiple_refs_in_one_params");
    let Some(sandbox_backend) = sandbox_factory_or_skip("dispatch_substitutes_multiple_refs_in_one_params") else { return; };
    let Some(worker_bin) = shell_exec_binary_or_skip("dispatch_substitutes_multiple_refs_in_one_params") else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("kastellan-test-{suffix}"),
        &format!("kastellan-test-{suffix}.log"),
        &format!("kastellan-test-{suffix}"),
    );
    let pool = cluster.runtime_pool().await;
    let kp = test_key_provider();

    kastellan_db::secrets::put(&pool, &kp, "a", b"alpha", None).await.unwrap();
    kastellan_db::secrets::put(&pool, &kp, "b", b"bravo", None).await.unwrap();

    let vault = Arc::new(Vault::new());
    let ref_a = vault.materialize(&pool, &kp, "a", "test").await.unwrap();
    let ref_b = vault.materialize(&pool, &kp, "b", "test").await.unwrap();

    let printf_bin = "/usr/bin/printf";
    let policy = kastellan_tests_common::worker_strict_policy_allow(printf_bin);
    let spec = WorkerSpec {
        policy: &policy,
        program: std::path::Path::new(&worker_bin),
        args: &[printf_bin.to_string()],
    };
    let mut worker = tool_host::spawn_worker(&*sandbox_backend, spec).unwrap();

    let params = json!({"argv": [printf_bin, "%s/%s\n", ref_a.as_str(), ref_b.as_str()]});
    let result = tool_host::dispatch(&pool, &vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect("dispatch");

    let stdout = result["stdout"].as_str().expect("stdout");
    assert!(stdout.contains("alpha/bravo"), "got stdout: {stdout:?}");

    let redeemed_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='policy' AND action='secret.redeemed'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(redeemed_count, 2, "exactly two secret.redeemed rows for two distinct refs");
}
```

**Important — tests_common helper availability check.** This test file uses `worker_strict_policy_allow` and assumes it exists in `kastellan_tests_common`. If it doesn't, the test file won't compile. Verify:

```sh
grep -n "worker_strict_policy_allow\|sandbox_factory_or_skip\|shell_exec_binary_or_skip" tests-common/src/lib.rs 2>&1 | head -10
```

If `worker_strict_policy_allow` isn't a real helper, either:
- (a) construct the policy inline in each test (copy the shape from `injection_guard_e2e.rs`), OR
- (b) add a thin pure helper to `tests-common/src/lib.rs` in this same step.

The pattern in `injection_guard_e2e.rs` is the authoritative reference — replicate its policy construction verbatim if needed.

- [ ] **Step 11: Verify the integration tests run live**

```sh
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/"
cargo test --test secret_vault_e2e 2>&1 | tail -20
```

Expected: 8 PASS, 0 SKIP. If any test SKIPs with an `eprintln!("[SKIP] ...")` line, the corresponding fixture isn't present — investigate (most likely `printf` is at `/bin/printf` rather than `/usr/bin/printf` on macOS, in which case fix the test).

Also verify the printf path is right on this Mac:

```sh
which printf && printf "%s\n" "test"
```

If `/usr/bin/printf` is wrong, update the tests to use the correct path.

- [ ] **Step 12: Full workspace verify**

```sh
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1134 failed:0 ignored:3` (1126 from step 9 of Task 3 + 8 integration tests).

- [ ] **Step 13: Commit Task 4**

```sh
git add core/src/tool_host.rs core/src/scheduler/tool_dispatch.rs core/src/scheduler/runner.rs core/src/main.rs core/tests/secret_vault_e2e.rs
git commit -m "$(cat <<'EOF'
feat(tool_host): wire opaque secret refs into dispatch chokepoint

Task 4 of opaque secret references slice 1 (HANDOVER Item 31).

tool_host::dispatch now substitutes secret://<8-hex> refs in params
before the worker call. Fail-closed: any miss / Expired / UTF-8 error
writes `policy / secret.redemption_failed` and returns the new
ToolHostError::SecretRedemptionFailed (no worker call, no tool row).
On success, one `policy / secret.redeemed` row is emitted per
substitution (best-effort, same posture as the tool row).

ToolHostError is now `#[non_exhaustive]` (first variant addition since
Option M / 2026-05-10); scheduler::tool_dispatch::map_dispatch_result
gains a SecretRedemptionFailed -> POLICY_DENIED arm.

main.rs adds an `KASTELLAN_BOOTSTRAP_SECRETS` env-var loop that
materializes each named secret at startup via OsKeyringProvider. The
ref string itself is NOT logged — only the ref_hash via
tracing::info!.

ToolHostStepDispatcher and spawn_scheduler thread an Arc<Vault>
through to dispatch; every existing call site updated.

8 integration tests in core/tests/secret_vault_e2e.rs pin the
end-to-end audit shape:
- materialize_writes_audit_row_and_returns_ref
- materialize_fails_when_secret_missing
- redeem_returns_plaintext_within_ttl
- redeem_returns_expired_past_ttl
- dispatch_substitutes_and_writes_redeemed_row
- dispatch_fails_closed_on_missing_ref (asserts NO tool row written)
- policy_rows_contain_no_substring_of_redeemed_plaintext
  (the load-bearing privacy invariant; mirrors injection-guard's
   45627fd pin)
- dispatch_substitutes_multiple_refs_in_one_params

Workspace 1126 -> 1134 (+8) on macOS with PG live, all green.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Verification, clippy, docs sync, PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (Recently completed entry + working-state count)
- Modify: `docs/devel/ROADMAP.md` (tick Item 31)

- [ ] **Step 1: Full workspace test, with and without PG**

With PG (live integration tests):

```sh
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/"
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1134 failed:0 ignored:3`.

Without PG (skip-as-pass posture for CI):

```sh
unset KASTELLAN_PG_BIN_DIR
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{ p+=$4; f+=$6; i+=$8 } END { print "passed:" p, "failed:" f, "ignored:" i }'
```

Expected: `passed:1134 failed:0 ignored:3` (integration tests skip-as-pass silently — same posture as `injection_guard_e2e`).

Re-export `KASTELLAN_PG_BIN_DIR` before continuing.

- [ ] **Step 2: Clippy check — no new warnings**

```sh
cargo clippy --workspace --all-targets 2>&1 | grep -E "warning:|error:" | head -30
```

Expected: every warning matches a pre-existing one documented in Item 30's verification step (the 10 `kastellan-core` constant-assertion warnings, 4 `MutexGuard`-across-await in `worker_lifecycle::manager::_test_slot_*`, 3 doc-list-indent in `db::probe`, 2 `io_other_error` in `kastellan-protocol`, 1 `mem_burner` `set_len()`-after-`reserve`). No new warnings introduced.

If any new warning appears, fix it before continuing.

- [ ] **Step 3: Update HANDOVER.md**

Read the top of [docs/devel/handovers/HANDOVER.md](docs/devel/handovers/HANDOVER.md) (lines 1-15). Bump the `Last updated:` header to include a new opening paragraph for this slice. The shape mirrors Item 30's:

```markdown
**Last updated:** 2026-05-28 (★ **Opaque secret references — Slice 1** — shipped on branch `feat/opaque-secret-refs-slice-1`, PR pending, closes HANDOVER Item 31. New module `core::secrets` with `Vault` (TTL'd in-process cache, default 1h, `std::sync::RwLock<HashMap>`), `SecretRef` opaque type, `RedeemResult` (Hit/Expired/NotFound), `substitute_refs_in_params` recursive walker (exact-match only). Wired into `tool_host::dispatch` chokepoint BEFORE `worker.call`: on miss/expired/non-UTF-8, fail-closed with `ToolHostError::SecretRedemptionFailed` + one `policy / secret.redemption_failed` row, **worker not called, no tool row**. On Hit, substitute in place and emit one `policy / secret.redeemed` row per ref (best-effort). Materialize writes one `policy / secret.materialized` row carrying `{name, ref_hash, ttl_secs, actor}` — **hard-fail on audit error** (asymmetric vs redeem: no materialized-but-unaudited ref can exist). `KASTELLAN_BOOTSTRAP_SECRETS=name1,name2` env var drives bootstrap-time materialization in `main.rs`. `ToolHostError` widened to `#[non_exhaustive]` (first variant addition since Option M / 2026-05-10); scheduler maps `SecretRedemptionFailed` → `POLICY_DENIED`. 4 substantive commits TDD-ordered: Task 1 module skeleton at `<HASH>`, Task 2 Vault impl at `<HASH>`, Task 3 walker at `<HASH>`, Task 4 dispatch wiring + 8 integration tests at `<HASH>`. **Workspace 1096 → 1134 (+38) on macOS with PG live**, all green; no new clippy warnings. 8 integration tests pin end-to-end audit shape; privacy invariant (`policy_rows_contain_no_substring_of_redeemed_plaintext`) mirrors injection-guard's `45627fd` precedent. Item 31 closed; **next pickup TBD from the operator-picks bucket** (Slice 2: CLI ↔ daemon IPC + `kastellan-cli secrets materialize`; `tool_host.rs` sibling-lift bundling Items 30 + 31). — earlier 2026-05-28:
```

Replace the `<HASH>` placeholders by running:

```sh
git log --oneline feat/opaque-secret-refs-slice-1 -10
```

and inserting the four commit hashes from Tasks 1-4.

Also bump the headline test-count line at ~line 1926: `1096` → `1134`.

- [ ] **Step 4: Add a Recently-completed section header**

Below the header paragraph, insert a new `## Recently completed (this session, 2026-05-28 — ★ Opaque secret references slice 1, branch ...)` section. Mirror the Item 30 entry's shape (sections at lines 51-110 in HANDOVER as of the doc-sync commit). Include:

- One-paragraph description of what shipped.
- "What shipped" task-by-task breakdown.
- Architectural notes (why substitution BEFORE injection guard; why materialize-audit is hard-fail while redeem-audit is best-effort; why the tool row's `payload.req` is allowed to carry plaintext).
- File-size watch (`tool_host.rs` 767 → ~837 LOC, 337 over the 500-LOC cap; defer sibling-lift to a separate slice bundling Items 30 + 31).
- Open follow-ups for future slices.

- [ ] **Step 5: Tick Item 31 off in ROADMAP**

Read [docs/devel/ROADMAP.md](docs/devel/ROADMAP.md) and find the Item 31 entry (currently unticked; lives near the Item 30 entry around line 165). Update:

```markdown
- [x] **Opaque secret references — Slice 1 (2026-05-28)** — shipped on branch `feat/opaque-secret-refs-slice-1`, PR pending. [... mirror Item 30's roadmap blurb shape, including 4-commit breakdown and 1096 → 1134 workspace delta ...]
```

- [ ] **Step 6: Commit the docs sync**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): claim opaque secret references slice 1

Item 31 closed on branch feat/opaque-secret-refs-slice-1.

Workspace 1096 -> 1134 (+38) on macOS with PG live, all green.
No new clippy warnings.

PR pending; HANDOVER Recently-completed section + ROADMAP Item 31
checkbox updated.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 7: Push the branch**

```sh
git push -u origin feat/opaque-secret-refs-slice-1
```

- [ ] **Step 8: Open the PR**

```sh
gh pr create --title "feat(secrets): opaque secret references slice 1" --body "$(cat <<'EOF'
## Summary

- Ships the in-process `Vault` + chokepoint substitution that lets the planner see only `secret://<8-hex>` opaque refs.
- Core substitutes refs → plaintext at `tool_host::dispatch` immediately before `worker.call`.
- Three new audit-row kinds (`secret.materialized` hard-fail, `secret.redeemed` best-effort, `secret.redemption_failed` fail-closed) — never the plaintext.
- Closes HANDOVER Item 31.

## Architecture

- New `core::secrets` module: `Vault` (TTL'd `std::sync::RwLock<HashMap>` with lazy GC), `SecretRef` opaque type, `RedeemResult` (Hit/Expired/NotFound), `substitute_refs_in_params` walker (exact-match only).
- `KASTELLAN_BOOTSTRAP_SECRETS=name1,name2` env var materializes each named secret at daemon startup via `OsKeyringProvider`. The ref string itself is never logged; only `ref_hash` (SHA-256).
- `ToolHostError` widened to `#[non_exhaustive]`; new variant `SecretRedemptionFailed`. Scheduler maps to `POLICY_DENIED`.
- Asymmetry documented inline: materialize-time audit is hard-fail (no materialized-but-unaudited ref); redeem-time audit is best-effort (don't kill a tool call with plaintext already in process memory).

## Verification

- `cargo test --workspace` on macOS with `KASTELLAN_PG_BIN_DIR=/Applications/Postgres 2.app/Contents/Versions/18/bin/`: **1134 / 0 / 3** (1096 → 1134, +38).
- `cargo clippy --workspace --all-targets`: no new warnings.
- Skip-as-pass posture preserved on hosts without PG.

## Test plan

- [x] 7 unit tests in `core::secrets::tests` (constants, `SecretRef` round-trip).
- [x] 9 unit tests in `core::secrets::vault::tests` (TTL, lifecycle, lazy GC, Drop smoke, concurrent readers).
- [x] 14 unit tests in `core::secrets::substitute::tests` (`FakeVault` covers every walker branch: top-level, nested, embedded-substring left alone, uppercase-hex left alone, 3 wrong-length tails, NotFound, Expired, non-UTF-8, empty containers, no-ref string, multiple distinct refs).
- [x] 8 integration tests in `core/tests/secret_vault_e2e.rs`:
  - materialize_writes_audit_row_and_returns_ref
  - materialize_fails_when_secret_missing
  - redeem_returns_plaintext_within_ttl
  - redeem_returns_expired_past_ttl
  - dispatch_substitutes_and_writes_redeemed_row
  - dispatch_fails_closed_on_missing_ref (asserts no tool row written)
  - policy_rows_contain_no_substring_of_redeemed_plaintext (load-bearing privacy invariant)
  - dispatch_substitutes_multiple_refs_in_one_params

## Known limitations (deferred to Slice 2)

- No CLI surface yet — operators can only stage secrets via `KASTELLAN_BOOTSTRAP_SECRETS`. Slice 2 builds CLI ↔ daemon IPC.
- Per-process (not per-task) vault scope.
- Exact-match substitution only (no embedded `"Bearer <ref>"`).
- `tool_host.rs` 767 → ~837 LOC (337 over the 500-LOC cap); sibling-lift should bundle Item 30 + Item 31.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 9: Capture the PR URL and final commit hash**

```sh
gh pr view --json url,headRefOid 2>&1 | head -5
```

Record both in the HANDOVER follow-up if the PR is reviewed and merged in this session.

---

## Summary

This plan ships Item 31 (opaque secret references) as 4 substantive TDD-ordered commits + 1 verification commit, mirroring the just-shipped Item 30's structure exactly. The chokepoint pattern (proven by Item 30 a few hours earlier) means the wiring is small and the test seam is well-precedented.

**Expected workspace test delta:** 1096 → 1134 (+38) on macOS with PG live.

**Expected file-size impact:** `tool_host.rs` 767 → ~837 LOC (337 over cap; defer sibling-lift to a separate slice bundling Items 30 + 31). All new files in `secrets/` are under cap.

**Expected branch lifetime:** 1 session.

---

## Test plan (cumulative)

- [ ] Pre-flight: baseline 1096 / 0 / 3 on `main` at `c505b36` confirmed.
- [ ] Task 1: +7 unit tests (`core::secrets::tests`) → 1103.
- [ ] Task 2: +9 unit tests (`core::secrets::vault::tests`) → 1112.
- [ ] Task 3: +14 unit tests (`core::secrets::substitute::tests`) → 1126.
- [ ] Task 4: +8 integration tests (`core/tests/secret_vault_e2e.rs`) → 1134.
- [ ] Task 5: full workspace, both with and without `KASTELLAN_PG_BIN_DIR` → both 1134 / 0 / 3.
- [ ] Task 5: `cargo clippy --workspace --all-targets` no new warnings.

---

## Self-review checklist

- [x] Every Task has at least one failing-test step before its implementation step.
- [x] Every step shows the actual code, not "fill in X".
- [x] File paths are absolute repo-relative throughout.
- [x] Type names are consistent across tasks: `Vault`, `SecretRef`, `RedeemResult` (`Hit`/`Expired`/`NotFound`), `VaultError`, `SubstituteError` (`MissingRef`/`PlaintextNotUtf8`), `MissingReason` (`NotFound`/`Expired`), `RedemptionEvent`, `RedeemFromVault`, `ToolHostError::SecretRedemptionFailed`.
- [x] Constants are pinned (`DEFAULT_TTL`, `REF_PREFIX`, `REF_HEX_LEN`) and tested.
- [x] Audit-row payloads are spelled out exactly (Section 4 of the spec, restated in dispatch wiring step).
- [x] The PG env-var workflow is in the pre-flight and the verification steps — no "this Mac doesn't have PG" drift.
- [x] The walker's exact-match-only invariant is tested by 3 negative cases (embedded substring, uppercase hex, wrong length).
- [x] The privacy invariant is tested by a positive-presence assertion (in test 7) as well as the negative substring check, mirroring `injection_guard_e2e`'s 45627fd precedent.
- [x] Fail-closed posture is tested by `dispatch_fails_closed_on_missing_ref` (zero tool rows, one redemption_failed row).

---

## Notes for the executor

- **PG env override is mandatory** during this slice. Set `KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/"` at the start of every implementing session; PG-required integration tests must pass live (not skip-as-pass).
- **`#[non_exhaustive]` on `ToolHostError` may surface match-arm errors elsewhere.** Task 4 Step 2 finds them; the most likely site is `scheduler::tool_dispatch::map_dispatch_result`. Add an `_ =>` fallback OR an explicit `SecretRedemptionFailed =>` arm — the latter is cleaner because the scheduler genuinely wants to map it to `POLICY_DENIED`.
- **`worker_strict_policy_allow` may not exist in `tests-common`.** Task 4 Step 10 says so explicitly; verify before writing the test file and adapt if needed (copy the policy-construction shape from `injection_guard_e2e.rs`).
- **`printf` path on macOS.** This Mac has `/usr/bin/printf`. The integration tests assume that path; verify with `which printf` before running.
- **No new SQL migrations.** `db::secrets` schema is unchanged; `audit_log` is unchanged. The three new `(actor, action)` pairs are wire-shape additions only.
