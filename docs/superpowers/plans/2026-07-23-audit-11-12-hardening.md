# Audit findings #11 (#389) + #12 (#388) Hardening — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the four Low-severity defense-in-depth gaps from audit findings #11 and #12 — the last two audit-#7-family siblings — as one PR.

**Architecture:** Each sub-fix is a **pure function in a reusable module** with unit tests, plus a thin wiring layer at the relevant chokepoint. Nothing changes the containment boundary; these close observability/robustness gaps around it. Spec: [`docs/superpowers/specs/2026-07-23-audit-11-12-hardening-design.md`](../specs/2026-07-23-audit-11-12-hardening-design.md).

**Tech Stack:** Rust workspace crates `kastellan-leak-scan`, `kastellan-db`, `kastellan-core`. `libc` (already a core dep), `keyring` v3, `tracing`, `sha2`. Std only otherwise — **no new dependencies**.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new dependency is added by this plan.
- **Cross-platform Linux + macOS first-class.** Sub-fix A uses `#[cfg(unix)]` (both targets are Unix); every pure function is platform-agnostic and unit-tested on both hosts.
- **Security house rule (audit-#7 family):** every security assertion must be **proved to fail against un-hardened code** before it is trusted (see Task 5).
- **Keep files < 500 LoC.** All four touched files stay well under after these additions.
- **Commits stage specific files** — never `git add -A` (untracked drafts/lockfiles must stay out).
- **Mac cargo build-lock:** the IDE's rust-analyzer holds `target/debug/.cargo-lock`, so run per-crate `cargo test` with a scratch target dir if the CLI blocks: `CARGO_TARGET_DIR=/tmp/kt-verify cargo test -p <crate> …`. Final full-workspace `cargo test --workspace` + `clippy --workspace --all-targets -D warnings` runs on the **DGX** (`ssh dgx '<cmd>'`, native aarch64, real bwrap + live PG).
- **Branch:** `fix/388-389-audit-hardening` (already created; spec + HANDOVER fix already committed).

---

### Task 1: #389.2 — merge overlapping spans in the secret scrubber

**Files:**
- Modify: `leak-scan/src/redact.rs` (`push_marker` signature, the resolve/splice block in `redact`, add a private `Run` struct)
- Test: `leak-scan/src/redact.rs` (its inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `redact(input, patterns) -> RedactOutcome` — public signature unchanged; behavior changes from "greedy drop overlaps" to "merge overlaps into one redacted run". Marker for a single-contributor run is byte-identical (`[redacted:<8hex>]`); a multi-contributor run lists distinct shas (`[redacted:<8hex1>+<8hex2>]`). `RedactOutcome.hits` gains one entry per contributing span.

- [ ] **Step 1: Rewrite the three overlap tests + add two new ones (failing)**

In `leak-scan/src/redact.rs`, **replace** the three existing overlap tests (`overlapping_candidates_resolve_earliest_start`, `overlapping_candidates_resolve_longer_span_on_tie`, `overlapping_distinct_secrets_leave_second_suffix`) with these five:

```rust
    #[test]
    fn overlapping_candidates_merge_into_one_run() {
        // "abcdefghij" contains "abcdefgh" (len 8) at [0,8) and "cdefghij"
        // (len 8) at [2,10) — they overlap, so they MERGE into one redacted run
        // covering [0,10). Both secrets are recorded; no plaintext survives.
        let a = b"abcdefgh"; // [0,8)
        let b = b"cdefghij"; // [2,10)
        let out = redact(b"abcdefghij", &[fp(a), fp(b)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 2, "both overlapping secrets recorded");
        assert_eq!(out.hits[0].offset, 0);
        assert_eq!(out.hits[1].offset, 2);
        assert_eq!(body, format!("[redacted:{}+{}]", sha8(a), sha8(b)));
        assert!(!body.contains("cdefghij"));
        assert!(!body.contains("abcdefgh"));
    }

    #[test]
    fn nested_spans_same_start_merge_into_one_run() {
        // "abcdefghijklmnop" (len 16) at [0,16) and its prefix "abcdefgh"
        // (len 8) at [0,8) merge into [0,16); both recorded. The sort puts the
        // longer span first on the start tie, so its sha leads the marker.
        let short = b"abcdefgh"; // [0,8)
        let long = b"abcdefghijklmnop"; // [0,16)
        let out = redact(b"abcdefghijklmnop", &[fp(short), fp(long)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert_eq!(body, format!("[redacted:{}+{}]", sha8(long), sha8(short)));
        assert!(!body.contains("abcdefgh"));
    }

    #[test]
    fn overlapping_distinct_secrets_are_fully_redacted() {
        // Two DISTINCT len-8 secrets overlap: A="abcdefgh" [0,8) and
        // B="fghijklm" [5,13). Merging redacts the UNION [0,13), so B's suffix
        // ("ijklm") can no longer survive — the gap the greedy resolution left.
        let a = b"abcdefgh"; // [0,8)
        let b = b"fghijklm"; // [5,13)
        let out = redact(b"abcdefghijklm", &[fp(a), fp(b)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 2, "both overlapping secrets recorded");
        assert_eq!(out.hits[0].offset, 0);
        assert_eq!(out.hits[1].offset, 5);
        assert_eq!(body, format!("[redacted:{}+{}]", sha8(a), sha8(b)));
        assert!(!body.contains("fghijklm"));
        assert!(!body.contains("ijklm"), "the suffix must NOT survive after merge");
    }

    #[test]
    fn three_overlapping_secrets_merge_into_one_run() {
        // A [0,8), B [5,13), C [10,18) form an overlap chain → one run [0,18).
        let a = b"abcdefgh"; // [0,8)
        let b = b"fghijklm"; // [5,13)
        let c = b"klmnopqr"; // [10,18)
        let out = redact(b"abcdefghijklmnopqr", &[fp(a), fp(b), fp(c)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 3);
        assert_eq!(body, format!("[redacted:{}+{}+{}]", sha8(a), sha8(b), sha8(c)));
    }

    #[test]
    fn disjoint_overlapping_pairs_stay_separate_runs() {
        // Two overlap-pairs separated by a gap → two runs, two markers.
        let a = b"abcdefgh"; // pair 1: [0,8)
        let b = b"fghijklm"; // pair 1: [5,13)
        let c = b"ABCDEFGH"; // pair 2: [15,23)
        let d = b"FGHIJKLM"; // pair 2: [20,28)
        let input = b"abcdefghijklm  ABCDEFGHIJKLM";
        let out = redact(input, &[fp(a), fp(b), fp(c), fp(d)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(body.matches("[redacted:").count(), 2, "two separate runs");
        assert_eq!(out.hits.len(), 4, "all four secrets recorded");
        assert_eq!(
            body,
            format!(
                "[redacted:{}+{}]  [redacted:{}+{}]",
                sha8(a), sha8(b), sha8(c), sha8(d)
            )
        );
    }
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test -p kastellan-leak-scan redact::tests -- --nocapture`
Expected: FAIL — the greedy code still drops overlaps, so `overlapping_candidates_merge_into_one_run` sees `hits.len() == 1` and the marker lacks the `+`.

- [ ] **Step 3: Change `push_marker` to accept multiple shas**

**Replace** the current `push_marker` (lines ~37-42) with:

```rust
/// Write the redaction marker directly into `out`. Lists each contributing
/// secret's first-8 SHA-256 hex chars (joined by `+`) so a redaction correlates
/// to the matching `secret.redeemed` audit row(s) WITHOUT leaking plaintext. A
/// single-secret run yields exactly `[redacted:<8hex>]` (unchanged format).
fn push_marker(out: &mut Vec<u8>, sha256_hex_strs: &[String]) {
    use std::fmt::Write as _;
    let mut m = String::from("[redacted:");
    for (i, s) in sha256_hex_strs.iter().enumerate() {
        if i > 0 {
            m.push('+');
        }
        let _ = write!(m, "{}", &s[..8]);
    }
    m.push(']');
    out.extend_from_slice(m.as_bytes());
}
```

- [ ] **Step 4: Add the `Run` struct and replace the resolve/splice block**

Add this private struct just above `pub fn redact` (after `push_marker`):

```rust
/// A maximal run of overlapping redaction spans, merged into one redacted
/// region so no secret byte can survive between two coincidentally-overlapping
/// secrets.
struct Run {
    start: usize,
    end: usize,
    /// Contributing (offset, len, sha256) spans, in scan order.
    spans: Vec<(usize, usize, [u8; 32])>,
}
```

Then **replace** everything from the `raw.sort_by(...)` line through the final `RedactOutcome { bytes, hits }` (the greedy `next_free`/`chosen` loop, the `if chosen.is_empty()` early return, and the splice loop) with:

```rust
    // Merge overlapping spans into maximal runs so NO secret byte can survive.
    // Sort earliest-start first, longer span first on a tie; then fold each span
    // into the current run when it STRICTLY overlaps it (`off < run.end`).
    // Adjacent spans (`off == run.end`) start a fresh run, preserving
    // back-to-back redaction. A run redacts the union of its spans — a strict
    // superset of the old greedy behaviour, so over-redaction is always safe.
    // This closes the prior gap where two DISTINCT overlapping secrets let the
    // later one's non-overlapping suffix survive in plaintext (a coincidence of
    // two high-entropy values; not adversarially reachable, since agent code
    // cannot control vault secret values). Pinned by
    // `overlapping_distinct_secrets_are_fully_redacted`.
    raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut runs: Vec<Run> = Vec::new();
    for (off, len, sha) in raw {
        match runs.last_mut() {
            Some(run) if off < run.end => {
                run.end = run.end.max(off + len);
                run.spans.push((off, len, sha));
            }
            _ => runs.push(Run {
                start: off,
                end: off + len,
                spans: vec![(off, len, sha)],
            }),
        }
    }

    if runs.is_empty() {
        return RedactOutcome {
            bytes: input.to_vec(),
            hits: Vec::new(),
        };
    }

    // Splice one marker per run. The marker lists each DISTINCT contributing
    // secret's 8-hex in first-appearance order (for a non-overlapping run that
    // is exactly one secret → byte-identical to the prior format). One RedactHit
    // is recorded per contributing span (original offsets/lens preserved), so
    // the audit trail sees every secret that appeared.
    let mut bytes = Vec::with_capacity(input.len());
    let mut hits = Vec::new();
    let mut cursor = 0usize;
    for run in runs {
        bytes.extend_from_slice(&input[cursor..run.start]);
        let mut marker_hexes: Vec<String> = Vec::new();
        let mut seen: Vec<[u8; 32]> = Vec::new();
        for (off, len, sha) in &run.spans {
            let sha_hex = sha256_hex(sha);
            if !seen.contains(sha) {
                seen.push(*sha);
                marker_hexes.push(sha_hex.clone());
            }
            hits.push(RedactHit {
                sha256_hex: sha_hex,
                offset: *off,
                len: *len,
            });
        }
        push_marker(&mut bytes, &marker_hexes);
        cursor = run.end;
    }
    bytes.extend_from_slice(&input[cursor..]);
    RedactOutcome { bytes, hits }
```

Also update the doc comment on `pub fn redact` (the "Earliest match wins; ... scanning resumes past a chosen span" sentence) to describe merge semantics:

```rust
/// Find every occurrence of any `patterns` value in `input` and replace it with
/// a `[redacted:<8hex>]` marker. Overlapping matches MERGE into one redacted
/// region (no secret byte survives an overlap); adjacent matches stay separate.
/// Empty `patterns` (or none matching) returns `input` unchanged with no hits.
/// Bounded full-buffer scan: O(input.len()) per distinct pattern length.
```

- [ ] **Step 5: Run all redact tests to verify they pass**

Run: `cargo test -p kastellan-leak-scan redact::tests`
Expected: PASS (all, including the unchanged single-secret and adjacent tests).

- [ ] **Step 6: Tighten the encoded-secret doc note (no code)**

At the top module doc of `leak-scan/src/redact.rs`, append one sentence after the existing intro:

```rust
//! An ENCODED appearance of a secret (base64/hex/url-encoded) is NOT scrubbed —
//! this matches verbatim value bytes only, as does the streaming matcher. The
//! containment boundary for encoded egress is the sandbox + egress proxy, not
//! this fingerprint scanner.
```

- [ ] **Step 7: Commit**

```bash
git add leak-scan/src/redact.rs
git commit -m "security(#389): merge overlapping spans in secret scrubber so no plaintext survives

detect + redact the UNION of overlapping secret spans instead of greedily
dropping the later one; closes the coincidental-overlap suffix leak. Single-
secret markers are byte-identical; multi-contributor runs list distinct shas.
Encoded-secret limitation documented (inherent; boundary is sandbox+proxy).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: #389.1 — keyring first-init read-back-verify + converge

**Files:**
- Modify: `db/src/secrets/key_provider.rs` (add `KeyringOps` seam + `resolve_or_init`; rewrite `ensure_initialized_for`; update the `ensure_initialized` concurrency doc)
- Test: `db/src/secrets/key_provider.rs` (its inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `SecretKey`, `KEY_LEN`, `SecretsError::{Keyring, KeyLengthInvalid}` (already imported).
- Produces: private `trait KeyringOps`, `enum KeyringOpsError { NoEntry, Other(String) }`, `enum FirstInit { ExistingKey, FreshKey, RacedConverged }`, `fn resolve_or_init(ops: &dyn KeyringOps, gen: impl FnOnce() -> [u8; KEY_LEN]) -> Result<([u8; KEY_LEN], FirstInit), SecretsError>`. `OsKeyringProvider::ensure_initialized[_for]` public behavior unchanged for the single-daemon case.

- [ ] **Step 1: Write the failing unit tests for `resolve_or_init`**

Add to the `#[cfg(test)] mod tests` block in `db/src/secrets/key_provider.rs`. First add these imports at the top of the `mod tests` (after `use super::*;`):

```rust
    use std::cell::RefCell;
    use std::collections::VecDeque;
```

Then add the fake + tests:

```rust
    /// Scripted [`KeyringOps`] fake: `get_secret` returns queued responses in
    /// order; `set_secret` records writes. A second queued get that differs from
    /// the stored write simulates a racing first-init writer.
    struct ScriptedOps {
        gets: RefCell<VecDeque<Result<Vec<u8>, KeyringOpsError>>>,
        sets: RefCell<Vec<Vec<u8>>>,
    }
    impl ScriptedOps {
        fn new(gets: Vec<Result<Vec<u8>, KeyringOpsError>>) -> Self {
            Self {
                gets: RefCell::new(gets.into()),
                sets: RefCell::new(Vec::new()),
            }
        }
    }
    impl KeyringOps for ScriptedOps {
        fn get_secret(&self) -> Result<Vec<u8>, KeyringOpsError> {
            self.gets
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(KeyringOpsError::NoEntry))
        }
        fn set_secret(&self, bytes: &[u8]) -> Result<(), KeyringOpsError> {
            self.sets.borrow_mut().push(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn resolve_returns_existing_key_without_writing() {
        let ops = ScriptedOps::new(vec![Ok(vec![7u8; KEY_LEN])]);
        let (key, outcome) = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap();
        assert_eq!(key, [7u8; KEY_LEN]);
        assert_eq!(outcome, FirstInit::ExistingKey);
        assert!(ops.sets.borrow().is_empty(), "must not write when an entry exists");
    }

    #[test]
    fn resolve_generates_and_stores_on_no_entry() {
        // NoEntry, then read-back returns exactly what we stored → FreshKey.
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Ok(vec![1u8; KEY_LEN])]);
        let (key, outcome) = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap();
        assert_eq!(key, [1u8; KEY_LEN]);
        assert_eq!(outcome, FirstInit::FreshKey);
        assert_eq!(ops.sets.borrow().as_slice(), &[vec![1u8; KEY_LEN]]);
    }

    #[test]
    fn resolve_converges_on_racing_writers_key() {
        // NoEntry, we store K1, but the read-back returns a DIFFERENT valid key
        // K2 — a racer won. We adopt K2 (converge), not keep K1.
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Ok(vec![2u8; KEY_LEN])]);
        let (key, outcome) = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap();
        assert_eq!(key, [2u8; KEY_LEN], "must adopt the winner's key");
        assert_eq!(outcome, FirstInit::RacedConverged);
        assert_eq!(ops.sets.borrow().as_slice(), &[vec![1u8; KEY_LEN]]);
    }

    #[test]
    fn resolve_rejects_existing_wrong_length() {
        let ops = ScriptedOps::new(vec![Ok(vec![0u8; 10])]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::KeyLengthInvalid { expected, got }
            if expected == KEY_LEN && got == 10));
    }

    #[test]
    fn resolve_propagates_get_error() {
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::Other("boom".into()))]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::Keyring(s) if s.contains("boom")));
    }

    #[test]
    fn resolve_errors_when_readback_wrong_length() {
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Ok(vec![0u8; 5])]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::KeyLengthInvalid { got, .. } if got == 5));
    }

    #[test]
    fn resolve_errors_when_entry_vanishes_after_set() {
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Err(KeyringOpsError::NoEntry)]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::Keyring(s) if s.contains("vanished")));
    }
```

- [ ] **Step 2: Run to verify they fail (won't compile — items undefined)**

Run: `cargo test -p kastellan-db secrets::key_provider::tests::resolve -- --nocapture`
Expected: FAIL — `resolve_or_init`, `KeyringOps`, `KeyringOpsError`, `FirstInit` are not defined yet.

- [ ] **Step 3: Add the seam + pure logic**

Insert this block into `db/src/secrets/key_provider.rs` immediately **before** `impl OsKeyringProvider {` (after the `OsKeyringProvider` struct definition, ~line 96):

```rust
/// Minimal keyring surface the first-init logic needs, so the get→set→read-back
/// decision can be unit-tested without a real keyring. Production impl
/// ([`KeyringEntryOps`]) wraps `keyring::Entry`; tests fake it and can return a
/// different read-back value to simulate a racing writer.
trait KeyringOps {
    fn get_secret(&self) -> Result<Vec<u8>, KeyringOpsError>;
    fn set_secret(&self, bytes: &[u8]) -> Result<(), KeyringOpsError>;
}

/// Errors from a [`KeyringOps`] call. `NoEntry` is modelled explicitly (it
/// drives the first-init branch); everything else is opaque.
enum KeyringOpsError {
    NoEntry,
    Other(String),
}

/// How [`resolve_or_init`] resolved, for caller-side logging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstInit {
    /// An entry already existed; its key was returned.
    ExistingKey,
    /// No entry existed; we generated, stored, and read back OUR key.
    FreshKey,
    /// No entry existed; we stored a key but the read-back returned a DIFFERENT
    /// key — a concurrent process won the first-init race, and we adopted its
    /// key so both converge. See the concurrency note on
    /// [`OsKeyringProvider::ensure_initialized`]: this catches the race only
    /// when the competing `set` lands before our read-back; it is NOT a mutex.
    RacedConverged,
}

/// Validate a raw keyring value into a fixed-size key.
fn to_key(bytes: Vec<u8>) -> Result<[u8; KEY_LEN], SecretsError> {
    if bytes.len() != KEY_LEN {
        return Err(SecretsError::KeyLengthInvalid {
            expected: KEY_LEN,
            got: bytes.len(),
        });
    }
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Pure first-init logic over a [`KeyringOps`] seam. On `NoEntry`: generate a
/// key with `gen`, store it, then READ IT BACK. If the read-back differs from
/// what we wrote, a concurrent process overwrote us — adopt its key
/// (`RacedConverged`) so both processes converge on ONE key before any secret
/// is encrypted. `gen` is injected (OsRng in production, fixed in tests), so
/// this function is deterministic under test.
fn resolve_or_init(
    ops: &dyn KeyringOps,
    gen: impl FnOnce() -> [u8; KEY_LEN],
) -> Result<([u8; KEY_LEN], FirstInit), SecretsError> {
    match ops.get_secret() {
        Ok(existing) => Ok((to_key(existing)?, FirstInit::ExistingKey)),
        Err(KeyringOpsError::NoEntry) => {
            let fresh = gen();
            ops.set_secret(&fresh)
                .map_err(|e| SecretsError::Keyring(format!("set_secret failed: {}", op_err(e))))?;
            // Read back to detect a racing writer that overwrote us.
            let after = match ops.get_secret() {
                Ok(b) => to_key(b)?,
                Err(KeyringOpsError::NoEntry) => {
                    return Err(SecretsError::Keyring(
                        "keyring entry vanished immediately after set_secret".into(),
                    ))
                }
                Err(KeyringOpsError::Other(s)) => {
                    return Err(SecretsError::Keyring(format!("read-back get_secret failed: {s}")))
                }
            };
            if after == fresh {
                Ok((fresh, FirstInit::FreshKey))
            } else {
                Ok((after, FirstInit::RacedConverged))
            }
        }
        Err(KeyringOpsError::Other(s)) => {
            Err(SecretsError::Keyring(format!("get_secret failed: {s}")))
        }
    }
}

/// Render a [`KeyringOpsError`] for an error message.
fn op_err(e: KeyringOpsError) -> String {
    match e {
        KeyringOpsError::NoEntry => "no entry".into(),
        KeyringOpsError::Other(s) => s,
    }
}

/// Production [`KeyringOps`] wrapping a real `keyring::Entry`.
struct KeyringEntryOps {
    entry: keyring::Entry,
}

impl KeyringOps for KeyringEntryOps {
    fn get_secret(&self) -> Result<Vec<u8>, KeyringOpsError> {
        match self.entry.get_secret() {
            Ok(b) => Ok(b),
            Err(keyring::Error::NoEntry) => Err(KeyringOpsError::NoEntry),
            Err(other) => Err(KeyringOpsError::Other(other.to_string())),
        }
    }
    fn set_secret(&self, bytes: &[u8]) -> Result<(), KeyringOpsError> {
        self.entry
            .set_secret(bytes)
            .map_err(|e| KeyringOpsError::Other(e.to_string()))
    }
}
```

- [ ] **Step 4: Rewrite `ensure_initialized_for` to use `resolve_or_init`**

**Replace** the body of `ensure_initialized_for` (lines ~124-157, from `let entry = ...` through the `Ok(Self { ... })`) with:

```rust
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| SecretsError::Keyring(format!("Entry::new failed: {e}")))?;
        let ops = KeyringEntryOps { entry };
        let (bytes, outcome) = resolve_or_init(&ops, || {
            let mut fresh = [0u8; KEY_LEN];
            OsRng.fill_bytes(&mut fresh);
            fresh
        })?;
        if outcome == FirstInit::RacedConverged {
            tracing::warn!(
                service,
                account,
                "concurrent keyring first-init detected; converged on the winning key. \
                 Defence-in-depth, NOT full serialisation — ensure exactly one process \
                 performs the first-ever init (see OsKeyringProvider docs)."
            );
        }
        Ok(Self {
            current_id: format!("{service}.{account}"),
            key_bytes: Zeroizing::new(bytes),
        })
```

- [ ] **Step 5: Update the `ensure_initialized` concurrency doc-comment**

**Replace** the `/// **Concurrency contract.** ...` paragraph (lines ~107-115) on `ensure_initialized` with:

```rust
    /// **Concurrency contract.** First-init does a read-back-verify: after
    /// storing a freshly generated key it reads the entry back and, if a
    /// concurrent process overwrote it, ADOPTS that process's key so both
    /// converge on one (logged at WARN). This closes the common race window but
    /// is **not** a full mutex — the read-back only catches a competing `set`
    /// that lands before it, so an unfavourable interleaving (the other
    /// process's `get` precedes our `set`) can still leave the two holding
    /// different keys. Callers must therefore still ensure exactly one process
    /// performs the first-ever initialisation. The agent's single-daemon /
    /// single-user model makes this trivially true in practice; callers spawning
    /// multiple instances must serialise the first call externally.
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p kastellan-db secrets::key_provider`
Expected: PASS (the new `resolve_*` tests plus the existing `map_key_provider_*` tests).

- [ ] **Step 7: Commit**

```bash
git add db/src/secrets/key_provider.rs
git commit -m "security(#389): keyring first-init read-back-verify + converge on race

extract get->set->read-back into a pure resolve_or_init over a KeyringOps seam;
on a detected concurrent first-init, adopt the winner's key (logged) so both
converge instead of silently diverging. Documented as defence-in-depth, not a
full mutex (the residual interleaving is stated plainly).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: #388.2 — manifest under-lock detector + spawn-path WARN

**Files:**
- Modify: `core/src/tool_host/lockdown_env.rs` (add `LockdownOverride`, `detect_lockdown_overrides`, `warn_lockdown_overrides`)
- Modify: `core/src/tool_host.rs:30` (extend the `pub use` re-export); `core/src/tool_host.rs:401` (call the warn helper)
- Modify: `core/src/worker_lifecycle/persistent.rs` (call the warn helper after its `derive_lockdown_env`)
- Test: `core/src/tool_host/lockdown_env.rs` (its inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `SandboxPolicy`, `ENV_SECCOMP_PROFILE`, `ENV_LANDLOCK_PROFILE`, `derive_lockdown_env` (in-module).
- Produces: `pub struct LockdownOverride { pub var: String, pub value: String }`, `pub fn detect_lockdown_overrides(&SandboxPolicy) -> Vec<LockdownOverride>`, `pub fn warn_lockdown_overrides(worker: &str, &SandboxPolicy)`.

- [ ] **Step 1: Write the failing detector tests**

Add to the `#[cfg(test)] mod tests` block in `core/src/tool_host/lockdown_env.rs`:

```rust
    #[test]
    fn detect_flags_seccomp_none() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "none".into()));
        let ov = detect_lockdown_overrides(&p);
        assert_eq!(ov.len(), 1);
        assert_eq!(ov[0].var, ENV_SECCOMP_PROFILE);
        assert_eq!(ov[0].value, "none");
    }

    #[test]
    fn detect_flags_landlock_none() {
        let mut p = base_policy();
        p.env.push((ENV_LANDLOCK_PROFILE.into(), "none".into()));
        let ov = detect_lockdown_overrides(&p);
        assert_eq!(ov.len(), 1);
        assert_eq!(ov[0].var, ENV_LANDLOCK_PROFILE);
    }

    #[test]
    fn detect_flags_both_disabled() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "none".into()));
        p.env.push((ENV_LANDLOCK_PROFILE.into(), "none".into()));
        assert_eq!(detect_lockdown_overrides(&p).len(), 2);
    }

    #[test]
    fn detect_empty_for_derived_default_policy() {
        // A normal policy through derive_lockdown_env gets a real seccomp
        // profile ("strict"), never "none" → nothing flagged.
        let derived = derive_lockdown_env(&base_policy());
        assert!(detect_lockdown_overrides(&derived).is_empty());
    }

    #[test]
    fn detect_empty_for_explicit_strict() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "strict".into()));
        assert!(detect_lockdown_overrides(&p).is_empty());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p kastellan-core tool_host::lockdown_env::tests::detect -- --nocapture`
Expected: FAIL — `detect_lockdown_overrides` is undefined.

- [ ] **Step 3: Add the detector + warn helper**

Insert into `core/src/tool_host/lockdown_env.rs` immediately **after** `derive_lockdown_env` (before the `#[cfg(test)] mod tests`):

```rust
/// A lockdown env entry that DISABLES a sandbox layer, weakening the
/// profile-derived default (audit #12 / #388). Produced by
/// [`detect_lockdown_overrides`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockdownOverride {
    pub var: String,
    pub value: String,
}

/// Inspect a *finalized* policy for sandbox-DISABLING lockdown env entries:
/// `KASTELLAN_SECCOMP_PROFILE` set to `"none"`/`""` (the prelude parses both as
/// "no filter") or `KASTELLAN_LANDLOCK_PROFILE` set to `"none"`. Returns one
/// entry per disabled layer; empty when nothing is weakened.
///
/// `derive_lockdown_env` honours a manifest-supplied value verbatim, so a
/// manifest author could silently under-lock a worker. This pure detector is
/// the guard the audit asked for; the spawn paths log its output at WARN (see
/// [`warn_lockdown_overrides`]). It does NOT reject — matrix legitimately sets
/// both to `none` under the `--enforce-sandbox=false` dev opt-out — it only
/// makes a sandbox-disabled spawn loud.
pub fn detect_lockdown_overrides(policy: &SandboxPolicy) -> Vec<LockdownOverride> {
    let mut out = Vec::new();
    for (k, v) in &policy.env {
        let disabled = if k == ENV_SECCOMP_PROFILE {
            matches!(v.as_str(), "none" | "")
        } else if k == ENV_LANDLOCK_PROFILE {
            v == "none"
        } else {
            false
        };
        if disabled {
            out.push(LockdownOverride {
                var: k.clone(),
                value: v.clone(),
            });
        }
    }
    out
}

/// Detect (via [`detect_lockdown_overrides`]) and log every sandbox-disabling
/// lockdown override in `policy` at WARN, naming `worker` for context. The one
/// place the log format lives, so the spawn paths that call it
/// (`tool_host::spawn_worker`, `worker_lifecycle::persistent`) cannot drift.
pub fn warn_lockdown_overrides(worker: &str, policy: &SandboxPolicy) {
    for ov in detect_lockdown_overrides(policy) {
        tracing::warn!(
            worker,
            var = %ov.var,
            value = %ov.value,
            "worker spawns with a sandbox layer DISABLED via a policy.env override"
        );
    }
}
```

- [ ] **Step 4: Extend the `tool_host` re-export**

In `core/src/tool_host.rs:30`, **replace** the existing `pub use lockdown_env::{...}` line with:

```rust
pub use lockdown_env::{derive_lockdown_env, detect_lockdown_overrides, warn_lockdown_overrides, LockdownOverride, ENV_CPU_MS, ENV_LANDLOCK_PROFILE, ENV_LANDLOCK_RO, ENV_LANDLOCK_RW, ENV_SECCOMP_PROFILE};
```

- [ ] **Step 5: Wire the WARN into `spawn_worker`**

In `core/src/tool_host.rs`, in `spawn_worker` (line ~401), **replace**:

```rust
    let derived = derive_lockdown_env(spec.policy);
```

with:

```rust
    let derived = derive_lockdown_env(spec.policy);
    // #388.2: surface any manifest that disabled a sandbox layer via policy.env.
    warn_lockdown_overrides(spec.program, &derived);
```

- [ ] **Step 6: Wire the WARN into the persistent-worker path**

In `core/src/worker_lifecycle/persistent.rs`, in `ClientTransport::spawn`, **replace**:

```rust
        let derived = crate::tool_host::derive_lockdown_env(policy);
```

with:

```rust
        let derived = crate::tool_host::derive_lockdown_env(policy);
        // #388.2: the matrix channel's `--enforce-sandbox=false` dev opt-out
        // flows through here; make a sandbox-disabled persistent worker loud.
        crate::tool_host::warn_lockdown_overrides(program, &derived);
```

- [ ] **Step 7: Run the detector tests + confirm the crate builds**

Run: `cargo test -p kastellan-core tool_host::lockdown_env`
Expected: PASS (new `detect_*` tests + existing `derive_*` tests). If the Mac CLI blocks on the build-lock, prefix `CARGO_TARGET_DIR=/tmp/kt-verify`.

- [ ] **Step 8: Commit**

```bash
git add core/src/tool_host/lockdown_env.rs core/src/tool_host.rs core/src/worker_lifecycle/persistent.rs
git commit -m "security(#388): warn when a manifest disables a sandbox layer via policy.env

pure detect_lockdown_overrides flags KASTELLAN_SECCOMP/LANDLOCK_PROFILE=none;
warn_lockdown_overrides logs each at WARN from both spawn paths (spawn_worker +
persistent). Derive-then-warn, not reject — matrix's --enforce-sandbox=false
dev opt-out stays valid but is now loudly logged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: #388.1 — worker-discovery install-dir trust probe

**Files:**
- Modify: `core/src/worker_manifest.rs` (add `InstallDirTrust`, `InstallDirFacts`, `assess_install_dir`)
- Modify: `core/src/worker_lifecycle/force_route.rs:422` (`pub(crate) fn env_flag_enabled` → `pub fn`)
- Modify: `core/src/main.rs` (add `probe_install_dir_trust` fn + call it after `exe_dir` derivation)
- Test: `core/src/worker_manifest.rs` (its inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `kastellan_core::worker_lifecycle::force_route::env_flag_enabled` (widened to `pub`); `libc::geteuid`; `std::os::unix::fs::MetadataExt`.
- Produces: `pub enum InstallDirTrust { Trusted, Untrusted { reason: String } }`, `pub struct InstallDirFacts { pub owner_uid: u32, pub mode: u32 }`, `pub fn assess_install_dir(self_euid: u32, &InstallDirFacts) -> InstallDirTrust`.

- [ ] **Step 1: Write the failing classifier tests**

Add to the `#[cfg(test)] mod tests` block in `core/src/worker_manifest.rs`:

```rust
    #[test]
    fn self_owned_0755_is_trusted() {
        // The normal per-user install: owned by self, drwxr-xr-x.
        let facts = InstallDirFacts { owner_uid: 1000, mode: 0o040755 };
        assert_eq!(assess_install_dir(1000, &facts), InstallDirTrust::Trusted);
    }

    #[test]
    fn root_owned_0755_is_trusted() {
        let facts = InstallDirFacts { owner_uid: 0, mode: 0o040755 };
        assert_eq!(assess_install_dir(1000, &facts), InstallDirTrust::Trusted);
    }

    #[test]
    fn world_writable_is_untrusted() {
        let facts = InstallDirFacts { owner_uid: 1000, mode: 0o040757 };
        assert!(matches!(assess_install_dir(1000, &facts), InstallDirTrust::Untrusted { .. }));
    }

    #[test]
    fn group_writable_is_untrusted() {
        let facts = InstallDirFacts { owner_uid: 1000, mode: 0o040775 };
        assert!(matches!(assess_install_dir(1000, &facts), InstallDirTrust::Untrusted { .. }));
    }

    #[test]
    fn owned_by_other_nonroot_uid_is_untrusted() {
        let facts = InstallDirFacts { owner_uid: 1234, mode: 0o040755 };
        assert!(matches!(assess_install_dir(1000, &facts), InstallDirTrust::Untrusted { .. }));
    }

    #[test]
    fn world_writable_beats_root_ownership() {
        // Writability dominates: even root-owned but world-writable is untrusted
        // (e.g. a /tmp-like sticky 1777 dir must never host worker binaries).
        let facts = InstallDirFacts { owner_uid: 0, mode: 0o041777 };
        assert!(matches!(assess_install_dir(1000, &facts), InstallDirTrust::Untrusted { .. }));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p kastellan-core worker_manifest::tests -- --nocapture`
Expected: FAIL — `assess_install_dir`, `InstallDirFacts`, `InstallDirTrust` undefined.

- [ ] **Step 3: Add the classifier**

Insert into `core/src/worker_manifest.rs` immediately **after** `discover_binary` (before the `#[cfg(test)] mod tests`):

```rust
/// Trust verdict for the directory workers are discovered from (audit #12 /
/// #388). See [`assess_install_dir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallDirTrust {
    Trusted,
    Untrusted { reason: String },
}

/// Ownership + permission facts about the install directory, read from Unix
/// metadata by the caller. A plain integer struct so [`assess_install_dir`] is
/// pure and testable without a real filesystem.
#[derive(Clone, Copy, Debug)]
pub struct InstallDirFacts {
    /// Owning uid (`MetadataExt::uid`).
    pub owner_uid: u32,
    /// Permission + type bits (`MetadataExt::mode`).
    pub mode: u32,
}

/// Classify the install directory's trust. **Untrusted** iff it is writable by
/// a principal OTHER than root or the daemon's own euid — i.e. someone who
/// could drop a malicious `kastellan-worker-*` sibling that [`discover_binary`]
/// would register on restart:
///   - world-writable (`mode & 0o002`), OR
///   - group-writable (`mode & 0o020`), OR
///   - owned by a uid that is neither 0 (root) nor `self_euid`.
///
/// The normal per-user install (`~/.local/lib/kastellan`, owned by the daemon
/// user, mode 0755) is **Trusted** — writability by the daemon's own user is
/// already inside the threat-model boundary (a compromise there owns the worker
/// slot regardless). Defence-in-depth backstop for the documented "install dir
/// must not be user-writable" deploy assumption; pure, so unit-tested on both
/// hosts.
pub fn assess_install_dir(self_euid: u32, facts: &InstallDirFacts) -> InstallDirTrust {
    let perms = facts.mode & 0o777;
    if facts.mode & 0o002 != 0 {
        return InstallDirTrust::Untrusted {
            reason: format!("world-writable (mode {perms:04o})"),
        };
    }
    if facts.mode & 0o020 != 0 {
        return InstallDirTrust::Untrusted {
            reason: format!("group-writable (mode {perms:04o})"),
        };
    }
    if facts.owner_uid != 0 && facts.owner_uid != self_euid {
        return InstallDirTrust::Untrusted {
            reason: format!(
                "owned by uid {} (neither root nor the daemon's euid {self_euid})",
                facts.owner_uid
            ),
        };
    }
    InstallDirTrust::Trusted
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p kastellan-core worker_manifest::tests`
Expected: PASS.

- [ ] **Step 5: Widen `env_flag_enabled` to `pub`**

In `core/src/worker_lifecycle/force_route.rs:422`, change:

```rust
pub(crate) fn env_flag_enabled(value: Option<String>) -> bool {
```

to:

```rust
pub fn env_flag_enabled(value: Option<String>) -> bool {
```

(Reused by the daemon binary for `KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR` so the strict-mode flag honours the same `1|true|yes|on` dialect as every other opt-in flag — #459 residual.)

- [ ] **Step 6: Add the startup probe to `main.rs`**

Add these two functions to `core/src/main.rs` at module level (e.g. just above `async fn main`):

```rust
/// #388.1: install-dir trust probe. Defence-in-depth backstop for the
/// documented "install dir must not be user-writable" deploy assumption: warn
/// (or, with `KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR` set, fail closed) when the
/// directory workers are discovered from is writable by a principal other than
/// root or the daemon's own euid. The normal per-user install
/// (`~/.local/lib/kastellan`, self-owned 0755) passes silently.
#[cfg(unix)]
fn probe_install_dir_trust(exe_dir: Option<&std::path::Path>) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(dir) = exe_dir else { return Ok(()) };
    let meta = match std::fs::metadata(dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e,
                "could not stat install dir for the trust probe (continuing)");
            return Ok(());
        }
    };
    let facts = kastellan_core::worker_manifest::InstallDirFacts {
        owner_uid: meta.uid(),
        mode: meta.mode(),
    };
    // SAFETY: geteuid() has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if let kastellan_core::worker_manifest::InstallDirTrust::Untrusted { reason } =
        kastellan_core::worker_manifest::assess_install_dir(euid, &facts)
    {
        let strict = kastellan_core::worker_lifecycle::force_route::env_flag_enabled(
            std::env::var("KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR").ok(),
        );
        if strict {
            anyhow::bail!(
                "install dir {} is untrusted ({reason}); refusing to start because \
                 KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR is set",
                dir.display()
            );
        }
        tracing::error!(
            dir = %dir.display(), reason = %reason,
            "install dir is writable by a principal other than root/self; a malicious \
             sibling worker binary could be registered on restart. Set \
             KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR=1 to fail closed."
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn probe_install_dir_trust(_exe_dir: Option<&std::path::Path>) -> anyhow::Result<()> {
    Ok(())
}
```

Then, in `async fn main`, immediately **after** the `exe_dir` derivation (the `let exe_dir = std::env::current_exe()...` block, ~line 118), add:

```rust
    // #388.1: probe the install dir before wiring worker discovery off it.
    probe_install_dir_trust(exe_dir.as_deref())?;
```

- [ ] **Step 7: Build the daemon binary to confirm wiring compiles**

Run: `cargo build -p kastellan-core --bin kastellan` (Mac; prefix `CARGO_TARGET_DIR=/tmp/kt-verify` if the build-lock blocks).
Expected: builds clean.

- [ ] **Step 8: Commit**

```bash
git add core/src/worker_manifest.rs core/src/worker_lifecycle/force_route.rs core/src/main.rs
git commit -m "security(#388): probe install-dir trust at startup (warn + opt-in strict)

pure assess_install_dir flags an install dir writable by a principal other than
root/self (world/group-writable or foreign owner); the normal per-user install
passes. main.rs logs a loud ERROR, or fails closed under
KASTELLAN_REQUIRE_TRUSTED_INSTALL_DIR=1 (unified env_flag_enabled dialect).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Integration verification (prove-it-fails + workspace + DGX)

**Files:** none changed (verification + negative-case proofs). If a proof reveals a real gap, fix it under the relevant task and re-commit.

- [ ] **Step 1: Prove each security assertion fails against un-hardened code**

For each, temporarily revert the fix, confirm the guard test FAILS, then restore (audit-#7 house rule). Do this with a scratch worktree/stash or an in-place edit-then-revert — do NOT commit the reverts.

- **#389.2 merge:** restore the greedy `next_free`/`chosen` loop → `overlapping_distinct_secrets_are_fully_redacted` must FAIL (the `ijklm` suffix survives). Restore.
- **#389.1 converge:** in `resolve_or_init`, drop the read-back branch (return `(fresh, FreshKey)` unconditionally) → `resolve_converges_on_racing_writers_key` must FAIL. Restore.
- **#388.2 detect:** make `detect_lockdown_overrides` always return `vec![]` → `detect_flags_seccomp_none` must FAIL. Restore.
- **#388.1 probe:** make `assess_install_dir` always return `Trusted` → `world_writable_is_untrusted` must FAIL. Restore.

- [ ] **Step 2: Full per-crate test run on the Mac**

Run (prefix `CARGO_TARGET_DIR=/tmp/kt-verify` if needed):
```
cargo test -p kastellan-leak-scan
cargo test -p kastellan-db secrets
cargo test -p kastellan-core tool_host::lockdown_env worker_manifest
```
Expected: all PASS.

- [ ] **Step 3: DGX full-workspace acceptance gate**

Run (writes the log to `~` per the DGX /tmp-scrub gotcha, not `/tmp`):
```bash
ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && git fetch origin && git checkout fix/388-389-audit-hardening && git pull --ff-only && setsid bash -lc "cargo test --workspace > ~/dgx-388389.log 2>&1; echo DONE_EXIT=\$? >> ~/dgx-388389.log" </dev/null & echo launched'
```
Then poll `ssh dgx 'tail -5 ~/dgx-388389.log'` until `DONE_EXIT=0`. Also run:
```bash
ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo clippy --workspace --all-targets -- -D warnings'
```
Expected: roughly `cargo test --workspace` = **~2656 / 0 / 50** (the 2636 `main`+#387 baseline + the new always-running tests: redact **+2 net** [3 rewritten, 2 added], keyring **+7** `resolve_*`, lockdown-env **+5** `detect_*`, worker-manifest **+6** `assess_*` = +20). **Read the exact delta from the actual run and record it — do not assume; the figure above is the expectation to verify, not a fact to assert.** clippy clean, **0 `[SKIP]`**. Fix any warning/failure before proceeding.

- [ ] **Step 4: Update HANDOVER.md + ROADMAP.md (session-end)**

Move #388/#389 into the merged/recently-completed sections of both docs, record the new DGX baseline count, and prune to stay concise. (Handled per the closeout, not a code commit here.)

- [ ] **Step 5: Push + open the PR**

```bash
git push -u origin fix/388-389-audit-hardening
gh pr create --base main --title "security: harden worker-discovery, lockdown-env, keyring init, and secret scrub (closes #388, #389)" --body "<summary + closes #388 #389 + DGX counts>"
```

---

## Self-Review

**Spec coverage:**
- #388.1 install-dir probe → Task 4 ✔ (pure `assess_install_dir` + warn/strict wiring).
- #388.2 under-lock warn → Task 3 ✔ (`detect_lockdown_overrides` + `warn_lockdown_overrides` at both spawn paths).
- #389.1 keyring race → Task 2 ✔ (`resolve_or_init` + converge + honest-limitation doc).
- #389.2 scrub merge → Task 1 ✔ (merge runs + encoded-secret doc note).
- Prove-it-fails house rule → Task 5 Step 1 ✔.
- Cross-platform (`#[cfg(unix)]`, pure fns both hosts) → Tasks 4 + Global Constraints ✔.

**Placeholder scan:** none — every code step shows complete code. The PR body in Task 5 Step 5 is intentionally a fill-at-time summary (the counts come from the actual DGX run).

**Test-count note:** Task 1 rewrites 3 existing overlap tests and adds 2 → **net +2** in `redact`; Tasks 2/3/4 add +7/+5/+6. Net new always-running tests ≈ **+20**. The DGX baseline delta must be **read from the actual run**, not assumed (Task 5 Step 3 says so explicitly) — the `2643` figure is the expectation to verify, not a fact to assert.

**Type consistency:** `InstallDirFacts { owner_uid: u32, mode: u32 }`, `assess_install_dir(u32, &InstallDirFacts)`, `detect_lockdown_overrides(&SandboxPolicy) -> Vec<LockdownOverride>`, `warn_lockdown_overrides(&str, &SandboxPolicy)`, `resolve_or_init(&dyn KeyringOps, impl FnOnce() -> [u8; KEY_LEN]) -> Result<([u8; KEY_LEN], FirstInit), SecretsError>`, `push_marker(&mut Vec<u8>, &[String])` — used consistently across steps.
